#![allow(dead_code)]
//! Newline-delimited JSON-RPC 2.0 framing for the synthetic agent.
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
    Response { id: RequestId, result: Result<Value, RpcError> },
    Request { id: RequestId, method: String, params: Value },
    Notification { method: String, params: Value },
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

/// Decode exactly one line. Batch arrays and JSON-RPC 1.0 are rejected: ACP
/// uses individual request/notification/response objects.
pub fn decode_line(line: &[u8], max_bytes: usize) -> Result<Incoming, FrameError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return Err(FrameError::EmptyLine);
    }
    if line.len() > max_bytes {
        return Err(FrameError::TooLong { bytes: line.len(), limit: max_bytes });
    }
    let value: Value = serde_json::from_slice(line).map_err(|error| {
        let mut detail = error.to_string();
        if std::env::var_os("SHISHI_ACP_DIAGNOSTICS").is_some_and(|v| v == "1") {
            let prefix = String::from_utf8_lossy(line).chars().take(240).collect::<String>();
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
            Ok(Incoming::Request { id, method: method.to_owned(), params })
        } else {
            Ok(Incoming::Notification { method: method.to_owned(), params })
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
