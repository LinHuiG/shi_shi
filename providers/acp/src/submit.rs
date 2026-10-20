//! `submit_analysis` MCP server and ticket/result store.
//!
//! The ACP client supplies this server in `session/new.mcpServers` as a stdio
//! server. DSH wraps its tools as `mcp__shishi_submit__submit_analysis`
//! (dsh-mcp-client naming contract). The server accepts the exact caller
//! schema, writes one accepted result atomically, and rejects duplicates,
//! missing fields, extra fields and a second invalid correction.

use crate::schema_validate;
use crate::sha256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const TICKET_VERSION: u32 = 1;
const MAX_TICKET_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
const MAX_MCP_LINE: usize = 4 * 1024 * 1024;
const MAX_INVALID: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTicket {
    pub version: u32,
    pub request_id: String,
    pub kind: String,
    pub schema: Value,
    pub schema_hash: String,
    #[serde(default = "default_max_invalid")]
    pub max_invalid: u32,
}

fn default_max_invalid() -> u32 {
    MAX_INVALID
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResultFile {
    pub version: u32,
    pub request_id: String,
    pub kind: String,
    pub schema_hash: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct AcceptedSubmission {
    pub request_id: String,
    pub kind: String,
    pub schema_hash: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct SubmitPaths {
    pub directory: PathBuf,
    pub ticket_path: PathBuf,
    pub result_path: PathBuf,
    pub failure_path: PathBuf,
    pub state_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum SubmitError {
    Io(String),
    InvalidPath(String),
    InvalidTicket(String),
    Missing,
    Closed(String),
    Duplicate,
    Schema(Vec<String>),
    Protocol(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "submit store I/O error: {message}"),
            Self::InvalidPath(message) => write!(f, "invalid submit path: {message}"),
            Self::InvalidTicket(message) => write!(f, "invalid submit ticket: {message}"),
            Self::Missing => f.write_str("no accepted submit_analysis result"),
            Self::Closed(reason) => write!(f, "submit_analysis is closed: {reason}"),
            Self::Duplicate => f.write_str("duplicate submit_analysis call"),
            Self::Schema(errors) => write!(f, "submit_analysis schema invalid: {errors:?}"),
            Self::Protocol(message) => write!(f, "submit_analysis protocol error: {message}"),
        }
    }
}

impl std::error::Error for SubmitError {}

impl SubmitPaths {
    pub fn create(
        base: &Path,
        request_id: &str,
        kind: &str,
        schema: Value,
    ) -> Result<(Self, SubmitTicket), SubmitError> {
        validate_component(request_id, "request id")?;
        validate_component(kind, "kind")?;
        if !schema.is_object() {
            return Err(SubmitError::InvalidTicket(
                "schema must be an object".into(),
            ));
        }
        let directory = base.join(request_id);
        fs::create_dir_all(&directory).map_err(|error| SubmitError::Io(error.to_string()))?;
        let schema_hash = sha256::hex(&serde_json::to_vec(&schema).unwrap_or_default());
        let ticket = SubmitTicket {
            version: TICKET_VERSION,
            request_id: request_id.to_string(),
            kind: kind.to_string(),
            schema,
            schema_hash,
            max_invalid: MAX_INVALID,
        };
        let paths = Self {
            ticket_path: directory.join("ticket.json"),
            result_path: directory.join("result.json"),
            failure_path: directory.join("failure.json"),
            state_path: directory.join("state.json"),
            directory,
        };
        paths.write_ticket(&ticket)?;
        Ok((paths, ticket))
    }

    pub fn load(ticket_path: &Path) -> Result<(Self, SubmitTicket), SubmitError> {
        let bytes = read_bounded(ticket_path, MAX_TICKET_BYTES)?;
        let ticket: SubmitTicket = serde_json::from_slice(&bytes)
            .map_err(|error| SubmitError::InvalidTicket(error.to_string()))?;
        if ticket.version != TICKET_VERSION {
            return Err(SubmitError::InvalidTicket(format!(
                "unsupported ticket version {}",
                ticket.version
            )));
        }
        validate_component(&ticket.request_id, "request id")?;
        validate_component(&ticket.kind, "kind")?;
        if !ticket.schema.is_object() {
            return Err(SubmitError::InvalidTicket(
                "schema must be an object".into(),
            ));
        }
        if ticket.max_invalid > MAX_INVALID {
            return Err(SubmitError::InvalidTicket(
                "max_invalid exceeds the one-correction limit".into(),
            ));
        }
        let actual_hash = sha256::hex(&serde_json::to_vec(&ticket.schema).unwrap_or_default());
        if actual_hash != ticket.schema_hash {
            return Err(SubmitError::InvalidTicket(
                "schema hash does not match the ticket".into(),
            ));
        }
        let directory = ticket_path
            .parent()
            .ok_or_else(|| SubmitError::InvalidPath("ticket has no parent".into()))?
            .to_path_buf();
        Ok((
            Self {
                ticket_path: ticket_path.to_path_buf(),
                result_path: directory.join("result.json"),
                failure_path: directory.join("failure.json"),
                state_path: directory.join("state.json"),
                directory,
            },
            ticket,
        ))
    }

    fn write_ticket(&self, ticket: &SubmitTicket) -> Result<(), SubmitError> {
        let bytes =
            serde_json::to_vec(ticket).map_err(|error| SubmitError::Io(error.to_string()))?;
        write_atomic_new(&self.ticket_path, &bytes)
    }

    pub fn read_accepted(
        &self,
        expected_request_id: &str,
        expected_kind: &str,
        expected_schema: &Value,
    ) -> Result<AcceptedSubmission, SubmitError> {
        if self.failure_path.is_file() {
            let reason = fs::read_to_string(&self.failure_path)
                .unwrap_or_default()
                .chars()
                .take(240)
                .collect::<String>();
            return Err(SubmitError::Closed(reason));
        }
        if !self.result_path.is_file() {
            return Err(SubmitError::Missing);
        }
        let bytes = read_bounded(&self.result_path, MAX_RESULT_BYTES)?;
        let result: SubmitResultFile = serde_json::from_slice(&bytes)
            .map_err(|error| SubmitError::Protocol(error.to_string()))?;
        if result.version != TICKET_VERSION {
            return Err(SubmitError::Protocol("unsupported result version".into()));
        }
        if result.request_id != expected_request_id || result.kind != expected_kind {
            return Err(SubmitError::Protocol(
                "result does not match the request id/kind".into(),
            ));
        }
        let expected_hash = sha256::hex(&serde_json::to_vec(expected_schema).unwrap_or_default());
        if result.schema_hash != expected_hash {
            return Err(SubmitError::Protocol(
                "result schema hash does not match the caller schema".into(),
            ));
        }
        schema_validate::validate(expected_schema, &result.value).map_err(SubmitError::Schema)?;
        Ok(AcceptedSubmission {
            request_id: result.request_id,
            kind: result.kind,
            schema_hash: result.schema_hash,
            value: result.value,
        })
    }

    pub fn submit(&self, ticket: &SubmitTicket, arguments: &Value) -> Result<Value, SubmitError> {
        if self.result_path.is_file() {
            return Err(SubmitError::Duplicate);
        }
        if self.failure_path.is_file() {
            return Err(SubmitError::Closed(
                "invalid correction already exhausted".into(),
            ));
        }
        let mut invalid = read_invalid_count(&self.state_path);
        match schema_validate::validate(&ticket.schema, arguments) {
            Ok(()) => {
                let result = SubmitResultFile {
                    version: TICKET_VERSION,
                    request_id: ticket.request_id.clone(),
                    kind: ticket.kind.clone(),
                    schema_hash: ticket.schema_hash.clone(),
                    value: arguments.clone(),
                };
                let bytes = serde_json::to_vec(&result)
                    .map_err(|error| SubmitError::Io(error.to_string()))?;
                write_atomic_new(&self.result_path, &bytes)?;
                Ok(json!({
                    "content": [{"type": "text", "text": "accepted"}],
                    "isError": false
                }))
            }
            Err(errors) => {
                invalid = invalid.saturating_add(1);
                fs::write(&self.state_path, json!({"invalid": invalid}).to_string())
                    .map_err(|error| SubmitError::Io(error.to_string()))?;
                let bounded = errors
                    .iter()
                    .take(8)
                    .map(|error| error.chars().take(240).collect::<String>())
                    .collect::<Vec<_>>();
                if invalid > ticket.max_invalid {
                    let reason = json!({
                        "reason": "invalid_correction_exhausted",
                        "errors": bounded
                    });
                    let _ = fs::write(&self.failure_path, reason.to_string());
                    Err(SubmitError::Closed(bounded.join("; ")))
                } else {
                    Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "submit_analysis rejected (one correction allowed): {}",
                                bounded.join("; ")
                            )
                        }],
                        "isError": true
                    }))
                }
            }
        }
    }
}

fn read_invalid_count(path: &Path) -> u32 {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("invalid").and_then(Value::as_u64))
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

fn validate_component(value: &str, field: &str) -> Result<(), SubmitError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(SubmitError::InvalidPath(format!(
            "{field} must be 1..=64 of [A-Za-z0-9_-]"
        )));
    }
    Ok(())
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, SubmitError> {
    let mut file = fs::File::open(path).map_err(|error| SubmitError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SubmitError::Io(error.to_string()))?;
    if bytes.len() > max {
        return Err(SubmitError::Io(format!(
            "file exceeds {max} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn write_atomic_new(path: &Path, bytes: &[u8]) -> Result<(), SubmitError> {
    let parent = path
        .parent()
        .ok_or_else(|| SubmitError::InvalidPath("output path has no parent".into()))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("submit"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| SubmitError::Io(error.to_string()))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(SubmitError::Io(error.to_string()));
    }
    drop(file);
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists || path.exists() => {
            let _ = fs::remove_file(&temp);
            Err(SubmitError::Duplicate)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(SubmitError::Io(error.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// MCP stdio server

pub fn serve_from_args() -> Result<(), String> {
    let args = std::env::args().collect::<Vec<_>>();
    let ticket = args
        .iter()
        .position(|arg| arg == "--analysis-submit-mcp")
        .and_then(|index| args.get(index + 1))
        .or_else(|| args.get(1))
        .ok_or_else(|| "missing --analysis-submit-mcp <ticket>".to_string())?;
    serve(Path::new(ticket)).map_err(|error| error.to_string())
}

pub fn serve(ticket_path: &Path) -> Result<(), SubmitError> {
    let (paths, ticket) = SubmitPaths::load(ticket_path)?;
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let Some(line) = read_mcp_line(&mut reader)? else {
            return Ok(());
        };
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                write_line(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": "parse error"}
                    }),
                )?;
                continue;
            }
        };
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let has_id = value.get("id").is_some();
        if method.is_empty() {
            // A response from the client: this server never sends requests.
            continue;
        }
        if !has_id {
            // notifications/initialized and unknown notifications are no-ops.
            continue;
        }
        let response = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "shishi_submit", "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": [{
                    "name": "submit_analysis",
                    "description": "Submit the single structured analysis result for this request. Call exactly once with the complete result object. Invalid input is rejected and at most one corrected call is allowed.",
                    "inputSchema": ticket.schema
                }]
            })),
            "tools/call" => {
                let name = value
                    .get("params")
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name != "submit_analysis" {
                    Ok(json!({
                        "content": [{"type": "text", "text": "unknown tool"}],
                        "isError": true
                    }))
                } else {
                    let arguments = value
                        .get("params")
                        .and_then(|params| params.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    match paths.submit(&ticket, &arguments) {
                        Ok(result) => Ok(result),
                        Err(SubmitError::Duplicate) => Ok(json!({
                            "content": [{"type": "text", "text": "duplicate submit_analysis call: a result was already accepted"}],
                            "isError": true
                        })),
                        Err(SubmitError::Closed(reason)) => Ok(json!({
                            "content": [{"type": "text", "text": format!("submit_analysis closed: {reason}")}],
                            "isError": true
                        })),
                        Err(SubmitError::Schema(errors)) => Ok(json!({
                            "content": [{"type": "text", "text": format!("submit_analysis rejected: {}", errors.join("; "))}],
                            "isError": true
                        })),
                        Err(error) => Err(error),
                    }
                }
            }
            _ => {
                write_line(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    }),
                )?;
                continue;
            }
        };
        match response {
            Ok(result) => write_line(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )?,
            Err(error) => write_line(
                &mut out,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": error.to_string()}
                }),
            )?,
        }
    }
}

fn read_mcp_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, SubmitError> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| SubmitError::Io(error.to_string()))?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            let take = position + 1;
            if line.len() + take > MAX_MCP_LINE + 2 {
                too_long = true;
            } else {
                line.extend_from_slice(&available[..take]);
            }
            reader.consume(take);
            break;
        }
        if line.len() + available.len() > MAX_MCP_LINE + 2 {
            too_long = true;
        } else {
            line.extend_from_slice(available);
        }
        let consumed = available.len();
        reader.consume(consumed);
    }
    if too_long {
        return Err(SubmitError::Protocol("MCP line is too long".into()));
    }
    while line
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        line.pop();
    }
    Ok(Some(line))
}

fn write_line(out: &mut impl Write, value: &Value) -> Result<(), SubmitError> {
    serde_json::to_writer(&mut *out, value).map_err(|error| SubmitError::Io(error.to_string()))?;
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|error| SubmitError::Io(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "reason": {"type": "string", "maxLength": 10},
                "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
            },
            "required": ["reason", "changes"]
        })
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shishi-submit-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn accepts_one_valid_result_and_rejects_duplicate() {
        let base = temp_dir("valid");
        let (paths, ticket) = SubmitPaths::create(&base, "req-1", "edit", schema()).unwrap();
        let result = paths
            .submit(&ticket, &json!({"reason": "ok", "changes": ["a"]}))
            .unwrap();
        assert_eq!(result["isError"], false);
        let accepted = paths.read_accepted("req-1", "edit", &schema()).unwrap();
        assert_eq!(accepted.value["reason"], "ok");
        assert!(matches!(
            paths.submit(&ticket, &json!({"reason": "ok", "changes": []})),
            Err(SubmitError::Duplicate)
        ));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn allows_one_correction_then_closes() {
        let base = temp_dir("correct");
        let (paths, ticket) = SubmitPaths::create(&base, "req-2", "edit", schema()).unwrap();
        let first = paths.submit(&ticket, &json!({"reason": "ok"})).unwrap();
        assert_eq!(first["isError"], true);
        let second = paths
            .submit(&ticket, &json!({"reason": "ok", "changes": ["a"]}))
            .unwrap();
        assert_eq!(second["isError"], false);
        assert!(paths.read_accepted("req-2", "edit", &schema()).is_ok());
        let _ = fs::remove_dir_all(base);

        let base = temp_dir("exhaust");
        let (paths, ticket) = SubmitPaths::create(&base, "req-3", "edit", schema()).unwrap();
        assert_eq!(
            paths.submit(&ticket, &json!({"reason": "ok"})).unwrap()["isError"],
            true
        );
        assert!(matches!(
            paths.submit(&ticket, &json!({"reason": "ok"})),
            Err(SubmitError::Closed(_))
        ));
        assert!(paths.failure_path.is_file());
        assert!(!paths.result_path.is_file());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn invalid_first_does_not_create_result() {
        let base = temp_dir("invalid");
        let (paths, ticket) = SubmitPaths::create(&base, "req-4", "edit", schema()).unwrap();
        let response = paths
            .submit(
                &ticket,
                &json!({"reason": "ok", "changes": [], "extra": true}),
            )
            .unwrap();
        assert_eq!(response["isError"], true);
        assert!(!paths.result_path.exists());
        assert!(matches!(
            paths.read_accepted("req-4", "edit", &schema()),
            Err(SubmitError::Missing)
        ));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn ticket_hash_and_expected_request_are_enforced() {
        let base = temp_dir("hash");
        let (paths, _ticket) = SubmitPaths::create(&base, "req-5", "edit", schema()).unwrap();
        let mut modified = schema();
        modified["properties"]["reason"]["maxLength"] = json!(20);
        assert!(paths.read_accepted("req-5", "edit", &modified).is_err());
        assert!(paths.read_accepted("other", "edit", &schema()).is_err());
        assert!(paths.ticket_path.is_file());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn invalid_request_component_is_rejected() {
        let base = temp_dir("path");
        assert!(SubmitPaths::create(&base, "../escape", "edit", schema()).is_err());
        assert!(SubmitPaths::create(&base, "ok", "bad kind", schema()).is_err());
        let _ = fs::remove_dir_all(base);
    }
}
