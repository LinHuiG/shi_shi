//! Synthetic DLL for process-boundary tests. Never ship in an installer.
use std::sync::atomic::{AtomicUsize, Ordering};
static CALLS: AtomicUsize = AtomicUsize::new(0);
thread_local! { static TLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[no_mangle]
pub extern "C" fn shishi_plugin_abi() -> u32 { 1 }
unsafe fn invoke(input: *const u8, len: usize, size: *mut usize) -> *mut u8 {
    let input = std::str::from_utf8(std::slice::from_raw_parts(input, len)).unwrap();
    let method = |name: &str| input.contains(&format!("\"method\":\"{name}\""));
    let response = std::panic::catch_unwind(|| {
        if method("abort") || (method("fetch_incremental") && input.contains("crash_scan")) { std::process::abort(); }
        if method("panic") { panic!("synthetic panic"); }
        if method("timeout") { std::thread::sleep(std::time::Duration::from_secs(120)); }
        if method("error") { return "{\"ok\":false,\"error\":\"synthetic business error\"}".into(); }
        if method("invalid") { return "not json".into(); }
        if method("validate") { return "{\"ok\":true,\"result\":{\"valid\":true,\"account_key\":\"synthetic\"}}".into(); }
        let calls = CALLS.fetch_add(1, Ordering::SeqCst)+1;
        let tls = TLS.with(|v| { v.set(v.get()+1); v.get() });
        let until = input.split("\"until\":").nth(1).map(|s|s.chars().take_while(|c|c.is_ascii_digit()).collect::<String>()).unwrap_or_else(||"0".into());
        format!("{{\"ok\":true,\"result\":{{\"pid\":{},\"calls\":{calls},\"tls\":{tls},\"candidates\":[],\"messages\":[],\"until\":{until},\"cursor\":{{\"position\":1}}}}}}", std::process::id())
    }).unwrap_or_else(|_| "{\"ok\":false,\"error\":\"caught synthetic panic\"}".into());
    let mut bytes = response.into_bytes().into_boxed_slice();
    *size = if method("oversized") { 33*1024*1024 } else { bytes.len() };
    let pointer = bytes.as_mut_ptr();
    // The oversized fixture uses a real allocation of the reported size.
    if method("oversized") { bytes = vec![b' '; *size].into_boxed_slice(); let p=bytes.as_mut_ptr(); std::mem::forget(bytes); return p; }
    std::mem::forget(bytes);
    pointer
}
#[no_mangle]
pub unsafe extern "C" fn shishi_plugin_call(input:*const u8,len:usize,size:*mut usize)->*mut u8 { invoke(input,len,size) }
#[no_mangle]
pub unsafe extern "C" fn shishi_plugin_settings_call(input:*const u8,len:usize,size:*mut usize,_cancel:Option<unsafe extern "C" fn(usize)->u32>,_context:usize)->*mut u8 { invoke(input,len,size) }
#[no_mangle]
pub unsafe extern "C" fn shishi_plugin_free(pointer:*mut u8,len:usize) { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer,len))); }
