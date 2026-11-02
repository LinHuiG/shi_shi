//! Analysis-specific assembly: caller-prepared prompt, one submit MCP server,
//! one new session, exact schema validation and one accepted result file.
//!
//! There is deliberately no prose fallback and no second prompt. If the MCP
//! server records no accepted result, the analysis fails.

use crate::client::{AcpClient, AcpEvent, ClientError, McpServer, PermissionPolicy, PromptResult};
use crate::submit::{SubmitError, SubmitPaths};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct PreparedAnalysis {
    pub paths: SubmitPaths,
    server: McpServer,
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
pub struct AnalysisAccepted {
    pub value: Value,
    pub session_id: String,
    pub prompt: PromptResult,
    /// Only a confirmed close permits the caller to reuse the ACP process.
    /// An unsupported/failed close does not invalidate an accepted result.
    pub session_closed: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AnalysisError {
    #[error("{0}")]
    Client(ClientError),
    #[error("{0}")]
    Submit(SubmitError),
    #[error("analysis ended without an accepted submit_analysis result")]
    MissingSubmission,
    #[error("analysis path must be absolute: {0}")]
    NonAbsolutePath(String),
    #[error("analysis prompt stopped without an acceptable result: {0}")]
    Stopped(String),
}

impl From<ClientError> for AnalysisError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<SubmitError> for AnalysisError {
    fn from(error: SubmitError) -> Self {
        Self::Submit(error)
    }
}

/// Create the one-time ticket and the exact MCP declarations for an analysis
/// session. `ticket_base` is app-owned (`.../data/analysis-submit`), never the
/// user profile.
pub fn prepare(ticket_base: &Path, spec: &AnalysisSpec) -> Result<PreparedAnalysis, AnalysisError> {
    if !spec.workdir.is_absolute() {
        return Err(AnalysisError::NonAbsolutePath(
            spec.workdir.display().to_string(),
        ));
    }
    if !spec.app_exe.is_absolute() {
        return Err(AnalysisError::NonAbsolutePath(
            spec.app_exe.display().to_string(),
        ));
    }
    let (paths, _) = SubmitPaths::create(
        ticket_base,
        &spec.request_id,
        &spec.kind,
        spec.schema.clone(),
    )?;
    let server = McpServer::stdio(
        "shishi_submit",
        &spec.app_exe,
        [
            "--analysis-submit-mcp".to_string(),
            paths.ticket_path.to_string_lossy().to_string(),
        ],
        Vec::<(String, String)>::new(),
    );
    Ok(PreparedAnalysis { paths, server })
}

/// Run exactly one prompt and read only the accepted MCP result. The prompt is
/// the caller's exact payload; this function adds no instructions or text.
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
            .ok_or_else(|| ClientError::Timeout {
                method: "analysis".into(),
                ambiguous: true,
            })
    };
    let session = client.new_session_with_policy(
        &spec.workdir,
        std::slice::from_ref(&prepared.server),
        remaining()?,
        PermissionPolicy::DenyAll,
    )?;
    if let Some((provider, model)) = &spec.model {
        client.select_model(&session.session_id, provider, model, remaining()?)?;
    }
    if let Some(effort) = &spec.effort {
        client.select_effort(&session.session_id, effort, remaining()?)?;
    }
    let timeout = remaining()?;
    let (prompt, value) = (|| -> Result<_, AnalysisError> {
        let prompt = client.prompt_with_images(
            &session.session_id,
            &spec.prompt,
            &spec.images,
            timeout,
            on_event,
        )?;
        if matches!(prompt.stop_reason.as_str(), "cancelled" | "refusal") {
            return Err(AnalysisError::Stopped(prompt.stop_reason));
        }
        let value = prepared
            .paths
            .read_accepted(&spec.request_id, &spec.kind, &spec.schema)
            .map_err(|error| match error {
                SubmitError::Missing => AnalysisError::MissingSubmission,
                error => AnalysisError::Submit(error),
            })?;
        Ok((prompt, value))
    })()
    .inspect_err(|_| {
        let _ = client.close_session(&session.session_id, Duration::from_secs(2));
    })?;
    let session_closed = remaining().is_ok_and(|timeout| {
        client
            .close_session(&session.session_id, timeout.min(Duration::from_secs(2)))
            .is_ok()
    });
    Ok(AnalysisAccepted {
        value,
        session_id: session.session_id,
        prompt,
        session_closed,
    })
}

/// Provider/model helper for callers that keep only a model id. It returns the
/// exact opaque ACP value, never a guessed default.
pub fn model_value(provider: &str, model: &str) -> String {
    serde_json::to_string(&[provider, model]).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(base: &Path) -> AnalysisSpec {
        AnalysisSpec {
            request_id: "req-analysis-1".into(),
            kind: "edit".into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"reason": {"type": "string"}},
                "required": ["reason"]
            }),
            prompt: "exact caller payload".into(),
            images: vec![],
            workdir: base.join("work"),
            app_exe: base.join("shishi.exe"),
            model: Some(("deepseek-official".into(), "deepseek-v4-flash".into())),
            effort: Some("high".into()),
        }
    }

    #[test]
    fn prepared_mcp_declarations_are_exact_stdio_servers() {
        let base = crate::test_path("analysis");
        std::fs::create_dir_all(&base).unwrap();
        let spec = spec(&base);
        let prepared = prepare(&base.join("tickets"), &spec).unwrap();
        assert_eq!(prepared.server.name, "shishi_submit");
        assert_eq!(prepared.server.command, spec.app_exe);
        assert_eq!(
            prepared.server.args,
            vec![
                "--analysis-submit-mcp".to_string(),
                prepared.paths.ticket_path.to_string_lossy().to_string()
            ]
        );
        assert!(prepared.server.env.is_empty());
        assert!(prepared.paths.ticket_path.is_file());
        assert_eq!(
            SubmitPaths::load(&prepared.paths.ticket_path)
                .unwrap()
                .1
                .schema_hash,
            crate::sha256::hex(&serde_json::to_vec(&spec.schema).unwrap())
        );
        assert_eq!(
            model_value("deepseek-official", "deepseek-v4-flash"),
            "[\"deepseek-official\",\"deepseek-v4-flash\"]"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn relative_paths_are_rejected_before_ticket_creation() {
        let base = crate::test_path("analysis-rel");
        let mut spec = spec(&base);
        spec.workdir = PathBuf::from("relative");
        assert!(matches!(
            prepare(&base.join("tickets"), &spec),
            Err(AnalysisError::NonAbsolutePath(_))
        ));
        assert!(!base.join("tickets").exists());
    }
}
