//! Newline-delimited JSON-RPC 2.0 framing and demultiplexing.
//!
//! The installed `@deepseek-ai/dsh-acp` bridge wraps `process.stdout`/`stdin`
//! with `ndJsonStream` (dsh-acp `lib/index.js`, production wiring), and the
//! `@deepseek-ai/dsh-acp-app` README states "Stdout is reserved for
//! newline-delimited ACP JSON-RPC frames". Therefore one compact JSON value per
//! line, `\n` terminated, with no `Content-Length` header.

use serde_json::{json, Value};
use std::fmt;

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

    pub fn display(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::String(s) => s.clone(),
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    EmptyLine,
    TooLong { bytes: usize, limit: usize },
    NotJson(String),
    NotObject,
    BadVersion,
    MissingMethod,
    BadId,
    BadResponse,
    BadRequest,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyLine => f.write_str("empty JSON-RPC frame"),
            Self::TooLong { bytes, limit } => {
                write!(f, "JSON-RPC frame is {bytes} bytes (limit {limit})")
            }
            Self::NotJson(message) => write!(f, "JSON-RPC frame is not JSON: {message}"),
            Self::NotObject => f.write_str("JSON-RPC frame is not an object"),
            Self::BadVersion => f.write_str("JSON-RPC frame is not version 2.0"),
            Self::MissingMethod => f.write_str("JSON-RPC frame has no method"),
            Self::BadId => f.write_str("JSON-RPC id must be a string or integer"),
            Self::BadResponse => {
                f.write_str("JSON-RPC response must contain exactly one of result/error")
            }
            Self::BadRequest => f.write_str("JSON-RPC request is missing a method"),
        }
    }
}

impl std::error::Error for FrameError {}

fn parse_params(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => json!({}),
        Some(v) => v.clone(),
    }
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
    let object = value.as_object().ok_or(FrameError::NotObject)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(FrameError::BadVersion);
    }

    let method = object.get("method").and_then(Value::as_str);
    let id_value = object.get("id");
    let has_id = id_value.is_some_and(|value| !value.is_null());

    if let Some(method) = method {
        if method.is_empty() || method.len() > 256 {
            return Err(FrameError::MissingMethod);
        }
        let params = parse_params(object.get("params"));
        if has_id {
            let id = RequestId::from_value(id_value.unwrap()).ok_or(FrameError::BadId)?;
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
        let id = RequestId::from_value(id_value).ok_or(FrameError::BadId)?;
        let result = object.get("result");
        let error = object.get("error");
        match (result, error) {
            (Some(result), None) => Ok(Incoming::Response {
                id,
                result: Ok(result.clone()),
            }),
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
                Ok(Incoming::Response {
                    id,
                    result: Err(RpcError {
                        code,
                        message,
                        data: error.get("data").cloned(),
                    }),
                })
            }
            _ => Err(FrameError::BadResponse),
        }
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
