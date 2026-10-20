//! Base-endpoint validation and bounded URL joining.
//!
//! Production callers must supply an absolute HTTPS endpoint without userinfo,
//! query, or fragment. The base path is an arbitrary bounded prefix (for
//! example `/openai/v1` or `/proxy/tenant/v1`); this module appends exactly
//! `/chat/completions` or `/models` and never assumes or injects `/v1`. A base
//! that already ends in one of those endpoint suffixes is rejected so a
//! duplicated path is impossible.
//!
//! Plain HTTP is available only through a `cfg(test)` loopback constructor for
//! local synthetic integration tests. There is no production insecure flag.

use std::fmt;
use std::net::IpAddr;

const MAX_BASE_URL_BYTES: usize = 2_048;
const MAX_PATH_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointErrorKind {
    Empty,
    TooLong,
    NonAscii,
    InvalidCharacter,
    UnsupportedScheme,
    UserInfo,
    Query,
    Fragment,
    InvalidHost,
    InvalidPort,
    InvalidPath,
    EndpointSuffix,
}

impl EndpointErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "endpoint is empty",
            Self::TooLong => "endpoint is too long",
            Self::NonAscii => "endpoint must be ASCII",
            Self::InvalidCharacter => "endpoint contains an invalid character",
            Self::UnsupportedScheme => "endpoint must use https",
            Self::UserInfo => "endpoint must not include userinfo",
            Self::Query => "endpoint must not include a query",
            Self::Fragment => "endpoint must not include a fragment",
            Self::InvalidHost => "endpoint host is invalid",
            Self::InvalidPort => "endpoint port is invalid",
            Self::InvalidPath => "endpoint path is invalid",
            Self::EndpointSuffix => "endpoint must be a base URL, not /chat/completions or /models",
        }
    }
}

/// Safe endpoint error. It intentionally contains no URL, path, or authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointError {
    kind: EndpointErrorKind,
}

impl EndpointError {
    fn new(kind: EndpointErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(self) -> EndpointErrorKind {
        self.kind
    }
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl std::error::Error for EndpointError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Https,
    HttpLocalhostTest,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::HttpLocalhostTest => "http",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::HttpLocalhostTest => 80,
        }
    }
}

/// A validated base endpoint. `Debug` hides the path prefix, which callers may
/// use for a tenant or gateway identifier.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    scheme: Scheme,
    host: String,
    ipv6: bool,
    port: u16,
    path_prefix: String,
}

impl Endpoint {
    /// Parse and validate a production endpoint. Only `https` is accepted.
    pub fn parse(base: &str) -> Result<Self, EndpointError> {
        Self::parse_inner(base, false)
    }

    /// Test-only loopback endpoint. The resulting endpoint can only represent
    /// `http://localhost`, `http://127.0.0.1`, or another loopback IP; the
    /// transport still accepts plaintext only through its own `cfg(test)`
    /// helper.
    #[cfg(test)]
    pub(crate) fn parse_localhost_for_tests(base: &str) -> Result<Self, EndpointError> {
        Self::parse_inner(base, true)
    }

    fn parse_inner(base: &str, allow_localhost_http: bool) -> Result<Self, EndpointError> {
        let input = base.trim();
        if input.is_empty() {
            return Err(EndpointError::new(EndpointErrorKind::Empty));
        }
        if input.len() > MAX_BASE_URL_BYTES {
            return Err(EndpointError::new(EndpointErrorKind::TooLong));
        }
        if !input.is_ascii() {
            return Err(EndpointError::new(EndpointErrorKind::NonAscii));
        }
        if input
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || byte == b'\\')
        {
            return Err(EndpointError::new(EndpointErrorKind::InvalidCharacter));
        }
        if input.contains('?') {
            return Err(EndpointError::new(EndpointErrorKind::Query));
        }
        if input.contains('#') {
            return Err(EndpointError::new(EndpointErrorKind::Fragment));
        }

        let scheme_end = input
            .find("://")
            .ok_or_else(|| EndpointError::new(EndpointErrorKind::InvalidCharacter))?;
        let scheme_text = &input[..scheme_end];
        let rest = &input[scheme_end + 3..];
        let scheme = if scheme_text.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else if scheme_text.eq_ignore_ascii_case("http") && allow_localhost_http {
            Scheme::HttpLocalhostTest
        } else {
            return Err(EndpointError::new(EndpointErrorKind::UnsupportedScheme));
        };

        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let raw_path = if authority_end == rest.len() {
            ""
        } else {
            &rest[authority_end..]
        };
        if authority.is_empty() {
            return Err(EndpointError::new(EndpointErrorKind::InvalidHost));
        }
        if authority.contains('@') {
            return Err(EndpointError::new(EndpointErrorKind::UserInfo));
        }

        let (host, ipv6, port_text) = if let Some(after_bracket) = authority.strip_prefix('[') {
            let close = after_bracket
                .find(']')
                .ok_or_else(|| EndpointError::new(EndpointErrorKind::InvalidHost))?;
            let inner = &after_bracket[..close];
            let suffix = &after_bracket[close + 1..];
            let address: std::net::Ipv6Addr = inner
                .parse()
                .map_err(|_| EndpointError::new(EndpointErrorKind::InvalidHost))?;
            let port_text = if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| EndpointError::new(EndpointErrorKind::InvalidHost))?,
                )
            };
            (address.to_string(), true, port_text)
        } else {
            if authority.matches(':').count() > 1 {
                return Err(EndpointError::new(EndpointErrorKind::InvalidHost));
            }
            let (host, port_text) = match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            };
            if host.is_empty()
                || !host.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_'
                })
                || !host.bytes().any(|byte| byte.is_ascii_alphanumeric())
            {
                return Err(EndpointError::new(EndpointErrorKind::InvalidHost));
            }
            (host.to_ascii_lowercase(), false, port_text)
        };

        let port = match port_text {
            None => scheme.default_port(),
            Some(text) => {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(EndpointError::new(EndpointErrorKind::InvalidPort));
                }
                let value: u32 = text
                    .parse()
                    .map_err(|_| EndpointError::new(EndpointErrorKind::InvalidPort))?;
                if !(1..=u16::MAX as u32).contains(&value) {
                    return Err(EndpointError::new(EndpointErrorKind::InvalidPort));
                }
                value as u16
            }
        };

        if scheme == Scheme::HttpLocalhostTest && !is_loopback_host(&host) {
            return Err(EndpointError::new(EndpointErrorKind::UnsupportedScheme));
        }

        let path_prefix = normalize_path(raw_path)?;
        Ok(Self {
            scheme,
            host,
            ipv6,
            port,
            path_prefix,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_https(&self) -> bool {
        self.scheme == Scheme::Https
    }

    pub(crate) fn chat_completions_url(&self) -> String {
        self.url_for("/chat/completions")
    }

    pub(crate) fn models_url(&self) -> String {
        self.url_for("/models")
    }

    fn url_for(&self, suffix: &str) -> String {
        // `suffix` is a crate-internal constant and always starts with '/'.
        let mut url =
            String::with_capacity(self.host.len() + self.path_prefix.len() + suffix.len() + 16);
        url.push_str(self.scheme.as_str());
        url.push_str("://");
        if self.ipv6 {
            url.push('[');
            url.push_str(&self.host);
            url.push(']');
        } else {
            url.push_str(&self.host);
        }
        if self.port != self.scheme.default_port() {
            url.push(':');
            url.push_str(&self.port.to_string());
        }
        url.push_str(&self.path_prefix);
        url.push_str(suffix);
        url
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("scheme", &self.scheme.as_str())
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path_prefix", &"<redacted>")
            .finish()
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn normalize_path(path: &str) -> Result<String, EndpointError> {
    if path.is_empty() {
        return Ok(String::new());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
    }
    if !path.starts_with('/') {
        return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let mut segments = Vec::new();
    for (index, segment) in trimmed.split('/').enumerate() {
        if index == 0 {
            if !segment.is_empty() {
                return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
            }
            continue;
        }
        if segment.is_empty() || segment == "." || segment == ".." || has_encoded_traversal(segment)
        {
            return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
    }
    let prefix = format!("/{}", segments.join("/"));
    if prefix.len() > MAX_PATH_BYTES {
        return Err(EndpointError::new(EndpointErrorKind::InvalidPath));
    }
    let lower = prefix.to_ascii_lowercase();
    if lower.ends_with("/chat/completions") || lower.ends_with("/models") {
        return Err(EndpointError::new(EndpointErrorKind::EndpointSuffix));
    }
    Ok(prefix)
}

fn has_encoded_traversal(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_paths_without_adding_or_duplicating_v1() {
        let endpoint = Endpoint::parse("https://api.example.test").unwrap();
        assert_eq!(
            endpoint.chat_completions_url(),
            "https://api.example.test/chat/completions"
        );
        assert_eq!(endpoint.models_url(), "https://api.example.test/models");

        let endpoint = Endpoint::parse("https://api.example.test/v1/").unwrap();
        assert_eq!(
            endpoint.chat_completions_url(),
            "https://api.example.test/v1/chat/completions"
        );

        let endpoint =
            Endpoint::parse("https://gateway.example.test/proxy/tenant/openai/v1").unwrap();
        assert_eq!(
            endpoint.models_url(),
            "https://gateway.example.test/proxy/tenant/openai/v1/models"
        );
        assert!(!endpoint.chat_completions_url().contains("/v1/v1/chat"));
    }

    #[test]
    fn explicit_non_default_port_is_preserved() {
        let endpoint = Endpoint::parse("https://api.example.test:8443/openai/v1").unwrap();
        assert_eq!(
            endpoint.chat_completions_url(),
            "https://api.example.test:8443/openai/v1/chat/completions"
        );
        let default = Endpoint::parse("https://api.example.test:443/v1").unwrap();
        assert_eq!(default.models_url(), "https://api.example.test/v1/models");
    }

    #[test]
    fn endpoint_suffix_is_rejected_instead_of_duplicated() {
        for base in [
            "https://api.example.test/chat/completions",
            "https://api.example.test/v1/models/",
            "https://api.example.test/MODELS",
            "https://api.example.test/v1/CHAT/COMPLETIONS/",
        ] {
            let error = Endpoint::parse(base).unwrap_err();
            assert_eq!(error.kind(), EndpointErrorKind::EndpointSuffix, "{base}");
        }
    }

    #[test]
    fn unsafe_or_ambiguous_endpoints_are_rejected() {
        let cases = [
            ("", EndpointErrorKind::Empty),
            (
                "ftp://api.example.test/v1",
                EndpointErrorKind::UnsupportedScheme,
            ),
            (
                "http://api.example.test/v1",
                EndpointErrorKind::UnsupportedScheme,
            ),
            (
                "https://user:pass@api.example.test/v1",
                EndpointErrorKind::UserInfo,
            ),
            (
                "https://api.example.test/v1?token=secret",
                EndpointErrorKind::Query,
            ),
            (
                "https://api.example.test/v1#fragment",
                EndpointErrorKind::Fragment,
            ),
            ("https:///v1", EndpointErrorKind::InvalidHost),
            (
                "https://api.example.test:0/v1",
                EndpointErrorKind::InvalidPort,
            ),
            (
                "https://api.example.test:99999/v1",
                EndpointErrorKind::InvalidPort,
            ),
            (
                "https://api.example.test:abc/v1",
                EndpointErrorKind::InvalidPort,
            ),
            (
                "https://api.example.test/v1/../secret",
                EndpointErrorKind::InvalidPath,
            ),
            (
                "https://api.example.test/v1/%2e%2e/secret",
                EndpointErrorKind::InvalidPath,
            ),
            (
                "https://api.example.test/v1//models",
                EndpointErrorKind::InvalidPath,
            ),
            (
                "https://api.example.test/v1\\secret",
                EndpointErrorKind::InvalidCharacter,
            ),
            (
                "https://api.example.test/v1 path",
                EndpointErrorKind::InvalidCharacter,
            ),
            ("https://api.例子.test/v1", EndpointErrorKind::NonAscii),
        ];
        for (base, expected) in cases {
            let error = Endpoint::parse(base).unwrap_err();
            assert_eq!(error.kind(), expected, "{base}");
        }
    }

    #[test]
    fn overlong_base_and_path_are_bounded() {
        let base = format!("https://api.example.test/{}", "a".repeat(MAX_PATH_BYTES));
        assert_eq!(
            Endpoint::parse(&base).unwrap_err().kind(),
            EndpointErrorKind::InvalidPath
        );
        let base = format!(
            "https://api.example.test/{}",
            "a".repeat(MAX_BASE_URL_BYTES)
        );
        assert_eq!(
            Endpoint::parse(&base).unwrap_err().kind(),
            EndpointErrorKind::TooLong
        );
    }

    #[test]
    fn ipv6_and_outer_whitespace_are_handled_explicitly() {
        let endpoint = Endpoint::parse("  https://[::1]:8443/openai/v1  ").unwrap();
        assert_eq!(endpoint.host(), "::1");
        assert_eq!(
            endpoint.chat_completions_url(),
            "https://[::1]:8443/openai/v1/chat/completions"
        );
    }

    #[test]
    fn debug_redacts_path_prefix() {
        let endpoint =
            Endpoint::parse("https://api.example.test/tenant/synthetic-secret/v1").unwrap();
        let rendered = format!("{endpoint:?}");
        assert!(!rendered.contains("synthetic-secret"));
        assert!(!rendered.contains("/tenant"));
        assert!(rendered.contains("api.example.test"));
    }

    #[test]
    fn errors_do_not_contain_url_or_path() {
        let error = Endpoint::parse(
            "https://synthetic-user:synthetic-password@api.example.test/secret?token=x",
        )
        .unwrap_err();
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("synthetic"));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("token"));
    }

    #[test]
    fn test_only_http_is_limited_to_loopback() {
        let endpoint =
            Endpoint::parse_localhost_for_tests("http://127.0.0.1:18080/openai/v1").unwrap();
        assert!(!endpoint.is_https());
        assert_eq!(
            endpoint.models_url(),
            "http://127.0.0.1:18080/openai/v1/models"
        );
        let endpoint = Endpoint::parse_localhost_for_tests("http://localhost:18080").unwrap();
        assert_eq!(
            endpoint.chat_completions_url(),
            "http://localhost:18080/chat/completions"
        );
        assert_eq!(
            Endpoint::parse_localhost_for_tests("http://example.test/v1")
                .unwrap_err()
                .kind(),
            EndpointErrorKind::UnsupportedScheme
        );
    }
}
