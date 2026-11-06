//! Worker side of plugin calls (`--plugin-worker`): validate each
//! request against the installed manifest, then call the plugin DLL.
use crate::worker::Request;
use serde_json::{json, Value};
use shishi_core::sources::plugin::Plugin;
use std::path::PathBuf;

/// One validated call: the DLL to load and the exact bytes it receives.
#[derive(Debug, PartialEq)]
pub struct Call {
    pub library: PathBuf,
    pub settings: bool,
    pub input: Vec<u8>,
}

pub fn prepare(request: &Request) -> Result<Call, String> {
    let plugin = Plugin::read(&request.manifest).map_err(|e| e.0)?;
    let library = plugin.dir.join(&plugin.manifest.entrypoint);
    let method = request.input["method"].as_str().ok_or("缺少插件方法")?;
    let input = if request.settings {
        let settings = plugin.manifest.settings.as_ref().ok_or("插件未提供配置页面")?;
        if !settings.methods.iter().any(|m| m == method) {
            return Err("插件未声明此配置方法".into());
        }
        let input = serde_json::to_vec(&json!({"protocol": 1, "method": method,
            "config": request.input["config"], "params": request.input["params"],
            "context": {"plugin_dir": plugin.dir, "timeout_ms": 90000}}))
        .map_err(|e| e.to_string())?;
        if input.len() > 2 * 1024 * 1024 {
            return Err("插件配置请求过大".into());
        }
        input
    } else {
        if !plugin.has(method) {
            return Err("插件未声明此方法".into());
        }
        serde_json::to_vec(&request.input).map_err(|e| e.to_string())?
    };
    Ok(Call { library, settings: request.settings, input })
}

/// Serve stdin requests until the host closes it. Libraries stay loaded
/// until this process exits; the host ends it to release them.
pub fn serve() -> Result<(), String> {
    #[cfg(windows)]
    let mut libraries = std::collections::BTreeMap::<PathBuf, native::Library>::new();
    crate::worker::serve(std::io::stdin().lock(), std::io::stdout(), &mut |request| -> Result<Value, String> {
        let call = prepare(request)?;
        #[cfg(windows)]
        {
            if !libraries.contains_key(&call.library) {
                libraries.insert(call.library.clone(), native::Library::open(&call.library)?);
            }
            let library = &libraries[&call.library];
            if call.settings {
                library.settings_call(&call.input)
            } else {
                library.call(&call.input)
            }
        }
        #[cfg(not(windows))]
        {
            let _ = call;
            Err("此平台不支持加载插件".into())
        }
    })
}

#[cfg(windows)]
mod native {
    use serde_json::Value;
    use std::{
        path::Path,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        },
    };

    const MAX: usize = crate::worker::MAX as usize;
    type Free = unsafe extern "C" fn(*mut u8, usize);
    type Call = unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8;
    type SettingsCall = unsafe extern "C" fn(
        *const u8,
        usize,
        *mut usize,
        Option<unsafe extern "C" fn(usize) -> u32>,
        usize,
    ) -> *mut u8;

    pub struct Library(HMODULE);

    impl Drop for Library {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }

    /// The settings call polls this with a pointer to its deadline.
    unsafe extern "C" fn cancelled(context: usize) -> u32 {
        u32::from(Instant::now() >= *(context as *const Instant))
    }

    impl Library {
        pub fn open(path: &Path) -> Result<Self, String> {
            let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let handle = unsafe {
                LoadLibraryExW(
                    wide.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
            };
            if handle.is_null() {
                return Err(format!("无法加载插件 DLL：{}", std::io::Error::last_os_error()));
            }
            Ok(Self(handle))
        }

        fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize, String> {
            unsafe { GetProcAddress(self.0, name.as_ptr()).ok_or_else(|| "插件缺少 ABI 导出函数".into()) }
        }

        fn checked(&self) -> Result<Free, String> {
            unsafe {
                let abi: unsafe extern "C" fn() -> u32 = std::mem::transmute(self.symbol(b"shishi_plugin_abi\0")?);
                if abi() != 1 {
                    return Err("插件 ABI 不兼容".into());
                }
                Ok(std::mem::transmute::<_, Free>(self.symbol(b"shishi_plugin_free\0")?))
            }
        }

        /// Copy, free and decode one plugin response.
        unsafe fn take(free: Free, pointer: *mut u8, length: usize, too_large: &str) -> Result<Value, String> {
            if pointer.is_null() {
                return Err("插件返回空响应".into());
            }
            if length > MAX {
                free(pointer, length);
                return Err(too_large.into());
            }
            let parsed = serde_json::from_slice::<Value>(std::slice::from_raw_parts(pointer, length));
            free(pointer, length);
            parsed.map_err(|e| format!("插件响应无效：{e}"))
        }

        fn result(value: Value, fallback: &str) -> Result<Value, String> {
            if value["ok"] != true {
                return Err(value["error"].as_str().unwrap_or(fallback).into());
            }
            Ok(value["result"].clone())
        }

        pub fn call(&self, input: &[u8]) -> Result<Value, String> {
            let free = self.checked()?;
            unsafe {
                let call: Call = std::mem::transmute(self.symbol(b"shishi_plugin_call\0")?);
                let mut length = 0;
                let pointer = call(input.as_ptr(), input.len(), &mut length);
                Self::result(Self::take(free, pointer, length, "插件响应超过 32 MiB")?, "插件调用失败")
            }
        }

        /// The host ends this process on cancellation; the DLL itself
        /// observes the 90 s deadline through the callback.
        pub fn settings_call(&self, input: &[u8]) -> Result<Value, String> {
            let free = self.checked()?;
            let deadline = Instant::now() + Duration::from_secs(90);
            let context = &deadline as *const Instant as usize;
            unsafe {
                let invoke: SettingsCall = std::mem::transmute(self.symbol(b"shishi_plugin_settings_call\0")?);
                let mut length = 0;
                let pointer = invoke(input.as_ptr(), input.len(), &mut length, Some(cancelled), context);
                let value = Self::take(free, pointer, length, "插件配置响应过大");
                if cancelled(context) != 0 {
                    return Err("插件配置操作已取消或超时".into());
                }
                Self::result(value?, "插件配置操作失败")
            }
        }
    }
}

#[cfg(test)]
mod tests;
