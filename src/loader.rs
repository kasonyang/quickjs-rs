//! js module loader
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::fs::File;
use std::io;
use std::io::{Error, Read};
use std::os::raw::c_int;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::str::FromStr;
use std::sync::Mutex;
use libquickjs_sys::{JS_Eval, JS_EVAL_FLAG_COMPILE_ONLY, JS_EVAL_TYPE_MODULE, JS_FreeValue, JS_IsException, JSContext, JSModuleDef, size_t, JS_VALUE_GET_PTR};

/// Combined loader data: holds both the user's JS module loader and a pointer
/// to the native module definitions map.
pub(crate) struct ModuleLoaderData {
    pub user_loader: *mut Box<dyn JsModuleLoader>,
    pub native_modules: *const Mutex<HashMap<String, *mut JSModuleDef>>,
}

/// js module loader callback
pub unsafe extern "C" fn quickjs_rs_module_loader(
        ctx: *mut JSContext,
        module_name: *const ::std::os::raw::c_char,
        opaque: *mut ::std::os::raw::c_void,
    ) -> *mut JSModuleDef {
    let module_name_cstr = CStr::from_ptr(module_name);
    let module_name_str = match module_name_cstr.to_str() {
        Ok(s) => s,
        Err(_) => return null_mut(),
    };
    // println!("loading module:{:?}", module_name_cstr);

    let data = &*(opaque as *mut ModuleLoaderData);

    // Check native modules first
    if !data.native_modules.is_null() {
        let native_modules = &*data.native_modules;
        if let Some(m) = native_modules.lock().unwrap_or_else(|e| e.into_inner()).get(module_name_str) {
            return *m;
        }
    }

    // Fall back to user's JS module loader
    if data.user_loader.is_null() {
        return null_mut();
    }
    let loader = &mut *data.user_loader;
    let input = match loader.load(module_name_str) {
        Ok(e) => e,
        Err(_err) => {
            return null_mut()
        }
    };
    let code_len = input.len();
    let code = CString::new(input).unwrap();
    let func_val = JS_Eval(
        ctx,
        code.as_ptr() as *const c_char,
        code_len as size_t,
        module_name_cstr.as_ptr(),
        (JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY) as c_int
    );
    if JS_IsException(func_val) {
        return null_mut();
        // return Err(anyhow!("Failed to load module"));
    }
    // js_module_set_import_meta(ctx, func_val, true as c_int, false as c_int);
    let ptr = JS_VALUE_GET_PTR(func_val);
    JS_FreeValue(ctx, func_val);
    ptr as *mut JSModuleDef
}

/// js module loader trait
pub trait JsModuleLoader: 'static {
    /// load a module
    fn load(&mut self, module_name: &str) -> Result<String, io::Error>;
}

/// File system module loader
pub struct FsJsModuleLoader {
    base: PathBuf,
}

impl FsJsModuleLoader {

    /// create a new FsJsModuleLoader
    pub fn new(base: &str) -> Self {
        Self {
            base: PathBuf::from_str(base).unwrap()
        }
    }
}

impl JsModuleLoader for FsJsModuleLoader {
    fn load(&mut self, module_name: &str) -> Result<String, Error> {
        let path = self.base.join(module_name);
        let mut file = File::open(path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok(content)
    }
}

/// A no-op module loader that returns an error for all modules.
/// Used as the default when no user loader is set, so native modules
/// (which are checked first by the loader) can still be imported.
pub struct NoopModuleLoader;

impl JsModuleLoader for NoopModuleLoader {
    fn load(&mut self, _module_name: &str) -> Result<String, Error> {
        Err(Error::new(io::ErrorKind::NotFound, "No module loader configured"))
    }
}