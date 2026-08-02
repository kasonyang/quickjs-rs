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
use libquickjs_sys::{JS_Eval, JS_EVAL_FLAG_COMPILE_ONLY, JS_EVAL_TYPE_MODULE, JS_FreeValue, JS_IsException, JSContext, JSModuleDef, size_t, JS_VALUE_GET_PTR, js_malloc, js_strdup, js_module_set_import_meta};

/// Combined loader data: holds both the user's JS module loader and a pointer
/// to the native module definitions map.
pub(crate) struct ModuleLoaderData {
    pub user_loader: *mut Box<dyn JsModuleLoader>,
    pub native_modules: *const Mutex<HashMap<String, *mut JSModuleDef>>,
}

/// JS module normalize function
pub unsafe extern "C" fn quickjs_rs_module_normalize_func(
    ctx: *mut JSContext,
    module_base_name: *const ::std::os::raw::c_char,
    module_name: *const ::std::os::raw::c_char,
    opaque: *mut ::std::os::raw::c_void,
) -> *mut ::std::os::raw::c_char {
    if module_base_name.is_null() || module_name.is_null() {
        return std::ptr::null_mut();
    }
    if let Some(resolved) = resolve_by_loader(module_base_name, module_name, opaque) {
        return js_strdup(ctx, CString::new(resolved).unwrap_or_default().as_ptr());
    }
    default_module_normalize_func(ctx, module_base_name, module_name)
}

unsafe fn resolve_by_loader( module_base_name: *const ::std::os::raw::c_char,
                      module_name: *const ::std::os::raw::c_char,
                      opaque: *mut ::std::os::raw::c_void) -> Option<String> {
    let data = &*(opaque as *mut ModuleLoaderData);
    if !data.user_loader.is_null() {
        let module_base_name = CStr::from_ptr(module_base_name);
        let module_name = CStr::from_ptr(module_name);
        let loader = &mut *data.user_loader;
        loader.resolve_module_path(module_base_name.to_str().ok()?, module_name.to_str().ok()?)
    } else {
        None
    }
}

unsafe fn default_module_normalize_func(
    ctx: *mut JSContext,
    module_base_name: *const ::std::os::raw::c_char,
    module_name: *const ::std::os::raw::c_char,
) -> *mut ::std::os::raw::c_char {
    let name = CStr::from_ptr(module_name);
    let name_bytes = name.to_bytes();

    if name_bytes.is_empty() || name_bytes[0] != b'.' {
        return js_strdup(ctx, module_name);
    }

    let base = CStr::from_ptr(module_base_name);
    let base_bytes = base.to_bytes();

    let dir_len = base_bytes.iter().rposition(|&b| b == b'/').map_or(0, |p| p);

    let name_len = name_bytes.len();
    let cap = dir_len + name_len + 2;
    let filename = js_malloc(ctx, cap as size_t) as *mut u8;
    if filename.is_null() {
        return std::ptr::null_mut();
    }

    std::ptr::copy_nonoverlapping(base_bytes.as_ptr(), filename, dir_len);
    *filename.add(dir_len) = b'\0';

    let mut f_len = dir_len;
    let mut r = 0;
    loop {
        if r + 2 <= name_len && name_bytes[r] == b'.' && name_bytes[r + 1] == b'/' {
            r += 2;
        } else if r + 3 <= name_len
            && name_bytes[r] == b'.'
            && name_bytes[r + 1] == b'.'
            && name_bytes[r + 2] == b'/'
        {
            if f_len == 0 {
                break;
            }
            let filename_slice = std::slice::from_raw_parts(filename, f_len);
            let last_slash = filename_slice.iter().rposition(|&b| b == b'/');
            let last_elem_start = match last_slash {
                Some(pos) => pos + 1,
                None => 0,
            };
            let last_elem = &filename_slice[last_elem_start..];
            if last_elem == b"." || last_elem == b".." {
                break;
            }
            f_len = last_slash.unwrap_or_else(|| 0);
            r += 3;
        } else {
            break;
        }
    }

    if f_len > 0 {
        *filename.add(f_len) = b'/';
        f_len += 1;
    }

    let remaining = &name_bytes[r..];
    std::ptr::copy_nonoverlapping(remaining.as_ptr(), filename.add(f_len), remaining.len());
    f_len += remaining.len();
    *filename.add(f_len) = b'\0';

    filename as *mut ::std::os::raw::c_char
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
    js_module_set_import_meta(ctx, func_val, false, false);
    let ptr = JS_VALUE_GET_PTR(func_val);
    JS_FreeValue(ctx, func_val);
    ptr as *mut JSModuleDef
}

/// js module loader trait
pub trait JsModuleLoader: 'static {

    /// resolve module path
    fn resolve_module_path(&mut self, base_module_name: &str, module_name: &str) -> Option<String> {
        let _ = (base_module_name, module_name);
        None
    }

    /// load a module
    fn load(&mut self, module_name: &str) -> Result<String, Error>;
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