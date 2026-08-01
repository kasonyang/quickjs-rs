mod compile;
//TODO no pub?
pub mod convert;
mod droppable_value;
//TODO no pub?
pub mod value;

use std::{collections::HashMap, ffi::{CStr, CString}, os::raw::{c_int, c_void}, sync::Mutex};
use std::any::Any;
use std::cell::{Cell, UnsafeCell};
use std::ptr::{null_mut};
use std::rc::Rc;
use anyhow::Context;
use libquickjs_sys as q;
use libquickjs_sys::{JS_EVAL_TYPE_MODULE, JSClassID, JSContext, JSValue, JS_VALUE_GET_PTR};

use crate::{callback::{Arguments, Callback}, console::ConsoleBackend, ContextError, ExecutionError, JsValue, ResourceValue, ValueError};

use value::{JsFunction, OwnedJsObject};

pub use value::{JsCompiledFunction, OwnedJsValue};
use crate::bindings::convert::deserialize_value;
use crate::exception::{HostPromiseRejectionTracker, HostPromiseRejectionTrackerWrapper};
use crate::loader::{quickjs_rs_module_loader, quickjs_rs_module_normalize_func, JsModuleLoader, ModuleLoaderData, NoopModuleLoader};

// JS_TAG_* constants from quickjs.
// For some reason bindgen does not pick them up.
#[cfg(feature = "bigint")]
const TAG_BIG_INT: i64 = -10;
const TAG_STRING: i64 = -7;
const TAG_FUNCTION_BYTECODE: i64 = -2;
const TAG_OBJECT: i64 = -1;
const TAG_INT: i64 = 0;
const TAG_BOOL: i64 = 1;
const TAG_NULL: i64 = 2;
const TAG_UNDEFINED: i64 = 3;
pub const TAG_EXCEPTION: i64 = 6;
const TAG_FLOAT64: i64 = 7;

extern "C" fn host_promise_rejection_tracker(
    ctx: *mut JSContext,
    promise: JSValue,
    reason: JSValue,
    is_handled: bool,
    opaque: *mut ::std::os::raw::c_void,
) {
    let promise =  deserialize_value(ctx, &promise).unwrap();
    let reason = deserialize_value(ctx, &reason).unwrap();
    let mut opaque = opaque as *mut HostPromiseRejectionTrackerWrapper;
    unsafe {
        (*opaque).tracker.track_promise_rejection(promise, reason, is_handled);
    }
}

/// Helper for creating CStrings.
pub fn make_cstring(value: impl Into<Vec<u8>>) -> Result<CString, ValueError> {
    CString::new(value).map_err(ValueError::StringWithZeroBytes)
}

pub struct ClassId {
    id: Cell<JSClassID>,
}

pub struct ResourceObject {
    pub data: ResourceValue,
}

unsafe impl Send for ClassId {}
unsafe impl Sync for ClassId {}

impl ClassId {
    pub const fn new() -> Self {
        ClassId {
            id: Cell::new(0)
        }
    }
}

trait JsClass {
    const NAME: &'static str;

    fn class_id() -> Rc<ClassId>;

}

thread_local! {
    static CLASS_ID: Rc<ClassId> = Rc::new(ClassId::new());
}

struct Resource;

impl JsClass for Resource {
    const NAME: &'static str = "Resource";

    fn class_id() -> Rc<ClassId> {
        CLASS_ID.with(|c| c.clone())
    }
}

type WrappedCallback = dyn Fn(c_int, *mut q::JSValue) -> q::JSValue;

/// Taken from: https://s3.amazonaws.com/temp.michaelfbryan.com/callbacks/index.html
///
/// Create a C wrapper function for a Rust closure to enable using it as a
/// callback function in the Quickjs runtime.
///
/// Both the boxed closure and the boxed data are returned and must be stored
/// by the caller to guarantee they stay alive.
unsafe fn build_closure_trampoline<F>(
    closure: F,
) -> ((Box<WrappedCallback>, Box<q::JSValue>), q::JSCFunctionData)
where
    F: Fn(c_int, *mut q::JSValue) -> q::JSValue + 'static,
{
    unsafe extern "C" fn trampoline<F>(
        _ctx: *mut q::JSContext,
        _this: q::JSValue,
        argc: c_int,
        argv: *mut q::JSValue,
        _magic: c_int,
        data: *mut q::JSValue,
    ) -> q::JSValue
    where
        F: Fn(c_int, *mut q::JSValue) -> q::JSValue,
    {
        let closure_ptr = JS_VALUE_GET_PTR(*data);
        let closure: &mut F = &mut *(closure_ptr as *mut F);
        (*closure)(argc, argv)
    }

    let boxed_f = Box::new(closure);

    let data = Box::new(
        q::JS_MKPTR(q::JS_TAG_NULL, (&*boxed_f) as *const F as *mut c_void)
    );

    ((boxed_f, data), Some(trampoline::<F>))
}

/// OwnedValueRef wraps a Javascript value from the quickjs runtime.
/// It prevents leaks by ensuring that the inner value is deallocated on drop.
pub struct OwnedValueRef<'a> {
    context: &'a ContextWrapper,
    value: q::JSValue,
}

impl<'a> Drop for OwnedValueRef<'a> {
    fn drop(&mut self) {
        unsafe {
            q::JS_FreeValue(self.context.context, self.value);
        }
    }
}

impl<'a> Clone for OwnedValueRef<'a> {
    fn clone(&self) -> Self {
        Self::new_dup(self.context, self.value)
    }
}

impl<'a> std::fmt::Debug for OwnedValueRef<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        unsafe {
            match q::JS_VALUE_GET_TAG(self.value) {
                q::JS_TAG_EXCEPTION => write!(f, "Exception(?)"),
                q::JS_TAG_NULL => write!(f, "NULL"),
                q::JS_TAG_UNDEFINED => write!(f, "UNDEFINED"),
                q::JS_TAG_BOOL => write!(f, "Bool(?)",),
                q::JS_TAG_INT => write!(f, "Int(?)"),
                q::JS_TAG_FLOAT64 => write!(f, "Float(?)"),
                q::JS_TAG_STRING => write!(f, "String(?)"),
                q::JS_TAG_OBJECT => write!(f, "Object(?)"),
                q::JS_TAG_FUNCTION_BYTECODE => write!(f, "Bytecode(?)"),
                _ => write!(f, "?"),
            }
        }

    }
}

impl<'a> OwnedValueRef<'a> {
    pub fn new(context: &'a ContextWrapper, value: q::JSValue) -> Self {
        Self { context, value }
    }
    pub fn new_dup(context: &'a ContextWrapper, value: q::JSValue) -> Self {
        let ret = Self::new(context, value);
        unsafe { q::JS_DupValue(ret.context.context, ret.value) };
        ret
    }

    /// Get the inner JSValue without freeing in drop.
    ///
    /// Unsafe because the caller is responsible for freeing the returned value.
    unsafe fn into_inner(self) -> q::JSValue {
        let v = self.value;
        std::mem::forget(self);
        v
    }

    /// Get the inner JSValue without increasing ref count
    pub(crate) fn as_inner(&self) -> &q::JSValue {
        &self.value
    }

    /// Get the inner JSValue while increasing ref count, this is handy when you pass a JSValue to a new owner like e.g. setProperty
    #[allow(dead_code)]
    pub(crate) fn as_inner_dup(&self) -> &q::JSValue {
        unsafe { q::JS_DupValue(self.context.context, self.value) };
        &self.value
    }

    pub fn is_null(&self) -> bool {
        q::JS_IsNull(self.value)
    }

    pub fn is_bool(&self) -> bool {
        q::JS_IsBool(self.value)
    }

    pub fn is_exception(&self) -> bool {
        q::JS_IsException(self.value)
    }

    pub fn is_object(&self) -> bool {
        q::JS_IsObject(self.value)
    }

    pub fn is_string(&self) -> bool {
        q::JS_IsString(self.value)
    }

    pub fn is_compiled_function(&self) -> bool {
        q::JS_VALUE_GET_TAG(self.value) == q::JS_TAG_FUNCTION_BYTECODE
    }

    pub fn to_string(&self) -> Result<String, ExecutionError> {
        let value = if self.is_string() {
            self.to_value()?
        } else {
            let raw = unsafe { q::JS_ToString(self.context.context, self.value) };
            let value = OwnedValueRef::new(self.context, raw);

            if !value.is_string() {
                return Err(ExecutionError::Exception(
                    "Could not convert value to string".into(),
                ));
            }
            value.to_value()?
        };

        Ok(value.as_str().unwrap().to_string())
    }

    pub fn to_value(&self) -> Result<JsValue, ValueError> {
        self.context.to_value(&self.value)
    }

    pub fn to_bool(&self) -> Result<bool, ValueError> {
        match self.to_value()? {
            JsValue::Bool(b) => Ok(b),
            _ => Err(ValueError::UnexpectedType),
        }
    }

    #[cfg(test)]
    pub fn get_ref_count(&self) -> i32 {
        if q::JS_VALUE_GET_TAG(self.value) < 0 {
            // This transmute is OK since if tag < 0, the union will be a refcount
            // pointer.
            let ptr = unsafe { q::JS_VALUE_GET_PTR(self.value) as *mut q::JSRefCountHeader };
            let pref: &mut q::JSRefCountHeader = &mut unsafe { *ptr };
            pref.ref_count
        } else {
            -1
        }
    }
}

/// Wraps an object from the quickjs runtime.
/// Provides convenience property accessors.
pub struct OwnedObjectRef<'a> {
    value: OwnedValueRef<'a>,
}

impl<'a> OwnedObjectRef<'a> {
    pub fn new(value: OwnedValueRef<'a>) -> Result<Self, ValueError> {
        if !value.is_object() {
            Err(ValueError::Internal("Expected an object".into()))
        } else {
            Ok(Self { value })
        }
    }

    fn into_value(self) -> OwnedValueRef<'a> {
        self.value
    }

    /// Get the tag of a property.
    fn property_tag(&self, name: &str) -> Result<i64, ValueError> {
        let cname = make_cstring(name)?;
        let raw = unsafe {
            q::JS_GetPropertyStr(self.value.context.context, self.value.value, cname.as_ptr())
        };
        let t = unsafe {
            q::JS_VALUE_GET_TAG(raw)
        };
        unsafe {
            q::JS_FreeValue(self.value.context.context, raw);
        }
        Ok(t as i64)
    }

    /// Determine if the object is a promise by checking the presence of
    /// a 'then' and a 'catch' property.
    fn is_promise(&self) -> Result<bool, ValueError> {
        if self.property_tag("then")? == TAG_OBJECT && self.property_tag("catch")? == TAG_OBJECT {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn property(&self, name: &str) -> Result<OwnedValueRef<'a>, ExecutionError> {
        let cname = make_cstring(name)?;
        let raw = unsafe {
            q::JS_GetPropertyStr(self.value.context.context, self.value.value, cname.as_ptr())
        };

        if q::JS_IsException(raw) {
            Err(ExecutionError::Internal(format!(
                "Exception while getting property '{}'",
                name
            )))
        } else if q::JS_IsUndefined(raw) {
            Err(ExecutionError::Internal(format!(
                "Property '{}' not found",
                name
            )))
        } else {
            Ok(OwnedValueRef::new(self.value.context, raw))
        }
    }

    // Set a property on an object.
    // NOTE: this method takes ownership of the `JSValue`, so it must not be
    // freed later.
    unsafe fn set_property_raw(&self, name: &str, value: q::JSValue) -> Result<(), ExecutionError> {
        let cname = make_cstring(name)?;
        let ret = q::JS_SetPropertyStr(
            self.value.context.context,
            self.value.value,
            cname.as_ptr(),
            value,
        );
        if ret < 0 {
            Err(ExecutionError::Exception("Could not set property".into()))
        } else {
            Ok(())
        }
    }

    pub fn set_property(&self, name: &str, value: JsValue) -> Result<(), ExecutionError> {
        let qval = self.value.context.serialize_value(value)?;
        unsafe {
            // set_property_raw takes ownership, so we must prevent a free.
            self.set_property_raw(name, qval.extract())?;
        }
        Ok(())
    }
}

/// Data stored in context opaque for native module init callbacks.
/// Each entry maps a module name to its list of (export_name, func_value) pairs.
struct ModuleExportsData {
    exports: HashMap<String, Vec<(CString, q::JSValue)>>,
}

impl Drop for ModuleExportsData {
    fn drop(&mut self) {
        // Note: JS_FreeValue cannot be called here because we don't have a context.
        // The JSValues are freed when the context is freed (they are owned by the module's var_ref).
    }
}

/// extern "C" init callback for native modules.
/// Called during module evaluation when var_ref is set up.
unsafe extern "C" fn native_module_init(
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
) -> c_int {
    let opaque = q::JS_GetContextOpaque(ctx);
    if opaque.is_null() {
        return -1;
    }
    let data = &*(opaque as *mut ModuleExportsData);

    // Get module name from the module definition
    let module_atom = q::JS_GetModuleName(ctx, m);
    let module_name_ptr = q::JS_AtomToCString(ctx, module_atom);
    if module_name_ptr.is_null() {
        q::JS_FreeAtom(ctx, module_atom);
        return -1;
    }
    let module_name = match CStr::from_ptr(module_name_ptr).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            q::JS_FreeCString(ctx, module_name_ptr);
            q::JS_FreeAtom(ctx, module_atom);
            return -1;
        }
    };
    q::JS_FreeCString(ctx, module_name_ptr);
    q::JS_FreeAtom(ctx, module_atom);

    if let Some(exports) = data.exports.get(&module_name) {
        for (name, func_value) in exports {
            let duped = q::JS_DupValue(ctx, *func_value);
            q::JS_SetModuleExport(ctx, m, name.as_ptr(), duped);
        }
    }
    0
}

/// Wraps a quickjs context.
///
/// Cleanup of the context happens in drop.
pub struct ContextWrapper {
    runtime: *mut q::JSRuntime,
    pub(crate) context: *mut q::JSContext,
    /// Stores callback closures and quickjs data pointers.
    /// This array is write-only and only exists to ensure the lifetime of
    /// the closure.
    // A Mutex is used over a RefCell because it needs to be unwind-safe.
    callbacks: Mutex<Vec<(Box<WrappedCallback>, Box<q::JSValue>)>>,
    module_loader_data: Option<*mut ModuleLoaderData>,
    host_promise_rejection_tracker_wrapper: Option<*mut HostPromiseRejectionTrackerWrapper>,
    /// Stores native C module definitions by name for import resolution.
    native_modules: Box<Mutex<HashMap<String, *mut q::JSModuleDef>>>,
    /// Stores native module export data for init callbacks (pointed to by context opaque).
    module_exports_data: UnsafeCell<Option<*mut ModuleExportsData>>,
}

impl Drop for ContextWrapper {
    fn drop(&mut self) {
        unsafe {
            {
                if let Some(p) = self.host_promise_rejection_tracker_wrapper {
                    let _ = Box::from_raw(p);
                }
            }
            {
                if let Some(p) = *self.module_exports_data.get() {
                    // Free the JSValues stored in the export data
                    let data = &*p;
                    for exports in data.exports.values() {
                        for (_, func_value) in exports {
                            q::JS_FreeValue(self.context, *func_value);
                        }
                    }
                    let _ = Box::from_raw(p);
                }
            }
            q::JS_FreeContext(self.context);
            q::JS_FreeRuntime(self.runtime);
        }
    }
}

impl ContextWrapper {
    /// Initialize a wrapper by creating a JSRuntime and JSContext.
    pub fn new(memory_limit: Option<usize>) -> Result<Self, ContextError> {
        let runtime = unsafe { q::JS_NewRuntime() };
        if runtime.is_null() {
            return Err(ContextError::RuntimeCreationFailed);
        }

        // Configure memory limit if specified.
        if let Some(limit) = memory_limit {
            unsafe {
                q::JS_SetMemoryLimit(runtime, limit as _);
            }
        }

        unsafe  {
            //js_std_set_worker_new_context_func(JS_NewCustomContext);
            //js_std_init_handlers(runtime);
        }

        let context = unsafe { q::JS_NewContext(runtime) };
        if context.is_null() {
            unsafe {
                q::JS_FreeRuntime(runtime);
            }
            return Err(ContextError::ContextCreationFailed);
        }

        // Initialize the promise resolver helper code.
        // This code is needed by Self::resolve_value
        let mut wrapper = Self {
            runtime,
            context,
            callbacks: Mutex::new(Vec::new()),
            module_loader_data: None,
            host_promise_rejection_tracker_wrapper: None,
            native_modules: Box::new(Mutex::new(HashMap::new())),
            module_exports_data: UnsafeCell::new(None),
        };

        // Register default module loader so native modules can be imported
        // without requiring explicit set_module_loader call.
        wrapper.set_module_loader(Box::new(NoopModuleLoader));

        Ok(wrapper)
    }

    pub fn set_host_promise_rejection_tracker<F: HostPromiseRejectionTracker + 'static>(&mut self, tracker: F) {
        let tracker = HostPromiseRejectionTrackerWrapper::new(Box::new(tracker));
        let ptr = Box::into_raw(Box::new(tracker));
        self.host_promise_rejection_tracker_wrapper = Some(ptr);
        unsafe {
            q::JS_SetHostPromiseRejectionTracker(self.runtime, Some(host_promise_rejection_tracker), ptr as _);
        }
    }

    pub fn set_module_loader(&mut self, module_loader: Box<dyn JsModuleLoader>) {
        let user_loader = Box::new(module_loader);
        let data = Box::new(ModuleLoaderData {
            user_loader: Box::into_raw(user_loader),
            native_modules: &*self.native_modules as *const _,
        });
        unsafe {
            let data_ptr = Box::into_raw(data);
            self.module_loader_data = Some(data_ptr);
            q::JS_SetModuleLoaderFunc(
                self.runtime,
                Some(quickjs_rs_module_normalize_func),
                Some(quickjs_rs_module_loader),
                data_ptr as *mut c_void,
            );
        }
    }

    // See console standard: https://console.spec.whatwg.org
    pub fn set_console(&self, backend: Box<dyn ConsoleBackend>) -> Result<(), ExecutionError> {
        use crate::console::Level;

        self.add_callback("__console_write", move |args: Arguments| {
            let mut args = args.into_vec();

            if args.len() > 1 {
                let level_raw = args.remove(0);

                let level_opt = level_raw.as_str().and_then(|v| match v {
                    "trace" => Some(Level::Trace),
                    "debug" => Some(Level::Debug),
                    "log" => Some(Level::Log),
                    "info" => Some(Level::Info),
                    "warn" => Some(Level::Warn),
                    "error" => Some(Level::Error),
                    _ => None,
                });

                if let Some(level) = level_opt {
                    backend.log(level, args);
                }
            }
        })?;

        Ok(())
    }

    /// Reset the wrapper by creating a new context.
    pub fn reset(self) -> Result<Self, ContextError> {
        unsafe {
            q::JS_FreeContext(self.context);
        };
        self.callbacks.lock().unwrap().clear();
        let context = unsafe { q::JS_NewContext(self.runtime) };
        if context.is_null() {
            return Err(ContextError::ContextCreationFailed);
        }

        let mut s = self;
        s.context = context;
        Ok(s)
    }

    pub fn serialize_value(&self, value: JsValue) -> Result<OwnedJsValue<'_>, ExecutionError> {
        let serialized = convert::serialize_value(self.context, value)?;
        Ok(OwnedJsValue::new(self, serialized))
    }

    // Deserialize a quickjs runtime value into a Rust value.
    pub(crate) fn to_value(&self, value: &q::JSValue) -> Result<JsValue, ValueError> {
        convert::deserialize_value(self.context, value)
    }

    /// Get the global object.
    pub fn global(&self) -> Result<OwnedJsObject<'_>, ExecutionError> {
        let global_raw = unsafe { q::JS_GetGlobalObject(self.context) };
        let global_ref = OwnedJsValue::new(self, global_raw);
        let global = global_ref.try_into_object()?;
        Ok(global)
    }

    /// Get the last exception from the runtime, and if present, convert it to a ExceptionError.
    pub(crate) fn get_exception(&self) -> Option<ExecutionError> {
        let value = unsafe {
            let raw = q::JS_GetException(self.context);
            OwnedJsValue::new(self, raw)
        };

        if value.is_null() {
            None
        } else if value.is_exception() {
            Some(ExecutionError::Internal(
                "Could get exception from runtime".into(),
            ))
        } else {
            match value.js_to_string() {
                Ok(strval) => {
                    if strval.contains("out of memory") {
                        Some(ExecutionError::OutOfMemory)
                    } else {
                        Some(ExecutionError::Exception(JsValue::String(strval)))
                    }
                }
                Err(e) => Some(e),
            }
        }
    }

    /// Returns `Result::Err` when an error ocurred.
    pub(crate) fn ensure_no_excpetion(&self) -> Result<(), ExecutionError> {
        if let Some(e) = self.get_exception() {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// If the given value is a promise, run the event loop until it is
    /// resolved, and return the final value.
    fn resolve_value<'a>(
        &'a self,
        value: OwnedJsValue<'a>,
    ) -> Result<OwnedJsValue<'a>, ExecutionError> {
        if value.is_exception() {
            unsafe {
                //TODO remove
                // js_std_dump_error(self.context);
            }
            let err = self
                .get_exception()
                .unwrap_or_else(|| ExecutionError::Exception("Unknown exception".into()));
            Err(err)
        } else if value.is_object() {
            let obj = value.try_into_object()?;
            Ok(obj.into_value())
        } else {
            Ok(value)
        }
    }

    /// Evaluate javascript code.
    pub fn eval<'a>(&'a self, code: &str, eval_type: u32, filename: &str) -> Result<OwnedJsValue<'a>, ExecutionError> {
        let filename_c = make_cstring(filename)?;
        let code_c = make_cstring(code)?;

        let value_raw = unsafe {
            q::JS_Eval(
                self.context,
                code_c.as_ptr(),
                code.len() as _,
                filename_c.as_ptr(),
                eval_type as i32,
            )
        };
        let value = OwnedJsValue::new(self, value_raw);
        self.resolve_value(value)
    }

    /*
    /// Call a constructor function.
    fn call_constructor<'a>(
        &'a self,
        function: OwnedJsValue<'a>,
        args: Vec<OwnedJsValue<'a>>,
    ) -> Result<OwnedJsValue<'a>, ExecutionError> {
        let mut qargs = args.iter().map(|arg| arg.value).collect::<Vec<_>>();

        let value_raw = unsafe {
            q::JS_CallConstructor(
                self.context,
                function.value,
                qargs.len() as i32,
                qargs.as_mut_ptr(),
            )
        };
        let value = OwnedJsValue::new(self, value_raw);
        if value.is_exception() {
            let err = self
                .get_exception()
                .unwrap_or_else(|| ExecutionError::Exception("Unknown exception".into()));
            Err(err)
        } else {
            Ok(value)
        }
    }
    */

    /// Call a JS function with the given arguments.
    pub fn call_function<'a>(
        &'a self,
        function: JsFunction<'a>,
        args: Vec<OwnedJsValue<'a>>,
    ) -> Result<OwnedJsValue<'a>, ExecutionError> {
        let ret = function.call(args)?;
        self.resolve_value(ret)
    }

    /// Helper for executing a callback closure.
    fn exec_callback<F>(
        context: *mut q::JSContext,
        argc: c_int,
        argv: *mut q::JSValue,
        callback: &impl Callback<F>,
    ) -> Result<q::JSValue, ExecutionError> {
        let result = std::panic::catch_unwind(|| {
            let arg_slice = unsafe { std::slice::from_raw_parts(argv, argc as usize) };

            let mut args = Vec::with_capacity(arg_slice.len());
            for a in arg_slice {
                let a = deserialize_value(context, a)
                    .map_err(|e| {
                        ExecutionError::Internal(
                            format!("failed to deserialize arguments {} (zero-based) to JS value, {}", args.len(), e)
                        )
                    })?;
                args.push(a);
            }

            match callback.call(args) {
                Ok(Ok(result)) => {
                    let serialized = convert::serialize_value(context, result)
                        .map_err(|e| {
                            ExecutionError::Internal(format!("failed to serialize rust value to js value, {}", e))
                        })?;
                    Ok(serialized)
                }
                // TODO: better error reporting.
                Ok(Err(e)) => Err(ExecutionError::Exception(JsValue::String(e))),
                Err(e) => Err(e.into()),
            }
        });

        match result {
            Ok(r) => r,
            Err(_e) => Err(ExecutionError::Internal("Callback panicked!".to_string())),
        }
    }

    /// Add a global JS function that is backed by a Rust function or closure.
    pub fn create_callback<'a, F>(
        &'a self,
        name: &str,
        callback: impl Callback<F> + 'static,
    ) -> Result<JsFunction<'a>, ExecutionError> {
        let argcount = callback.argument_count() as i32;

        let context = self.context;
        let name = name.to_string();
        let wrapper = move |argc: c_int, argv: *mut q::JSValue| -> q::JSValue {
            match Self::exec_callback(context, argc, argv, &callback) {
                Ok(value) => value,
                // TODO: better error reporting.
                Err(e) => {
                    let js_exception_value = match e {
                        ExecutionError::Exception(e) => e,
                        other => format!("Failed to call [{}], {}", &name,  other.to_string()).into(),
                    };
                    let js_exception =
                        convert::serialize_value(context, js_exception_value).unwrap();
                    unsafe {
                        q::JS_Throw(context, js_exception);
                    }

                    q::JS_MKVAL(q::JS_TAG_EXCEPTION, 0)
                }
            }
        };

        let (pair, trampoline) = unsafe { build_closure_trampoline(wrapper) };
        let data = (&*pair.1) as *const q::JSValue as *mut q::JSValue;
        self.callbacks.lock().unwrap().push(pair);

        let obj = unsafe {
            let f = q::JS_NewCFunctionData(self.context, trampoline, argcount, 0, 1, data);
            OwnedJsValue::new(self, f)
        };

        let f = obj.try_into_function()?;
        Ok(f)
    }

    pub fn add_callback<'a, F>(
        &'a self,
        name: &str,
        callback: impl Callback<F> + 'static,
    ) -> Result<(), ExecutionError> {
        let cfunc = self.create_callback(name, callback)?;
        let global = self.global()?;
        global.set_property(name, cfunc.into_value())?;
        Ok(())
    }

    /// Get the pointer to the native modules map.
    pub fn native_modules_ptr(&self) -> *const Mutex<HashMap<String, *mut q::JSModuleDef>> {
        &*self.native_modules as *const _
    }

    /// Create a native C module builder that allows exporting Rust functions as JS module functions.
    ///
    /// Similar to the C pattern in fib.c, this creates a QuickJS native module
    /// with functions exported via `JS_NewCFunctionData` + `JS_SetModuleExport`.
    pub fn create_module(&self, module_name: &str) -> NativeModuleBuilder<'_> {
        NativeModuleBuilder {
            context: self,
            module_name: module_name.to_string(),
            exports: Vec::new(),
        }
    }

    /// return Ok(false) if no job pending, Ok(true) if a job was executed successfully.
    pub fn execute_pending_job(&self) -> Result<bool, ExecutionError> {
        let mut job_ctx = null_mut();
        let flag = unsafe {
            q::JS_ExecutePendingJob(self.runtime, &mut job_ctx)
        };
        if flag < 0 {
            //FIXME should get exception from job_ctx
            let e = self.get_exception().unwrap_or_else(|| {
                ExecutionError::Exception("Unknown exception".into())
            });
            return Err(e);
        }
        Ok(flag != 0)
    }

    pub fn execute_module(&self, module_name: &str) -> Result<(), ExecutionError> {
        // Check native modules first
        if let Some(m) = {
            self.native_modules.lock().unwrap_or_else(|e| e.into_inner()).get(module_name).cloned()
        } {
            unsafe {
                let module_obj = q::JS_GetModuleNamespace(self.context, m);
                if q::JS_IsException(module_obj) {
                    let e = self.get_exception().unwrap_or_else(|| {
                        ExecutionError::Exception("Failed to get module namespace".into())
                    });
                    return Err(e);
                }
                q::JS_FreeValue(self.context, module_obj);
            }
            return Ok(());
        }

        if let Some(ml) = self.module_loader_data {
            unsafe {
                let loader_data = &*ml;
                if !loader_data.user_loader.is_null() {
                    let loader = &mut *loader_data.user_loader;
                    let module = loader.load(module_name).map_err(|e| ExecutionError::Internal(format!("Fail to load module:{}", e)))?;
                    self.eval(&module, JS_EVAL_TYPE_MODULE, module_name)?;
                    return Ok(());
                }
            }
        }
        Err(ExecutionError::Internal("Module loader is not set".to_string()))
    }

}

/// A builder for creating a native QuickJS C module that exports Rust functions.
///
/// This mirrors the fib.c C module pattern: functions are registered as module
/// exports using `JS_NewCFunctionData` + `JS_SetModuleExport`.
///
/// # Example
/// ```no_run
/// # use deft_quick_js::Context;
/// let context = Context::new().unwrap();
/// context.create_module("my_module")
///     .add_function("fib", |n: i32| -> i32 { n })
///     .add_function("add", |a: i32, b: i32| -> i32 { a + b })
///     .build()
///     .unwrap();
/// ```
pub struct NativeModuleBuilder<'a> {
    context: &'a ContextWrapper,
    module_name: String,
    exports: Vec<(String, usize, q::JSValue)>,
}

impl<'a> NativeModuleBuilder<'a> {
    /// Add a function export to the module.
    ///
    /// The callback must satisfy the same requirements as `Context::add_callback`:
    /// * accepts 0 - 5 arguments
    /// * each argument must be convertible from a JsValue
    /// * must return a value convertible to JsValue (or Result<T, E>)
    pub fn add_function<F>(
        mut self,
        name: &str,
        callback: impl Callback<F> + 'static,
    ) -> Self {
        let argcount = callback.argument_count();
        let name_owned = name.to_string();
        let context = self.context.context;
        let func_name = name.to_string();
        let wrapper = move |argc: c_int, argv: *mut q::JSValue| -> q::JSValue {
            match ContextWrapper::exec_callback(context, argc, argv, &callback) {
                Ok(value) => value,
                Err(e) => {
                    let js_exception_value = match e {
                        ExecutionError::Exception(e) => e,
                        other => format!("Failed to call [{}], {}", &func_name, other.to_string()).into(),
                    };
                    let js_exception =
                        convert::serialize_value(context, js_exception_value).unwrap();
                    unsafe {
                        q::JS_Throw(context, js_exception);
                    }
                    q::JS_MKVAL(q::JS_TAG_EXCEPTION, 0)
                }
            }
        };

        let (pair, trampoline) = unsafe { build_closure_trampoline(wrapper) };
        let data = (&*pair.1) as *const q::JSValue as *mut q::JSValue;
        self.context.callbacks.lock().unwrap().push(pair);

        let func_value = unsafe {
            q::JS_NewCFunctionData(
                self.context.context,
                trampoline,
                argcount as i32,
                0,
                1,
                data,
            )
        };

        self.exports.push((name_owned, argcount, func_value));
        self
    }

    /// Finalize the builder and create the native C module.
    ///
    /// This creates a `JSModuleDef` via `JS_NewCModule`, registers each export
    /// name via `JS_AddModuleExport`, and stores the module for import resolution.
    /// The actual `JS_SetModuleExport` calls happen in the init callback
    /// (called during module evaluation when `var_ref` is initialized).
    ///
    /// Returns a JS object representing the module namespace, with all exported
    /// functions as properties.
    pub fn build(self) -> Result<OwnedJsValue<'a>, ExecutionError> {
        unsafe {
            let module_name_c = make_cstring(self.module_name.as_str())?;

            let m = q::JS_NewCModule(
                self.context.context,
                module_name_c.as_ptr(),
                Some(native_module_init),
            );

            if m.is_null() {
                for (_, _, fv) in &self.exports {
                    q::JS_FreeValue(self.context.context, *fv);
                }
                return Err(ExecutionError::Internal(
                    "Failed to create native module".into(),
                ));
            }

            // Register export names (but don't call JS_SetModuleExport yet -
            // that happens in native_module_init during module evaluation)
            for (name, _argcount, _func_value) in &self.exports {
                let name_c = make_cstring(name.as_str())?;
                q::JS_AddModuleExport(
                    self.context.context,
                    m,
                    name_c.as_ptr(),
                );
            }

            self.context.native_modules.lock().unwrap_or_else(|e| e.into_inner()).insert(
                self.module_name.clone(),
                m,
            );

            // Store export data for the init callback
            let mut exports_map: HashMap<String, Vec<(CString, q::JSValue)>> = HashMap::new();
            let mut export_entries = Vec::new();
            for (name, _argcount, func_value) in &self.exports {
                let name_c = make_cstring(name.as_str())?;
                // Dup the value: one ref for ModuleExportsData, one will be consumed by JS_SetModuleExport
                let duped = q::JS_DupValue(self.context.context, *func_value);
                export_entries.push((name_c, duped));
            }
            exports_map.insert(self.module_name.clone(), export_entries);

            // Create or update ModuleExportsData
            let cell = self.context.module_exports_data.get();
            if let Some(existing) = *cell {
                let data = &mut *existing;
                for (mod_name, entries) in exports_map {
                    data.exports.entry(mod_name).or_default().extend(entries);
                }
            } else {
                let data = Box::new(ModuleExportsData { exports: exports_map });
                let data_ptr = Box::into_raw(data);
                *cell = Some(data_ptr);
                q::JS_SetContextOpaque(self.context.context, data_ptr as *mut c_void);
            }

            // Build namespace object with all exports
            let ns = q::JS_NewObject(self.context.context);
            for (name, _argcount, func_value) in &self.exports {
                let name_c = make_cstring(name.as_str())?;
                q::JS_SetPropertyStr(
                    self.context.context,
                    ns,
                    name_c.as_ptr(),
                    q::JS_DupValue(self.context.context, *func_value),
                );
            }

            // Free the original function values from JS_NewCFunctionData.
            // All necessary references have been created via JS_DupValue:
            // - one in ModuleExportsData (freed in ContextWrapper::drop)
            // - one in the namespace object (freed when namespace is dropped)
            // - one will be consumed by JS_SetModuleExport in the init callback
            for (_, _, func_value) in self.exports {
                q::JS_FreeValue(self.context.context, func_value);
            }

            Ok(OwnedJsValue::new(self.context, ns))
        }
    }
}
