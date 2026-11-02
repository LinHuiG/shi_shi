//! Narrow transport seam for the Chat Completions client.
//!
//! Production uses [`SystemTransport`], which forwards to the bounded HTTP pool. Tests may inject a
//! deterministic fake transport through [`crate::openai::ChatCompletionsClient::with_transport`].
//! The request type deliberately has no `Debug` implementation because its
//! bearer field is a caller-resolved credential.

use crate::http_client::{HttpConfig, HttpErrorKind, HttpRequest};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub use crate::http_client::{HttpMethod, HttpResponse as TransportResponse};

/// A transport-level request. Getters are the only way to inspect it; no
/// `Debug` output can accidentally print headers or the credential.
pub struct TransportRequest<'a> {
    method: HttpMethod,
    url: &'a str,
    bearer: &'a str,
    body: Option<&'a [u8]>,
    max_response_bytes: usize,
}

impl<'a> TransportRequest<'a> {
    pub fn new(
        method: HttpMethod,
        url: &'a str,
        bearer: &'a str,
        body: Option<&'a [u8]>,
        max_response_bytes: usize,
    ) -> Self {
        Self {
            method,
            url,
            bearer,
            body,
            max_response_bytes,
        }
    }

    pub fn method(&self) -> HttpMethod {
        self.method
    }

    pub fn url(&self) -> &str {
        self.url
    }

    pub fn bearer(&self) -> &str {
        self.bearer
    }

    pub fn body(&self) -> Option<&[u8]> {
        self.body
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }
}

/// Safe transport error classes. No body, URL, header, or native diagnostic is
/// retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportErrorKind {
    InvalidRequest,
    Network,
    Timeout,
    Tls,
    Cancelled,
    ResponseTooLarge,
    Protocol,
}

impl TransportErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid request",
            Self::Network => "network error",
            Self::Timeout => "timeout",
            Self::Tls => "TLS validation failed",
            Self::Cancelled => "cancelled",
            Self::ResponseTooLarge => "response too large",
            Self::Protocol => "protocol error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("transport error: {}", .kind.as_str())]
pub struct TransportError {
    kind: TransportErrorKind,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(self) -> TransportErrorKind {
        self.kind
    }
}

/// Synchronous transport with an explicit remaining deadline and cancellation
/// flag. Implementations must not retry, log credentials, or exceed the
/// response-byte limit for buffered responses. Streaming consumers bound their
/// individual frames and assembled content; wire framing is not a total cap.
pub trait Transport: Sync {
    fn send(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<TransportResponse, TransportError>;
    fn send_stream(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
        chunk: &mut dyn FnMut(u16, &[u8]) -> bool,
    ) -> Result<TransportResponse, TransportError> {
        let response = self.send(request, timeout, cancel)?;
        if !chunk(response.status(), response.body()) {
            return Err(TransportError::new(TransportErrorKind::Protocol));
        }
        Ok(response)
    }
}

/// Production HTTP transport with test-only loopback access.
#[derive(Debug, Default)]
pub struct SystemTransport;

impl Transport for SystemTransport {
    fn send(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<TransportResponse, TransportError> {
        send(request, timeout, cancel, None)
    }

    fn send_stream(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
        chunk: &mut dyn FnMut(u16, &[u8]) -> bool,
    ) -> Result<TransportResponse, TransportError> {
        send(request, timeout, cancel, Some(chunk))
    }
}

fn send(
    request: &TransportRequest<'_>,
    timeout: Duration,
    cancel: &AtomicBool,
    chunk: Option<&mut dyn FnMut(u16, &[u8]) -> bool>,
) -> Result<TransportResponse, TransportError> {
    let config = HttpConfig {
        total_timeout: timeout,
        max_response_bytes: request.max_response_bytes,
        max_redirects: if chunk.is_some() { 0 } else { 3 },
    };
    let http = match request.method {
        HttpMethod::Post => HttpRequest::post_json(
            request.url,
            request
                .body
                .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidRequest))?,
        ),
        HttpMethod::Get => HttpRequest::get(request.url),
    }
    .bearer_token(request.bearer)
    .cancellation(cancel);
    crate::http_client::send_inner(&http, &config, cfg!(test), chunk)
        .map_err(|error| TransportError::new(map_error(error.kind())))
}

fn map_error(kind: HttpErrorKind) -> TransportErrorKind {
    match kind {
        HttpErrorKind::InvalidConfig
        | HttpErrorKind::InvalidUrl
        | HttpErrorKind::UnsupportedScheme
        | HttpErrorKind::UserInfoNotAllowed
        | HttpErrorKind::QueryNotAllowed
        | HttpErrorKind::FragmentNotAllowed
        | HttpErrorKind::InvalidHeader
        | HttpErrorKind::InvalidRequest => TransportErrorKind::InvalidRequest,
        HttpErrorKind::Network => TransportErrorKind::Network,
        HttpErrorKind::Timeout => TransportErrorKind::Timeout,
        HttpErrorKind::Tls => TransportErrorKind::Tls,
        HttpErrorKind::Cancelled => TransportErrorKind::Cancelled,
        HttpErrorKind::ResponseTooLarge => TransportErrorKind::ResponseTooLarge,
        HttpErrorKind::RedirectRejected | HttpErrorKind::Protocol => TransportErrorKind::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_http_response_debug_stays_redacted() {
        let response = crate::http_client::HttpResponse {
            status: 200,
            body: b"synthetic-secret-key-in-body".to_vec(),
            etag: Some("\"synthetic-etag\"".into()),
            last_modified: None,
        };
        let rendered = format!("{response:?}");
        assert!(!rendered.contains("synthetic-secret-key-in-body"));
        assert!(!rendered.contains("synthetic-etag"));
        assert!(rendered.contains("body_len"));
    }

    #[test]
    fn transport_errors_are_safe_classes() {
        let error = TransportError::new(TransportErrorKind::Network);
        assert_eq!(error.kind(), TransportErrorKind::Network);
        assert!(!format!("{error} {error:?}").contains("http"));
    }

    #[test]
    fn http_error_mapping_covers_all_public_classes() {
        assert_eq!(
            map_error(HttpErrorKind::Timeout),
            TransportErrorKind::Timeout
        );
        assert_eq!(
            map_error(HttpErrorKind::Cancelled),
            TransportErrorKind::Cancelled
        );
        assert_eq!(
            map_error(HttpErrorKind::Network),
            TransportErrorKind::Network
        );
        assert_eq!(map_error(HttpErrorKind::Tls), TransportErrorKind::Tls);
        assert_eq!(
            map_error(HttpErrorKind::ResponseTooLarge),
            TransportErrorKind::ResponseTooLarge
        );
        assert_eq!(
            map_error(HttpErrorKind::UserInfoNotAllowed),
            TransportErrorKind::InvalidRequest
        );
    }
}
