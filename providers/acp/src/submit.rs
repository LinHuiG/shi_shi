//! Structured analysis results: the `submit_analysis` stdio MCP server
//! (`app.exe --analysis-submit-mcp <ticket>`, exposed by DSH as
//! `mcp__shishi_submit__submit_analysis`) and the one-prompt analysis run
//! that reads its accepted result. There is no prose fallback: no accepted
//! submission means the analysis failed. One invalid call may be corrected.

use crate::client::{AcpClient, AcpEvent, ClientError, McpServer, PromptResult};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, thiserror::Error)]
pub enum SubmitError {
    #[error("submit store I/O error: {0}")]
    Io(String),
    #[error("no accepted submit_analysis result")]
    Missing,
    #[error("submit_analysis is closed: {0}")]
    Closed(String),
    #[error("duplicate submit_analysis call: a result was already accepted")]
    Duplicate,
    #[error("submit_analysis rejected: {0}")]
    Schema(String),
    #[error("submit_analysis protocol error: {0}")]
    Protocol(String),
}

fn io(error: impl std::fmt::Display) -> SubmitError {
    SubmitError::Io(error.to_string())
}

/// One request directory: `ticket.json` {request_id, kind, schema},
/// `result.json` once accepted, `state.json` counting invalid calls.
#[derive(Debug, Clone)]
pub struct SubmitPaths {
    pub ticket_path: PathBuf,
    pub result_path: PathBuf,
    pub state_path: PathBuf,
}

impl SubmitPaths {
    fn at(directory: &Path) -> Self {
        let file = |name| directory.join(name);
        Self { ticket_path: file("ticket.json"), result_path: file("result.json"), state_path: file("state.json") }
    }

    pub fn create(base: &Path, request_id: &str, kind: &str, schema: Value) -> Result<Self, SubmitError> {
        let paths = Self::at(&base.join(request_id));
        fs::create_dir_all(base.join(request_id)).map_err(io)?;
        let ticket = json!({"request_id": request_id, "kind": kind, "schema": schema});
        fs::write(&paths.ticket_path, ticket.to_string()).map_err(io)?;
        Ok(paths)
    }

    pub fn load(ticket_path: &Path) -> Result<(Self, Value), SubmitError> {
        let ticket = serde_json::from_slice(&fs::read(ticket_path).map_err(io)?).map_err(io)?;
        Ok((Self::at(ticket_path.parent().unwrap()), ticket))
    }

    fn state(&self) -> Value {
        fs::read(&self.state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({"invalid": 0}))
    }

    pub fn read_accepted(&self, request_id: &str, kind: &str, schema: &Value) -> Result<Value, SubmitError> {
        if let Some(reason) = self.state()["closed"].as_str() {
            return Err(SubmitError::Closed(reason.into()));
        }
        let bytes = fs::read(&self.result_path).map_err(|_| SubmitError::Missing)?;
        let result: Value = serde_json::from_slice(&bytes).map_err(|e| SubmitError::Protocol(e.to_string()))?;
        if result["request_id"] != request_id || result["kind"] != kind {
            return Err(SubmitError::Protocol("result does not match the request id/kind".into()));
        }
        shishi_api::schema_validate::validate(schema, &result["value"])
            .map_err(|e| SubmitError::Schema(e.to_string()))?;
        Ok(result["value"].clone())
    }

    /// The tool result for one `submit_analysis` call.
    pub fn submit(&self, ticket: &Value, arguments: &Value) -> Result<Value, SubmitError> {
        if self.result_path.is_file() {
            return Err(SubmitError::Duplicate);
        }
        let state = self.state();
        if let Some(reason) = state["closed"].as_str() {
            return Err(SubmitError::Closed(reason.into()));
        }
        let errors = match shishi_api::schema_validate::validate_output(&ticket["schema"], arguments.clone()) {
            Ok(value) => {
                let result = json!({"request_id": ticket["request_id"], "kind": ticket["kind"], "value": value});
                let temp = self.result_path.with_extension("tmp");
                fs::write(&temp, result.to_string()).and_then(|_| fs::rename(&temp, &self.result_path)).map_err(io)?;
                return Ok(json!({"content": [{"type": "text", "text": "accepted"}], "isError": false}));
            }
            Err(errors) => errors.to_string().chars().take(240).collect::<String>(),
        };
        let invalid = state["invalid"].as_u64().unwrap_or(0) + 1;
        let closed = (invalid > 1).then(|| errors.clone());
        fs::write(&self.state_path, json!({"invalid": invalid, "closed": closed}).to_string()).map_err(io)?;
        match closed {
            Some(reason) => Err(SubmitError::Closed(reason)),
            None => Ok(
                json!({"content": [{"type": "text", "text": format!("submit_analysis rejected (one correction allowed): {errors}")}], "isError": true}),
            ),
        }
    }
}

/// The MCP stdio loop for one ticket (protocol `2024-11-05`, one tool).
pub fn serve(ticket_path: &Path) -> Result<(), SubmitError> {
    let (paths, ticket) = SubmitPaths::load(ticket_path)?;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(io)?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            writeln!(
                out,
                "{}",
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})
            )
            .map_err(io)?;
            continue;
        };
        // Notifications and client responses need no answer.
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else { continue };
        let tool_error = |text: String| Ok(json!({"content": [{"type": "text", "text": text}], "isError": true}));
        let result = match method {
            "initialize" => {
                Ok(json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "shishi_submit", "version": env!("CARGO_PKG_VERSION")}}))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": [{"name": "submit_analysis", "inputSchema": ticket["schema"],
                "description": "Submit the single structured analysis result for this request. Call exactly once with the complete result object. Invalid input is rejected and at most one corrected call is allowed."}]})),
            "tools/call" if message["params"]["name"] != "submit_analysis" => tool_error("unknown tool".into()),
            "tools/call" => match paths.submit(&ticket, &message["params"]["arguments"]) {
                Err(e @ (SubmitError::Duplicate | SubmitError::Closed(_) | SubmitError::Schema(_))) => {
                    tool_error(e.to_string())
                }
                other => other.map_err(|e| (-32603, e.to_string())),
            },
            _ => Err((-32601, "method not found".into())),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        };
        writeln!(out, "{reply}").and_then(|_| out.flush()).map_err(io)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AnalysisSpec {
    pub request_id: String,
    pub kind: String,
    pub schema: Value,
    pub prompt: String,
    pub images: Vec<Value>,
    pub workdir: PathBuf,
    pub app_exe: PathBuf,
    pub model: Option<(String, String)>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PreparedAnalysis {
    pub paths: SubmitPaths,
    pub server: McpServer,
}

#[derive(Debug, Clone)]
pub struct AnalysisAccepted {
    pub value: Value,
    pub session_id: String,
    pub prompt: PromptResult,
    /// Only a confirmed close lets the caller reuse the process.
    pub session_closed: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AnalysisError {
    #[error("{0}")]
    Client(#[from] ClientError),
    #[error("{0}")]
    Submit(SubmitError),
    #[error("analysis ended without an accepted submit_analysis result")]
    MissingSubmission,
    #[error("analysis prompt stopped without an acceptable result: {0}")]
    Stopped(String),
}

/// The ticket and the `shishi_submit` server declaration for one analysis.
pub fn prepare(ticket_base: &Path, spec: &AnalysisSpec) -> Result<PreparedAnalysis, AnalysisError> {
    let paths = SubmitPaths::create(ticket_base, &spec.request_id, &spec.kind, spec.schema.clone())
        .map_err(AnalysisError::Submit)?;
    let args = ["--analysis-submit-mcp".to_string(), paths.ticket_path.to_string_lossy().into()];
    Ok(PreparedAnalysis { server: McpServer::stdio("shishi_submit", &spec.app_exe, args), paths })
}

/// One new session, one prompt of the caller's exact payload, and only the
/// accepted MCP result.
pub fn run(
    client: &AcpClient,
    spec: &AnalysisSpec,
    prepared: &PreparedAnalysis,
    timeout: Duration,
    on_event: &mut dyn FnMut(AcpEvent),
) -> Result<AnalysisAccepted, AnalysisError> {
    let deadline = Instant::now() + timeout;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| ClientError::Timeout("analysis".into()))
    };
    let session = client.new_session(&spec.workdir, std::slice::from_ref(&prepared.server), remaining()?)?.session_id;
    if let Some((provider, model)) = &spec.model {
        client.select_model(&session, provider, model, remaining()?)?;
    }
    if let Some(effort) = &spec.effort {
        client.select_effort(&session, effort, remaining()?)?;
    }
    let outcome = (|| {
        let prompt = client.prompt(&session, &spec.prompt, &spec.images, remaining()?, on_event)?;
        if matches!(prompt.stop_reason.as_str(), "cancelled" | "refusal") {
            return Err(AnalysisError::Stopped(prompt.stop_reason));
        }
        match prepared.paths.read_accepted(&spec.request_id, &spec.kind, &spec.schema) {
            Ok(value) => Ok((prompt, value)),
            Err(SubmitError::Missing) => Err(AnalysisError::MissingSubmission),
            Err(error) => Err(AnalysisError::Submit(error)),
        }
    })();
    let close = remaining().map_or(Duration::from_secs(2), |r| r.min(Duration::from_secs(2)));
    let session_closed = client.close_session(&session, close).is_ok();
    let (prompt, value) = outcome?;
    Ok(AnalysisAccepted { value, session_id: session, prompt, session_closed })
}

#[cfg(test)]
mod tests;
