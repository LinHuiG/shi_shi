//! Newline-delimited JSON-RPC 2.0 framing and demultiplexing.
//!
//! The installed `@deepseek-ai/dsh-acp` bridge wraps `process.stdout`/`stdin`
//! with `ndJsonStream` (dsh-acp `lib/index.js`, production wiring), and the
//! `@deepseek-ai/dsh-acp-app` README states "Stdout is reserved for
//! newline-delimited ACP JSON-RPC frames". Therefore one compact JSON value per
//! line, `\n` terminated, with no `Content-Length` header.

use serde_json::{json, Value};
use std::{
    fmt,
    io::{self, BufRead},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_i64().map(Self::Number),
            Value::String(s) if !s.is_empty() && s.len() <= 128 => Some(Self::String(s.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(n) => json!(n),
            Self::String(s) => json!(s),
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum Incoming {
    Response {
        id: RequestId,
        result: Result<Value, RpcError>,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("empty JSON-RPC frame")]
    EmptyLine,
    #[error("JSON-RPC frame is {bytes} bytes (limit {limit})")]
    TooLong { bytes: usize, limit: usize },
    #[error("JSON-RPC frame is not JSON: {0}")]
    NotJson(String),
    #[error("JSON-RPC frame is not an object")]
    NotObject,
    #[error("JSON-RPC frame is not version 2.0")]
    BadVersion,
    #[error("JSON-RPC frame has no method")]
    MissingMethod,
    #[error("JSON-RPC id must be a string or integer")]
    BadId,
    #[error("JSON-RPC response must contain exactly one of result/error")]
    BadResponse,
}

fn parse_params(value: Option<Value>) -> Value {
    match value {
        None | Some(Value::Null) => json!({}),
        Some(v) => v,
    }
}

/// Retain at most `limit` bytes, consume the whole line, and report its full size.
/// Callers either reject oversized frames or retain their prefix as diagnostics.
pub(crate) fn read_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize> {
    line.clear();
    let bytes = io::Read::take(&mut *reader, limit as u64).read_until(b'\n', line)?;
    if bytes == limit && !line.ends_with(b"\n") {
        return Ok(bytes.saturating_add(reader.skip_until(b'\n')?));
    }
    Ok(bytes)
}

/// Decode exactly one line. Batch arrays and JSON-RPC 1.0 are rejected: ACP
/// uses individual request/notification/response objects.
pub fn decode_line(line: &[u8], max_bytes: usize) -> Result<Incoming, FrameError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return Err(FrameError::EmptyLine);
    }
    if line.len() > max_bytes {
        return Err(FrameError::TooLong {
            bytes: line.len(),
            limit: max_bytes,
        });
    }
    let value: Value = serde_json::from_slice(line).map_err(|error| {
        let mut detail = error.to_string();
        if std::env::var_os("SHISHI_ACP_DIAGNOSTICS").is_some_and(|v| v == "1") {
            let prefix = String::from_utf8_lossy(line)
                .chars()
                .take(240)
                .collect::<String>();
            detail.push_str(&format!("; stdout_prefix={prefix:?}"));
        }
        FrameError::NotJson(detail)
    })?;
    let Value::Object(mut object) = value else {
        return Err(FrameError::NotObject);
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(FrameError::BadVersion);
    }

    let method = object.remove("method");
    let id_value = object.remove("id");
    let has_id = id_value.as_ref().is_some_and(|value| !value.is_null());

    if let Some(method) = method.as_ref().and_then(Value::as_str) {
        if method.is_empty() || method.len() > 256 {
            return Err(FrameError::MissingMethod);
        }
        let params = parse_params(object.remove("params"));
        if has_id {
            let id = RequestId::from_value(&id_value.unwrap()).ok_or(FrameError::BadId)?;
            Ok(Incoming::Request {
                id,
                method: method.to_owned(),
                params,
            })
        } else {
            Ok(Incoming::Notification {
                method: method.to_owned(),
                params,
            })
        }
    } else {
        let id_value = id_value.ok_or(FrameError::BadResponse)?;
        let id = RequestId::from_value(&id_value).ok_or(FrameError::BadId)?;
        let result = match (object.remove("result"), object.remove("error")) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => {
                let error = error.as_object().ok_or(FrameError::BadResponse)?;
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("JSON-RPC error")
                    .chars()
                    .take(512)
                    .collect();
                Err(RpcError { code, message })
            }
            _ => return Err(FrameError::BadResponse),
        };
        Ok(Incoming::Response { id, result })
    }
}

fn encode(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("JSON value serialization cannot fail");
    bytes.push(b'\n');
    bytes
}

pub fn encode_request(id: &RequestId, method: &str, params: &Value) -> Vec<u8> {
    encode(&json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "method": method,
        "params": params,
    }))
}

pub fn encode_notification(method: &str, params: &Value) -> Vec<u8> {
    encode(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
}

pub fn encode_response(id: &RequestId, result: &Value) -> Vec<u8> {
    encode(&json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "result": result,
    }))
}

pub fn encode_error(id: &RequestId, code: i64, message: &str) -> Vec<u8> {
    encode(&json!({
        "jsonrpc": "2.0",
        "id": id.to_value(),
        "error": {"code": code, "message": message},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: i64) -> RequestId {
        RequestId::Number(value)
    }

    #[test]
    fn bounded_reader_preserves_frames_and_drains_oversized_lines_across_chunks() {
        use std::io::{BufReader, Read};
        struct Interrupted<'a>(&'a [u8], bool);
        impl Read for Interrupted<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.1 = !self.1;
                if self.1 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    self.0.read(buffer)
                }
            }
        }
        let input = "\n中🙂文\r\nlonger than the limit\n尾".as_bytes();
        for capacity in [1, 2, 7, 64] {
            for limit in [0, 1, 2, 7, 64] {
                let mut reader = BufReader::with_capacity(capacity, Interrupted(input, false));
                let mut line = vec![];
                for expected in input.split_inclusive(|b| *b == b'\n') {
                    assert_eq!(
                        read_line(&mut reader, &mut line, limit).unwrap(),
                        expected.len()
                    );
                    assert_eq!(line, &expected[..expected.len().min(limit)]);
                    assert!(line.len() <= limit);
                }
                assert_eq!(read_line(&mut reader, &mut line, limit).unwrap(), 0);
                assert!(line.is_empty());
            }
        }
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        for limit in [2, 8] {
            let mut reader = BufReader::new((&b"prefix"[..]).chain(Failed));
            assert_eq!(
                read_line(&mut reader, &mut vec![], limit)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    }

    #[test]
    fn demuxes_responses_requests_and_notifications() {
        let response = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        match decode_line(response, 1024).unwrap() {
            Incoming::Response { id: rid, result } => {
                assert_eq!(rid, id(1));
                assert_eq!(result.unwrap()["ok"], true);
            }
            other => panic!("response demux failed: {other:?}"),
        }

        let request =
            br#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"x":1}}"#;
        match decode_line(request, 1024).unwrap() {
            Incoming::Request {
                id: rid,
                method,
                params,
            } => {
                assert_eq!(rid, RequestId::String("p1".into()));
                assert_eq!(method, "session/request_permission");
                assert_eq!(params["x"], 1);
            }
            other => panic!("request demux failed: {other:?}"),
        }

        let notification =
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#;
        match decode_line(notification, 1024).unwrap() {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "s");
            }
            other => panic!("notification demux failed: {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_frames_without_panicking() {
        assert_eq!(decode_line(b"\n", 1024).unwrap_err(), FrameError::EmptyLine);
        assert!(matches!(
            decode_line(b"not json\n", 1024).unwrap_err(),
            FrameError::NotJson(_)
        ));
        assert_eq!(
            decode_line(br#"{"id":1,"result":{}}"#, 1024).unwrap_err(),
            FrameError::BadVersion
        );
        assert_eq!(
            decode_line(br#"{"jsonrpc":"2.0","id":1}"#, 1024).unwrap_err(),
            FrameError::BadResponse
        );
        assert_eq!(
            decode_line(br#"{"jsonrpc":"2.0","id":true,"method":"x"}"#, 1024).unwrap_err(),
            FrameError::BadId
        );
        assert_eq!(
            decode_line(br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#, 1024).unwrap_err(),
            FrameError::BadResponse
        );
        assert!(matches!(
            decode_line(&[b'x'; 64], 32).unwrap_err(),
            FrameError::TooLong { .. }
        ));
    }

    #[test]
    fn encodes_one_compact_line_per_message() {
        let request = encode_request(&id(7), "initialize", &json!({"a":1}));
        assert_eq!(request.last(), Some(&b'\n'));
        assert_eq!(request.iter().filter(|b| **b == b'\n').count(), 1);
        let value: Value = serde_json::from_slice(&request[..request.len() - 1]).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "initialize");

        let notification = encode_notification("session/cancel", &json!({"sessionId":"s"}));
        let value: Value = serde_json::from_slice(&notification[..notification.len() - 1]).unwrap();
        assert!(value.get("id").is_none());
    }
}
