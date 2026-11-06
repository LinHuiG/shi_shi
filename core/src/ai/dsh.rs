//! The DSH (DeepSeek Harness) driver: model
//! discovery and readiness through the user's launcher, and analysis rounds
//! in one restricted ACP process per request ([`super::native`] owns the
//! tool loop). Handling sessions (`DshAgent`) belong to the handling domain.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shishi_acp::{materialize_profile, AcpClient, AcpEvent, AcpLaunch, PermissionPolicy, SessionUpdate};

use super::native::{self, Round, Rounds};
use super::provider::{Context, Driver, Event, Job, Reply};
use super::CANCELLED;
use crate::config::Connection as AiConnection;

const RPC: Duration = Duration::from_secs(20);

/// Remove only the per-request directory this process created, on every
/// exit path; never the shared analysis workdir or the user's DSH home.
pub(crate) struct JobCleanup(pub(crate) PathBuf);

impl Drop for JobCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn job(root: &Path, area: &str) -> Result<JobCleanup, String> {
    let dir = root.join(area).join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(JobCleanup(dir))
}

pub struct Dsh;

/// The user's Harness home (`DSH_HOME`, else `%USERPROFILE%\.dsh`).
pub fn home() -> Result<PathBuf, String> {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".dsh")))
        .ok_or_else(|| "无法定位 Harness 用户目录".into())
}

/// The SS session plugin of the Harness workspace: status, or install after
/// the user's explicit confirmation.
pub fn workspace_plugin(c: &AiConnection, install: bool, confirmed: bool) -> Result<Value, String> {
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

/// Create the built-in ACP profile after the user's explicit confirmation.
pub fn initialize_acp(c: &AiConnection, root: &Path, confirmed: bool) -> Result<Value, String> {
    if !confirmed || c.driver != "dsh" {
        return Err("请明确确认初始化 DSH 内置 ACP 环境".into());
    }
    let job = job(root, "tmp")?;
    let config = readiness(c, &job.0, root)?;
    let ready = shishi_acp::dsh::initialize_acp(&config).map_err(|e| e.guidance(&config))?;
    Ok(json!({"ready":true,"runtime_version":ready.runtime_version}))
}

fn usage(value: Option<shishi_acp::TokenUsage>) -> Value {
    value
        .map(|u| {
            json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,
                "cached_input_tokens":u.cached_read_tokens,"total_tokens":u.total_tokens})
        })
        .unwrap_or(Value::Null)
}

/// Model catalog values are opaque ACP strings. Accept either the exact
/// opaque value returned by discovery or a plain model id routed to the
/// default account provider; never invent a value for an unadvertised route.
pub(crate) fn model_route(value: &str) -> (String, String) {
    serde_json::from_str(value).unwrap_or_else(|_| ("deepseek-account".into(), value.into()))
}

fn readiness(c: &AiConnection, job: &Path, workdir: &Path) -> Result<shishi_acp::dsh::ReadinessConfig, String> {
    let home = home()?;
    Ok(materialize_profile(&job.join("profile"), &home)?.readiness(Path::new(&c.executable), &home, workdir))
}

pub(crate) fn console_safe_launch(launch: AcpLaunch, dir: &Path) -> Result<AcpLaunch, String> {
    let hook = dir.join("stdio-console.cjs");
    fs::write(&hook, include_str!("../../../providers/acp/assets/stdio-console.cjs")).map_err(|e| e.to_string())?;
    // NODE_OPTIONS tokenization consumes backslashes inside quoted values;
    // use Node's forward-slash Windows spelling, verbatim paths included.
    let path = hook.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/");
    if path.contains(['"', '\r', '\n']) {
        return Err("DSH 启动目录含不支持的字符".into());
    }
    let options = std::env::var("NODE_OPTIONS").unwrap_or_default();
    Ok(launch
        .env("SHISHI_DSH_ORIGINAL_NODE_OPTIONS", &options)
        .env("NODE_OPTIONS", format!("{options} --require=\"{path}\"")))
}

fn initialize_failed(e: impl std::fmt::Display) -> String {
    format!("ACP initialize 失败：{e}；标准输出必须仅含 JSON-RPC。SHISHI_ACP_DIAGNOSTICS=1 可启用本地失败帧诊断")
}

/// An unrestricted (surface-permission) client: model discovery and handling.
pub(crate) fn discovery_client(c: &AiConnection, root: &Path, cwd: &Path) -> Result<(AcpClient, JobCleanup), String> {
    let cleanup = job(root, "tmp")?;
    let dsh_home = home()?;
    let profile = materialize_profile(&cleanup.0.join("profile"), &dsh_home)?;
    let launch =
        shishi_acp::dsh::handling_launch(Path::new(&c.executable), cwd, &dsh_home, profile.providers.as_deref());
    let launch = console_safe_launch(launch, &cleanup.0)?;
    let client = AcpClient::spawn(&launch, PermissionPolicy::Surface).map_err(|e| e.to_string())?;
    client.initialize(RPC).map_err(initialize_failed)?;
    Ok((client, cleanup))
}

fn remaining(cancel: &AtomicBool, deadline: Instant) -> Result<Duration, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED.into());
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "AI 总时限已到".into())
}

/// Run `run` while a watcher aborts the client on cancellation or deadline.
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
    // The RPC may time out before the watcher wakes; teardown must still
    // skip the normal-exit grace period in that race.
    remaining(cancel, deadline).inspect_err(|_| client.abort())?;
    result
}

fn error_message(raw: &str) -> String {
    if raw.contains("unsupported stop reason refusal") || raw.ends_with("acceptable result: refusal") {
        format!("供应商拒答：DSH 收到 refusal 结束信号，未应用本次结果。原始错误：{raw}")
    } else {
        raw.into()
    }
}

/// One logical tool loop owns one restricted process. Its loaded profile is
/// fixed; each round still gets a fresh session and ticket. Field order
/// reaps the process before its profile is removed.
struct AnalysisProcess {
    client: AcpClient,
    cleanup: JobCleanup,
    workdir: PathBuf,
    dsh_home: PathBuf,
}

impl AnalysisProcess {
    fn prepare(c: &AiConnection, r: &Job, dsh_home: PathBuf) -> Result<Self, String> {
        let (cancel, deadline) = (r.flag(), r.deadline);
        remaining(cancel, deadline)?;
        validate(c, &r.route.model)?;
        let cleanup = job(&r.root, "ai-work")?;
        let workdir = super::prompt::analysis_workdir(&r.root, &r.config).map_err(|e| e.0)?;
        fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;
        let profile = materialize_profile(&cleanup.0.join("profile"), &dsh_home)?;
        let readiness = profile.readiness(Path::new(&c.executable), &dsh_home, &workdir);
        shishi_acp::dsh::check_readiness_until(&readiness, deadline, cancel)
            .map_err(|e| remaining(cancel, deadline).err().unwrap_or_else(|| e.guidance(&readiness)))?;
        remaining(cancel, deadline)?;
        let launch = profile.analysis_launch(Path::new(&c.executable), &workdir, &dsh_home);
        let launch = console_safe_launch(launch, &cleanup.0)?;
        let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).map_err(|e| e.to_string())?;
        with_deadline(&client, cancel, deadline, || {
            client.initialize(RPC.min(remaining(cancel, deadline)?)).map_err(initialize_failed)
        })?;
        Ok(Self { client, cleanup, workdir, dsh_home })
    }

    fn analyze(&self, r: &Job, round: &Round<'_>, event: &mut dyn FnMut(Event)) -> Reply {
        let (cancel, deadline) = (r.flag(), r.deadline);
        remaining(cancel, deadline)?;
        let job = self.cleanup.0.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&job).map_err(|e| e.to_string())?;
        let _cleanup = JobCleanup(job.clone());
        let (provider, model) = model_route(&r.route.model);
        let spec = shishi_acp::submit::AnalysisSpec {
            request_id: uuid::Uuid::new_v4().to_string(),
            kind: r.kind.clone(),
            schema: round.schema.clone(),
            prompt: round.prompt.into(),
            images: round.images.to_vec(),
            workdir: self.workdir.clone(),
            app_exe: std::env::current_exe().map_err(|e| e.to_string())?,
            model: Some((provider, model)),
            effort: (!r.route.effort.is_empty()).then(|| r.route.effort.clone()),
        };
        let prepared = shishi_acp::submit::prepare(&job.join("submit"), &spec).map_err(|e| e.to_string())?;
        let result = with_deadline(&self.client, cancel, deadline, || {
            let mut announced = false;
            let accepted =
                shishi_acp::submit::run(&self.client, &spec, &prepared, remaining(cancel, deadline)?, &mut |e| {
                    let AcpEvent { session_id, update } = e;
                    if !announced {
                        event(Event::SessionStarted(session_id));
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
                        if let Some(dir) = round.trace {
                            dir.event(&json!({"type":kind,"text":text}));
                        }
                        event(Event::Stream { kind: kind.into(), text });
                    }
                })
                .map_err(|e| error_message(&e.to_string()))?;
            if !accepted.session_closed {
                // Some ACP versions cannot close a session. Keep the
                // validated result, but retire the process before a later round.
                self.client.shutdown();
            }
            event(Event::Stream { kind: "phase".into(), text: "校验结果".into() });
            if !announced {
                event(Event::SessionStarted(accepted.session_id.clone()));
            }
            let tokens = usage(accepted.prompt.usage);
            event(Event::Usage(tokens.clone()));
            if let Err(error) = shishi_acp::workspace::publish(&self.dsh_home, &accepted.session_id) {
                event(Event::VisibilityIssue { thread_id: accepted.session_id.clone(), error });
            }
            let value = accepted.value;
            if let Some(dir) = round.trace {
                dir.put("output.txt", value.to_string());
            }
            Ok((value, tokens))
        });
        if result.is_err() {
            self.client.shutdown();
        }
        result
    }
}

/// Native rounds on one restricted process, replaced after a failure or a
/// disconnect; tools are never replayed.
struct Process<'c> {
    connection: &'c AiConnection,
    home: PathBuf,
    current: Option<AnalysisProcess>,
}

impl Rounds for Process<'_> {
    fn round(&mut self, request: &Job, round: Round<'_>, event: &mut dyn FnMut(Event)) -> Reply {
        if self.current.is_none() {
            self.current = Some(AnalysisProcess::prepare(self.connection, request, self.home.clone())?);
        }
        let process = self.current.as_ref().unwrap();
        let result = process.analyze(request, &round, event);
        if result.is_err() || !process.client.is_connected() {
            self.current = None;
        }
        result
    }
}

impl Driver for Dsh {
    fn analyze(&self, job: &Job, context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply {
        remaining(job.flag(), job.deadline)?;
        let mut process = Process { connection: &job.route.connection, home: home()?, current: None };
        native::analyze(job, context, event, "DSH", &mut process)
    }
}

/// The model catalog the launcher advertises.
pub fn models(c: &AiConnection, root: &Path) -> Result<Value, String> {
    let readiness = status(c, root).map_err(|e| format!("就绪检查失败：{e}"))?;
    if readiness["ready"] != true {
        return Err(readiness["status"].as_str().unwrap_or("Harness 未就绪").into());
    }
    let (client, _profile) = discovery_client(c, root, root)?;
    let session = client.new_session(root, &[], RPC).map_err(|e| format!("ACP session/new 失败：{e}"))?;
    let efforts = session
        .effort_option()
        .map(|o| o.choices.iter().map(|m| json!({"reasoningEffort":m.value,"description":m.name})).collect::<Vec<_>>())
        .unwrap_or_default();
    let models = session
        .model_option()
        .map(|o| {
            o.choices
                .iter()
                .map(|m| {
                    json!({"model":m.value,"displayName":m.name,"providerName":m.group,
                        "provider":serde_json::from_str::<Vec<String>>(&m.value).ok().and_then(|r|r.first().cloned()),
                        "supportedReasoningEfforts":efforts})
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current = session.current_model_route();
    client.shutdown();
    Ok(json!({"models":models,"efforts":efforts,"current":current.map(|(p,m)|json!({"provider":p,"model":m}))}))
}

/// A DSH route needs the launcher and a model.
pub fn validate(c: &AiConnection, model: &str) -> Result<(), String> {
    if !Path::new(&c.executable).is_file() {
        return Err("未找到桌面 Harness 启动器，请选择新版 dsh.cmd".into());
    }
    if model.is_empty() {
        return Err("请填写 Harness 模型编号".into());
    }
    Ok(())
}

/// Readiness of the launcher and its built-in ACP profile.
pub fn status(c: &AiConnection, root: &Path) -> Result<Value, String> {
    if !Path::new(&c.executable).is_file() {
        return Ok(json!({"ready":false,"status":"未找到 Harness 启动器；请选择新版 dsh.cmd 或 dsh.exe"}));
    }
    let job = job(root, "tmp")?;
    let config = readiness(c, &job.0, root)?;
    Ok(match shishi_acp::check_readiness(&config) {
        Ok(value) => json!({
            "ready": true,
            "status": format!("Harness 就绪：运行版本 {}，受限工具集 {} 项活动组件", value.runtime_version, value.active_ids.len()),
            "runtime_version": value.runtime_version,
            "active_count": value.active_ids.len(),
        }),
        Err(error) => json!({"ready":false,
            "can_initialize_acp":matches!(&error, shishi_acp::ReadinessError::ProfileMissing(_)),
            "dsh_home":config.dsh_home.to_string_lossy(),
            "status":format!("Harness 未就绪：{}", error.guidance(&config))}),
    })
}

#[cfg(test)]
mod tests;
