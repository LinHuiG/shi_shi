//! Blocking HTTPS over one shared reqwest runtime. One deadline covers the
//! whole call; the cancel flag is polled while waiting. Plain `http://` is
//! accepted only for loopback in test builds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP transport error: invalid URL")]
    InvalidUrl,
    #[error("HTTP transport error: network error")]
    Network,
    #[error("HTTP transport error: timeout")]
    Timeout,
    #[error("HTTP transport error: TLS validation failed")]
    Tls,
    #[error("HTTP transport error: cancelled")]
    Cancelled,
    #[error("HTTP transport error: response too large")]
    ResponseTooLarge,
    #[error("HTTP transport error: protocol error")]
    Protocol,
    #[error("HTTP transport error: file write failed")]
    Io,
}

impl From<reqwest::Error> for HttpError {
    fn from(error: reqwest::Error) -> Self {
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            if cause.is::<native_tls::Error>() {
                return Self::Tls;
            }
            source = cause.source();
        }
        match () {
            _ if error.is_timeout() => Self::Timeout,
            _ if error.is_builder() => Self::InvalidUrl,
            _ if error.is_body() || error.is_decode() => Self::Protocol,
            _ => Self::Network,
        }
    }
}

pub struct HttpRequest<'a> {
    pub post: Option<&'a [u8]>,
    pub url: &'a str,
    pub bearer: Option<&'a str>,
    pub headers: &'a [(&'a str, &'a str)],
    pub timeout: Duration,
    pub max_bytes: usize,
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> HttpRequest<'a> {
    pub fn get(url: &'a str, timeout: Duration, max_bytes: usize) -> Self {
        Self { post: None, url, bearer: None, headers: &[], timeout, max_bytes, cancel: None }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Plain `http://` to loopback in this crate's tests and in test builds of
/// crates enabling `loopback-http`.
pub(crate) const LOOPBACK_HTTP: bool = cfg!(any(test, feature = "loopback-http"));

fn loopback(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|h| ["localhost", "127.0.0.1", "[::1]"].contains(&h))
}

/// HTTPS, or plain HTTP to loopback when [`LOOPBACK_HTTP`].
pub(crate) fn allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" || LOOPBACK_HTTP && url.scheme() == "http" && loopback(url)
}

static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Register the core runtime for calls from threads outside it.
pub fn use_runtime(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

fn handle() -> tokio::runtime::Handle {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle;
    }
    #[cfg(any(test, feature = "loopback-http"))]
    if RUNTIME.get().is_none() {
        static TEST: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = TEST.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
        });
        use_runtime(rt.handle().clone());
    }
    RUNTIME.get().cloned().expect("HTTP runtime registered")
}

/// One client per proxy setting; redirects stay on HTTPS.
fn client(proxy: Option<String>) -> Result<reqwest::Client, HttpError> {
    static CLIENT: Mutex<Option<(Option<String>, reqwest::Client)>> = Mutex::new(None);
    let mut cached = CLIENT.lock().unwrap();
    if let Some((_, client)) = cached.as_ref().filter(|(key, _)| *key == proxy) {
        return Ok(client.clone());
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .https_only(!LOOPBACK_HTTP)
        .redirect(reqwest::redirect::Policy::limited(5))
        .retry(reqwest::retry::never())
        .http1_only()
        .user_agent("ShiShi/1.0");
    if let Some(address) = &proxy {
        builder = builder.proxy(reqwest::Proxy::all(address)?);
    }
    let client = builder.build()?;
    *cached = Some((proxy, client.clone()));
    Ok(client)
}

async fn guarded<T>(
    future: impl std::future::Future<Output = Result<T, reqwest::Error>>,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<T, HttpError> {
    tokio::pin!(future);
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(HttpError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpError::Timeout);
        }
        tokio::select! {
            result = &mut future => return result.map_err(HttpError::from),
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(20))) => {}
        }
    }
}

/// Send `request`. With `chunk`, a 2xx body goes to it (`false` aborts)
/// instead of being kept; other bodies are always kept for the caller.
pub fn send(
    request: &HttpRequest<'_>,
    mut chunk: Option<&mut dyn FnMut(&[u8]) -> bool>,
) -> Result<HttpResponse, HttpError> {
    let deadline = Instant::now() + request.timeout;
    let url = reqwest::Url::parse(request.url).ok().filter(allowed).ok_or(HttpError::InvalidUrl)?;
    let proxy = if loopback(&url) { None } else { system_proxy(&url) };
    handle().block_on(async {
        let client = client(proxy)?;
        let mut pending = match request.post {
            Some(body) => client.post(url).header("Content-Type", "application/json").body(body.to_vec()),
            None => client.get(url),
        };
        if let Some(token) = request.bearer {
            pending = pending.bearer_auth(token);
        }
        for (name, value) in request.headers {
            pending = pending.header(*name, *value);
        }
        let mut response = guarded(pending.send(), deadline, request.cancel).await?;
        let status = response.status().as_u16();
        let keep = chunk.is_none() || !(200..300).contains(&status);
        let mut body = Vec::new();
        while let Some(bytes) = guarded(response.chunk(), deadline, request.cancel).await? {
            if keep {
                if body.len() + bytes.len() > request.max_bytes {
                    return Err(HttpError::ResponseTooLarge);
                }
                body.extend_from_slice(&bytes);
            } else if !chunk.as_mut().unwrap()(&bytes) {
                return Err(HttpError::Protocol);
            }
        }
        Ok(HttpResponse { status, body })
    })
}

/// GET into `dest`, reporting the bytes written; a non-2xx status writes
/// nothing. A failure removes the partial file.
pub fn download(request: &HttpRequest<'_>, dest: &Path, progress: &mut dyn FnMut(u64)) -> Result<u16, HttpError> {
    use std::io::Write;
    let mut file = std::fs::File::create(dest).map_err(|_| HttpError::Io)?;
    let (mut written, mut error) = (0u64, None);
    let result = send(
        request,
        Some(&mut |bytes| {
            if written + bytes.len() as u64 > request.max_bytes as u64 {
                error = Some(HttpError::ResponseTooLarge);
            } else if file.write_all(bytes).is_err() {
                error = Some(HttpError::Io);
            } else {
                written += bytes.len() as u64;
                progress(written);
            }
            error.is_none()
        }),
    );
    let result = match (result, error) {
        (_, Some(e)) | (Err(e), None) => Err(e),
        (Ok(r), None) => file.sync_all().map(|_| r.status).map_err(|_| HttpError::Io),
    };
    if !matches!(result, Ok(s) if (200..300).contains(&s)) {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// The product ships for Windows only; development hosts connect directly.
#[cfg(not(windows))]
fn system_proxy(_: &reqwest::Url) -> Option<String> {
    None
}

#[cfg(windows)]
fn system_proxy(url: &reqwest::Url) -> Option<String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Networking::WinHttp::*;
    unsafe fn take(ptr: *mut u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        GlobalFree(ptr.cast());
        value
    }
    let mut info: WINHTTP_PROXY_INFO = unsafe { std::mem::zeroed() };
    if unsafe { WinHttpGetDefaultProxyConfiguration(&mut info) } == 0 {
        return None;
    }
    let (proxy, bypass) = unsafe { (take(info.lpszProxy), take(info.lpszProxyBypass)) };
    (info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY).then(|| proxy_for(url, &proxy, &bypass)).flatten()
}

/// The first WinHTTP proxy entry for `url`, unless a bypass pattern matches.
#[cfg_attr(not(windows), allow(dead_code))]
fn proxy_for(url: &reqwest::Url, proxy: &str, bypass: &str) -> Option<String> {
    let entries =
        |text: &str| text.split([';', ' ', '\t']).filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let host = url.host_str().unwrap_or("");
    if entries(bypass)
        .iter()
        .any(|p| (p.eq_ignore_ascii_case("<local>") && !host.contains('.')) || wildcard(&p.to_ascii_lowercase(), host))
    {
        return None;
    }
    entries(proxy).into_iter().find_map(|entry| {
        let address = match entry.split_once('=') {
            Some((scheme, address)) if scheme.eq_ignore_ascii_case(url.scheme()) => address.to_owned(),
            Some(_) => return None,
            None => entry,
        };
        Some(if address.contains("://") { address } else { format!("http://{address}") })
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn wildcard(pattern: &str, host: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == host,
        Some((head, rest)) => host.starts_with(head) && (head.len()..=host.len()).any(|i| wildcard(rest, &host[i..])),
    }
}

#[cfg(test)]
mod tests;
