//! ACP v1 client/session primitives.
//!
//! Protocol shapes are taken from the installed runtime source:
//! `@deepseek-ai/dsh-acp/lib/index.js` (set_config_option, session/new,
//! session/resume, prompt, cancel, request_permission, usage_update) and the
//! bundled `@agentclientprotocol/sdk` schema (`PROTOCOL_VERSION = 1`).
//! The client never parses assistant text as a business result.

use crate::jsonrpc::{
    encode_error, encode_notification, encode_request, encode_response, Incoming, RequestId,
    RpcError,
};
use crate::transport::{StderrSnapshot, Transport, TransportConfig, TransportError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_PENDING_PERMISSIONS: usize = 16;
const MAX_PROMPT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOOL_TEXT: usize = 4096;

#[derive(Debug, Clone)]
pub struct AcpLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub config: TransportConfig,
}

impl AcpLaunch {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            config: TransportConfig::default(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn config(mut self, config: TransportConfig) -> Self {
        self.config = config;
        self
    }

    pub fn command(&self) -> Result<Command, ClientError> {
        let command_line = build_child_command(&self.program, &self.args)?;
        let mut command = command_line;
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        Ok(command)
    }
}

fn build_child_command(program: &Path, args: &[OsString]) -> Result<Command, ClientError> {
    let extension = program
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "cmd" || extension == "bat" {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/Q", "/V:OFF", "/S", "/C"]);
            // Expand each opaque value exactly once from the environment.
            // cmd does not recursively expand percent signs in substituted
            // values, and /V:OFF preserves literal exclamation marks. Quote
            // every value so shell operators are never executable arguments.
            let valid = |value: &str| !value.contains(['"', '\r', '\n', '\0']);
            if !valid(&program.to_string_lossy()) {
                return Err(ClientError::Unsupported(
                    "launcher path contains quotes or control characters".into(),
                ));
            }
            command.env("SHISHI_ACP_PROGRAM", program.as_os_str());
            let mut inner = String::from("\"%SHISHI_ACP_PROGRAM%\"");
            for (index, arg) in args.iter().enumerate() {
                if !valid(&arg.to_string_lossy()) {
                    return Err(ClientError::Unsupported(
                        "launcher arguments may not contain quotes or control characters".into(),
                    ));
                }
                let key = format!("SHISHI_ACP_ARG_{index}");
                command.env(&key, arg);
                inner.push_str(&format!(" \"%{key}%\""));
            }
            command.raw_arg(format!("\"{inner}\""));
            return Ok(command);
        }
        #[cfg(not(windows))]
        {
            let _ = args;
            return Err(ClientError::Unsupported(
                "cmd/bat launchers are Windows-only".into(),
            ));
        }
    }
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Analysis: reject every permission request; never grant.
    DenyAll,
    /// Handling: surface the exact options and wait for an explicit caller
    /// decision. A prompt/message never approves anything.
    Surface,
}

#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServer {
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
        env: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: env.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        }
    }

    fn validate(&self) -> Result<(), ClientError> {
        let name = &self.name;
        if name.is_empty()
            || name.len() > 32
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ClientError::Unsupported(format!(
                "invalid MCP server name: {name:?}"
            )));
        }
        if !self.command.is_absolute() {
            return Err(ClientError::Unsupported(format!(
                "MCP server `{name}` command must be absolute: {}",
                self.command.display()
            )));
        }
        let mut names = std::collections::HashSet::new();
        for (key, _) in &self.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                return Err(ClientError::Unsupported(format!(
                    "MCP server `{name}` has an invalid environment name"
                )));
            }
            if !names.insert(key.clone()) {
                return Err(ClientError::Unsupported(format!(
                    "MCP server `{name}` has a duplicate environment name `{key}`"
                )));
            }
        }
        Ok(())
    }

    fn to_json(&self) -> Result<Value, ClientError> {
        self.validate()?;
        Ok(json!({
            "name": self.name,
            "command": self.command.to_string_lossy(),
            "args": self.args,
            "env": self.env.iter().map(|(name, value)| json!({"name": name, "value": value})).collect::<Vec<_>>(),
        }))
    }
}

fn mcp_servers_json(servers: &[McpServer]) -> Result<Value, ClientError> {
    let mut names = std::collections::HashSet::new();
    let mut values = Vec::with_capacity(servers.len());
    for server in servers {
        if !names.insert(server.name.clone()) {
            return Err(ClientError::Unsupported(format!(
                "duplicate MCP server name: {}",
                server.name
            )));
        }
        values.push(server.to_json()?);
    }
    Ok(Value::Array(values))
}

#[derive(Debug, Clone, Default)]
struct AgentCapabilities {
    resume: bool,
    close: bool,
    prompt_image: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigOption {
    pub id: String,
    pub current_value: String,
    pub choices: Vec<ConfigChoice>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub config_options: Vec<ConfigOption>,
    pub model_value: Option<String>,
}

impl SessionInfo {
    fn replace_options(&mut self, options: Vec<ConfigOption>) {
        self.config_options = options;
        self.model_value = self
            .model_option()
            .map(|option| option.current_value.clone());
    }

    pub fn model_option(&self) -> Option<&ConfigOption> {
        self.config_options
            .iter()
            .find(|option| option.id == "model")
    }

    pub fn effort_option(&self) -> Option<&ConfigOption> {
        self.config_options
            .iter()
            .find(|option| option.id == "reasoning_effort")
    }

    /// ACP model values are opaque. The installed bridge emits
    /// `JSON.stringify([provider, model])`; match that exact encoding only.
    pub fn find_model_value(&self, provider: &str, model: &str) -> Option<String> {
        self.model_option()?.choices.iter().find_map(|choice| {
            let route: [String; 2] = serde_json::from_str(&choice.value).ok()?;
            (route[0] == provider && route[1] == model).then(|| choice.value.clone())
        })
    }

    pub fn find_effort_value(&self, effort: &str) -> Option<String> {
        self.effort_option()?
            .choices
            .iter()
            .find(|choice| choice.value == effort)
            .map(|choice| choice.value.clone())
    }

    pub fn current_model_route(&self) -> Option<(String, String)> {
        serde_json::from_str(self.model_value.as_deref()?).ok()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_read_tokens: Option<u64>,
}

impl TokenUsage {
    /// `None` means unknown, never zero. The prompt response usage object is
    /// optional and unstable in the ACP schema; missing required counters is
    /// treated as unknown rather than fabricating zeros.
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let input_tokens = object.get("inputTokens")?.as_u64()?;
        let output_tokens = object.get("outputTokens")?.as_u64()?;
        Some(Self {
            total_tokens: object
                .get("totalTokens")
                .and_then(Value::as_u64)
                .or_else(|| input_tokens.checked_add(output_tokens))?,
            input_tokens,
            output_tokens,
            cached_read_tokens: object.get("cachedReadTokens").and_then(Value::as_u64),
        })
    }
}

#[derive(Debug, Clone)]
pub enum SessionUpdate {
    AgentMessageChunk {
        text: String,
    },
    AgentThoughtChunk {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        title: Option<String>,
        status: Option<String>,
        kind: Option<String>,
        raw_input: Value,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: Option<String>,
        text: String,
    },
    Other,
}

#[derive(Debug, Clone)]
pub struct AcpEvent {
    pub session_id: String,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub request_id: RequestId,
    pub session_id: String,
    pub options: Vec<PermissionOption>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct PromptResult {
    pub stop_reason: String,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Transport(TransportError),
    #[error("ACP error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("ACP protocol failure: {0}")]
    Protocol(String),
    #[error("ACP capability failure: {0}")]
    Unsupported(String),
    #[error("unknown ACP session: {0}")]
    UnknownSession(String),
    #[error(
        "model {provider}/{model} is not advertised by the ACP session; refusing to use a default"
    )]
    ModelNotAdvertised { provider: String, model: String },
    #[error("reasoning effort {0:?} is not advertised by the ACP session")]
    EffortNotAdvertised(String),
    #[error("{method} timed out (ambiguous={ambiguous})")]
    Timeout { method: String, ambiguous: bool },
    #[error("{method} lost the ACP process (ambiguous={ambiguous})")]
    Disconnected { method: String, ambiguous: bool },
    #[error("no pending permission with that id")]
    PermissionNotFound,
    #[error("permission option was not offered by the agent")]
    PermissionOptionNotOffered,
    #[error("prompt must not be empty")]
    PromptEmpty,
    #[error("prompt exceeds the ACP frame limit")]
    PromptTooLarge,
}

impl From<TransportError> for ClientError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Timeout => Self::Timeout {
                method: "transport".into(),
                ambiguous: true,
            },
            TransportError::Closed => Self::Disconnected {
                method: "transport".into(),
                ambiguous: true,
            },
            other => Self::Transport(other),
        }
    }
}

pub struct AcpClient {
    transport: Transport,
    next_id: AtomicI64,
    request_lock: Mutex<()>,
    policy: PermissionPolicy,
    session_policies: Mutex<HashMap<String, PermissionPolicy>>,
    capabilities: Mutex<Option<AgentCapabilities>>,
    sessions: Mutex<HashMap<String, SessionInfo>>,
    permission_by_id: Mutex<HashMap<RequestId, PendingPermission>>,
    initialized: AtomicBool,
}

impl AcpClient {
    pub fn spawn(launch: &AcpLaunch, policy: PermissionPolicy) -> Result<Self, ClientError> {
        let command = launch.command()?;
        let transport = Transport::spawn(command, launch.config)?;
        Ok(Self {
            transport,
            next_id: AtomicI64::new(1),
            request_lock: Mutex::new(()),
            policy,
            session_policies: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            permission_by_id: Mutex::new(HashMap::new()),
            initialized: AtomicBool::new(false),
        })
    }

    pub fn stderr_snapshot(&self) -> StderrSnapshot {
        self.transport.stderr_snapshot()
    }

    pub fn is_connected(&self) -> bool {
        !self.transport.is_closed()
    }

    pub fn initialize(&self, timeout: Duration) -> Result<(), ClientError> {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "auth": {"terminal": false}
                },
                "clientInfo": {"name": "shishi", "version": env!("CARGO_PKG_VERSION")}
            }),
            timeout,
            &mut |_| {},
        )?;
        let protocol_version = response
            .get("protocolVersion")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                ClientError::Protocol("initialize has no numeric protocolVersion".into())
            })?;
        if protocol_version != 1 {
            return Err(ClientError::Unsupported(format!(
                "ACP protocol version {protocol_version} is not supported (need 1)"
            )));
        }
        let caps = &response["agentCapabilities"];
        let session_caps = caps.get("sessionCapabilities").and_then(Value::as_object);
        let prompt_caps = caps.get("promptCapabilities").and_then(Value::as_object);
        let capabilities = AgentCapabilities {
            resume: session_caps.is_some_and(|object| object.contains_key("resume")),
            close: session_caps.is_some_and(|object| object.contains_key("close")),
            prompt_image: prompt_caps
                .and_then(|object| object.get("image"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        *self.capabilities.lock().unwrap() = Some(capabilities);
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn new_session(
        &self,
        cwd: impl AsRef<Path>,
        mcp_servers: &[McpServer],
        timeout: Duration,
    ) -> Result<SessionInfo, ClientError> {
        self.new_session_with_policy(cwd, mcp_servers, timeout, self.policy)
    }

    pub fn new_session_with_policy(
        &self,
        cwd: impl AsRef<Path>,
        mcp_servers: &[McpServer],
        timeout: Duration,
        policy: PermissionPolicy,
    ) -> Result<SessionInfo, ClientError> {
        self.require_initialized()?;
        let cwd = cwd.as_ref();
        if !cwd.is_absolute() {
            return Err(ClientError::Unsupported(format!(
                "session cwd must be absolute: {}",
                cwd.display()
            )));
        }
        let response = self.request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": mcp_servers_json(mcp_servers)?
            }),
            timeout,
            &mut |_| {},
        )?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Protocol("session/new has no sessionId".into()))?
            .to_string();
        let session = self.remember_session(&session_id, cwd, &response)?;
        self.session_policies
            .lock()
            .unwrap()
            .insert(session_id, policy);
        Ok(session)
    }

    pub fn resume_session(
        &self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        mcp_servers: &[McpServer],
        timeout: Duration,
    ) -> Result<SessionInfo, ClientError> {
        self.require_initialized()?;
        let capabilities = self
            .capabilities
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();
        if !capabilities.resume {
            return Err(ClientError::Unsupported(
                "the ACP server did not advertise sessionCapabilities.resume".into(),
            ));
        }
        let cwd = cwd.as_ref();
        if !cwd.is_absolute() {
            return Err(ClientError::Unsupported(format!(
                "resume cwd must be absolute: {}",
                cwd.display()
            )));
        }
        let response = self.request(
            "session/resume",
            json!({
                "sessionId": session_id,
                "cwd": cwd.to_string_lossy(),
                "mcpServers": mcp_servers_json(mcp_servers)?
            }),
            timeout,
            &mut |_| {},
        )?;
        self.remember_session(session_id, cwd, &response)
    }

    fn remember_session(
        &self,
        session_id: &str,
        cwd: &Path,
        response: &Value,
    ) -> Result<SessionInfo, ClientError> {
        let config_options = parse_config_options(response.get("configOptions"))?;
        let mut session = SessionInfo {
            session_id: session_id.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            model_value: None,
            config_options: Vec::new(),
        };
        session.replace_options(config_options);
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), session.clone());
        Ok(session)
    }

    pub fn close_session(&self, session_id: &str, timeout: Duration) -> Result<(), ClientError> {
        let capabilities = self
            .capabilities
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();
        if !capabilities.close {
            return Err(ClientError::Unsupported(
                "the ACP server did not advertise sessionCapabilities.close".into(),
            ));
        }
        self.request(
            "session/close",
            json!({"sessionId": session_id}),
            timeout,
            &mut |_| {},
        )?;
        self.sessions.lock().unwrap().remove(session_id);
        self.cancel_pending(Some(session_id));
        Ok(())
    }

    pub fn session(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    pub fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<(), ClientError> {
        let response = self.request(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": config_id, "value": value}),
            timeout,
            &mut |_| {},
        )?;
        let config_options = parse_config_options(response.get("configOptions"))?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ClientError::UnknownSession(session_id.to_string()))?;
        session.config_options = config_options;
        if config_id == "model" {
            session.model_value = Some(value.to_string());
        }
        Ok(())
    }

    pub fn select_model(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<(), ClientError> {
        let value = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .ok_or_else(|| ClientError::UnknownSession(session_id.to_string()))?
            .find_model_value(provider, model)
            .ok_or_else(|| ClientError::ModelNotAdvertised {
                provider: provider.to_string(),
                model: model.to_string(),
            })?;
        self.set_config_option(session_id, "model", &value, timeout)
    }

    pub fn select_effort(
        &self,
        session_id: &str,
        effort: &str,
        timeout: Duration,
    ) -> Result<(), ClientError> {
        let value = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .ok_or_else(|| ClientError::UnknownSession(session_id.to_string()))?
            .find_effort_value(effort)
            .ok_or_else(|| ClientError::EffortNotAdvertised(effort.to_string()))?;
        self.set_config_option(session_id, "reasoning_effort", &value, timeout)
    }

    pub fn prompt(
        &self,
        session_id: &str,
        exact_prompt: &str,
        timeout: Duration,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<PromptResult, ClientError> {
        self.prompt_with_images(session_id, exact_prompt, &[], timeout, on_event)
    }

    pub fn prompt_with_images(
        &self,
        session_id: &str,
        exact_prompt: &str,
        images: &[Value],
        timeout: Duration,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<PromptResult, ClientError> {
        if exact_prompt.is_empty() {
            return Err(ClientError::PromptEmpty);
        }
        if exact_prompt.len() > MAX_PROMPT_BYTES {
            return Err(ClientError::PromptTooLarge);
        }
        if !self.sessions.lock().unwrap().contains_key(session_id) {
            return Err(ClientError::UnknownSession(session_id.to_string()));
        }
        let mut content = vec![json!({"type":"text","text":exact_prompt})];
        if !images.is_empty()
            && !self
                .capabilities
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|c| c.prompt_image)
        {
            return Err(ClientError::Protocol(
                "Agent does not support native image input".into(),
            ));
        }
        if images.len() > 2 {
            return Err(ClientError::Protocol("Too many input images".into()));
        }
        for image in images {
            let data = image["data"]
                .as_str()
                .filter(|s| s.len() <= 4 * 1024 * 1024)
                .ok_or_else(|| ClientError::Protocol("Invalid input image".into()))?;
            let mime = image["mimeType"]
                .as_str()
                .filter(|s| matches!(*s, "image/jpeg" | "image/png" | "image/webp" | "image/gif"))
                .ok_or_else(|| ClientError::Protocol("Invalid image type".into()))?;
            content.push(json!({"type":"image","data":data,"mimeType":mime}));
        }
        let response = self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": content
            }),
            timeout,
            on_event,
        )?;
        let stop_reason = response
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Protocol("session/prompt has no stopReason".into()))?
            .to_string();
        let usage = response.get("usage").and_then(TokenUsage::from_value);
        Ok(PromptResult { stop_reason, usage })
    }

    /// Cancel a session without consuming the response stream. This is safe to
    /// call from a UI thread while `prompt` is blocked. The agent must answer
    /// the prompt with `stopReason: "cancelled"`; a timeout still makes the
    /// outcome ambiguous and must never be auto-resubmitted.
    pub fn cancel_session(&self, session_id: &str) {
        let _ = self.transport.send_line(encode_notification(
            "session/cancel",
            &json!({"sessionId": session_id}),
        ));
        self.cancel_pending(Some(session_id));
    }

    /// Authoritative live permissions; callers must not keep a second cache
    /// after cancellation, prompt completion or disconnection.
    pub fn permissions_for(&self, session_id: &str) -> Vec<PendingPermission> {
        self.permission_by_id
            .lock()
            .unwrap()
            .values()
            .filter(|permission| permission.session_id == session_id)
            .cloned()
            .collect()
    }

    pub fn respond_permission(
        &self,
        request_id: &RequestId,
        option_id: &str,
    ) -> Result<(), ClientError> {
        {
            let mut permissions = self.permission_by_id.lock().unwrap();
            let permission = permissions
                .get(request_id)
                .ok_or(ClientError::PermissionNotFound)?;
            if !permission
                .options
                .iter()
                .any(|option| option.option_id == option_id)
            {
                return Err(ClientError::PermissionOptionNotOffered);
            }
            // Claim the request while the offered option is still valid.
            // Completion, cancellation and concurrent replies use this same map.
            permissions.remove(request_id);
        }
        self.transport.send_line(encode_response(
            request_id,
            &json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
        ))?;
        Ok(())
    }

    pub fn shutdown(&self) {
        self.cancel_pending(None);
        self.transport.shutdown();
    }

    pub fn abort(&self) {
        self.transport.abort();
        self.cancel_pending(None);
    }

    fn require_initialized(&self) -> Result<(), ClientError> {
        if !self.initialized.load(Ordering::SeqCst) {
            return Err(ClientError::Protocol(
                "initialize must complete before creating a session".into(),
            ));
        }
        Ok(())
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<Value, ClientError> {
        let _guard: MutexGuard<'_, ()> = self.request_lock.lock().unwrap();
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let deadline = Instant::now() + timeout;
        self.transport
            .send_line(encode_request(&id, method, &params))
            .map_err(|error| ClientError::from(error))?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.on_timeout(&id, method, &params);
                return Err(ClientError::Timeout {
                    method: method.to_string(),
                    ambiguous: true,
                });
            }
            let remaining = deadline.saturating_duration_since(now);
            match self.transport.recv(remaining) {
                Ok(Incoming::Response {
                    id: response_id,
                    result,
                }) => {
                    if response_id != id {
                        // A late response from a previously timed-out request.
                        continue;
                    }
                    if method == "session/prompt" {
                        if let Some(session_id) = params["sessionId"].as_str() {
                            self.cancel_pending(Some(session_id));
                        }
                    }
                    return result.map_err(|error: RpcError| ClientError::Rpc {
                        code: error.code,
                        message: error.message,
                    });
                }
                Ok(Incoming::Notification {
                    method: event_method,
                    params,
                }) => {
                    self.dispatch_notification(&event_method, params, on_event);
                }
                Ok(Incoming::Request {
                    id: request_id,
                    method: request_method,
                    params,
                }) => {
                    self.handle_server_request(request_id, &request_method, params);
                }
                Err(TransportError::Timeout) => {
                    self.on_timeout(&id, method, &params);
                    return Err(ClientError::Timeout {
                        method: method.to_string(),
                        ambiguous: true,
                    });
                }
                Err(TransportError::Closed) => {
                    self.cancel_pending(None);
                    return Err(ClientError::Disconnected {
                        method: method.to_string(),
                        ambiguous: true,
                    });
                }
                Err(error) => {
                    self.cancel_pending(None);
                    return Err(ClientError::from(error));
                }
            }
        }
    }

    fn on_timeout(&self, id: &RequestId, method: &str, params: &Value) {
        let _ = self.transport.send_line(encode_notification(
            "$/cancel_request",
            &json!({"requestId": id.to_value()}),
        ));
        if method == "session/prompt" {
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                self.cancel_session(session_id);
            }
        }
    }

    fn dispatch_notification(
        &self,
        method: &str,
        params: Value,
        on_event: &mut dyn FnMut(AcpEvent),
    ) {
        if method != "session/update" {
            return;
        }
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let update = &params["update"];
        if update["sessionUpdate"] == "config_option_update" {
            let options = parse_config_options(update.get("configOptions")).unwrap_or_default();
            if let Some(session) = self.sessions.lock().unwrap().get_mut(&session_id) {
                session.replace_options(options);
            }
        }
        on_event(AcpEvent {
            session_id,
            update: parse_session_update(update),
        });
    }

    fn handle_server_request(&self, id: RequestId, method: &str, params: Value) {
        if method != "session/request_permission" {
            // Client capabilities advertise no fs/terminal/elicitation. Any
            // unexpected server request is denied, never improvised.
            let _ = self.transport.send_line(encode_error(
                &id,
                -32601,
                &format!("Method not supported by this client: {method}"),
            ));
            return;
        }
        let Some(session_id) = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            let _ = self.transport.send_line(encode_error(
                &id,
                -32602,
                "permission request has no sessionId",
            ));
            return;
        };
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        Some(PermissionOption {
                            option_id: option.get("optionId")?.as_str()?.to_string(),
                            name: option.get("name")?.as_str()?.to_string(),
                            kind: option.get("kind")?.as_str()?.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if options.is_empty() {
            let _ = self.transport.send_line(encode_response(
                &id,
                &json!({"outcome": {"outcome": "cancelled"}}),
            ));
            return;
        }
        let pending = PendingPermission {
            request_id: id.clone(),
            session_id: session_id.clone(),
            options,
            raw: params,
        };
        let policy = self
            .session_policies
            .lock()
            .unwrap()
            .get(&session_id)
            .copied()
            .unwrap_or(self.policy);
        match policy {
            PermissionPolicy::DenyAll => {
                let reject = pending
                    .options
                    .iter()
                    .find(|option| option.kind.starts_with("reject"))
                    .map(|option| option.option_id.clone());
                let response = match reject {
                    Some(option_id) => {
                        json!({"outcome": {"outcome": "selected", "optionId": option_id}})
                    }
                    None => json!({"outcome": {"outcome": "cancelled"}}),
                };
                let _ = self.transport.send_line(encode_response(&id, &response));
            }
            PermissionPolicy::Surface => {
                let mut by_id = self.permission_by_id.lock().unwrap();
                if by_id.len() >= MAX_PENDING_PERMISSIONS {
                    let _ = self.transport.send_line(encode_response(
                        &id,
                        &json!({"outcome": {"outcome": "cancelled"}}),
                    ));
                    return;
                }
                by_id.insert(id, pending);
            }
        }
    }

    fn cancel_pending(&self, session_id: Option<&str>) {
        let ids = {
            let mut ids = Vec::new();
            self.permission_by_id
                .lock()
                .unwrap()
                .retain(|id, permission| {
                    let remove = session_id.is_none_or(|session| permission.session_id == session);
                    if remove {
                        ids.push(id.clone());
                    }
                    !remove
                });
            ids
        };
        for id in ids {
            let _ = self.transport.send_line(encode_response(
                &id,
                &json!({"outcome": {"outcome": "cancelled"}}),
            ));
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn parse_session_update(update: &Value) -> SessionUpdate {
    fn text(content: &Value) -> Option<&str> {
        (content["type"] == "text")
            .then_some(&content["text"])
            .and_then(Value::as_str)
    }
    let string = |field| {
        update
            .get(field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let kind = update["sessionUpdate"].as_str().unwrap_or("unknown");
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let text = text(&update["content"]).unwrap_or_default().to_owned();
            if kind == "agent_message_chunk" {
                SessionUpdate::AgentMessageChunk { text }
            } else {
                SessionUpdate::AgentThoughtChunk { text }
            }
        }
        "tool_call" => SessionUpdate::ToolCall {
            tool_call_id: string("toolCallId").unwrap_or_default(),
            title: string("title"),
            status: string("status"),
            kind: string("kind"),
            raw_input: update["rawInput"].clone(),
        },
        "tool_call_update" => {
            let mut preview = String::new();
            for value in update["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| text(&item["content"]))
            {
                if preview.len() >= MAX_TOOL_TEXT {
                    break;
                }
                preview.extend(value.chars().take(MAX_TOOL_TEXT - preview.len()));
            }
            SessionUpdate::ToolCallUpdate {
                tool_call_id: string("toolCallId").unwrap_or_default(),
                status: string("status"),
                text: preview,
            }
        }
        _ => SessionUpdate::Other,
    }
}

fn parse_config_options(value: Option<&Value>) -> Result<Vec<ConfigOption>, ClientError> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut options = Vec::with_capacity(values.len());
    for value in values {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Protocol("config option has no id".into()))?;
        let current_value = value
            .get("currentValue")
            .and_then(|current| match current {
                Value::String(text) => Some(text.clone()),
                Value::Bool(flag) => Some(flag.to_string()),
                _ => None,
            })
            .unwrap_or_default();
        let mut choices = Vec::new();
        if let Some(raw_options) = value.get("options").and_then(Value::as_array) {
            for choice in raw_options {
                if let Some(grouped) = choice.get("options").and_then(Value::as_array) {
                    let group = choice
                        .get("name")
                        .or_else(|| choice.get("group"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    for grouped_choice in grouped {
                        push_choice(&mut choices, grouped_choice, group.clone());
                    }
                } else {
                    push_choice(&mut choices, choice, None);
                }
            }
        }
        options.push(ConfigOption {
            id: id.to_string(),
            current_value,
            choices,
        });
    }
    Ok(options)
}

fn push_choice(choices: &mut Vec<ConfigChoice>, value: &Value, group: Option<String>) {
    let Some(choice_value) = value.get("value").and_then(Value::as_str) else {
        return;
    };
    choices.push(ConfigChoice {
        value: choice_value.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(choice_value)
            .to_string(),
        group,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_chunks_are_complete_even_when_larger_than_a_tool_preview() {
        let text = "完整文本🙂".repeat(MAX_TOOL_TEXT);
        for kind in ["agent_message_chunk", "agent_thought_chunk"] {
            let update = parse_session_update(
                &json!({"sessionUpdate":kind,"content":{"type":"text","text":text}}),
            );
            match update {
                SessionUpdate::AgentMessageChunk { text: actual }
                | SessionUpdate::AgentThoughtChunk { text: actual } => assert_eq!(actual, text),
                _ => panic!("expected text chunk"),
            }
        }
    }
}
