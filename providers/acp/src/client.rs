//! ACP v1 client: newline-delimited JSON-RPC 2.0 over a child's stdio, one
//! compact frame per line (the dsh-acp bridge's `ndJsonStream`). Shapes follow
//! `@deepseek-ai/dsh-acp` and the bundled `@agentclientprotocol/sdk`
//! (`PROTOCOL_VERSION = 1`). Assistant text is never parsed as a result.

use crate::process::{spawn_in_job, JobGuard};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct AcpLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

impl AcpLaunch {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), args: vec![], cwd: None, env: vec![] }
    }

    pub fn arg(self, arg: impl Into<OsString>) -> Self {
        self.args([arg])
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
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

    pub fn command(&self) -> Result<Command, ClientError> {
        let mut command = launcher(&self.program, &self.args)?;
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        Ok(command)
    }
}

/// `cmd`/`bat` launchers run through `cmd.exe`, every value expanded exactly
/// once from the environment and quoted (`/V:OFF` keeps `!` literal), so no
/// value can become a shell operator.
fn launcher(program: &Path, args: &[OsString]) -> Result<Command, ClientError> {
    let ext = program.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    if !matches!(ext.as_deref(), Some("cmd" | "bat")) {
        let mut command = Command::new(program);
        command.args(args);
        return Ok(command);
    }
    #[cfg(not(windows))]
    return Err(ClientError::Unsupported("cmd/bat launchers are Windows-only".into()));
    #[cfg(windows)]
    {
        let values = std::iter::once(program.as_os_str()).chain(args.iter().map(|a| a.as_os_str()));
        if values.clone().any(|v| v.to_string_lossy().contains(['"', '\r', '\n', '\0'])) {
            return Err(ClientError::Unsupported(
                "launcher values may not contain quotes or control characters".into(),
            ));
        }
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/Q", "/V:OFF", "/S", "/C"]);
        let mut inner = Vec::new();
        for (index, value) in values.enumerate() {
            let key = format!("SHISHI_ACP_ARG_{index}");
            command.env(&key, value);
            inner.push(format!("\"%{key}%\""));
        }
        std::os::windows::process::CommandExt::raw_arg(&mut command, format!("\"{}\"", inner.join(" ")));
        Ok(command)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Analysis: answer every permission request with a reject option.
    DenyAll,
    /// Handling: hold requests until the caller explicitly picks an option.
    Surface,
}

#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
}

impl McpServer {
    pub fn stdio(name: &str, command: impl Into<PathBuf>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { name: name.into(), command: command.into(), args: args.into_iter().map(Into::into).collect() }
    }

    fn to_json(&self) -> Value {
        json!({"name": self.name, "command": self.command.to_string_lossy(), "args": self.args, "env": []})
    }
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
    pub config_options: Vec<ConfigOption>,
    pub model_value: Option<String>,
}

impl SessionInfo {
    fn replace_options(&mut self, options: Vec<ConfigOption>) {
        self.config_options = options;
        self.model_value = self.model_option().map(|o| o.current_value.clone());
    }

    fn option(&self, id: &str) -> Option<&ConfigOption> {
        self.config_options.iter().find(|o| o.id == id)
    }

    pub fn model_option(&self) -> Option<&ConfigOption> {
        self.option("model")
    }

    pub fn effort_option(&self) -> Option<&ConfigOption> {
        self.option("reasoning_effort")
    }

    /// Model values are opaque; the bridge emits `JSON.stringify([provider, model])`.
    pub fn find_model_value(&self, provider: &str, model: &str) -> Option<String> {
        let wanted = (provider.to_string(), model.to_string());
        let choices = &self.model_option()?.choices;
        choices.iter().find(|c| serde_json::from_str(&c.value).ok() == Some(wanted.clone())).map(|c| c.value.clone())
    }

    pub fn find_effort_value(&self, effort: &str) -> Option<String> {
        self.effort_option()?.choices.iter().find(|c| c.value == effort).map(|c| c.value.clone())
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
    /// Missing counters mean unknown usage, never zero.
    pub fn from_value(value: &Value) -> Option<Self> {
        let (input, output) = (value["inputTokens"].as_u64()?, value["outputTokens"].as_u64()?);
        Some(Self {
            total_tokens: value["totalTokens"].as_u64().or_else(|| input.checked_add(output))?,
            input_tokens: input,
            output_tokens: output,
            cached_read_tokens: value["cachedReadTokens"].as_u64(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum SessionUpdate {
    AgentMessageChunk { text: String },
    AgentThoughtChunk { text: String },
    ToolCall { tool_call_id: String, title: Option<String>, status: Option<String> },
    ToolCallUpdate { tool_call_id: String, status: Option<String> },
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
    pub request_id: Value,
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
    #[error("ACP error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("ACP protocol failure: {0}")]
    Protocol(String),
    #[error("ACP capability failure: {0}")]
    Unsupported(String),
    #[error("unknown ACP session: {0}")]
    UnknownSession(String),
    #[error("model {0}/{1} is not advertised by the ACP session; refusing to use a default")]
    ModelNotAdvertised(String, String),
    #[error("reasoning effort {0:?} is not advertised by the ACP session")]
    EffortNotAdvertised(String),
    /// The request may still have run; never resubmit automatically.
    #[error("{0} timed out; its outcome is unknown")]
    Timeout(String),
    #[error("{0} lost the ACP process; its outcome is unknown")]
    Disconnected(String),
    #[error("no pending permission with that id")]
    PermissionNotFound,
}

pub struct AcpClient {
    child: Mutex<Child>,
    job: JobGuard,
    stdin: Mutex<Option<ChildStdin>>,
    /// Incoming frames; holding the lock serializes requests.
    frames: Mutex<Receiver<Result<Value, String>>>,
    closed: Arc<AtomicBool>,
    shut: AtomicBool,
    policy: PermissionPolicy,
    next_id: AtomicI64,
    capabilities: Mutex<Value>,
    sessions: Mutex<HashMap<String, SessionInfo>>,
    permissions: Mutex<HashMap<String, PendingPermission>>,
}

impl AcpClient {
    pub fn spawn(launch: &AcpLaunch, policy: PermissionPolicy) -> Result<Self, ClientError> {
        let mut command = launch.command()?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let (mut child, job) = spawn_in_job(&mut command).map_err(ClientError::Unsupported)?;
        let (tx, rx) = channel();
        let closed = Arc::new(AtomicBool::new(false));
        let (stdout, reader_closed) = (child.stdout.take().unwrap(), closed.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let frame = line.map_err(|e| e.to_string()).and_then(|l| {
                    serde_json::from_slice::<Value>(l.strip_suffix(b"\r").unwrap_or(&l))
                        .map_err(|e| format!("frame is not JSON: {e}"))
                });
                let failed = frame.is_err();
                if tx.send(frame).is_err() || failed {
                    break;
                }
            }
            reader_closed.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            stdin: Mutex::new(child.stdin.take()),
            child: Mutex::new(child),
            job,
            frames: Mutex::new(rx),
            closed,
            shut: AtomicBool::new(false),
            policy,
            next_id: AtomicI64::new(1),
            capabilities: Mutex::new(Value::Null),
            sessions: Mutex::new(HashMap::new()),
            permissions: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    pub fn initialize(&self, timeout: Duration) -> Result<(), ClientError> {
        let response = self.request("initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "auth": {"terminal": false}},
            "clientInfo": {"name": "shishi", "version": env!("CARGO_PKG_VERSION")}
        }), timeout, &mut |_| {})?;
        match response["protocolVersion"].as_i64() {
            Some(1) => {}
            Some(v) => {
                return Err(ClientError::Unsupported(format!("ACP protocol version {v} is not supported (need 1)")))
            }
            None => return Err(ClientError::Protocol("initialize has no numeric protocolVersion".into())),
        }
        *self.capabilities.lock().unwrap() = response["agentCapabilities"].clone();
        Ok(())
    }

    pub fn new_session(
        &self,
        cwd: impl AsRef<Path>,
        servers: &[McpServer],
        timeout: Duration,
    ) -> Result<SessionInfo, ClientError> {
        let params = json!({"cwd": cwd.as_ref().to_string_lossy(), "mcpServers": servers.iter().map(McpServer::to_json).collect::<Vec<_>>()});
        let response = self.request("session/new", params, timeout, &mut |_| {})?;
        let id = response["sessionId"]
            .as_str()
            .ok_or_else(|| ClientError::Protocol("session/new has no sessionId".into()))?;
        Ok(self.remember(id, &response))
    }

    pub fn resume_session(
        &self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        servers: &[McpServer],
        timeout: Duration,
    ) -> Result<SessionInfo, ClientError> {
        self.require("resume")?;
        let params = json!({"sessionId": session_id, "cwd": cwd.as_ref().to_string_lossy(),
            "mcpServers": servers.iter().map(McpServer::to_json).collect::<Vec<_>>()});
        let response = self.request("session/resume", params, timeout, &mut |_| {})?;
        Ok(self.remember(session_id, &response))
    }

    pub fn close_session(&self, session_id: &str, timeout: Duration) -> Result<(), ClientError> {
        self.require("close")?;
        self.request("session/close", json!({"sessionId": session_id}), timeout, &mut |_| {})?;
        self.sessions.lock().unwrap().remove(session_id);
        self.cancel_pending(Some(session_id));
        Ok(())
    }

    fn require(&self, capability: &str) -> Result<(), ClientError> {
        let advertised = self.capabilities.lock().unwrap()["sessionCapabilities"].get(capability).is_some();
        advertised.then_some(()).ok_or_else(|| {
            ClientError::Unsupported(format!("the ACP server did not advertise sessionCapabilities.{capability}"))
        })
    }

    fn remember(&self, session_id: &str, response: &Value) -> SessionInfo {
        let mut session = SessionInfo { session_id: session_id.into(), config_options: vec![], model_value: None };
        session.replace_options(config_options(&response["configOptions"]));
        self.sessions.lock().unwrap().insert(session_id.into(), session.clone());
        session
    }

    pub fn session(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    fn set_option(&self, session_id: &str, id: &str, value: String, timeout: Duration) -> Result<(), ClientError> {
        let params = json!({"sessionId": session_id, "configId": id, "value": value});
        let response = self.request("session/set_config_option", params, timeout, &mut |_| {})?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(session_id).ok_or_else(|| ClientError::UnknownSession(session_id.into()))?;
        session.config_options = config_options(&response["configOptions"]);
        if id == "model" {
            session.model_value = Some(value);
        }
        Ok(())
    }

    fn find(
        &self,
        session_id: &str,
        pick: impl FnOnce(&SessionInfo) -> Option<String>,
    ) -> Result<Option<String>, ClientError> {
        let sessions = self.sessions.lock().unwrap();
        Ok(pick(sessions.get(session_id).ok_or_else(|| ClientError::UnknownSession(session_id.into()))?))
    }

    pub fn select_model(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<(), ClientError> {
        let value = self
            .find(session_id, |s| s.find_model_value(provider, model))?
            .ok_or_else(|| ClientError::ModelNotAdvertised(provider.into(), model.into()))?;
        self.set_option(session_id, "model", value, timeout)
    }

    pub fn select_effort(&self, session_id: &str, effort: &str, timeout: Duration) -> Result<(), ClientError> {
        let value = self
            .find(session_id, |s| s.find_effort_value(effort))?
            .ok_or_else(|| ClientError::EffortNotAdvertised(effort.into()))?;
        self.set_option(session_id, "reasoning_effort", value, timeout)
    }

    /// Send the caller's exact text (plus native images) as one prompt.
    pub fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[Value],
        timeout: Duration,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<PromptResult, ClientError> {
        if !images.is_empty() && self.capabilities.lock().unwrap()["promptCapabilities"]["image"] != true {
            return Err(ClientError::Unsupported("Agent does not support native image input".into()));
        }
        let mut content = vec![json!({"type": "text", "text": text})];
        content.extend(images.iter().map(|i| json!({"type": "image", "data": i["data"], "mimeType": i["mimeType"]})));
        let response =
            self.request("session/prompt", json!({"sessionId": session_id, "prompt": content}), timeout, on_event)?;
        let stop_reason = response["stopReason"]
            .as_str()
            .ok_or_else(|| ClientError::Protocol("session/prompt has no stopReason".into()))?;
        Ok(PromptResult { stop_reason: stop_reason.into(), usage: TokenUsage::from_value(&response["usage"]) })
    }

    /// Safe while `prompt` blocks on another thread; the agent then answers
    /// that prompt with `stopReason: "cancelled"`.
    pub fn cancel_session(&self, session_id: &str) {
        let _ = self.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}));
        self.cancel_pending(Some(session_id));
    }

    /// The live permission requests of a session; prompt completion,
    /// cancellation and disconnection clear them.
    pub fn permissions_for(&self, session_id: &str) -> Vec<PendingPermission> {
        self.permissions.lock().unwrap().values().filter(|p| p.session_id == session_id).cloned().collect()
    }

    pub fn respond_permission(&self, request_id: &Value, option_id: &str) -> Result<(), ClientError> {
        {
            // Claim the request under the same lock completion and cancellation use.
            let mut permissions = self.permissions.lock().unwrap();
            let pending = permissions.get(&request_id.to_string()).ok_or(ClientError::PermissionNotFound)?;
            if !pending.options.iter().any(|o| o.option_id == option_id) {
                return Err(ClientError::Unsupported("permission option was not offered by the agent".into()));
            }
            permissions.remove(&request_id.to_string());
        }
        self.reply(request_id, json!({"outcome": {"outcome": "selected", "optionId": option_id}}))
    }

    pub fn shutdown(&self) {
        self.cancel_pending(None);
        self.close(Duration::from_secs(2));
    }

    /// Cancellation and expired deadlines skip the graceful exit.
    pub fn abort(&self) {
        self.close(Duration::ZERO);
        self.cancel_pending(None);
    }

    fn close(&self, grace: Duration) {
        if self.shut.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        if grace.is_zero() {
            // Unblocks a writer stuck on a full pipe before taking its lock.
            self.job.terminate();
            let _ = self.child.lock().unwrap().kill();
        }
        self.stdin.lock().unwrap().take();
        let mut child = self.child.lock().unwrap();
        let deadline = Instant::now() + grace;
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.job.terminate();
        let _ = child.kill();
        let _ = child.wait();
    }

    fn send(&self, frame: Value) -> Result<(), ClientError> {
        let mut stdin = self.stdin.lock().unwrap();
        let pipe = stdin.as_mut().ok_or_else(|| ClientError::Disconnected("write".into()))?;
        let mut line = frame.to_string();
        line.push('\n');
        pipe.write_all(line.as_bytes())
            .and_then(|_| pipe.flush())
            .map_err(|_| ClientError::Disconnected("write".into()))
    }

    fn reply(&self, id: &Value, result: Value) -> Result<(), ClientError> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        on_event: &mut dyn FnMut(AcpEvent),
    ) -> Result<Value, ClientError> {
        let frames = self.frames.lock().unwrap();
        let id = json!(self.next_id.fetch_add(1, Ordering::SeqCst));
        let session = params["sessionId"].as_str().map(str::to_owned);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .map_err(|_| ClientError::Disconnected(method.into()))?;
        let deadline = Instant::now() + timeout;
        loop {
            let frame = match frames.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(frame)) if frame["jsonrpc"] == "2.0" => frame,
                Ok(Ok(_)) => return Err(ClientError::Protocol("frame is not JSON-RPC 2.0".into())),
                Ok(Err(error)) => {
                    self.cancel_pending(None);
                    return Err(ClientError::Protocol(error));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ =
                        self.send(json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}}));
                    if let (Some(session), "session/prompt") = (&session, method) {
                        self.cancel_session(session);
                    }
                    return Err(ClientError::Timeout(method.into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.cancel_pending(None);
                    return Err(ClientError::Disconnected(method.into()));
                }
            };
            match (frame["method"].as_str(), frame.get("id").filter(|id| !id.is_null())) {
                (Some("session/update"), None) => self.update(&frame["params"], on_event),
                (Some(server_method), Some(server_id)) => {
                    self.server_request(server_id, server_method, &frame["params"])
                }
                // Late responses belong to requests that already timed out.
                (None, Some(response_id)) if *response_id == id => {
                    if let (Some(session), "session/prompt") = (&session, method) {
                        self.cancel_pending(Some(session));
                    }
                    return match frame.get("error") {
                        Some(e) => Err(ClientError::Rpc {
                            code: e["code"].as_i64().unwrap_or(-32603),
                            message: e["message"].as_str().unwrap_or("JSON-RPC error").into(),
                        }),
                        None => Ok(frame["result"].clone()),
                    };
                }
                _ => {}
            }
        }
    }

    fn update(&self, params: &Value, on_event: &mut dyn FnMut(AcpEvent)) {
        let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
        let update = &params["update"];
        let text = |content: &Value| {
            content["text"].as_str().filter(|_| content["type"] == "text").unwrap_or_default().to_string()
        };
        let string = |field: &str| update[field].as_str().map(str::to_owned);
        let update = match update["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => SessionUpdate::AgentMessageChunk { text: text(&update["content"]) },
            "agent_thought_chunk" => SessionUpdate::AgentThoughtChunk { text: text(&update["content"]) },
            "tool_call" => SessionUpdate::ToolCall {
                tool_call_id: string("toolCallId").unwrap_or_default(),
                title: string("title"),
                status: string("status"),
            },
            "tool_call_update" => SessionUpdate::ToolCallUpdate {
                tool_call_id: string("toolCallId").unwrap_or_default(),
                status: string("status"),
            },
            "config_option_update" => {
                if let Some(session) = self.sessions.lock().unwrap().get_mut(&session_id) {
                    session.replace_options(config_options(&update["configOptions"]));
                }
                SessionUpdate::Other
            }
            _ => SessionUpdate::Other,
        };
        on_event(AcpEvent { session_id, update });
    }

    /// The client advertises no fs/terminal capability: anything but a
    /// permission request is refused.
    fn server_request(&self, id: &Value, method: &str, params: &Value) {
        if method != "session/request_permission" {
            let message = format!("Method not supported by this client: {method}");
            let _ = self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message}}));
            return;
        }
        let options = params["options"].as_array().into_iter().flatten().filter_map(|o| {
            Some(PermissionOption {
                option_id: o["optionId"].as_str()?.into(),
                name: o["name"].as_str()?.into(),
                kind: o["kind"].as_str()?.into(),
            })
        });
        let options = options.collect::<Vec<_>>();
        let reject = options.iter().find(|o| o.kind.starts_with("reject")).map(|o| o.option_id.clone());
        match (params["sessionId"].as_str(), self.policy) {
            (Some(session), PermissionPolicy::Surface) if !options.is_empty() => {
                let pending = PendingPermission {
                    request_id: id.clone(),
                    session_id: session.into(),
                    options,
                    raw: params.clone(),
                };
                self.permissions.lock().unwrap().insert(id.to_string(), pending);
            }
            (Some(_), PermissionPolicy::DenyAll) if reject.is_some() => {
                let _ = self.reply(id, json!({"outcome": {"outcome": "selected", "optionId": reject}}));
            }
            _ => drop(self.reply(id, json!({"outcome": {"outcome": "cancelled"}}))),
        }
    }

    fn cancel_pending(&self, session_id: Option<&str>) {
        let mut cancelled = Vec::new();
        self.permissions.lock().unwrap().retain(|_, p| {
            let keep = session_id.is_some_and(|s| s != p.session_id);
            if !keep {
                cancelled.push(p.request_id.clone());
            }
            keep
        });
        for id in cancelled {
            let _ = self.reply(&id, json!({"outcome": {"outcome": "cancelled"}}));
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Flatten config options; grouped choices carry their group name.
fn config_options(value: &Value) -> Vec<ConfigOption> {
    let choice = |c: &Value, group: Option<&str>| {
        let value = c["value"].as_str()?;
        Some(ConfigChoice {
            value: value.into(),
            name: c["name"].as_str().unwrap_or(value).into(),
            group: group.map(Into::into),
        })
    };
    let options = value.as_array().into_iter().flatten().filter_map(|o| {
        let choices = o["options"].as_array().into_iter().flatten().flat_map(|c| match c["options"].as_array() {
            Some(grouped) => {
                grouped.iter().filter_map(|g| choice(g, c["name"].as_str().or(c["group"].as_str()))).collect()
            }
            None => choice(c, None).into_iter().collect::<Vec<_>>(),
        });
        let current = match &o["currentValue"] {
            Value::Bool(flag) => flag.to_string(),
            current => current.as_str().unwrap_or_default().into(),
        };
        Some(ConfigOption { id: o["id"].as_str()?.into(), current_value: current, choices: choices.collect() })
    });
    options.collect()
}
