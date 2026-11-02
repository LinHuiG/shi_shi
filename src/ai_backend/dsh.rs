use super::*;
use serde_json::json;
use shishi_acp::{AcpClient, AcpEvent, DshLaunch, McpServer, PermissionPolicy, SessionUpdate};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{atomic::Ordering, Mutex},
    time::{Duration, Instant},
};
const RPC: Duration = Duration::from_secs(20);
/// Remove only the per-request temporary directory this process created, on
/// every exit path (including readiness and spawn failures). It never touches
/// the shared analysis workdir or the user's DSH home.
struct JobCleanup(PathBuf);
impl Drop for JobCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub struct Dsh;
pub fn workspace_plugin(c: &Connection, install: bool, confirmed: bool) -> Result<Value, String> {
    if c.driver != "dsh" || !Path::new(&c.executable).is_file() {
        return Err("请先选择有效的 DSH 启动器".into());
    }
    if install {
        if !confirmed {
            return Err("请点击安装按钮确认安装 SS 会话插件".into());
        }
        shishi_acp::workspace::install(&home()?)
    } else {
        shishi_acp::workspace::status(&home()?)
    }
}
pub fn initialize_acp(c: &Connection, root: &Path, confirmed: bool) -> Result<Value, String> {
    if !confirmed || c.driver != "dsh" {
        return Err("请明确确认初始化 DSH 内置 ACP 环境".into());
    }
    let job = crate::storage::data_dir(&root)
        .join("tmp")
        .join(crate::model::id());
    fs::create_dir_all(&job).map_err(|e| e.to_string())?;
    let _cleanup = JobCleanup(job.clone());
    let profile = connection_profile(&job.join("profile"))?;
    let config = shishi_acp::restricted::ReadinessConfig {
        dsh_cmd: PathBuf::from(&c.executable),
        dsh_home: home()?,
        agents_home: Some(profile.agents_dir),
        patch: profile.patch,
        workdir: Some(root.to_path_buf()),
    };
    let ready = shishi_acp::restricted::initialize_acp(&config, confirmed)
        .map_err(|e| e.guidance(&config))?;
    Ok(json!({"ready":true,"runtime_version":ready.runtime_version}))
}
fn home() -> Result<PathBuf, String> {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".dsh")))
        .ok_or("无法定位 Harness 用户目录".into())
}
fn usage(value: Option<shishi_acp::TokenUsage>) -> Value {
    value.map(|u|json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,"cached_input_tokens":u.cached_read_tokens,"total_tokens":u.total_tokens})).unwrap_or(Value::Null)
}
/// Model catalog values are opaque ACP strings. Accept either the exact opaque
/// value returned by model discovery or a plain model id routed to the default
/// account provider, and never invent a value for an unadvertised route.
fn model_route(value: &str) -> (String, String) {
    serde_json::from_str(value).unwrap_or_else(|_| ("deepseek-account".into(), value.into()))
}
fn selection(client: &AcpClient, session: &str, c: &Config) -> Result<(), String> {
    let (provider, model) = model_route(&c.model);
    client
        .select_model(session, &provider, &model, RPC)
        .map_err(|e| e.to_string())?;
    if !c.reasoning_effort.is_empty() {
        client
            .select_effort(session, &c.reasoning_effort, RPC)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn connection_profile(dir: &Path) -> Result<shishi_acp::MaterializedProfile, String> {
    let profile = shishi_acp::materialize_profile(dir).map_err(|e| e.to_string())?;
    shishi_acp::providers::inherit_desktop_providers(&profile, &home()?)?;
    Ok(profile)
}
fn console_safe_launch(
    launch: shishi_acp::AcpLaunch,
    dir: &Path,
) -> Result<shishi_acp::AcpLaunch, String> {
    let hook = dir.join("stdio-console.cjs");
    fs::write(
        &hook,
        include_str!("../../providers/acp/assets/stdio-console.cjs"),
    )
    .map_err(|e| e.to_string())?;
    // NODE_OPTIONS tokenization consumes backslashes inside quoted values.
    // Use Node's supported forward-slash Windows spelling, including verbatim paths.
    let path = hook
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    if path.contains(['"', '\r', '\n']) {
        return Err("DSH 启动目录含不支持的字符".into());
    }
    let options = std::env::var("NODE_OPTIONS").unwrap_or_default();
    Ok(launch
        .env("SHISHI_DSH_ORIGINAL_NODE_OPTIONS", &options)
        .env("NODE_OPTIONS", format!("{options} --require=\"{path}\"")))
}
fn handling_client(c: &Connection, cwd: &Path) -> Result<(AcpClient, JobCleanup), String> {
    let dir = std::env::temp_dir().join(format!("shishi-dsh-{}", crate::model::id()));
    let cleanup = JobCleanup(dir.clone());
    let profile =
        shishi_acp::materialize_profile(&dir.join("profile")).map_err(|e| e.to_string())?;
    let dsh_home = home()?;
    let patch = shishi_acp::providers::inherit_desktop_providers(&profile, &dsh_home)?;
    let mut spec = DshLaunch::new(&c.executable, PathBuf::new(), cwd).handling();
    spec.patch = patch;
    spec.dsh_home = Some(dsh_home);
    let launch = console_safe_launch(spec.acp_launch(), &dir)?;
    let client = AcpClient::spawn(&launch, PermissionPolicy::Surface).map_err(|e| e.to_string())?;
    client.initialize(RPC).map_err(|e|format!("ACP initialize 失败：{e}；标准输出必须仅含 JSON-RPC。SHISHI_ACP_DIAGNOSTICS=1 可启用本地失败帧诊断"))?;
    Ok((client, cleanup))
}
fn remaining(cancel: &AtomicBool, deadline: Instant) -> Result<Duration, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err(crate::ai_control::CANCELLED.into());
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "AI 总时限已到".into())
}

fn with_deadline<T>(
    client: &AcpClient,
    cancel: &AtomicBool,
    deadline: Instant,
    run: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    remaining(cancel, deadline).inspect_err(|_| client.abort())?;
    let done = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    client.abort();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let result = run();
        done.store(true, Ordering::SeqCst);
        result
    });
    // The RPC may time out before the watcher wakes; teardown must still skip
    // the normal-exit grace period in that race.
    remaining(cancel, deadline).inspect_err(|_| client.abort())?;
    result
}

/// One logical tool loop owns one restricted process. Its loaded profile is
/// fixed (HMR is disabled); each round still gets a fresh session and ticket.
/// Field order ensures the process is reaped before its profile is removed.
pub(super) struct AnalysisProcess {
    client: AcpClient,
    cleanup: JobCleanup,
    workdir: PathBuf,
    dsh_home: PathBuf,
}

impl AnalysisProcess {
    pub(super) fn prepare(
        c: &Connection,
        r: &AnalysisRequest<'_>,
        deadline: Instant,
    ) -> Result<Self, String> {
        remaining(r.cancel, deadline)?;
        Self::prepare_at(c, r, deadline, home()?)
    }

    fn prepare_at(
        c: &Connection,
        r: &AnalysisRequest<'_>,
        deadline: Instant,
        dsh_home: PathBuf,
    ) -> Result<Self, String> {
        remaining(r.cancel, deadline)?;
        Dsh.validate(c, &r.config.model, &r.config.reasoning_effort)?;
        let job = crate::storage::data_dir(&r.root)
            .join("ai-work")
            .join(crate::model::id());
        fs::create_dir_all(&job).map_err(|e| e.to_string())?;
        let cleanup = JobCleanup(job.clone());
        let workdir = crate::ai::analysis_workdir(r.root, r.config)?;
        fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;
        let profile =
            shishi_acp::materialize_profile(&job.join("profile")).map_err(|e| e.to_string())?;
        shishi_acp::providers::inherit_desktop_providers(&profile, &dsh_home)?;
        let readiness_config = shishi_acp::restricted::ReadinessConfig {
            dsh_cmd: PathBuf::from(&c.executable),
            dsh_home: dsh_home.clone(),
            agents_home: Some(profile.agents_dir.clone()),
            patch: profile.patch.clone(),
            workdir: Some(workdir.clone()),
        };
        shishi_acp::restricted::check_readiness_until(&readiness_config, deadline, r.cancel)
            .map_err(|e| {
                remaining(r.cancel, deadline)
                    .err()
                    .unwrap_or_else(|| e.guidance(&readiness_config))
            })?;
        remaining(r.cancel, deadline)?;
        let launch = profile
            .analysis_launch(&c.executable, &workdir, &dsh_home)
            .acp_launch();
        let launch = console_safe_launch(launch, &job)?;
        let client =
            AcpClient::spawn(&launch, PermissionPolicy::DenyAll).map_err(|e| e.to_string())?;
        with_deadline(&client, r.cancel, deadline, || {
            client.initialize(RPC.min(remaining(r.cancel, deadline)?))
                .map_err(|e| format!("ACP initialize 失败：{e}；标准输出必须仅含 JSON-RPC。SHISHI_ACP_DIAGNOSTICS=1 可启用本地失败帧诊断"))?;
            Ok(())
        })?;
        Ok(Self {
            client,
            cleanup,
            workdir,
            dsh_home,
        })
    }

    pub(super) fn analyze(
        &self,
        r: AnalysisRequest<'_>,
        deadline: Instant,
        event: &mut dyn FnMut(AnalysisEvent),
    ) -> Result<(Value, Value), String> {
        remaining(r.cancel, deadline)?;
        let job = self.cleanup.0.join(crate::model::id());
        fs::create_dir_all(&job).map_err(|e| e.to_string())?;
        let _cleanup = JobCleanup(job.clone());
        let (provider, model) = model_route(&r.config.model);
        let spec = shishi_acp::analysis::AnalysisSpec {
            request_id: crate::model::id(),
            kind: r.kind.into(),
            schema: r.schema.clone(),
            prompt: r.prompt.into(),
            images: r.payload["_model_images"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            workdir: self.workdir.clone(),
            app_exe: std::env::current_exe().map_err(|e| e.to_string())?,
            model: Some((provider, model)),
            effort: (!r.config.reasoning_effort.is_empty())
                .then(|| r.config.reasoning_effort.clone()),
        };
        let prepared =
            shishi_acp::analysis::prepare(&job.join("submit"), &spec).map_err(|e| e.to_string())?;
        let result = with_deadline(&self.client, r.cancel, deadline, || {
            let mut announced = false;
            let accepted = shishi_acp::analysis::run(
                &self.client,
                &spec,
                &prepared,
                remaining(r.cancel, deadline)?,
                &mut |e| {
                    let AcpEvent { session_id, update } = e;
                    if !announced {
                        event(AnalysisEvent::SessionStarted(session_id));
                        announced = true;
                    }
                    let progress = match update {
                        SessionUpdate::AgentMessageChunk { text } => Some(("content", text)),
                        SessionUpdate::AgentThoughtChunk { text } => Some(("reasoning", text)),
                        SessionUpdate::ToolCall { title, .. } => {
                            Some(("tool", title.unwrap_or_else(|| "读取上下文".into())))
                        }
                        _ => None,
                    };
                    if let Some((kind, text)) = progress {
                        if r.config.retain_ai_traces {
                            crate::ai::append_trace_event(
                                r.trace,
                                &json!({"type":kind,"text":text}),
                            );
                        }
                        event(AnalysisEvent::Stream {
                            kind: kind.into(),
                            text,
                            append: kind != "tool",
                        });
                    }
                },
            )
            .map_err(|e| error_message(&e.to_string()))?;
            if !accepted.session_closed {
                // Some ACP versions cannot close a session. Keep the validated
                // result, but retire this process before a later round starts.
                self.client.shutdown();
            }
            event(AnalysisEvent::Stream {
                kind: "phase".into(),
                text: "校验结果".into(),
                append: false,
            });
            if !announced {
                event(AnalysisEvent::SessionStarted(accepted.session_id.clone()));
            }
            let tokens = usage(accepted.prompt.usage);
            event(AnalysisEvent::Usage(tokens.clone()));
            if let Err(error) = shishi_acp::workspace::publish(&self.dsh_home, &accepted.session_id)
            {
                event(AnalysisEvent::VisibilityIssue {
                    thread_id: accepted.session_id.clone(),
                    error,
                });
            }
            let value = accepted.value;
            if r.config.retain_ai_traces {
                if let Err(error) = fs::write(r.trace.join("output.txt"), value.to_string()) {
                    event(AnalysisEvent::Stream {
                        kind: "diagnostic".into(),
                        text: format!("分析结果已取得，跟踪文件保存失败：{error}"),
                        append: false,
                    });
                }
            }
            Ok((value, tokens))
        });
        if result.is_err() {
            self.client.shutdown();
        }
        result
    }

    pub(super) fn is_connected(&self) -> bool {
        self.client.is_connected()
    }
}

impl Backend for Dsh {
    fn models(&self, c: &Connection, root: &Path) -> Result<Value, String> {
        let readiness = self
            .status(c, root)
            .map_err(|e| format!("就绪检查失败：{e}"))?;
        if readiness["ready"] != true {
            return Err(readiness["status"]
                .as_str()
                .unwrap_or("Harness 未就绪")
                .into());
        }
        let (client, _profile) = handling_client(c, root)?;
        let session = client
            .new_session(root, &[], RPC)
            .map_err(|e| format!("ACP session/new 失败：{e}"))?;
        let efforts = session
            .effort_option()
            .map(|o| {
                o.choices
                    .iter()
                    .map(|m| json!({"reasoningEffort":m.value,"description":m.name}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let models=session.model_option().map(|o|o.choices.iter().map(|m|json!({"model":m.value,"displayName":m.name,"providerName":m.group,"provider":serde_json::from_str::<Vec<String>>(&m.value).ok().and_then(|r|r.first().cloned()),"supportedReasoningEfforts":efforts})).collect::<Vec<_>>()).unwrap_or_default();
        let current = session.current_model_route();
        client.shutdown();
        Ok(
            json!({"models":models,"efforts":efforts,"current":current.map(|(p,m)|json!({"provider":p,"model":m}))}),
        )
    }
    fn validate(&self, c: &Connection, model: &str, _: &str) -> Result<(), String> {
        if !Path::new(&c.executable).is_file() {
            return Err("未找到桌面 Harness 启动器，请选择新版 dsh.cmd".into());
        }
        if model.is_empty() {
            return Err("请填写 Harness 模型编号".into());
        }
        Ok(())
    }
    fn status(&self, c: &Connection, root: &Path) -> Result<Value, String> {
        if !Path::new(&c.executable).is_file() {
            return Ok(
                json!({"ready":false,"status":"未找到 Harness 启动器；请选择新版 dsh.cmd 或 dsh.exe"}),
            );
        }
        let job = crate::storage::data_dir(&root)
            .join("tmp")
            .join(crate::model::id());
        fs::create_dir_all(&job).map_err(|e| e.to_string())?;
        let _cleanup = JobCleanup(job.clone());
        let profile = connection_profile(&job.join("profile"))?;
        let dsh_home = home()?;
        let readiness_config = shishi_acp::restricted::ReadinessConfig {
            dsh_cmd: PathBuf::from(&c.executable),
            dsh_home,
            agents_home: Some(profile.agents_dir),
            patch: profile.patch,
            workdir: Some(root.to_path_buf()),
        };
        let readiness = shishi_acp::check_readiness(&readiness_config);
        Ok(match readiness {
            Ok(value) => json!({
                "ready": true,
                "status": format!("Harness 就绪：运行版本 {}，受限工具集 {} 项活动组件", value.runtime_version, value.active_count),
                "runtime_version": value.runtime_version,
                "active_count": value.active_count,
            }),
            Err(error) => {
                json!({"ready":false,"can_initialize_acp":matches!(&error, shishi_acp::restricted::ReadinessError::ProfileMissing(_)),"dsh_home":readiness_config.dsh_home.to_string_lossy(),"status":format!("Harness 未就绪：{}", error.guidance(&readiness_config))})
            }
        })
    }
    fn agent(&self, c: &Connection) -> Result<Arc<dyn AgentSession>, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let (client, profile) = handling_client(c, &cwd)?;
        Ok(Arc::new(DshAgent {
            client: Arc::new(client),
            _profile: profile,
            history: Arc::new(Mutex::new(BTreeMap::new())),
            events: Arc::new(Mutex::new(vec![])),
        }))
    }
    fn open_session(&self, thread: &str) -> Result<(), String> {
        Err(format!("请在 Harness 客户端按项目查看会话 {thread}"))
    }
}

struct DshAgent {
    _profile: JobCleanup,
    client: Arc<AcpClient>,
    history: Arc<Mutex<BTreeMap<String, Vec<Value>>>>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
}
fn update_items(items: &mut Vec<Value>, update: SessionUpdate) -> Option<AgentUpdate> {
    match update {
        SessionUpdate::AgentMessageChunk { text } => {
            if let Some(item) = items.iter_mut().find(|item| item["id"] == "answer") {
                match &mut item["text"] {
                    Value::String(buffer) => buffer.push_str(&text),
                    value => *value = Value::String(text.clone()),
                }
            } else {
                items.push(
                    json!({"id":"answer","type":"agentMessage","phase":"final_answer","text":text}),
                );
            }
            Some(AgentUpdate::MessageDelta {
                item_id: "answer".into(),
                text,
            })
        }
        SessionUpdate::ToolCall {
            tool_call_id,
            title,
            status,
            kind,
            raw_input,
        } => {
            items.push(json!({"id":tool_call_id,"type":"mcpToolCall","status":status,"tool":title,"arguments":raw_input,"kind":kind}));
            items
                .iter()
                .find(|item| item["id"] == tool_call_id)
                .cloned()
                .map(AgentUpdate::Item)
        }
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status,
            text,
        } => {
            let item = items.iter_mut().find(|item| item["id"] == tool_call_id)?;
            item["status"] = json!(status);
            item["result"] = json!(text);
            Some(AgentUpdate::Item(item.clone()))
        }
        _ => None,
    }
}
fn context(tool: &ContextTool) -> McpServer {
    McpServer::stdio(
        "shishi_context",
        &tool.executable,
        [
            "--context-mcp".into(),
            tool.context_path.to_string_lossy().to_string(),
        ],
        Vec::<(String, String)>::new(),
    )
}
impl AgentSession for DshAgent {
    fn close(&self) -> Result<(), String> {
        self.client.shutdown();
        Ok(())
    }
    fn is_alive(&self) -> bool {
        self.client.is_connected()
    }
    fn start(&self, tool: &ContextTool, config: &Config) -> Result<Value, String> {
        let session = self
            .client
            .new_session(&tool.cwd, &[context(tool)], RPC)
            .map_err(|e| format!("ACP session/new 失败：{e}"))?;
        shishi_acp::workspace::publish(&home()?, &session.session_id)?;
        selection(&self.client, &session.session_id, config)?;
        Ok(json!({"thread":{"id":session.session_id},"model":config.model}))
    }
    fn submit(
        &self,
        thread: &str,
        text: &str,
        config: &Config,
        message_id: &str,
    ) -> Result<Value, String> {
        selection(&self.client, thread, config)
            .map_err(|e| format!("{}{e}", super::NOT_SUBMITTED))?;
        let turn = crate::model::id();
        {
            let mut history = self.history.lock().unwrap();
            let rows = history.entry(thread.into()).or_default();
            if rows.iter().any(|r| r["status"] == "inProgress") {
                return Err(format!(
                    "{}该会话正在处理，请先中断或等待结束",
                    super::NOT_SUBMITTED
                ));
            }
            rows.push(json!({"id":turn,"status":"inProgress","startedAt":crate::model::now()/1000,"items":[{"id":message_id,"clientId":message_id,"type":"userMessage","content":[{"type":"text","text":text}]}]}));
        }
        let client = self.client.clone();
        let history = self.history.clone();
        let events = self.events.clone();
        let thread = thread.to_owned();
        let text = text.to_owned();
        let tid = turn.clone();
        let timeout = Duration::from_secs(config.ai_timeout_seconds);
        std::thread::spawn(move || {
            let emit = |update| {
                events.lock().unwrap().push(AgentEvent {
                    thread_id: thread.clone(),
                    turn_id: tid.clone(),
                    update,
                })
            };
            let result = client.prompt(&thread, &text, timeout, &mut |event| {
                let AcpEvent { session_id, update } = event;
                if session_id != thread {
                    return;
                }
                let mut map = history.lock().unwrap();
                let Some(row) = map
                    .get_mut(&thread)
                    .and_then(|v| v.iter_mut().find(|v| v["id"] == tid))
                else {
                    return;
                };
                if let Some(update) = update_items(row["items"].as_array_mut().unwrap(), update) {
                    emit(update);
                    crate::codex_bridge::signal_activity();
                }
            });
            let mut map = history.lock().unwrap();
            if let Some(row) = map
                .get_mut(&thread)
                .and_then(|v| v.iter_mut().find(|v| v["id"] == tid))
            {
                row["completedAt"] = json!(crate::model::now() / 1000);
                row["status"] = json!(match &result {
                    Ok(p) if p.stop_reason == "cancelled" => "interrupted",
                    Ok(p) if p.stop_reason == "end_turn" => "completed",
                    _ => "failed",
                });
                if let Err(e) = &result {
                    row["error"] = json!({"message":error_message(&e.to_string())});
                } else if let Ok(prompt) = &result {
                    if !matches!(prompt.stop_reason.as_str(), "end_turn" | "cancelled") {
                        row["error"] =
                            json!({"message":format!("Harness stopped: {}",prompt.stop_reason)});
                    }
                }
                emit(AgentUpdate::Completed(row.clone()));
            }
            if let Ok(result) = result {
                if let Some(u) = result.usage {
                    emit(AgentUpdate::Usage {
                        input: u.input_tokens,
                        output: u.output_tokens,
                        cached: u.cached_read_tokens,
                    });
                }
            }
            crate::codex_bridge::signal_activity();
        });
        Ok(json!({"turn":{"id":turn}}))
    }
    fn read(&self, thread: &str) -> Result<Value, String> {
        let session = self
            .client
            .session(thread)
            .ok_or("Harness 会话尚未载入，请恢复连接；不会自动重发任务")?;
        let history = self.history.lock().unwrap();
        let active = history
            .get(thread)
            .is_some_and(|v| v.iter().any(|r| r["status"] == "inProgress"));
        Ok(
            json!({"thread":{"id":thread,"cwd":session.cwd,"status":{"type":if active{"active"}else{"idle"}}}}),
        )
    }
    fn history(&self, thread: &str) -> Result<Value, String> {
        if self.client.session(thread).is_none() {
            return Err("Harness 会话尚未载入，请先恢复连接".into());
        }
        Ok(
            json!({"data":self.history.lock().unwrap().get(thread).map(|v|v.iter().rev().cloned().collect::<Vec<_>>()).unwrap_or_default()}),
        )
    }
    fn resume(&self, thread: &str, tool: &ContextTool) -> Result<Value, String> {
        if self
            .history
            .lock()
            .unwrap()
            .get(thread)
            .is_some_and(|rows| rows.iter().any(|r| r["status"] == "inProgress"))
        {
            return Err("Harness 会话正在处理，不能重复恢复".into());
        }
        let info = self
            .client
            .resume_session(thread, &tool.cwd, &[context(tool)], RPC)
            .map_err(|e| e.to_string())?;
        Ok(json!({"thread":{"id":info.session_id,"cwd":info.cwd,"status":{"type":"idle"}}}))
    }
    fn interrupt(&self, thread: &str) -> Result<(), String> {
        self.client.cancel_session(thread);
        Ok(())
    }
    fn events(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    fn requests(&self, thread: &str) -> Vec<Value> {
        self.client.permissions_for(thread).iter().map(|p|json!({"id":p.request_id.to_value(),"method":"dsh/tool/requestApproval","params":{"threadId":thread,"tool_call":p.raw["toolCall"],"dsh_options":p.options.iter().map(|o|json!({"id":o.option_id,"name":o.name,"kind":o.kind})).collect::<Vec<_>>()}})).collect()
    }
    fn respond(&self, thread: &str, key: &str, answer: &Value) -> Result<(), String> {
        let pending = self.client.permissions_for(thread);
        let p = pending
            .iter()
            .find(|p| p.request_id.to_value().to_string() == key)
            .ok_or("请求已结束或不属于此会话")?;
        if answer["action"] != "dshOption" {
            return Err("请选择 Harness 本次明确提供的权限选项".into());
        }
        let option = p
            .options
            .iter()
            .find(|o| Some(o.option_id.as_str()) == answer["option_id"].as_str())
            .ok_or("Harness 未提供该权限选项")?;
        self.client
            .respond_permission(&p.request_id, &option.option_id)
            .map_err(|e| e.to_string())
    }
}

fn error_message(raw: &str) -> String {
    if raw.contains("unsupported stop reason refusal")
        || raw.ends_with("acceptable result: refusal")
    {
        format!("供应商拒答：DSH 收到 refusal 结束信号，未应用本次结果。原始错误：{raw}")
    } else {
        raw.into()
    }
}
#[cfg(test)]
#[path = "dsh_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod error_tests {
    use super::*;
    #[test]
    fn streamed_answer_reuses_storage_and_retains_complete_unicode_text() {
        let mut items = Vec::new();
        update_items(
            &mut items,
            SessionUpdate::AgentMessageChunk {
                text: "start:".into(),
            },
        );
        let Value::String(buffer) = &mut items[0]["text"] else {
            panic!("answer text")
        };
        buffer.reserve(100_000);
        let pointer = buffer.as_ptr();
        for _ in 0..10_000 {
            assert!(
                matches!(update_items(&mut items, SessionUpdate::AgentMessageChunk { text: "中文🙂".into() }), Some(AgentUpdate::MessageDelta { item_id, text }) if item_id == "answer" && text == "中文🙂")
            );
        }
        assert_eq!(items.len(), 1);
        let text = items[0]["text"].as_str().unwrap();
        assert_eq!(text, format!("start:{}", "中文🙂".repeat(10_000)));
        assert_eq!(
            text.as_ptr(),
            pointer,
            "capacity is reused while appending chunks"
        );
        assert!(update_items(
            &mut items,
            SessionUpdate::AgentThoughtChunk {
                text: "private".into()
            }
        )
        .is_none());
        assert_eq!(items.len(), 1);
        let update = SessionUpdate::ToolCallUpdate {
            tool_call_id: "tool".into(),
            status: Some("completed".into()),
            text: "合成结果🚀".into(),
        };
        assert!(update_items(&mut items, update.clone()).is_none());
        let first = update_items(
            &mut items,
            SessionUpdate::ToolCall {
                tool_call_id: "tool".into(),
                title: Some("synthetic".into()),
                status: Some("inProgress".into()),
                kind: Some("read".into()),
                raw_input: json!({"path":"fixture.txt"}),
            },
        );
        assert!(matches!(first, Some(AgentUpdate::Item(item)) if item == items[1]));
        let final_item = update_items(&mut items, update);
        assert!(
            matches!(final_item, Some(AgentUpdate::Item(item)) if item == items[1] && item["result"] == "合成结果🚀")
        );
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn refusal_is_explicit_but_unknown_internal_errors_stay_unknown() {
        assert!(error_message("ACP error -32603: Internal error: DeepSeek Messages stream: unsupported stop reason refusal").starts_with("供应商拒答"));
        assert!(
            error_message("analysis prompt stopped without an acceptable result: refusal")
                .starts_with("供应商拒答")
        );
        assert_eq!(
            error_message("ACP error -32603: Internal error"),
            "ACP error -32603: Internal error"
        );
    }
}
