//! Stable, UTF-8 JSON C ABI. Each allocation is freed by the library that created it.
pub use serde_json;
pub const ABI_VERSION: u32 = 1;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Export a thread-safe `fn(Value) -> Result<Value, String>` handler.
/// Initialization must be deferred until the explicit `validate` or read methods.
#[macro_export]
macro_rules! export_plugin {
    ($handler:path) => {
        #[no_mangle]
        pub extern "C" fn shishi_plugin_abi() -> u32 { $crate::ABI_VERSION }
        #[no_mangle]
        pub unsafe extern "C" fn shishi_plugin_call(input: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
            if out_len.is_null() { return std::ptr::null_mut(); }
            *out_len = 0;
            if input.is_null() || len > $crate::MAX_BYTES { return std::ptr::null_mut(); }
            let result = std::panic::catch_unwind(|| {
                let request = $crate::serde_json::from_slice(std::slice::from_raw_parts(input, len)).map_err(|e|e.to_string())?;
                $handler(request)
            }).unwrap_or_else(|_|Err("插件内部错误".into()));
            let envelope = match result {
                Ok(value) => $crate::serde_json::json!({"ok":true,"result":value}),
                Err(error) => $crate::serde_json::json!({"ok":false,"error":error}),
            };
            let mut bytes = $crate::serde_json::to_vec(&envelope).unwrap_or_default();
            if bytes.len() > $crate::MAX_BYTES { bytes = b"{\"ok\":false,\"error\":\"Response exceeds 32 MiB\"}".to_vec(); }
            let mut buffer = bytes.into_boxed_slice();
            *out_len = buffer.len();
            let ptr = buffer.as_mut_ptr();
            std::mem::forget(buffer);
            ptr
        }
        #[no_mangle]
        pub unsafe extern "C" fn shishi_plugin_free(ptr: *mut u8, len: usize) {
            if !ptr.is_null() { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))); }
        }
    };
}
