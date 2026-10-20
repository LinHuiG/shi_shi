//! Narrow HTTP transport built on asynchronous WinHTTP.
//!
//! Production callers can only use absolute `https://` URLs. Plain `http://`
//! is accepted solely for loopback hosts through the crate-private
//! `send_localhost_for_tests` helper compiled under `cfg(test)`; it is not part
//! of the production API. URLs may not contain userinfo, a query, or a
//! fragment. The transport uses the system WinHTTP proxy configuration and
//! validates TLS with the system defaults.
//!
//! Every call has one monotonic overall deadline. It is enforced while waiting
//! for DNS/proxy resolution, connect, request send, response headers, and every
//! body read, and it is shared by all redirects. Cancellation is observed at
//! most one poll interval after the flag is set, then the pending request handle
//! is closed to abort WinHTTP. The callback state, request body, and read buffer
//! are owned by the request-handle context: its final `HANDLE_CLOSING` callback
//! releases them exactly once, even if that callback arrives after the call has
//! returned. Session and connection handles carry no context. This avoids both
//! long native-timeout overshoot and use-after-free without an unbounded
//! permanent leak.
//!
//! A request is executed once unless it is a `GET` with an HTTP redirect to the
//! same origin. Cross-origin redirects and any downgrade are rejected. `POST`
//! redirects are returned to the caller as a normal `3xx` response so that a
//! request body is never replayed automatically. Requests are never retried.
//!
//! Errors expose only a safe class and an optional WinHTTP error code. They
//! never contain URLs, headers, bearer tokens, or response bodies. The
//! response itself is returned to the internal caller together with its status
//! code and optional bounded `ETag`/`Last-Modified` cache validators so
//! provider-specific status mapping remains outside this module.

use std::ffi::c_void;
use std::fmt;
use std::net::IpAddr;
use std::ptr::{null, null_mut};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER};
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetStatusCallback, WinHttpSetTimeouts,
    ERROR_WINHTTP_CLIENT_AUTH_CERT_NEEDED, ERROR_WINHTTP_CLIENT_AUTH_CERT_NEEDED_PROXY,
    ERROR_WINHTTP_CLIENT_CERT_NO_ACCESS_PRIVATE_KEY, ERROR_WINHTTP_CLIENT_CERT_NO_PRIVATE_KEY,
    ERROR_WINHTTP_HEADER_NOT_FOUND, ERROR_WINHTTP_OPERATION_CANCELLED,
    ERROR_WINHTTP_SECURE_CERT_CN_INVALID, ERROR_WINHTTP_SECURE_FAILURE,
    ERROR_WINHTTP_SECURE_FAILURE_PROXY, ERROR_WINHTTP_SECURE_INVALID_CA,
    ERROR_WINHTTP_SECURE_INVALID_CERT, ERROR_WINHTTP_TIMEOUT, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_ASYNC_RESULT, WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS,
    WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE, WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING,
    WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE, WINHTTP_CALLBACK_STATUS_READ_COMPLETE,
    WINHTTP_CALLBACK_STATUS_REQUEST_ERROR, WINHTTP_CALLBACK_STATUS_SECURE_FAILURE,
    WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE, WINHTTP_DISABLE_COOKIES,
    WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_ASYNC, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_CONTEXT_VALUE, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_CUSTOM,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};

/// Default maximum response body size: 2 MiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Default whole-request deadline: 120 seconds.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

const MAX_CONFIGURED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TIMEOUT_MS: u128 = 1_800_000;
const MAX_REDIRECTS: u8 = 10;
const MAX_URL_BYTES: usize = 8 * 1024;
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
const READ_CHUNK_BYTES: usize = 8 * 1024;
const USER_AGENT: &str = "ShiShi/1.0";
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CLOSE_CALLBACK_WAIT: Duration = Duration::from_secs(1);

/// HTTP method supported by this narrow transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
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
    #[cfg(test)]
    test_probe: Option<Arc<TestProbe>>,
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
            #[cfg(test)]
            test_probe: None,
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
            #[cfg(test)]
            test_probe: None,
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
    /// WinHTTP runs asynchronously. While waiting for the network, this
    /// transport wakes at least every 20 ms, checks the flag, and on
    /// cancellation closes the pending request to abort the operation. A
    /// successful response whose body was fully read is returned even if the
    /// flag is set afterwards; a cancellation observed before completion
    /// returns [`HttpErrorKind::Cancelled`]. [`send`] blocks the calling
    /// thread, so callers should run it on a worker thread and set this flag
    /// from their stop path.
    pub fn cancellation(mut self, flag: &'a AtomicBool) -> Self {
        self.cancellation = Some(flag);
        self
    }

    #[cfg(test)]
    fn test_probe(mut self, probe: Arc<TestProbe>) -> Self {
        self.test_probe = Some(probe);
        self
    }
}

/// Finite transport limits.
///
/// `total_timeout` is one monotonic deadline for the whole call: DNS/proxy
/// resolution, connect, request send, response headers, all body reads, and
/// every same-origin redirect. It is enforced by the transport, not by native
/// blocking timeouts. Values are 1 ms–30 minutes. `max_redirects` applies to
/// GET only and is at most 10.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub total_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_redirects: u8,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_redirects: 3,
        }
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
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
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

/// Public transport error containing only a safe class and WinHTTP code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpError {
    kind: HttpErrorKind,
    winhttp_code: Option<u32>,
}

impl HttpError {
    fn new(kind: HttpErrorKind) -> Self {
        Self {
            kind,
            winhttp_code: None,
        }
    }

    fn from_winhttp(code: u32) -> Self {
        let kind = match code {
            ERROR_WINHTTP_TIMEOUT => HttpErrorKind::Timeout,
            ERROR_WINHTTP_OPERATION_CANCELLED => HttpErrorKind::Cancelled,
            ERROR_WINHTTP_SECURE_CERT_CN_INVALID
            | ERROR_WINHTTP_SECURE_FAILURE
            | ERROR_WINHTTP_SECURE_FAILURE_PROXY
            | ERROR_WINHTTP_SECURE_INVALID_CA
            | ERROR_WINHTTP_SECURE_INVALID_CERT
            | ERROR_WINHTTP_CLIENT_AUTH_CERT_NEEDED
            | ERROR_WINHTTP_CLIENT_AUTH_CERT_NEEDED_PROXY
            | ERROR_WINHTTP_CLIENT_CERT_NO_ACCESS_PRIVATE_KEY
            | ERROR_WINHTTP_CLIENT_CERT_NO_PRIVATE_KEY => HttpErrorKind::Tls,
            _ => HttpErrorKind::Network,
        };
        Self {
            kind,
            winhttp_code: Some(code),
        }
    }

    fn from_winhttp_with_cancel(code: u32, cancellation: Option<&AtomicBool>) -> Self {
        if is_cancelled(cancellation) {
            HttpError::new(HttpErrorKind::Cancelled)
        } else {
            HttpError::from_winhttp(code)
        }
    }

    pub fn kind(&self) -> HttpErrorKind {
        self.kind
    }

    /// Optional native WinHTTP error code. This is a safe diagnostic; it
    /// contains no request data.
    pub fn winhttp_code(&self) -> Option<u32> {
        self.winhttp_code
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.winhttp_code {
            Some(code) => write!(
                f,
                "HTTP transport error: {} (WinHTTP {})",
                self.kind.as_str(),
                code
            ),
            None => write!(f, "HTTP transport error: {}", self.kind.as_str()),
        }
    }
}

impl std::error::Error for HttpError {}

/// Send a request with production HTTPS-only policy and default limits.
pub fn send(request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
    send_with_config(request, &HttpConfig::default())
}

/// Send a request with production HTTPS-only policy and explicit limits.
///
/// This blocks while the asynchronous WinHTTP request runs and is bounded by
/// `config.total_timeout`. Call it outside the UI thread.
pub fn send_with_config(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, false, &mut |_, _| {})
}

/// Incremental body delivery on the caller thread. The same overall deadline,
/// byte cap, cancellation and HTTPS rules apply; no automatic resubmission.
pub fn send_stream_with_config(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
    chunk: &mut dyn FnMut(u16, &[u8]),
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, false, chunk)
}
#[cfg(test)]
pub(crate) fn send_stream_localhost_for_tests(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
    chunk: &mut dyn FnMut(u16, &[u8]),
) -> Result<HttpResponse, HttpError> {
    send_inner(request, config, true, chunk)
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
    send_inner(request, config, true, &mut |_, _| {})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Https,
    Http,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Http => 80,
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedUrl {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
}

impl ParsedUrl {
    fn parse(input: &str, allow_localhost_http: bool) -> Result<Self, HttpError> {
        if input.is_empty()
            || input.len() > MAX_URL_BYTES
            || !input.is_ascii()
            || input
                .bytes()
                .any(|byte| byte <= b' ' || byte == 0x7f || byte == b'\\')
        {
            return Err(HttpError::new(HttpErrorKind::InvalidUrl));
        }
        if input.contains('?') {
            return Err(HttpError::new(HttpErrorKind::QueryNotAllowed));
        }
        if input.contains('#') {
            return Err(HttpError::new(HttpErrorKind::FragmentNotAllowed));
        }

        let scheme_end = input
            .find("://")
            .ok_or_else(|| HttpError::new(HttpErrorKind::InvalidUrl))?;
        let scheme_text = &input[..scheme_end];
        let rest = &input[scheme_end + 3..];
        let scheme = if scheme_text.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else if scheme_text.eq_ignore_ascii_case("http") {
            Scheme::Http
        } else {
            return Err(HttpError::new(HttpErrorKind::UnsupportedScheme));
        };

        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let path = if authority_end == rest.len() {
            "/"
        } else {
            &rest[authority_end..]
        };
        if authority.is_empty() {
            return Err(HttpError::new(HttpErrorKind::InvalidUrl));
        }
        if authority.contains('@') {
            return Err(HttpError::new(HttpErrorKind::UserInfoNotAllowed));
        }

        let (host, port_text, ipv6_literal) =
            if let Some(after_bracket) = authority.strip_prefix('[') {
                let close = after_bracket
                    .find(']')
                    .ok_or_else(|| HttpError::new(HttpErrorKind::InvalidUrl))?;
                let inner = &after_bracket[..close];
                let suffix = &after_bracket[close + 1..];
                let address: std::net::Ipv6Addr = inner
                    .parse()
                    .map_err(|_| HttpError::new(HttpErrorKind::InvalidUrl))?;
                let port_text = if suffix.is_empty() {
                    None
                } else {
                    Some(
                        suffix
                            .strip_prefix(':')
                            .ok_or_else(|| HttpError::new(HttpErrorKind::InvalidUrl))?,
                    )
                };
                (address.to_string(), port_text, true)
            } else {
                if authority.matches(':').count() > 1 {
                    // Unbracketed IPv6 literals are ambiguous and rejected.
                    return Err(HttpError::new(HttpErrorKind::InvalidUrl));
                }
                let (host, port_text) = match authority.split_once(':') {
                    Some((host, port)) => (host, Some(port)),
                    None => (authority, None),
                };
                if host.is_empty()
                    || !host.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_'
                    })
                {
                    return Err(HttpError::new(HttpErrorKind::InvalidUrl));
                }
                (host.to_ascii_lowercase(), port_text, false)
            };

        let port = match port_text {
            None => scheme.default_port(),
            Some(text) => {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(HttpError::new(HttpErrorKind::InvalidUrl));
                }
                let port: u32 = text
                    .parse()
                    .map_err(|_| HttpError::new(HttpErrorKind::InvalidUrl))?;
                if !(1..=u16::MAX as u32).contains(&port) {
                    return Err(HttpError::new(HttpErrorKind::InvalidUrl));
                }
                port as u16
            }
        };

        if !path.starts_with('/') {
            return Err(HttpError::new(HttpErrorKind::InvalidUrl));
        }

        if scheme == Scheme::Http {
            if !allow_localhost_http {
                return Err(HttpError::new(HttpErrorKind::UnsupportedScheme));
            }
            if !is_loopback_host(&host, ipv6_literal) {
                return Err(HttpError::new(HttpErrorKind::UnsupportedScheme));
            }
        }

        Ok(Self {
            scheme,
            host,
            port,
            path: path.to_string(),
        })
    }

    fn same_origin(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
    }
}

fn is_loopback_host(host: &str, _ipv6_literal: bool) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn send_inner(
    request: &HttpRequest<'_>,
    config: &HttpConfig,
    allow_localhost_http: bool,
    chunk: &mut dyn FnMut(u16, &[u8]),
) -> Result<HttpResponse, HttpError> {
    validate_config(config)?;
    validate_request(request)?;

    let mut url = ParsedUrl::parse(request.url, allow_localhost_http)?;
    let header_block = build_header_block(request);
    let header_wide = wide(&header_block);
    let deadline = Instant::now() + config.total_timeout;
    let mut redirects = 0u8;

    loop {
        check_cancelled(request.cancellation)?;
        if Instant::now() >= deadline {
            return Err(HttpError::new(HttpErrorKind::Timeout));
        }
        let single = execute_once(
            request,
            &url,
            &header_wide,
            config,
            allow_localhost_http,
            deadline,
            chunk,
        )?;

        if request.method == HttpMethod::Get && is_redirect_status(single.status) {
            if redirects >= config.max_redirects {
                return Err(HttpError::new(HttpErrorKind::RedirectRejected));
            }
            let location = single
                .location
                .ok_or_else(|| HttpError::new(HttpErrorKind::RedirectRejected))?;
            let next = resolve_redirect(&url, &location, allow_localhost_http)?;
            url = next;
            redirects += 1;
            continue;
        }

        return Ok(HttpResponse {
            status: single.status,
            body: single.body,
            etag: single.etag,
            last_modified: single.last_modified,
        });
    }
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

fn build_header_block(request: &HttpRequest<'_>) -> String {
    let mut headers = String::new();
    for header in &request.headers {
        headers.push_str(header.name);
        headers.push_str(": ");
        headers.push_str(header.value);
        headers.push_str("\r\n");
    }
    if let Some(token) = request.bearer_token {
        headers.push_str("Authorization: Bearer ");
        headers.push_str(token);
        headers.push_str("\r\n");
    }
    headers
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
        ParsedUrl::parse(location, allow_localhost_http)
            .map_err(|_| HttpError::new(HttpErrorKind::RedirectRejected))?
    } else if has_unsupported_absolute_scheme(location) {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    } else if let Some(scheme_relative) = location.strip_prefix("//") {
        let absolute = format!("{}://{}", current.scheme.as_str(), scheme_relative);
        ParsedUrl::parse(&absolute, allow_localhost_http)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsyncEvent {
    SendComplete,
    HeadersAvailable,
    DataAvailable(u32),
    ReadComplete(u32),
    RequestError(u32),
    SecureFailure,
}

struct AsyncInner {
    event: Option<AsyncEvent>,
    /// Handles that own the callback context. Only the request handle receives
    /// the context, so this is 0 or 1; the counter keeps the final-callback
    /// rule explicit.
    attached_handles: usize,
    closed_handles: usize,
}

/// Test-only accounting probe. It observes exactly when the callback-owned
/// state (including request body and read buffer) is released.
#[cfg(test)]
#[derive(Default)]
struct TestProbe {
    released: AtomicUsize,
    delay_closing: AtomicBool,
}

#[cfg(test)]
impl TestProbe {
    fn released(&self) -> usize {
        self.released.load(Ordering::SeqCst)
    }

    fn set_delay_closing(&self) {
        self.delay_closing.store(true, Ordering::SeqCst);
    }

    fn maybe_delay_closing(&self) {
        if self.delay_closing.load(Ordering::SeqCst) {
            // Longer than CLOSE_CALLBACK_WAIT to model a late final callback.
            std::thread::sleep(CLOSE_CALLBACK_WAIT + Duration::from_millis(200));
        }
    }
}

/// Callback state and native buffers.
///
/// WinHTTP may deliver callbacks until the final `HANDLE_CLOSING` notification
/// for the request handle. The request body and read buffer therefore live here
/// rather than on the calling stack. Only the request handle carries this
/// context value; session and connection handles keep a zero context so their
/// later callbacks cannot reach this state.
struct AsyncState {
    inner: Mutex<AsyncInner>,
    ready: Condvar,
    request_body: Option<Box<[u8]>>,
    read_buffer: Mutex<Box<[u8]>>,
    #[cfg(test)]
    test_probe: Option<Arc<TestProbe>>,
}

impl AsyncState {
    fn new(request_body: Option<Box<[u8]>>) -> Self {
        Self {
            inner: Mutex::new(AsyncInner {
                event: None,
                attached_handles: 0,
                closed_handles: 0,
            }),
            ready: Condvar::new(),
            request_body,
            read_buffer: Mutex::new(vec![0u8; READ_CHUNK_BYTES].into_boxed_slice()),
            #[cfg(test)]
            test_probe: None,
        }
    }

    #[cfg(test)]
    fn new_with_probe(request_body: Option<Box<[u8]>>, probe: Option<Arc<TestProbe>>) -> Self {
        let mut state = Self::new(request_body);
        state.test_probe = probe;
        state
    }
}

impl Drop for AsyncState {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(probe) = &self.test_probe {
            probe.released.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn lock_state(state: &AsyncState) -> MutexGuard<'_, AsyncInner> {
    state
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

unsafe extern "system" fn status_callback(
    _handle: *mut c_void,
    context: usize,
    status: u32,
    information: *mut c_void,
    information_length: u32,
) {
    if context == 0 {
        return;
    }
    let state = unsafe { &*(context as *const AsyncState) };
    #[cfg(test)]
    if status == WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING {
        if let Some(probe) = &state.test_probe {
            probe.maybe_delay_closing();
        }
    }
    let mut release_native = false;
    let mut inner = lock_state(state);
    match status {
        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => {
            inner.event = Some(AsyncEvent::SendComplete);
        }
        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => {
            inner.event = Some(AsyncEvent::HeadersAvailable);
        }
        WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE => {
            let available = if information.is_null() {
                0
            } else {
                unsafe { *(information as *const u32) }
            };
            inner.event = Some(AsyncEvent::DataAvailable(available));
        }
        WINHTTP_CALLBACK_STATUS_READ_COMPLETE => {
            inner.event = Some(AsyncEvent::ReadComplete(information_length));
        }
        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            let code = if information.is_null() {
                0
            } else {
                unsafe { (*(information as *const WINHTTP_ASYNC_RESULT)).dwError }
            };
            inner.event = Some(AsyncEvent::RequestError(code));
        }
        WINHTTP_CALLBACK_STATUS_SECURE_FAILURE => {
            inner.event = Some(AsyncEvent::SecureFailure);
        }
        WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING if inner.attached_handles != 0 => {
            inner.closed_handles += 1;
            if inner.closed_handles == inner.attached_handles {
                // This is the last callback that can access the state.
                release_native = true;
            }
        }
        _ => {}
    }
    // Notify while still holding the mutex. The owner may free the state after
    // observing the final closing callback; no access to `state` may happen
    // after this guard is released.
    state.ready.notify_all();
    drop(inner);
    if release_native {
        // The raw strong reference created in execute_once is the native-side
        // ownership of the state. Releasing it here keeps the body, read
        // buffer, and state alive until WinHTTP's final request callback.
        unsafe {
            drop(Arc::from_raw(context as *const AsyncState));
        }
    }
}

fn wait_for_event(
    state: &AsyncState,
    deadline: Instant,
    cancellation: Option<&AtomicBool>,
) -> Result<AsyncEvent, HttpError> {
    let mut inner = lock_state(state);
    loop {
        if let Some(event) = inner.event.take() {
            return Ok(event);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HttpError::new(HttpErrorKind::Timeout));
        }
        if is_cancelled(cancellation) {
            return Err(HttpError::new(HttpErrorKind::Cancelled));
        }
        let wait = (deadline - now).min(CANCEL_POLL_INTERVAL);
        let (next, _) = state
            .ready
            .wait_timeout(inner, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner = next;
    }
}

fn wait_for_close(state: &AsyncState, expected: usize) -> bool {
    if expected == 0 {
        return true;
    }
    let deadline = Instant::now() + CLOSE_CALLBACK_WAIT;
    let mut inner = lock_state(state);
    while inner.closed_handles < expected {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (next, _) = state
            .ready
            .wait_timeout(inner, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner = next;
    }
    true
}

fn close_handles(handles: &mut Vec<*mut c_void>) {
    for raw in handles.drain(..).rev() {
        unsafe {
            WinHttpCloseHandle(raw);
        }
    }
}

/// Owns all created native handles until cleanup. Its `Drop` also closes
/// handles if the operation unwinds, so a panic cannot leak open handles or
/// release the callback state before the final `HANDLE_CLOSING` notification.
struct HandleCleanup<'a> {
    state: &'a Arc<AsyncState>,
    native_state: *const AsyncState,
    handles: Vec<*mut c_void>,
    finished: bool,
}

impl HandleCleanup<'_> {
    fn finish(&mut self) -> bool {
        if self.finished {
            return true;
        }
        self.finished = true;
        let expected = lock_state(self.state).attached_handles;
        close_handles(&mut self.handles);
        let cleaned = wait_for_close(self.state, expected);
        if expected == 0 {
            // No handle stored the context value, so no callback can reference
            // it. Reclaim the native-side strong reference immediately.
            unsafe {
                drop(Arc::from_raw(self.native_state));
            }
        }
        cleaned
    }
}

impl Drop for HandleCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct SingleResponse {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    location: Option<String>,
}

fn execute_once(
    request: &HttpRequest<'_>,
    url: &ParsedUrl,
    header_wide: &[u16],
    config: &HttpConfig,
    allow_localhost_http: bool,
    deadline: Instant,
    chunk: &mut dyn FnMut(u16, &[u8]),
) -> Result<SingleResponse, HttpError> {
    let cancellation = request.cancellation;
    check_cancelled(cancellation)?;

    let request_body = request.body.map(|bytes| bytes.to_vec().into_boxed_slice());
    #[cfg(test)]
    let state = Arc::new(AsyncState::new_with_probe(
        request_body,
        request.test_probe.clone(),
    ));
    #[cfg(not(test))]
    let state = Arc::new(AsyncState::new(request_body));
    // One native-side strong reference. It is released exactly once by the
    // final request-handle HANDLE_CLOSING callback, or reclaimed here if no
    // handle ever stored the context value.
    let native_state = Arc::into_raw(Arc::clone(&state));
    let mut cleanup = HandleCleanup {
        state: &state,
        native_state,
        handles: Vec::new(),
        finished: false,
    };
    let result = run_async(
        request.method,
        url,
        header_wide,
        cancellation,
        config,
        allow_localhost_http,
        deadline,
        &state,
        native_state as usize,
        &mut cleanup.handles,
        chunk,
    );
    // On a late final callback this returns false after closing the handles;
    // the callback still owns the state and releases it when it arrives. There
    // is no explicit leak and no detached cleanup thread.
    let _ = cleanup.finish();
    result
}

#[allow(clippy::too_many_arguments)]
fn run_async(
    method: HttpMethod,
    url: &ParsedUrl,
    header_wide: &[u16],
    cancellation: Option<&AtomicBool>,
    config: &HttpConfig,
    allow_localhost_http: bool,
    deadline: Instant,
    state: &Arc<AsyncState>,
    context: usize,
    handles: &mut Vec<*mut c_void>,
    chunk: &mut dyn FnMut(u16, &[u8]),
) -> Result<SingleResponse, HttpError> {
    let agent = wide(USER_AGENT);
    let access_type = if allow_localhost_http && url.scheme == Scheme::Http {
        WINHTTP_ACCESS_TYPE_NO_PROXY
    } else {
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY
    };
    let session = unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            access_type,
            null(),
            null(),
            WINHTTP_FLAG_ASYNC,
        )
    };
    if session.is_null() {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    handles.push(session);
    // WinHTTP otherwise retains its shorter default receive timeout, cutting
    // off non-streaming reasoning before our monotonic request deadline.
    let native_timeout = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .clamp(1, i32::MAX as u128) as i32;
    if unsafe {
        WinHttpSetTimeouts(
            session,
            native_timeout,
            native_timeout,
            native_timeout,
            native_timeout,
        )
    } == 0
    {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    install_status_callback(session, cancellation)?;

    let host_wide = wide(&url.host);
    let connection = unsafe { WinHttpConnect(session, host_wide.as_ptr(), url.port, 0) };
    if connection.is_null() {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    handles.push(connection);

    let verb = wide(match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
    });
    let path_wide = wide(&url.path);
    let flags = if url.scheme == Scheme::Https {
        WINHTTP_FLAG_SECURE
    } else {
        0
    };
    let request = unsafe {
        WinHttpOpenRequest(
            connection,
            verb.as_ptr(),
            path_wide.as_ptr(),
            null(),
            null(),
            null(),
            flags,
        )
    };
    if request.is_null() {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    handles.push(request);
    attach_context(request, context, cancellation, state)?;

    // WINHTTP_OPTION_DISABLE_FEATURE is valid only on request handles. Both
    // redirect controls are set before sending; a failure aborts the request
    // rather than risking an automatic cross-origin or downgrade redirect.
    let disabled_features = WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES;
    set_u32_option(
        request,
        WINHTTP_OPTION_DISABLE_FEATURE,
        disabled_features,
        cancellation,
    )?;
    set_u32_option(
        request,
        WINHTTP_OPTION_REDIRECT_POLICY,
        WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        cancellation,
    )?;

    check_cancelled(cancellation)?;
    if Instant::now() >= deadline {
        return Err(HttpError::new(HttpErrorKind::Timeout));
    }

    let headers_len = if header_wide.len() > 1 {
        (header_wide.len() - 1) as u32
    } else {
        0
    };
    let headers_ptr = if headers_len == 0 {
        null()
    } else {
        header_wide.as_ptr()
    };
    let (body_ptr, body_len) = match state.request_body.as_deref() {
        Some(body) => (body.as_ptr() as *const c_void, body.len() as u32),
        None => (null(), 0),
    };
    let sent = unsafe {
        WinHttpSendRequest(
            request,
            headers_ptr,
            headers_len,
            body_ptr,
            body_len,
            body_len,
            context,
        )
    };
    if sent == 0 {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    match wait_for_event(state, deadline, cancellation)? {
        AsyncEvent::SendComplete => {}
        AsyncEvent::RequestError(code) => {
            return Err(HttpError::from_winhttp_with_cancel(code, cancellation));
        }
        AsyncEvent::SecureFailure => return Err(HttpError::new(HttpErrorKind::Tls)),
        _ => return Err(HttpError::new(HttpErrorKind::Protocol)),
    }

    check_cancelled(cancellation)?;
    if unsafe { WinHttpReceiveResponse(request, null_mut()) } == 0 {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    match wait_for_event(state, deadline, cancellation)? {
        AsyncEvent::HeadersAvailable => {}
        AsyncEvent::RequestError(code) => {
            return Err(HttpError::from_winhttp_with_cancel(code, cancellation));
        }
        AsyncEvent::SecureFailure => return Err(HttpError::new(HttpErrorKind::Tls)),
        _ => return Err(HttpError::new(HttpErrorKind::Protocol)),
    }

    let status = query_status(request)?;

    if method == HttpMethod::Get && is_redirect_status(status) {
        let location = query_location(request)?
            .ok_or_else(|| HttpError::new(HttpErrorKind::RedirectRejected))?;
        return Ok(SingleResponse {
            status,
            body: Vec::new(),
            etag: None,
            last_modified: None,
            location: Some(location),
        });
    }

    if let Some(length) = query_content_length(request) {
        if length as usize > config.max_response_bytes {
            return Err(HttpError::new(HttpErrorKind::ResponseTooLarge));
        }
    }

    // Optional cache validators are deliberately non-fatal: an absent,
    // oversized, or malformed value becomes `None` so it cannot break an
    // otherwise valid response.
    let etag = query_cache_header(request, "ETag", MAX_ETAG_BYTES, is_valid_etag);
    let last_modified = query_cache_header(
        request,
        "Last-Modified",
        MAX_LAST_MODIFIED_BYTES,
        is_valid_http_date,
    );

    let mut read_buffer = state
        .read_buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut response_body = Vec::new();
    loop {
        check_cancelled(cancellation)?;
        if Instant::now() >= deadline {
            return Err(HttpError::new(HttpErrorKind::Timeout));
        }
        // In asynchronous mode the out parameter must be NULL; the callback
        // reports how many bytes are available.
        if unsafe { WinHttpQueryDataAvailable(request, null_mut()) } == 0 {
            return Err(HttpError::from_winhttp_with_cancel(
                unsafe { GetLastError() },
                cancellation,
            ));
        }
        let available = match wait_for_event(state, deadline, cancellation)? {
            AsyncEvent::DataAvailable(available) => available,
            AsyncEvent::RequestError(code) => {
                return Err(HttpError::from_winhttp_with_cancel(code, cancellation));
            }
            AsyncEvent::SecureFailure => return Err(HttpError::new(HttpErrorKind::Tls)),
            _ => return Err(HttpError::new(HttpErrorKind::Protocol)),
        };
        if available == 0 {
            break;
        }

        let to_read = available.min(read_buffer.len() as u32);
        // In asynchronous mode the bytes-read out parameter must be NULL; the
        // callback reports the actual count.
        if unsafe {
            WinHttpReadData(
                request,
                read_buffer.as_mut_ptr() as *mut c_void,
                to_read,
                null_mut(),
            )
        } == 0
        {
            return Err(HttpError::from_winhttp_with_cancel(
                unsafe { GetLastError() },
                cancellation,
            ));
        }
        let read = match wait_for_event(state, deadline, cancellation)? {
            AsyncEvent::ReadComplete(read) => read,
            AsyncEvent::RequestError(code) => {
                return Err(HttpError::from_winhttp_with_cancel(code, cancellation));
            }
            AsyncEvent::SecureFailure => return Err(HttpError::new(HttpErrorKind::Tls)),
            _ => return Err(HttpError::new(HttpErrorKind::Protocol)),
        };
        if read == 0 {
            break;
        }
        if read as usize > read_buffer.len() {
            return Err(HttpError::new(HttpErrorKind::Protocol));
        }
        match response_body.len().checked_add(read as usize) {
            Some(length) if length <= config.max_response_bytes => {}
            _ => return Err(HttpError::new(HttpErrorKind::ResponseTooLarge)),
        }
        chunk(status, &read_buffer[..read as usize]);
        response_body.extend_from_slice(&read_buffer[..read as usize]);
    }

    // No cancellation check here by design: EOF means the response is already
    // complete. A flag set after this point must not turn a completed response
    // into a cancellation error.
    Ok(SingleResponse {
        status,
        body: response_body,
        etag,
        last_modified,
        location: None,
    })
}

fn install_status_callback(
    session: *mut c_void,
    cancellation: Option<&AtomicBool>,
) -> Result<(), HttpError> {
    let previous = unsafe {
        WinHttpSetStatusCallback(
            session,
            Some(status_callback),
            WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS,
            0,
        )
    };
    if previous.map(|callback| callback as usize) == Some(usize::MAX) {
        return Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ));
    }
    Ok(())
}

/// Publish `context` on the request handle and count it as an owner.
///
/// Only the request handle receives the context. Session and connection
/// callbacks therefore carry a zero context and cannot outlive the request
/// state. The count is incremented before the option is published so a final
/// callback cannot race the accounting; it is rolled back if publishing fails.
fn attach_context(
    handle: *mut c_void,
    context: usize,
    cancellation: Option<&AtomicBool>,
    state: &AsyncState,
) -> Result<(), HttpError> {
    {
        let mut inner = lock_state(state);
        inner.attached_handles += 1;
    }
    let ok = unsafe {
        WinHttpSetOption(
            handle,
            WINHTTP_OPTION_CONTEXT_VALUE,
            &context as *const usize as *const c_void,
            std::mem::size_of::<usize>() as u32,
        )
    };
    if ok == 0 {
        let mut inner = lock_state(state);
        inner.attached_handles = inner.attached_handles.saturating_sub(1);
        Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ))
    } else {
        Ok(())
    }
}

fn set_u32_option(
    handle: *mut c_void,
    option: u32,
    value: u32,
    cancellation: Option<&AtomicBool>,
) -> Result<(), HttpError> {
    let ok = unsafe {
        WinHttpSetOption(
            handle,
            option,
            &value as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if ok == 0 {
        Err(HttpError::from_winhttp_with_cancel(
            unsafe { GetLastError() },
            cancellation,
        ))
    } else {
        Ok(())
    }
}

fn query_status(handle: *mut c_void) -> Result<u16, HttpError> {
    let mut status: u32 = 0;
    let mut length = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut length,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(HttpError::from_winhttp(unsafe { GetLastError() }));
    }
    if !(100..=599).contains(&status) {
        return Err(HttpError::new(HttpErrorKind::Protocol));
    }
    Ok(status as u16)
}

fn query_content_length(handle: *mut c_void) -> Option<u32> {
    let mut length: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut length as *mut u32 as *mut c_void,
            &mut size,
            null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(length)
    }
}

fn query_location(handle: *mut c_void) -> Result<Option<String>, HttpError> {
    let mut required_bytes: u32 = 0;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_LOCATION,
            null(),
            null_mut(),
            &mut required_bytes,
            null_mut(),
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_WINHTTP_HEADER_NOT_FOUND {
            return Ok(None);
        }
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(HttpError::from_winhttp(code));
        }
    }
    if required_bytes as usize > MAX_LOCATION_BYTES + 2 {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }
    if required_bytes == 0 {
        return Ok(Some(String::new()));
    }

    let units = (required_bytes as usize).div_ceil(2) + 1;
    let mut buffer = vec![0u16; units];
    let mut buffer_bytes = (buffer.len() * 2) as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_LOCATION,
            null(),
            buffer.as_mut_ptr() as *mut c_void,
            &mut buffer_bytes,
            null_mut(),
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_INSUFFICIENT_BUFFER {
            return Err(HttpError::new(HttpErrorKind::RedirectRejected));
        }
        return Err(HttpError::from_winhttp(code));
    }

    let used_units = ((buffer_bytes as usize).min(buffer.len() * 2) / 2).min(buffer.len());
    let value = String::from_utf16_lossy(&buffer[..used_units]);
    let value = value.trim_end_matches('\0').to_string();
    if value.len() > MAX_LOCATION_BYTES {
        return Err(HttpError::new(HttpErrorKind::RedirectRejected));
    }
    Ok(Some(value))
}

/// Query a named response header and return a validated, bounded value.
///
/// Missing headers, native query failures, oversized values, and malformed
/// values all return `None`: these validators are optional hints, so a server
/// cannot turn a bad cache header into a failed body fetch. No header value is
/// ever placed in an error or log message.
fn query_cache_header(
    handle: *mut c_void,
    name: &str,
    max_bytes: usize,
    validate: fn(&str) -> bool,
) -> Option<String> {
    let name_wide = wide(name);
    let mut required_bytes: u32 = 0;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_CUSTOM,
            name_wide.as_ptr(),
            null_mut(),
            &mut required_bytes,
            null_mut(),
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_WINHTTP_HEADER_NOT_FOUND {
            return None;
        }
        if code != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
    }
    if required_bytes == 0 || required_bytes as usize > max_bytes * 2 + 2 {
        return None;
    }

    let units = ((required_bytes as usize).div_ceil(2) + 1).min(max_bytes + 2);
    let mut buffer = vec![0u16; units];
    let mut buffer_bytes = (buffer.len() * 2) as u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            handle,
            WINHTTP_QUERY_CUSTOM,
            name_wide.as_ptr(),
            buffer.as_mut_ptr() as *mut c_void,
            &mut buffer_bytes,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }

    // On success WinHTTP reports string bytes minus the terminating null.
    let used_units = ((buffer_bytes as usize) / 2).min(buffer.len());
    let value = String::from_utf16_lossy(&buffer[..used_units]);
    let value = value.trim_matches(|c| c == ' ' || c == '\t');
    if value.is_empty() || value.len() > max_bytes || !value.is_ascii() || !validate(value) {
        return None;
    }
    Some(value.to_string())
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

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const SYNTHETIC_BEARER: &str = "synthetic-bearer-value-9f4a2c";
    const TEST_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

    struct TestRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        include_content_length: bool,
    }

    impl TestResponse {
        fn ok(body: &[u8]) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.to_vec(),
                include_content_length: true,
            }
        }

        fn status(status: u16, body: &[u8]) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_vec(),
                include_content_length: true,
            }
        }

        fn redirect(location: &str) -> Self {
            Self {
                status: 302,
                headers: vec![("Location".to_string(), location.to_string())],
                body: Vec::new(),
                include_content_length: true,
            }
        }

        fn without_content_length(mut self) -> Self {
            self.include_content_length = false;
            self
        }
    }

    struct TestServer {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<TestRequest>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start<F>(expected_requests: usize, handler: F) -> Self
        where
            F: Fn(&TestRequest, usize) -> TestResponse + Send + 'static,
        {
            Self::start_with(expected_requests, move |stream, request, index| {
                let response = handler(request, index);
                write_test_response(stream, &response);
            })
        }

        fn start_with<F>(expected_requests: usize, handler: F) -> Self
        where
            F: Fn(&mut TcpStream, &TestRequest, usize) + Send + 'static,
        {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let addr = listener.local_addr().expect("test server address");
            listener
                .set_nonblocking(true)
                .expect("nonblocking test listener");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + TEST_ACCEPT_TIMEOUT;
                let mut served = 0;
                while served < expected_requests && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking test stream");
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            let request = read_test_request(&mut stream);
                            handler(&mut stream, &request, served);
                            captured.lock().expect("requests mutex").push(request);
                            served += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                requests,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn join(mut self) -> Vec<TestRequest> {
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join test server");
            }
            let requests = match Arc::try_unwrap(self.requests) {
                Ok(requests) => requests,
                Err(_) => panic!("test server requests still shared"),
            };
            requests.into_inner().expect("requests mutex")
        }
    }

    fn test_config() -> HttpConfig {
        HttpConfig {
            total_timeout: Duration::from_secs(2),
            max_response_bytes: 4096,
            max_redirects: 3,
        }
    }

    fn wait_for_release(probe: &TestProbe, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let released = probe.released();
            if released != 0 || Instant::now() >= deadline {
                return released;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn read_test_request(stream: &mut TcpStream) -> TestRequest {
        let mut data = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read test request");
            if read == 0 {
                panic!("connection closed before request headers");
            }
            data.extend_from_slice(&chunk[..read]);
            if let Some(position) = find_bytes(&data, b"\r\n\r\n") {
                break position + 4;
            }
            if data.len() > 64 * 1024 {
                panic!("request headers too large");
            }
        };

        let head = std::str::from_utf8(&data[..header_end - 4]).expect("ASCII request headers");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method").to_string();
        let path = parts.next().expect("path").to_string();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let content_length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = data[header_end..].to_vec();
        while body.len() < content_length {
            let read = stream.read(&mut chunk).expect("read test body");
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);
        TestRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn write_test_response(stream: &mut TcpStream, response: &TestResponse) {
        let mut head = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", response.status);
        if response.include_content_length {
            head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
        }
        for (name, value) in &response.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&response.body);
        let _ = stream.flush();
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
        assert!(response.is_success());
        assert_eq!(response.body, br#"{"status":"ok"}"#);
        let requests = server.join();
        assert_eq!(requests.len(), 1);
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
    fn cache_validators_are_returned_when_valid() {
        const ETAG: &str = "\"synthetic-etag-42\"";
        const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
        let server = TestServer::start(1, |_, _| {
            let mut response = TestResponse::ok(b"cached");
            response.headers = vec![
                ("ETag".to_string(), ETAG.to_string()),
                ("Last-Modified".to_string(), LAST_MODIFIED.to_string()),
            ];
            response
        });
        let url = server.url("/cache");
        let response =
            send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).expect("GET");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"cached");
        assert_eq!(response.etag.as_deref(), Some(ETAG));
        assert_eq!(response.last_modified.as_deref(), Some(LAST_MODIFIED));
        let rendered = format!("{:?}", response);
        assert!(!rendered.contains(ETAG));
        assert!(!rendered.contains("2015"));
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn missing_cache_validators_are_none() {
        let server = TestServer::start(1, |_, _| TestResponse::ok(b"plain"));
        let url = server.url("/plain");
        let response =
            send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).expect("GET");
        assert_eq!(response.body, b"plain");
        assert_eq!(response.etag, None);
        assert_eq!(response.last_modified, None);
        let _ = server.join();
    }

    #[test]
    fn oversized_cache_validators_are_ignored() {
        let oversized_etag = format!("\"{}\"", "e".repeat(MAX_ETAG_BYTES + 2));
        let oversized_last_modified = format!(
            "Sun, 06 Nov 1994 08:49:37 GMT{}",
            "X".repeat(MAX_LAST_MODIFIED_BYTES)
        );
        let server = TestServer::start(1, move |_, _| {
            let mut response = TestResponse::ok(b"bounded");
            response.headers = vec![
                ("ETag".to_string(), oversized_etag.clone()),
                ("Last-Modified".to_string(), oversized_last_modified.clone()),
            ];
            response
        });
        let url = server.url("/oversized-cache");
        let response =
            send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).expect("GET");
        assert_eq!(response.body, b"bounded");
        assert_eq!(response.etag, None);
        assert_eq!(response.last_modified, None);
        let _ = server.join();
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
    fn malformed_cache_validators_are_ignored() {
        let server = TestServer::start(1, |_, _| {
            let mut response = TestResponse::ok(b"still-ok");
            response.headers = vec![
                ("ETag".to_string(), "not-a-quoted-tag".to_string()),
                ("Last-Modified".to_string(), "yesterday".to_string()),
            ];
            response
        });
        let url = server.url("/malformed-cache");
        let response =
            send_localhost_for_tests(&HttpRequest::get(&url), &test_config()).expect("GET");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"still-ok");
        assert_eq!(response.etag, None);
        assert_eq!(response.last_modified, None);
        let _ = server.join();
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
    fn non_streaming_response_can_wait_beyond_winhttp_default_receive_timeout() {
        let server = TestServer::start(1, |_, _| {
            thread::sleep(Duration::from_secs(32));
            TestResponse::ok(b"slow synthetic response")
        });
        let config = HttpConfig {
            total_timeout: Duration::from_secs(40),
            ..test_config()
        };
        let response =
            send_localhost_for_tests(&HttpRequest::get(&server.url("/slow-reasoning")), &config)
                .expect("configured deadline must permit a slow non-streaming response");
        assert_eq!(response.body, b"slow synthetic response");
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
            let probe = Arc::new(TestProbe::default());
            let client_probe = Arc::clone(&probe);
            let client = thread::spawn(move || {
                let request = HttpRequest::get(&client_url)
                    .cancellation(client_cancellation.as_ref())
                    .test_probe(client_probe);
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
            assert_eq!(
                wait_for_release(&probe, Duration::from_secs(2)),
                1,
                "each raced call must release its callback state exactly once"
            );
        }
        assert_eq!(server.join().len(), ITERATIONS);
    }

    #[test]
    fn completed_request_releases_callback_state_once() {
        let server = TestServer::start(1, |_, _| TestResponse::ok(b"released"));
        let url = server.url("/release");
        let probe = Arc::new(TestProbe::default());
        let request = HttpRequest::get(&url).test_probe(Arc::clone(&probe));
        let response =
            send_localhost_for_tests(&request, &test_config()).expect("GET should succeed");
        assert_eq!(response.body, b"released");
        assert_eq!(
            wait_for_release(&probe, Duration::from_secs(2)),
            1,
            "final request close must release the state exactly once"
        );
        let _ = server.join();
    }

    #[test]
    fn unattached_callback_state_is_reclaimed_once() {
        let probe = Arc::new(TestProbe::default());
        let state = Arc::new(AsyncState::new_with_probe(
            Some(b"synthetic-body".to_vec().into_boxed_slice()),
            Some(Arc::clone(&probe)),
        ));
        let native_state = Arc::into_raw(Arc::clone(&state));
        let mut cleanup = HandleCleanup {
            state: &state,
            native_state,
            handles: Vec::new(),
            finished: false,
        };
        assert!(
            cleanup.finish(),
            "no attached context means no close callback is owed"
        );
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "the native-side reference must be reclaimed exactly once"
        );
        drop(cleanup);
        assert_eq!(probe.released(), 0);
        drop(state);
        assert_eq!(probe.released(), 1);
    }

    #[test]
    fn final_close_callback_releases_native_reference_once() {
        let probe = Arc::new(TestProbe::default());
        let state = Arc::new(AsyncState::new_with_probe(
            Some(b"synthetic-body".to_vec().into_boxed_slice()),
            Some(Arc::clone(&probe)),
        ));
        let native_state = Arc::into_raw(Arc::clone(&state));
        lock_state(&state).attached_handles = 1;

        unsafe {
            status_callback(
                null_mut(),
                native_state as usize,
                WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING,
                null_mut(),
                0,
            );
        }
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "the final close callback must drop only the native reference"
        );
        drop(state);
        assert_eq!(probe.released(), 1);
    }

    #[test]
    fn delayed_close_releases_state_after_call_returns() {
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let server = TestServer::start_with(1, move |stream, _, _| {
            accepted_tx.send(()).expect("signal accepted request");
            let _ = release_rx
                .lock()
                .expect("release mutex")
                .recv_timeout(Duration::from_secs(5));
            write_test_response(stream, &TestResponse::ok(b"late"));
        });
        let url = server.url("/delayed-close");
        let cancellation = Arc::new(AtomicBool::new(false));
        let client_cancellation = Arc::clone(&cancellation);
        let probe = Arc::new(TestProbe::default());
        probe.set_delay_closing();
        let client_probe = Arc::clone(&probe);
        let client = thread::spawn(move || {
            let request = HttpRequest::post_json(&url, br#"{"synthetic":true}"#)
                .cancellation(client_cancellation.as_ref())
                .test_probe(client_probe);
            send_localhost_for_tests(&request, &test_config())
        });

        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("request reaches server");
        thread::sleep(Duration::from_millis(100));
        cancellation.store(true, Ordering::SeqCst);
        let error = client
            .join()
            .expect("join client")
            .expect_err("stalled request must be cancelled");
        assert_eq!(error.kind(), HttpErrorKind::Cancelled);
        assert_eq!(
            probe.released(),
            0,
            "a delayed final close must keep the callback-owned state alive"
        );
        assert_eq!(
            wait_for_release(&probe, Duration::from_secs(3)),
            1,
            "the late final callback must release the state exactly once"
        );
        release_tx.send(()).expect("release server");
        let _ = server.join();
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
        let current = ParsedUrl::parse("https://api.example.test/v1/start", false)
            .expect("valid HTTPS origin");
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
        let parsed = ParsedUrl::parse("http://[::1]:8080/v1/holidays/2026", true)
            .expect("IPv6 loopback URL");
        assert_eq!(parsed.scheme, Scheme::Http);
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.path, "/v1/holidays/2026");
        assert!(is_loopback_host(&parsed.host, true));

        let error = ParsedUrl::parse("http://[::1]:8080/v1?year=2026", true)
            .expect_err("query must still be rejected");
        assert_eq!(error.kind(), HttpErrorKind::QueryNotAllowed);

        let error = ParsedUrl::parse("http://example.test:8080/", true)
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
            },
        )
        .unwrap();
        assert_eq!(received, b"first!");
        assert_eq!(response.body, b"first!");
    }
}
