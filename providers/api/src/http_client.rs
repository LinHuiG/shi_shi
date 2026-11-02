//! Bounded HTTPS transport with system TLS and the Windows WinHTTP proxy policy.
//!
//! A shared connection pool owns network resources. Each call retains one overall
//! deadline and cancellation flag through headers, body reads and GET redirects.
//! POST is never replayed; redirects must stay on the original origin. Errors
//! expose only a safe class, never request or response data.

use crate::endpoint::{ParsedUrl, Scheme, MAX_URL_BYTES};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpGetDefaultProxyConfiguration, WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_PROXY_INFO,
};

/// Default maximum response body size: 2 MiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Default whole-request deadline: 120 seconds.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

const MAX_CONFIGURED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TIMEOUT_MS: u128 = 1_800_000;
const MAX_REDIRECTS: u8 = 10;
const MAX_LOCATION_BYTES: usize = 4 * 1024;
/// ETag is an opaque validator; holiday-cn publishes short quoted tags. This
/// cap matches the calendar cache's text limit.
const MAX_ETAG_BYTES: usize = 512;
/// IMF-fixdate is 29 bytes; the calendar cache allows a little more.
const MAX_LAST_MODIFIED_BYTES: usize = 128;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 1024;
const MAX_BEARER_BYTES: usize = 4096;
const USER_AGENT: &str = "ShiShi/1.0";
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// HTTP method supported by this narrow transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// A caller-supplied request header.
///
/// Header names and values are validated when the request is sent. The type
/// intentionally does not implement `Debug`: header values can contain
/// credentials even though secret headers are discouraged.
pub struct HttpHeader<'a> {
    name: &'a str,
    value: &'a str,
}

impl<'a> HttpHeader<'a> {
    pub fn new(name: &'a str, value: &'a str) -> Self {
        Self { name, value }
    }
}

/// A bounded HTTP request with borrowed body and cancellation flag.
///
/// The type intentionally does not implement `Debug`; URL paths, headers, and
/// the bearer token must not be dumped into logs by accident.
pub struct HttpRequest<'a> {
    method: HttpMethod,
    url: &'a str,
    headers: Vec<HttpHeader<'a>>,
    bearer_token: Option<&'a str>,
    body: Option<&'a [u8]>,
    cancellation: Option<&'a AtomicBool>,
}

impl<'a> HttpRequest<'a> {
    /// Create a GET request. The URL must pass the production HTTPS policy
    /// when sent through [`send`] or [`send_with_config`].
    pub fn get(url: &'a str) -> Self {
        Self {
            method: HttpMethod::Get,
            url,
            headers: Vec::new(),
            bearer_token: None,
            body: None,
            cancellation: None,
        }
    }

    /// Create a POST request whose body is already serialized JSON.
    ///
    /// `Content-Type: application/json` is added automatically. Adding a
    /// second content-type header is rejected.
    pub fn post_json(url: &'a str, body: &'a [u8]) -> Self {
        Self {
            method: HttpMethod::Post,
            url,
            headers: vec![HttpHeader::new("Content-Type", "application/json")],
            bearer_token: None,
            body: Some(body),
            cancellation: None,
        }
    }

    /// Append a non-secret header. CR, LF, NUL, non-ASCII values, auth-like
    /// header names, and duplicate names are rejected by the transport.
    pub fn header(mut self, name: &'a str, value: &'a str) -> Self {
        self.headers.push(HttpHeader::new(name, value));
        self
    }

    /// Attach a standard `Authorization: Bearer <token>` header.
    ///
    /// The token is sent only to the original origin. The transport rejects
    /// cross-origin redirects instead of forwarding credentials.
    pub fn bearer_token(mut self, token: &'a str) -> Self {
        self.bearer_token = Some(token);
        self
    }

    /// Observe a caller-owned cancellation flag.
    ///
    /// While waiting for the network, this
    /// transport wakes at least every 20 ms, checks the flag, and on
    /// cancellation drops the pending request to abort the operation. A
    /// successful response whose body was fully read is returned even if the
    /// flag is set afterwards; a cancellation observed before completion
    /// returns [`HttpErrorKind::Cancelled`]. [`send`] blocks the calling
    /// thread, so callers should run it on a worker thread and set this flag
    /// from their stop path.
    pub fn cancellation(mut self, flag: &'a AtomicBool) -> Self {
        self.cancellation = Some(flag);
        self
    }
}

crate::default_struct! {
    /// Finite transport limits.
    ///
    /// `total_timeout` is one monotonic deadline for the whole call: DNS/proxy
    /// resolution, connect, request send, response headers, all body reads, and
    /// every same-origin redirect. It is enforced by the transport, not by native
    /// blocking timeouts. Values are 1 ms–30 minutes. `max_redirects` applies to
    /// GET only and is at most 10.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct HttpConfig {
        pub total_timeout: Duration = DEFAULT_TOTAL_TIMEOUT,
        pub max_response_bytes: usize = DEFAULT_MAX_RESPONSE_BYTES,
        pub max_redirects: u8 = 3,
    }
}

/// Completed HTTP response returned to the internal provider caller.
///
/// Non-2xx statuses are returned here, not turned into transport errors.
/// `Debug` reports only the status, body length, and validator presence so
/// provider-controlled body text is not dumped into logs by accident.
///
/// `etag` and `last_modified` are optional cache validators from the final
/// response. They are `None` when absent, malformed, or oversized; a bad
/// optional validator never fails an otherwise valid response. Values are
/// bounded, printable ASCII and are never included in error text.
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl HttpResponse {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            etag: None,
            last_modified: None,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("has_etag", &self.etag.is_some())
            .field("has_last_modified", &self.last_modified.is_some())
            .finish()
    }
}

/// Safe public error classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpErrorKind {
    InvalidConfig,
    InvalidUrl,
    UnsupportedScheme,
    UserInfoNotAllowed,
    QueryNotAllowed,
    FragmentNotAllowed,
    InvalidHeader,
    InvalidRequest,
    Network,
    Timeout,
    Tls,
    Cancelled,
    ResponseTooLarge,
    RedirectRejected,
    Protocol,
}

impl HttpErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid configuration",
            Self::InvalidUrl => "invalid URL",
            Self::UnsupportedScheme => "unsupported scheme",
            Self::UserInfoNotAllowed => "URL userinfo is not allowed",
            Self::QueryNotAllowed => "URL query is not allowed",
            Self::FragmentNotAllowed => "URL fragment is not allowed",
            Self::InvalidHeader => "invalid header",
            Self::InvalidRequest => "invalid request",
            Self::Network => "network error",
            Self::Timeout => "timeout",
            Self::Tls => "TLS validation failed",
            Self::Cancelled => "cancelled",
            Self::ResponseTooLarge => "response too large",
            Self::RedirectRejected => "redirect rejected",
            Self::Protocol => "protocol error",
        }
    }
}

/// Public transport error containing only a safe class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("HTTP transport error: {}", .kind.as_str())]
pub struct HttpError {
    kind: HttpErrorKind,
}

impl HttpError {
    fn new(kind: HttpErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> HttpErrorKind {
        self.kind
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(error: reqwest::Error) -> Self {
        use std::error::Error;
        let mut source = error.source();
        while let Some(cause) = source {
            if cause.is::<native_tls::Error>() {
                return Self::new(HttpErrorKind::Tls);
            }
            source = cause.source();
        }
        Self::new(if error.is_timeout() {
            HttpErrorKind::Timeout
        } else if error.is_body() || error.is_decode() {
            HttpErrorKind::Protocol
        } else {
            HttpErrorKind::Network
        })
    }
}

/// Send a request with production HTTPS-only policy and default limits.
pub fn send(request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
    send_with_config(request, &HttpConfig::default())
}

/// Send a request with production HTTPS-only policy and explicit limits.
///
/// This blocks while the asynchronous request runs and is bounded by
/// `config.total_timeout`. Call it outside the UI thread.
pub fn send_with_config(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, false, None)
}

#[cfg(test)]
pub(crate) fn send_stream_localhost_for_tests(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
    chunk: &mut dyn FnMut(u16, &[u8]) -> bool,
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, true, Some(chunk))
}

/// Test-only loopback HTTP entry point.
///
/// This is compiled only under `cfg(test)`. Production code has no switch that
/// permits a plaintext endpoint. It uses no system proxy so local tests cannot
/// be redirected by host proxy configuration.
#[cfg(test)]
pub(crate) fn send_localhost_for_tests(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, true, None)
}

fn parse_url(input: &str, allow_localhost_http: bool) -> Result<ParsedUrl, HttpError> {
    ParsedUrl::parse(input, allow_localhost_http).map_err(|error| {
        use crate::endpoint::EndpointErrorKind as E;
        HttpError::new(match error.kind() {
            E::UnsupportedScheme => HttpErrorKind::UnsupportedScheme,
            E::UserInfo => HttpErrorKind::UserInfoNotAllowed,
            E::Query => HttpErrorKind::QueryNotAllowed,
            E::Fragment => HttpErrorKind::FragmentNotAllowed,
            _ => HttpErrorKind::InvalidUrl,
        })
    })
}

pub(crate) fn send_inner(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
    allow_localhost_http: bool,
    mut chunk: Option<&mut dyn FnMut(u16, &[u8]) -> bool>,
) -> Result<HttpResponse, HttpError> {
    validate_config(config)?;
    validate_request(request)?;
    let mut url = parse_url(request.url, allow_localhost_http)?;
    let deadline = Instant::now() + config.total_timeout;
    check_cancelled(request.cancellation)?;
    let proxy = if allow_localhost_http {
        None
    } else {
        system_proxy(&url)?
    };
    let runtime = runtime()?;
    runtime.block_on(async {
        let client = pooled_client(proxy)?;
        for redirects in 0..=config.max_redirects {
            let host = if url.host.contains(':') {
                format!("[{}]", url.host)
            } else {
                url.host.clone()
            };
            let target = format!(
                "{}://{}:{}{}",
                url.scheme.as_str(),
                host,
                url.port,
                url.path
            );
            let method = match request.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
            };
            let mut pending = client.request(method, target);
            for header in &request.headers {
                pending = pending.header(header.name, header.value);
            }
            if let Some(token) = request.bearer_token {
                pending = pending.bearer_auth(token);
            }
            if let Some(body) = request.body {
                pending = pending.body(body.to_vec());
            }
            let mut response = guarded(pending.send(), deadline, request.cancellation).await?;
            let status = response.status().as_u16();
            if request.method == HttpMethod::Get && is_redirect_status(status) {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| {
                        value.len() <= MAX_LOCATION_BYTES && redirects < config.max_redirects
                    })
                    .ok_or_else(|| HttpError::new(HttpErrorKind::RedirectRejected))?;
                url = resolve_redirect(&url, location, allow_localhost_http)?;
                continue;
            }
            let etag = cache_header(response.headers(), "etag", MAX_ETAG_BYTES, is_valid_etag);
            let last_modified = cache_header(
                response.headers(),
                "last-modified",
                MAX_LAST_MODIFIED_BYTES,
                is_valid_http_date,
            );
            let retain = chunk.is_none() || !(200..300).contains(&status);
            if retain
                && response
                    .content_length()
                    .is_some_and(|n| n > config.max_response_bytes as u64)
            {
                return Err(HttpError::new(HttpErrorKind::ResponseTooLarge));
            }
            let mut body = Vec::new();
            while let Some(bytes) =
                guarded(response.chunk(), deadline, request.cancellation).await?
            {
                if retain {
                    if bytes.len() > config.max_response_bytes.saturating_sub(body.len()) {
                        return Err(HttpError::new(HttpErrorKind::ResponseTooLarge));
                    }
                    body.extend_from_slice(&bytes);
                }
                if chunk
                    .as_mut()
                    .is_some_and(|consume| !consume(status, &bytes))
                {
                    return Err(HttpError::new(HttpErrorKind::Protocol));
                }
            }
            return Ok(HttpResponse {
                status,
                body,
                etag,
                last_modified,
            });
        }
        Err(HttpError::new(HttpErrorKind::RedirectRejected))
    })
}

async fn guarded<T>(
    future: impl Future<Output = Result<T, reqwest::Error>>,
    deadline: Instant,
    cancellation: Option<&AtomicBool>,
) -> Result<T, HttpError> {
    tokio::pin!(future);
    loop {
        check_cancelled(cancellation)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| HttpError::new(HttpErrorKind::Timeout))?;
        tokio::select! {
            result = &mut future => return result.map_err(HttpError::from),
            _ = tokio::time::sleep(remaining.min(CANCEL_POLL_INTERVAL)) => {}
        }
    }
}

fn cache_header(
    headers: &reqwest::header::HeaderMap,
    name: &str,
    max_bytes: usize,
    validate: fn(&str) -> bool,
) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim_matches([' ', '\t']);
    (!value.is_empty() && value.len() <= max_bytes && value.is_ascii() && validate(value))
        .then(|| value.to_owned())
}

fn validate_config(config: &HttpConfig) -> Result<(), HttpError> {
    if config.total_timeout.as_millis() == 0
        || config.total_timeout.as_millis() > MAX_TIMEOUT_MS
        || config.max_response_bytes == 0
        || config.max_response_bytes > MAX_CONFIGURED_RESPONSE_BYTES
        || config.max_redirects > MAX_REDIRECTS
    {
        return Err(HttpError::new(HttpErrorKind::InvalidConfig));
    }
    Ok(())
}

fn validate_request(request: &HttpRequest<'_>) -> Result<(), HttpError> {
    if request.headers.len() > MAX_HEADERS {
        return Err(HttpError::new(HttpErrorKind::InvalidHeader));
    }

    let mut seen: Vec<&str> = Vec::with_capacity(request.headers.len());
    for header in &request.headers {
        validate_header_name(header.name)?;
        if is_reserved_header_name(header.name)
            || seen
                .iter()
                .any(|name| name.eq_ignore_ascii_case(header.name))
        {
            return Err(HttpError::new(HttpErrorKind::InvalidHeader));
        }
        validate_header_value(header.value)?;
        seen.push(header.name);
    }

    if let Some(token) = request.bearer_token {
        if token.is_empty()
            || token.len() > MAX_BEARER_BYTES
            || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(HttpError::new(HttpErrorKind::InvalidHeader));
        }
    }

    if let Some(body) = request.body {
        if body.len() > u32::MAX as usize {
            return Err(HttpError::new(HttpErrorKind::InvalidRequest));
        }
    }

    Ok(())
}

fn validate_header_name(name: &str) -> Result<(), HttpError> {
    if name.is_empty()
        || name.len() > MAX_HEADER_NAME_BYTES
        || !name.bytes().all(is_http_token_byte)
    {
        return Err(HttpError::new(HttpErrorKind::InvalidHeader));
    }
    Ok(())
}

fn validate_header_value(value: &str) -> Result<(), HttpError> {
    if value.len() > MAX_HEADER_VALUE_BYTES
        || !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Err(HttpError::new(HttpErrorKind::InvalidHeader));
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn is_reserved_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("cookie")
        || name.eq_ignore_ascii_case("set-cookie")
        || name.eq_ignore_ascii_case("api-key")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("expect")
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn resolve_redirect(
    current: &ParsedUrl,
    location: &str,
    allow_localhost_http: bool,
) -> Result<ParsedUrl, HttpError> {
    if location.is_empty()
        || location.len() > MAX_LOCATION_BYTES
        || !location.is_ascii()
        || location
            .bytes()
            .any(|byte| byte < 0x20 || byte == 0x7f || byte == b' ' || byte == b'\\')
        || location.contains('?')
        || location.contains('#')
    {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }

    if current.scheme == Scheme::Https && starts_with_ascii_case(location, "http://") {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }

    let next = if starts_with_ascii_case(location, "https://")
        || starts_with_ascii_case(location, "http://")
    {
        parse_url(location, allow_localhost_http)
            .map_err(|_| HttpError::new(HttpErrorKind::RedirectRejected))?
    } else if has_unsupported_absolute_scheme(location) {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    } else if let Some(scheme_relative) = location.strip_prefix("//") {
        let absolute = format!("{}://{}", current.scheme.as_str(), scheme_relative);
        parse_url(&absolute, allow_localhost_http)
            .map_err(|_| HttpError::new(HttpErrorKind::RedirectRejected))?
    } else if location.starts_with('/') {
        let mut next = current.clone();
        next.path = normalize_path(location)?;
        next
    } else {
        let directory_end = current.path.rfind('/').map_or(1, |index| index + 1);
        let combined = format!("{}{}", &current.path[..directory_end], location);
        let mut next = current.clone();
        next.path = normalize_path(&combined)?;
        next
    };

    if current.scheme == Scheme::Https && next.scheme != Scheme::Https {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }
    if !current.same_origin(&next) {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }
    Ok(next)
}

fn starts_with_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .map(|head| head.eq_ignore_ascii_case(prefix))
        .unwrap_or(false)
}

fn has_unsupported_absolute_scheme(location: &str) -> bool {
    let Some(colon) = location.find(':') else {
        return false;
    };
    let slash = location.find('/').unwrap_or(usize::MAX);
    colon < slash
}

fn normalize_path(path: &str) -> Result<String, HttpError> {
    if !path.starts_with('/') || path.len() > MAX_URL_BYTES {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut normalized = String::from("/");
    normalized.push_str(&segments.join("/"));
    if trailing_slash && !normalized.ends_with('/') {
        normalized.push('/');
    }
    Ok(normalized)
}

fn check_cancelled(cancellation: Option<&AtomicBool>) -> Result<(), HttpError> {
    if is_cancelled(cancellation) {
        Err(HttpError::new(HttpErrorKind::Cancelled))
    } else {
        Ok(())
    }
}

fn is_cancelled(cancellation: Option<&AtomicBool>) -> bool {
    cancellation
        .map(|flag| flag.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn runtime() -> Result<&'static tokio::runtime::Runtime, HttpError> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, HttpError>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("shishi-http")
                .build()
                .map_err(|_| HttpError::new(HttpErrorKind::Network))
        })
        .as_ref()
        .map_err(|error| *error)
}

fn pooled_client(proxy: Option<String>) -> Result<reqwest::Client, HttpError> {
    // Bound retained pools even when Windows proxy settings change repeatedly.
    static CLIENTS: Mutex<Vec<(Option<String>, reqwest::Client)>> = Mutex::new(Vec::new());
    let mut clients = CLIENTS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((_, client)) = clients.iter().find(|(key, _)| key == &proxy) {
        return Ok(client.clone());
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .http1_only()
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(2);
    if let Some(address) = &proxy {
        builder = builder.proxy(reqwest::Proxy::all(address).map_err(HttpError::from)?);
    }
    let client = builder.build().map_err(HttpError::from)?;
    if clients.len() == 4 {
        clients.remove(0);
    }
    clients.push((proxy, client.clone()));
    Ok(client)
}

fn system_proxy(url: &ParsedUrl) -> Result<Option<String>, HttpError> {
    let mut info: WINHTTP_PROXY_INFO = unsafe { std::mem::zeroed() };
    if unsafe { WinHttpGetDefaultProxyConfiguration(&mut info) } == 0 {
        return Err(HttpError::new(HttpErrorKind::Network));
    }
    // WinHTTP returns two independently allocated, null-terminated strings.
    unsafe fn take_string(ptr: *mut u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        GlobalFree(ptr.cast());
        value
    }
    let proxy = unsafe { take_string(info.lpszProxy) };
    let bypass = unsafe { take_string(info.lpszProxyBypass) };
    Ok(if info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY {
        proxy_for(url, &proxy, &bypass)
    } else {
        None
    })
}

fn proxy_for(url: &ParsedUrl, proxy: &str, bypass: &str) -> Option<String> {
    fn entries(text: &str) -> impl Iterator<Item = &str> {
        text.split(|c: char| c == ';' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
    }
    if entries(bypass).any(|pattern| {
        (pattern.eq_ignore_ascii_case("<local>") && !url.host.contains('.'))
            || wildcard_matches(&pattern.to_ascii_lowercase(), &url.host)
    }) {
        return None;
    }
    // Like WinHTTP, select the first applicable entry; never replay a request
    // against alternate proxies or fall back to direct access after a failure.
    entries(proxy).find_map(|entry| {
        let address = match entry.split_once('=') {
            Some((scheme, address)) if scheme.eq_ignore_ascii_case(url.scheme.as_str()) => address,
            Some(_) => return None,
            None => entry,
        };
        Some(if address.contains("://") {
            address.to_owned()
        } else {
            format!("http://{address}")
        })
    })
}

fn wildcard_matches(pattern: &str, host: &str) -> bool {
    let (mut pattern, mut host) = (pattern.as_bytes(), host.as_bytes());
    let (mut star, mut retry) = (None, host);
    while !host.is_empty() {
        if pattern.first() == Some(&b'*') {
            pattern = &pattern[1..];
            star = Some(pattern);
            retry = host;
        } else if pattern.first() == host.first() {
            pattern = &pattern[1..];
            host = &host[1..];
        } else if let Some(after_star) = star {
            retry = &retry[1..];
            host = retry;
            pattern = after_star;
        } else {
            return false;
        }
    }
    pattern.iter().all(|byte| *byte == b'*')
}

/// RFC 7232 entity-tag: optional `W/` and a quoted string of `etagc` bytes.
/// Only printable ASCII is accepted so the value is safe to persist and log by
/// identity only.
fn is_valid_etag(value: &str) -> bool {
    let bytes = value.as_bytes();
    let bytes = bytes.strip_prefix(b"W/").unwrap_or(bytes);
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return false;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|&byte| byte == 0x21 || (0x23..=0x7e).contains(&byte))
}

/// Strict IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) validation.
///
/// Obsolete HTTP date forms are deliberately not accepted; the cache can omit
/// an unusable validator, but it must not persist an ambiguous one.
fn is_valid_http_date(value: &str) -> bool {
    const WEEKDAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];

    let bytes = value.as_bytes();
    if bytes.len() != 29
        || !WEEKDAYS.iter().any(|day| bytes[0..3] == day[..])
        || bytes[3] != b','
        || bytes[4] != b' '
        || !MONTHS.iter().any(|month| bytes[8..11] == month[..])
        || bytes[7] != b' '
        || bytes[11] != b' '
        || bytes[16] != b' '
        || bytes[19] != b':'
        || bytes[22] != b':'
        || bytes[25] != b' '
        || bytes[26] != b'G'
        || bytes[27] != b'M'
        || bytes[28] != b'T'
    {
        return false;
    }

    let (Some(day), Some(year), Some(hour), Some(minute), Some(second)) = (
        decimal(&bytes[5..7]),
        decimal(&bytes[12..16]),
        decimal(&bytes[17..19]),
        decimal(&bytes[20..22]),
        decimal(&bytes[23..25]),
    ) else {
        return false;
    };
    (1..=31).contains(&day) && year >= 1900 && hour <= 23 && minute <= 59 && second <= 59
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        read_test_request, write_test_response, TestResponse, TestServer, TEST_ACCEPT_TIMEOUT,
    };
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const SYNTHETIC_BEARER: &str = "synthetic-bearer-value-9f4a2c";

    fn test_config() -> HttpConfig {
        HttpConfig {
            total_timeout: Duration::from_secs(2),
            max_response_bytes: 4096,
            max_redirects: 3,
        }
    }

    #[test]
    fn untrusted_tls_certificate_is_rejected_without_sending_http() {
        let identity = native_tls::Identity::from_pkcs12(
            include_bytes!("../tests/fixtures/untrusted-localhost.pfx"),
            "synthetic-test-only",
        )
        .unwrap();
        let acceptor = native_tls::TlsAcceptor::new(identity).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + TEST_ACCEPT_TIMEOUT;
            while Instant::now() < deadline {
                if let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    // TLS 1.3 may finish on the server before seeing the client's
                    // rejection; in either case no HTTP bytes may arrive.
                    if let Ok(mut tls) = acceptor.accept(stream) {
                        assert!(!matches!(tls.read(&mut [0; 1]), Ok(n) if n > 0));
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("TLS client never connected");
        });
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.kind(), HttpErrorKind::Tls);
        assert!(!format!("{error} {error:?}").contains("localhost"));
    }

    fn write_test_headers(stream: &mut TcpStream, status: u16, content_length: usize) {
        let head = format!(
            "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {content_length}\r\n\r\n"
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
    }

    #[test]
    fn get_200_returns_status_and_body() {
        let server = TestServer::start(1, |request, _| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/v1/status");
            TestResponse::ok(br#"{"status":"ok"}"#)
        });
        let url = server.url("/v1/status");
        let response = send_localhost_for_tests(&HttpRequest::get(&url), &test_config())
            .expect("GET should succeed");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"status":"ok"}"#);
        let requests = server.join();
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn completed_requests_reuse_the_same_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}/pooled", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            for _ in 0..2 {
                let request = read_test_request(&mut stream);
                assert_eq!(request.path, "/pooled");
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .unwrap();
            }
        });
        for _ in 0..2 {
            let response =
                send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).unwrap();
            assert_eq!(response.body, b"ok");
        }
        server.join().unwrap();
    }

    #[test]
    fn windows_proxy_routes_and_bypass_are_preserved() {
        for (target, proxies, bypass, expected) in [
            (
                "https://example.test/",
                "http=plain:80;https=secure:81",
                "",
                Some("http://secure:81"),
            ),
            (
                "https://example.test/",
                "first:80 second:81",
                "",
                Some("http://first:80"),
            ),
            ("https://example.test/", "http=plain:80", "", None),
            (
                "https://example.test/",
                "proxy:80",
                "*.example.test",
                Some("http://proxy:80"),
            ),
            (
                "https://a.example.test/",
                "proxy:80",
                "*.EXAMPLE.test",
                None,
            ),
            ("https://intranet/", "proxy:80", "<local>", None),
            (
                "https://a.example.test/",
                "proxy:80",
                "other.test;*.example.test",
                None,
            ),
            ("https://example.test/", "http://proxy:80", "e*e.test", None),
        ] {
            let url = parse_url(target, false).unwrap();
            assert_eq!(
                proxy_for(&url, proxies, bypass).as_deref(),
                expected,
                "{target} {bypass}"
            );
        }
    }

    #[test]
    fn post_json_sends_body_and_bearer() {
        let server = TestServer::start(1, |request, _| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/v1/chat/completions");
            assert_eq!(request.header("content-type"), Some("application/json"));
            assert_eq!(
                request.header("authorization"),
                Some(&format!("Bearer {SYNTHETIC_BEARER}")[..])
            );
            assert_eq!(request.body, br#"{"model":"synthetic-model"}"#);
            TestResponse::ok(br#"{"ok":true}"#)
        });
        let url = server.url("/v1/chat/completions");
        let body = br#"{"model":"synthetic-model"}"#;
        let request = HttpRequest::post_json(&url, body).bearer_token(SYNTHETIC_BEARER);
        let response =
            send_localhost_for_tests(&request, &test_config()).expect("POST should succeed");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"ok":true}"#);
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn cache_headers_preserve_valid_values_and_ignore_unusable_values() {
        const ETAG: &str = "\"synthetic-etag-42\"";
        const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
        let cases = [
            (
                "cached",
                Some((ETAG.to_string(), LAST_MODIFIED.to_string())),
                true,
            ),
            ("plain", None, false),
            (
                "bounded",
                Some((
                    format!("\"{}\"", "e".repeat(MAX_ETAG_BYTES + 2)),
                    format!(
                        "Sun, 06 Nov 1994 08:49:37 GMT{}",
                        "X".repeat(MAX_LAST_MODIFIED_BYTES)
                    ),
                )),
                false,
            ),
            (
                "still-ok",
                Some(("not-a-quoted-tag".into(), "yesterday".into())),
                false,
            ),
        ];
        let replies = cases.clone();
        let server = TestServer::start(cases.len(), move |_, index| {
            let (body, headers, _) = &replies[index];
            TestResponse {
                headers: headers
                    .as_ref()
                    .map(|(etag, modified)| {
                        vec![
                            ("ETag".into(), etag.clone()),
                            ("Last-Modified".into(), modified.clone()),
                        ]
                    })
                    .unwrap_or_default(),
                ..TestResponse::ok(body.as_bytes())
            }
        });
        for (body, headers, valid) in &cases {
            let url = server.url(&format!("/{body}"));
            let response = send_localhost_for_tests(&HttpRequest::get(&url), &test_config())
                .unwrap_or_else(|error| panic!("{body}: {error}"));
            assert_eq!(response.status, 200, "{body}");
            assert_eq!(response.body, body.as_bytes(), "{body}");
            let expected = headers.as_ref().filter(|_| *valid);
            assert_eq!(
                response.etag.as_deref(),
                expected.map(|h| h.0.as_str()),
                "{body}"
            );
            assert_eq!(
                response.last_modified.as_deref(),
                expected.map(|h| h.1.as_str()),
                "{body}"
            );
            let rendered = format!("{response:?}");
            assert!(!rendered.contains(ETAG), "{body}");
            assert!(!rendered.contains("2015"), "{body}");
        }
        assert_eq!(server.join().len(), cases.len());
    }

    #[test]
    fn cache_validator_syntax_is_strict() {
        assert!(is_valid_etag("\"abc-123\""));
        assert!(is_valid_etag("W/\"weak\""));
        assert!(is_valid_etag("\"\""));
        assert!(!is_valid_etag("abc-123"));
        assert!(!is_valid_etag("W/"));
        assert!(!is_valid_etag("\"bad space\""));
        assert!(!is_valid_etag("\"unterminated"));

        assert!(is_valid_http_date("Sun, 06 Nov 1994 08:49:37 GMT"));
        assert!(!is_valid_http_date("Sun, 06 Xxx 1994 08:49:37 GMT"));
        assert!(!is_valid_http_date("Sun, 06 Nov 1994 24:49:37 GMT"));
        assert!(!is_valid_http_date("Sun, 06 Nov 1994 08:49:60 GMT"));
        assert!(!is_valid_http_date("Sun, 06 Nov 1994 08:49:37 UTC"));
        assert!(!is_valid_http_date("Sunday, 06 Nov 1994 08:49:37 GMT"));
    }

    #[test]
    fn cache_header_values_do_not_leak_into_errors() {
        const ETAG: &str = "\"synthetic-secret-etag-77\"";
        const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
        let server = TestServer::start(1, |_, _| {
            let mut response = TestResponse::ok(&[b'z'; 128]).without_content_length();
            response.headers = vec![
                ("ETag".to_string(), ETAG.to_string()),
                ("Last-Modified".to_string(), LAST_MODIFIED.to_string()),
            ];
            response
        });
        let url = server.url("/cache-error");
        let config = HttpConfig {
            max_response_bytes: 64,
            ..test_config()
        };
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &config)
            .expect_err("streamed body must be capped");
        assert_eq!(error.kind(), HttpErrorKind::ResponseTooLarge);
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(ETAG));
        assert!(!rendered.contains("2015"));
        assert!(!rendered.contains("ETag"));
        assert!(!rendered.contains("Last-Modified"));
        let _ = server.join();
    }

    #[test]
    fn non_2xx_is_returned_without_retry() {
        let server = TestServer::start(1, |_, _| {
            TestResponse::status(429, br#"{"error":"synthetic-rate-limit"}"#)
        });
        let url = server.url("/v1/chat/completions");
        let response =
            send_localhost_for_tests(&HttpRequest::post_json(&url, b"{}"), &test_config())
                .expect("non-2xx is a normal response");
        assert_eq!(response.status, 429);
        assert_eq!(response.body, br#"{"error":"synthetic-rate-limit"}"#);
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn declared_body_over_cap_is_rejected() {
        let server = TestServer::start(1, |_, _| TestResponse::ok(&vec![b'x'; 128]));
        let url = server.url("/large");
        let config = HttpConfig {
            max_response_bytes: 64,
            ..test_config()
        };
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &config)
            .expect_err("declared body must be capped");
        assert_eq!(error.kind(), HttpErrorKind::ResponseTooLarge);
        let _ = server.join();
    }

    #[test]
    fn streamed_body_over_cap_is_rejected() {
        let server = TestServer::start(1, |_, _| {
            TestResponse::ok(&vec![b'y'; 128]).without_content_length()
        });
        let url = server.url("/streamed");
        let config = HttpConfig {
            max_response_bytes: 64,
            ..test_config()
        };
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &config)
            .expect_err("streamed body must be capped");
        assert_eq!(error.kind(), HttpErrorKind::ResponseTooLarge);
        let _ = server.join();
    }

    #[test]
    fn slow_drip_is_stopped_by_overall_deadline() {
        let server = TestServer::start_with(1, |stream, _, _| {
            write_test_headers(stream, 200, 100);
            for _ in 0..100 {
                if stream.write_all(b"x").is_err() {
                    return;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(120));
            }
        });
        let url = server.url("/drip");
        let config = HttpConfig {
            total_timeout: Duration::from_millis(700),
            ..test_config()
        };
        let started = Instant::now();
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &config)
            .expect_err("slow drip must exceed the overall deadline");
        let elapsed = started.elapsed();
        eprintln!("slow-drip overall-deadline latency: {elapsed:?}");
        assert_eq!(error.kind(), HttpErrorKind::Timeout);
        assert!(
            elapsed < Duration::from_secs(2),
            "overall deadline overshot cancellation budget: {elapsed:?}"
        );
        let _ = server.join();
    }

    #[test]
    fn cancellation_while_waiting_for_headers_is_prompt() {
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let server = TestServer::start_with(1, move |stream, _, _| {
            accepted_tx.send(()).expect("signal accepted request");
            let _ = release_rx
                .lock()
                .expect("release mutex")
                .recv_timeout(Duration::from_secs(3));
            write_test_response(stream, &TestResponse::ok(b"late"));
        });
        let url = server.url("/stalled-headers");
        let cancellation = Arc::new(AtomicBool::new(false));
        let client_cancellation = Arc::clone(&cancellation);
        let client = thread::spawn(move || {
            let request = HttpRequest::get(&url).cancellation(client_cancellation.as_ref());
            send_localhost_for_tests(&request, &test_config())
        });

        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("request reaches server");
        thread::sleep(Duration::from_millis(100));
        let cancelled_at = Instant::now();
        cancellation.store(true, Ordering::SeqCst);
        let error = client
            .join()
            .expect("join client")
            .expect_err("stalled header wait must be cancelled");
        let elapsed = cancelled_at.elapsed();
        eprintln!("header cancellation latency: {elapsed:?}");
        assert_eq!(error.kind(), HttpErrorKind::Cancelled);
        assert!(
            elapsed < Duration::from_secs(1),
            "header cancellation overshot budget: {elapsed:?}"
        );
        release_tx.send(()).expect("release server");
        let _ = server.join();
    }

    #[test]
    fn cancellation_while_waiting_for_body_is_prompt() {
        let (sent_tx, sent_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let server = TestServer::start_with(1, move |stream, _, _| {
            write_test_headers(stream, 200, 4);
            let _ = stream.write_all(b"a");
            let _ = stream.flush();
            sent_tx.send(()).expect("signal partial body");
            let _ = release_rx
                .lock()
                .expect("release mutex")
                .recv_timeout(Duration::from_secs(3));
            let _ = stream.write_all(b"bcd");
            let _ = stream.flush();
        });
        let url = server.url("/stalled-body");
        let cancellation = Arc::new(AtomicBool::new(false));
        let client_cancellation = Arc::clone(&cancellation);
        let client = thread::spawn(move || {
            let request = HttpRequest::get(&url).cancellation(client_cancellation.as_ref());
            send_localhost_for_tests(&request, &test_config())
        });

        sent_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server sends headers and one body byte");
        thread::sleep(Duration::from_millis(100));
        let cancelled_at = Instant::now();
        cancellation.store(true, Ordering::SeqCst);
        let error = client
            .join()
            .expect("join client")
            .expect_err("stalled body wait must be cancelled");
        let elapsed = cancelled_at.elapsed();
        eprintln!("body cancellation latency: {elapsed:?}");
        assert_eq!(error.kind(), HttpErrorKind::Cancelled);
        assert!(
            elapsed < Duration::from_secs(1),
            "body cancellation overshot budget: {elapsed:?}"
        );
        release_tx.send(()).expect("release server");
        let _ = server.join();
    }

    #[test]
    fn cancellation_completion_race_is_safe() {
        const ITERATIONS: usize = 12;
        let (sent_tx, sent_rx) = mpsc::channel();
        let server = TestServer::start_with(ITERATIONS, move |stream, _, _| {
            write_test_response(stream, &TestResponse::ok(b"race-body"));
            let _ = sent_tx.send(());
        });
        let url = server.url("/race");
        for _ in 0..ITERATIONS {
            let cancellation = Arc::new(AtomicBool::new(false));
            let client_cancellation = Arc::clone(&cancellation);
            let client_url = url.clone();
            let config = test_config();
            let client = thread::spawn(move || {
                let request =
                    HttpRequest::get(&client_url).cancellation(client_cancellation.as_ref());
                send_localhost_for_tests(&request, &config)
            });
            sent_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server wrote response");
            cancellation.store(true, Ordering::SeqCst);
            match client.join().expect("join client") {
                Ok(response) => {
                    assert_eq!(response.status, 200);
                    assert_eq!(response.body, b"race-body");
                }
                Err(error) => assert_eq!(error.kind(), HttpErrorKind::Cancelled),
            }
        }
        assert_eq!(server.join().len(), ITERATIONS);
    }

    #[test]
    fn cancellation_before_request_is_observed() {
        let cancellation = AtomicBool::new(true);
        let request = HttpRequest::get("http://127.0.0.1:9/").cancellation(&cancellation);
        let error = send_localhost_for_tests(&request, &test_config())
            .expect_err("pre-cancelled request must not connect");
        assert_eq!(error.kind(), HttpErrorKind::Cancelled);
    }

    #[test]
    fn cancellation_after_completion_does_not_change_response() {
        let server = TestServer::start(1, |_, _| TestResponse::ok(b"complete"));
        let url = server.url("/complete");
        let cancellation = AtomicBool::new(false);
        let request = HttpRequest::get(&url).cancellation(&cancellation);
        let response =
            send_localhost_for_tests(&request, &test_config()).expect("response completes");
        cancellation.store(true, Ordering::SeqCst);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"complete");
        let _ = server.join();
    }

    #[test]
    fn relative_redirect_is_followed_same_origin() {
        let server = TestServer::start(2, |request, index| {
            if index == 0 {
                assert_eq!(request.path, "/v1/start");
                assert_eq!(
                    request.header("authorization"),
                    Some(&format!("Bearer {SYNTHETIC_BEARER}")[..])
                );
                TestResponse::redirect("../final")
            } else {
                assert_eq!(request.path, "/final");
                assert_eq!(
                    request.header("authorization"),
                    Some(&format!("Bearer {SYNTHETIC_BEARER}")[..])
                );
                TestResponse::ok(b"redirected")
            }
        });
        let url = server.url("/v1/start");
        let request = HttpRequest::get(&url).bearer_token(SYNTHETIC_BEARER);
        let response = send_localhost_for_tests(&request, &test_config())
            .expect("same-origin relative redirect should be followed");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"redirected");
        let requests = server.join();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].path, "/final");
    }

    #[test]
    fn redirects_share_the_overall_deadline() {
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let server = TestServer::start_with(2, move |stream, _, index| {
            if index == 0 {
                write_test_response(stream, &TestResponse::redirect("/final"));
            } else {
                let _ = release_rx
                    .lock()
                    .expect("release mutex")
                    .recv_timeout(Duration::from_secs(3));
                write_test_response(stream, &TestResponse::ok(b"too late"));
            }
        });
        let url = server.url("/start");
        let config = HttpConfig {
            total_timeout: Duration::from_millis(800),
            ..test_config()
        };
        let started = Instant::now();
        let error = send_localhost_for_tests(&HttpRequest::get(&url), &config)
            .expect_err("redirect must not reset the overall deadline");
        let elapsed = started.elapsed();
        eprintln!("redirect overall-deadline latency: {elapsed:?}");
        assert_eq!(error.kind(), HttpErrorKind::Timeout);
        assert!(
            elapsed < Duration::from_secs(2),
            "redirect deadline overshot cancellation budget: {elapsed:?}"
        );
        release_tx.send(()).expect("release server");
        assert_eq!(server.join().len(), 2);
    }

    #[test]
    fn cross_origin_redirect_is_rejected() {
        let other_port = TcpListener::bind(("127.0.0.1", 0))
            .expect("reserve other origin port")
            .local_addr()
            .expect("other origin address")
            .port();
        let server = TestServer::start(1, move |_, _| {
            TestResponse::redirect(&format!("http://127.0.0.1:{other_port}/final"))
        });
        let url = server.url("/start");
        let request = HttpRequest::get(&url).bearer_token(SYNTHETIC_BEARER);
        let error = send_localhost_for_tests(&request, &test_config())
            .expect_err("cross-origin redirect must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::RedirectRejected);
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SYNTHETIC_BEARER));
        assert!(!rendered.contains("Authorization"));
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn downgrade_redirect_is_rejected() {
        let current =
            parse_url("https://api.example.test/v1/start", false).expect("valid HTTPS origin");
        let error = resolve_redirect(&current, "http://api.example.test/final", false)
            .expect_err("downgrade must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::RedirectRejected);
    }

    #[test]
    fn post_redirect_is_returned_without_replay() {
        let server = TestServer::start(1, |request, _| {
            assert_eq!(request.method, "POST");
            TestResponse::redirect("/elsewhere")
        });
        let url = server.url("/submit");
        let response =
            send_localhost_for_tests(&HttpRequest::post_json(&url, br#"{"x":1}"#), &test_config())
                .expect("POST redirect is returned as a response");
        assert_eq!(response.status, 302);
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn malformed_urls_are_rejected_before_network() {
        let cases = [
            ("ftp://example.test/x", HttpErrorKind::UnsupportedScheme),
            (
                "https://synthetic_user:synthetic_password@example.test/x",
                HttpErrorKind::UserInfoNotAllowed,
            ),
            (
                "https://example.test/x?token=synthetic",
                HttpErrorKind::QueryNotAllowed,
            ),
            (
                "https://example.test/x#fragment",
                HttpErrorKind::FragmentNotAllowed,
            ),
            ("https://example.test:99999/", HttpErrorKind::InvalidUrl),
            ("https://[::1/", HttpErrorKind::InvalidUrl),
            ("https://example.test:notaport/", HttpErrorKind::InvalidUrl),
            ("http://localhost/", HttpErrorKind::UnsupportedScheme),
        ];
        for (url, expected) in cases {
            let error = send(&HttpRequest::get(url)).expect_err("URL must be rejected");
            assert_eq!(error.kind(), expected, "URL case: {url}");
        }
    }

    #[test]
    fn invalid_config_is_rejected_before_network() {
        for timeout in [Duration::ZERO, Duration::from_millis(1_800_001)] {
            let config = HttpConfig {
                total_timeout: timeout,
                ..test_config()
            };
            let error = send_with_config(&HttpRequest::get("https://example.test/"), &config)
                .expect_err("invalid timeout must be rejected");
            assert_eq!(error.kind(), HttpErrorKind::InvalidConfig);
        }
    }

    #[test]
    fn ipv6_localhost_url_parsing_is_explicit() {
        let parsed =
            parse_url("http://[::1]:8080/v1/holidays/2026", true).expect("IPv6 loopback URL");
        assert_eq!(parsed.scheme, Scheme::Http);
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.path, "/v1/holidays/2026");
        assert!(parsed
            .host
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .is_loopback());

        let error = parse_url("http://[::1]:8080/v1?year=2026", true)
            .expect_err("query must still be rejected");
        assert_eq!(error.kind(), HttpErrorKind::QueryNotAllowed);

        let error = parse_url("http://example.test:8080/", true)
            .expect_err("test HTTP is limited to loopback");
        assert_eq!(error.kind(), HttpErrorKind::UnsupportedScheme);
    }

    #[test]
    fn malformed_headers_and_bearer_are_rejected() {
        let request =
            HttpRequest::get("https://example.test/").header("X-Test", "value\r\nInjected: yes");
        let error = send(&request).expect_err("CRLF header must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::InvalidHeader);

        let request =
            HttpRequest::get("https://example.test/").header("Authorization", "Bearer synthetic");
        let error = send(&request).expect_err("auth header must use bearer_token");
        assert_eq!(error.kind(), HttpErrorKind::InvalidHeader);

        let request = HttpRequest::get("https://example.test/").bearer_token("bad\r\ntoken");
        let error = send(&request).expect_err("CRLF bearer must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::InvalidHeader);

        let request = HttpRequest::get("https://example.test/")
            .header("X-Test", "one")
            .header("x-test", "two");
        let error = send(&request).expect_err("duplicate header must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::InvalidHeader);

        let request = HttpRequest::get("https://example.test/").header("Host", "evil.test");
        let error = send(&request).expect_err("framing header must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::InvalidHeader);
    }

    #[test]
    fn url_errors_do_not_contain_bearer() {
        let request = HttpRequest::get("https://example.test/v1/chat?token=synthetic")
            .bearer_token(SYNTHETIC_BEARER);
        let error = send(&request).expect_err("query must be rejected");
        assert_eq!(error.kind(), HttpErrorKind::QueryNotAllowed);
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SYNTHETIC_BEARER));
        assert!(!rendered.contains("Authorization"));
    }
    #[test]
    fn streaming_callback_runs_before_server_finishes_response() {
        let (tx, rx) = std::sync::mpsc::channel();
        let server = TestServer::start_with(1, move |stream, _, _| {
            write_test_headers(stream, 200, 6);
            stream.write_all(b"first").unwrap();
            stream.flush().unwrap();
            // A buffering implementation cannot release the final byte.
            rx.recv_timeout(Duration::from_secs(2))
                .expect("caller saw first chunk before EOF");
            stream.write_all(b"!").unwrap();
            stream.flush().unwrap();
        });
        let url = server.url("/incremental");
        let request = HttpRequest::get(&url);
        let mut received = Vec::new();
        let response = send_stream_localhost_for_tests(
            &request,
            &HttpConfig {
                total_timeout: Duration::from_secs(4),
                ..HttpConfig::default()
            },
            &mut |status, bytes| {
                assert_eq!(status, 200);
                received.extend_from_slice(bytes);
                let _ = tx.send(());
                true
            },
        )
        .unwrap();
        assert_eq!(received, b"first!");
        assert!(response.body.is_empty());
    }

    #[test]
    fn successful_streams_do_not_buffer_or_cap_total_wire_bytes() {
        for declared_length in [false, true] {
            let server = TestServer::start(1, move |_, _| {
                let mut response = TestResponse::ok(&vec![b'x'; 32_000]);
                response.include_content_length = declared_length;
                response
            });
            let url = server.url("/stream");
            let mut received = 0;
            let response = send_stream_localhost_for_tests(
                &HttpRequest::get(&url),
                &HttpConfig {
                    max_response_bytes: 64,
                    ..test_config()
                },
                &mut |status, bytes| {
                    assert_eq!(status, 200);
                    received += bytes.len();
                    true
                },
            )
            .unwrap();
            assert_eq!(received, 32_000);
            assert!(response.body.is_empty());
        }
    }

    #[test]
    fn streaming_errors_keep_bounded_bodies_and_rejected_consumers_stop() {
        for bytes in [16, 128] {
            let server = TestServer::start(1, move |_, _| {
                let mut response = TestResponse::ok(&vec![b'e'; bytes]);
                response.status = 400;
                response.without_content_length()
            });
            let url = server.url("/error");
            let result = send_stream_localhost_for_tests(
                &HttpRequest::get(&url),
                &HttpConfig {
                    max_response_bytes: 64,
                    ..test_config()
                },
                &mut |_, _| true,
            );
            if bytes <= 64 {
                assert_eq!(result.unwrap().body, vec![b'e'; bytes]);
            } else {
                assert_eq!(result.unwrap_err().kind(), HttpErrorKind::ResponseTooLarge);
            }
        }
        let server = TestServer::start(1, |_, _| TestResponse::ok(b"rejected frame"));
        let url = server.url("/reject");
        let result = send_stream_localhost_for_tests(
            &HttpRequest::get(&url),
            &test_config(),
            &mut |_, _| false,
        );
        assert_eq!(result.unwrap_err().kind(), HttpErrorKind::Protocol);
    }
}
