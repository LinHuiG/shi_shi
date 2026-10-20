//! Analysis-specific assembly: caller-prepared prompt, two stdio MCP servers,
//! one new session, exact schema validation and one accepted result file.
//!
//! There is deliberately no prose fallback and no second prompt. If the MCP
//! server records no accepted result, the analysis fails.

use crate::client::{
    AcpClient, AcpEvent, ClientError, McpServer, PermissionPolicy, PromptResult, SessionInfo,
};
use crate::schema_validate;
use crate::submit::{AcceptedSubmission, SubmitError, SubmitPaths};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct PreparedAnalysis {
    pub paths: SubmitPaths,
    pub schema_hash: String,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone)]
pub struct AnalysisSpec {
    pub request_id: String,
    pub kind: String,
    pub schema: Value,
    pub prompt: String,
    pub workdir: PathBuf,
    pub snapshot_path: PathBuf,
    pub app_exe: PathBuf,
    pub model: Option<(String, String)>,
    pub effort: Option<String>,
    /// Whether the measured request contains read_context/read_image. When
    /// false, no analysis MCP server is declared at all, so the model cannot see
    /// tools that were excluded from the budget.
    pub retrieval: bool,
}

#[derive(Debug, Clone)]
pub struct AnalysisAccepted {
    pub value: Value,
    pub schema_hash: String,
    pub session: SessionInfo,
    pub prompt: PromptResult,
}

#[derive(Debug, Clone)]
pub enum AnalysisError {
    Client(ClientError),
    Submit(SubmitError),
    MissingSubmission,
    InvalidAccepted(Vec<String>),
    NonAbsolutePath(String),
    UnsafeToolset(String),
    Stopped(String),
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(f, "{error}"),
            Self::Submit(error) => write!(f, "{error}"),
            Self::MissingSubmission => {
                f.write_str("analysis ended without an accepted submit_analysis result")
            }
            Self::InvalidAccepted(errors) => {
                write!(
                    f,
                    "accepted submit_analysis result failed validation: {errors:?}"
                )
            }
            Self::NonAbsolutePath(path) => write!(f, "analysis path must be absolute: {path}"),
            Self::UnsafeToolset(message) => {
                write!(f, "analysis toolset is not restricted: {message}")
            }
            Self::Stopped(reason) => {
                write!(
                    f,
                    "analysis prompt stopped without an acceptable result: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for AnalysisError {}

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
    if !spec.snapshot_path.is_absolute() {
        return Err(AnalysisError::NonAbsolutePath(
            spec.snapshot_path.display().to_string(),
        ));
    }
    if !spec.app_exe.is_absolute() {
        return Err(AnalysisError::NonAbsolutePath(
            spec.app_exe.display().to_string(),
        ));
    }
    let (paths, ticket) = SubmitPaths::create(
        ticket_base,
        &spec.request_id,
        &spec.kind,
        spec.schema.clone(),
    )?;
    let mut mcp_servers = Vec::new();
    if spec.retrieval {
        mcp_servers.push(McpServer::stdio(
            "shishi_analysis",
            &spec.app_exe,
            [
                "--analysis-mcp".to_string(),
                spec.snapshot_path.to_string_lossy().to_string(),
            ],
            Vec::<(String, String)>::new(),
        ));
    }
    mcp_servers.push(McpServer::stdio(
        "shishi_submit",
        &spec.app_exe,
        [
            "--analysis-submit-mcp".to_string(),
            paths.ticket_path.to_string_lossy().to_string(),
        ],
        Vec::<(String, String)>::new(),
    ));
    let prepared = PreparedAnalysis {
        paths,
        schema_hash: ticket.schema_hash,
        mcp_servers,
    };
    prepared.validate(spec)?;
    Ok(prepared)
}

impl PreparedAnalysis {
    /// Re-check the exact two-server toolset after the struct has crossed a
    /// caller boundary. No built-in tool, HTTP MCP server, extra env, or extra
    /// MCP server is accepted for analysis.
    pub fn validate(&self, spec: &AnalysisSpec) -> Result<(), AnalysisError> {
        let expected = if spec.retrieval { 2 } else { 1 };
        if self.mcp_servers.len() != expected {
            return Err(AnalysisError::UnsafeToolset(format!(
                "expected exactly {expected} MCP servers, got {}",
                self.mcp_servers.len()
            )));
        }
        let submit = self.mcp_servers.last().expect("server count checked");
        if spec.retrieval {
            let analysis = &self.mcp_servers[0];
            if analysis.name != "shishi_analysis"
                || analysis.command != spec.app_exe
                || analysis.args
                    != vec![
                        "--analysis-mcp".to_string(),
                        spec.snapshot_path.to_string_lossy().to_string(),
                    ]
                || !analysis.env.is_empty()
            {
                return Err(AnalysisError::UnsafeToolset(
                    "shishi_analysis must be the app executable with only --analysis-mcp <snapshot>"
                        .into(),
                ));
            }
        }
        if submit.name != "shishi_submit"
            || submit.command != spec.app_exe
            || submit.args
                != vec![
                    "--analysis-submit-mcp".to_string(),
                    self.paths.ticket_path.to_string_lossy().to_string(),
                ]
            || !submit.env.is_empty()
        {
            return Err(AnalysisError::UnsafeToolset(
                "shishi_submit must be the app executable with only --analysis-submit-mcp <ticket>"
                    .into(),
            ));
        }
        Ok(())
    }
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
    prepared.validate(spec)?;
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
        &prepared.mcp_servers,
        remaining()?,
        PermissionPolicy::DenyAll,
    )?;
    if let Some((provider, model)) = &spec.model {
        client.select_model(&session.session_id, provider, model, remaining()?)?;
    }
    if let Some(effort) = &spec.effort {
        client.select_effort(&session.session_id, effort, remaining()?)?;
    }
    let prompt = match client.prompt(&session.session_id, &spec.prompt, remaining()?, on_event) {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = client.close_session(&session.session_id, Duration::from_secs(2));
            return Err(AnalysisError::Client(error));
        }
    };
    if prompt.stop_reason == "cancelled" || prompt.stop_reason == "refusal" {
        let _ = client.close_session(&session.session_id, Duration::from_secs(2));
        return Err(AnalysisError::Stopped(prompt.stop_reason));
    }
    let accepted: AcceptedSubmission =
        match prepared
            .paths
            .read_accepted(&spec.request_id, &spec.kind, &spec.schema)
        {
            Ok(accepted) => accepted,
            Err(SubmitError::Missing) => {
                let _ = client.close_session(&session.session_id, Duration::from_secs(2));
                return Err(AnalysisError::MissingSubmission);
            }
            Err(error) => {
                let _ = client.close_session(&session.session_id, Duration::from_secs(2));
                return Err(AnalysisError::Submit(error));
            }
        };
    if let Err(errors) = schema_validate::validate(&spec.schema, &accepted.value) {
        let _ = client.close_session(&session.session_id, Duration::from_secs(2));
        return Err(AnalysisError::InvalidAccepted(errors));
    }
    let latest = client.session(&session.session_id).unwrap_or(session);
    let _ = client.close_session(&latest.session_id, Duration::from_secs(2));
    Ok(AnalysisAccepted {
        value: accepted.value,
        schema_hash: accepted.schema_hash,
        session: latest,
        prompt,
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
            workdir: base.join("work"),
            snapshot_path: base.join("snapshot.json"),
            app_exe: base.join("shishi.exe"),
            model: Some(("deepseek-official".into(), "deepseek-v4-flash".into())),
            effort: Some("high".into()),
            retrieval: true,
        }
    }

    #[test]
    fn prepared_mcp_declarations_are_exact_stdio_servers() {
        let base = std::env::temp_dir().join(format!(
            "shishi-analysis-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let spec = spec(&base);
        let prepared = prepare(&base.join("tickets"), &spec).unwrap();
        assert_eq!(prepared.mcp_servers.len(), 2);
        assert_eq!(prepared.mcp_servers[0].name, "shishi_analysis");
        assert_eq!(prepared.mcp_servers[1].name, "shishi_submit");
        assert_eq!(prepared.mcp_servers[1].args[0], "--analysis-submit-mcp");
        assert!(prepared.paths.ticket_path.is_file());
        assert_eq!(
            prepared.schema_hash,
            crate::sha256::hex(&serde_json::to_vec(&spec.schema).unwrap())
        );
        assert_eq!(
            model_value("deepseek-official", "deepseek-v4-flash"),
            "[\"deepseek-official\",\"deepseek-v4-flash\"]"
        );
        let mut mutated = prepared.clone();
        mutated.mcp_servers.push(McpServer::stdio(
            "extra_shell",
            &spec.app_exe,
            ["--shell"],
            Vec::<(String, String)>::new(),
        ));
        assert!(matches!(
            mutated.validate(&spec),
            Err(AnalysisError::UnsafeToolset(_))
        ));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn no_retrieval_request_declares_only_submit_server() {
        let base = std::env::temp_dir().join(format!(
            "shishi-analysis-no-retrieval-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let mut spec = spec(&base);
        spec.retrieval = false;
        let prepared = prepare(&base.join("tickets"), &spec).unwrap();
        assert_eq!(prepared.mcp_servers.len(), 1);
        assert_eq!(prepared.mcp_servers[0].name, "shishi_submit");
        prepared.validate(&spec).unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn relative_paths_are_rejected_before_ticket_creation() {
        let base = std::env::temp_dir().join(format!(
            "shishi-analysis-rel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut spec = spec(&base);
        spec.workdir = PathBuf::from("relative");
        assert!(matches!(
            prepare(&base.join("tickets"), &spec),
            Err(AnalysisError::NonAbsolutePath(_))
        ));
        assert!(!base.join("tickets").exists());
    }
}
