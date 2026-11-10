pub mod agent;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::analysis::clip;
use crate::config::{Config, Connection};
use crate::params::Params;
use crate::state::Scope;
use crate::{exec, prompts, time, Ctx, Error, Result};
pub use agent::{Agent, Backends, Native, Turn};

pub const METHODS: &[&str] =
    &["handling_open", "handling_prepare", "handling_projects", "handling_start", "handling_brief"];
/// The built-in Codex of the settings page, which has no connection id.
const DEFAULT_CODEX: &str = "__default_codex";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    /// The connection at start, without its model list.
    pub connection: Connection,
    pub project_name: String,
    pub cwd: String,
    pub thread_id: String,
    pub model: String,
    pub effort: String,
    pub instructions: String,
    pub status: String,
    pub error: String,
    pub created: i64,
    pub updated: i64,
}

impl Run {
    pub fn busy(&self) -> bool {
        self.status == "running"
    }
}

/// First turns in progress.
#[derive(Default)]
pub struct Runtime {
    backends: RwLock<Option<Arc<dyn Backends>>>,
    live: Mutex<HashMap<String, Run>>,
}

impl Runtime {
    pub fn backends(&self) -> Arc<dyn Backends> {
        self.backends.read().unwrap().clone().unwrap_or_else(|| Arc::new(Native))
    }

    /// Replace the integrations (tests use fakes).
    pub fn set_backends(&self, backends: Arc<dyn Backends>) {
        *self.backends.write().unwrap() = Some(backends);
    }

    fn live(&self, id: &str) -> Option<Run> {
        self.live.lock().unwrap().get(id).cloned()
    }
}

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    match method {
        "handling_prepare" => prepare(ctx, p.need_str("task_id")?).await,
        "handling_projects" => Ok(json!(projects(ctx, p.str_of("connection_id")).await?)),
        "handling_start" => start(ctx, &p).await,
        "handling_brief" => {
            let mut run = load(ctx, p.str_of("run_id")).await?;
            let (task, _, policy) = ctx.db.handling_start(run.task_id.clone()).await?;
            let text = prompt(ctx, &policy, &run.instructions, &task);
            if let Some(thread) = p["thread_id"].as_str() {
                run.thread_id = thread.into();
                run.status = "completed".into();
                run.updated = time::now();
                ctx.db.save_handling_run(run).await?;
            }
            Ok(json!(text))
        }
        "handling_open" => {
            let run = load(ctx, p.str_of("run_id")).await?;
            open(ctx, &run).await
        }
        _ => Err(Error::new("未知请求")),
    }
}

/// Rows a previous process left running were cut off with it.
pub fn spawn(ctx: &Arc<Ctx>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = ctx.db.interrupt_handling_runs().await {
            ctx.db.record_activity("handling_recover_failed", json!({"error": e.0})).await;
        }
    });
}

async fn load(ctx: &Ctx, id: &str) -> Result<Run> {
    match ctx.handling.live(id) {
        Some(run) => Ok(run),
        None => ctx.db.handling_run(id.to_owned()).await?.ok_or_else(|| Error::new("处置记录不存在")),
    }
}

/// Bring the run's session up in its client.
async fn open(ctx: &Ctx, run: &Run) -> Result<Value> {
    if run.connection.driver == "codex" {
        if run.thread_id.is_empty() {
            if run.status != "waiting" {
                crate::bail!("没有记录到 Codex 会话编号，请在 Codex 中查找这条对话，或重新开始处置");
            }
            let kickoff = command(ctx, "handling_kickoff", &run.task_id, &run.id);
            ctx.handling.backends().compose(Path::new(&run.cwd), &kickoff)?;
            return Ok(json!({"kickoff": kickoff}));
        }
    } else if run.thread_id.is_empty() {
        crate::bail!("会话尚未建立，请稍候");
    } else if run.busy() {
        crate::bail!("首轮进行中，结束后即可继续");
    }
    let backends = ctx.handling.backends();
    let (connection, thread) = (run.connection.clone(), run.thread_id.clone());
    exec::blocking(move || backends.open(&connection, &thread)).await?;
    // dsh://open cannot select a session; name where it is.
    Ok(match run.connection.driver.as_str() {
        "dsh" => json!({"hint": format!("已唤起 DSH，请在「{}」项目中打开会话 {}", run.project_name, run.thread_id)}),
        _ => Value::Null,
    })
}

// ---------------------------------------------------------------- choices

/// Connections that can run a conversation, the built-in Codex first.
fn connections(config: &Config) -> Vec<Connection> {
    let builtin = Connection { id: DEFAULT_CODEX.into(), ..crate::ai::prompt::default_codex(config) };
    std::iter::once(builtin)
        .filter(|c| !c.executable.trim().is_empty())
        .chain(config.ai_connections.iter().filter(|c| ["codex", "dsh"].contains(&c.driver.as_str())).cloned())
        .collect()
}

fn connection(config: &Config, id: &str) -> Result<Connection> {
    let id = if id.is_empty() { DEFAULT_CODEX } else { id };
    connections(config).into_iter().find(|c| c.id == id).ok_or_else(|| Error::new("请选择可用的 Codex / DSH 接入"))
}

/// The settings' handling model when it is on a conversation connection,
/// else the first such connection with its own default model.
fn default_choice(config: &Config, choices: &[Connection]) -> Value {
    let choice = config.model_choice("handling");
    let id = if choice.connection_id.is_empty() { DEFAULT_CODEX } else { &choice.connection_id };
    match choices.iter().find(|c| c.id == id).or(choices.first()) {
        Some(c) if c.id == id => json!({"connection_id": id, "model": choice.model, "effort": choice.reasoning_effort}),
        Some(c) => json!({"connection_id": c.id, "model": "", "effort": ""}),
        None => Value::Null,
    }
}

/// The projects of the connection's own client: Codex projects for Codex,
/// DSH workspace projects for DSH. Read on demand, never merged.
pub(crate) async fn projects(ctx: &Arc<Ctx>, connection_id: &str) -> Result<Vec<Value>> {
    let driver = connection(&ctx.config.get(), connection_id)?.driver;
    let backends = ctx.handling.backends();
    exec::blocking(move || backends.projects(&driver)).await
}

/// The projects analysis may suggest for a task: those of the default
/// handling connection, which the dialog opens with.
pub(crate) async fn offered(ctx: &Arc<Ctx>) -> Value {
    let config = ctx.config.get();
    let Some(id) = default_choice(&config, &connections(&config))["connection_id"].as_str().map(str::to_owned) else {
        return json!([]);
    };
    let list = projects(ctx, &id).await.unwrap_or_default();
    list.iter().take(60).map(|p| json!({"id": p["id"], "name": p["name"], "cwd": p["cwd"]})).collect()
}

async fn prepare(ctx: &Arc<Ctx>, task: &str) -> Result<Value> {
    let config = ctx.config.get();
    let (task, prompt) = ctx.db.handling_prepare(task.to_string()).await?;
    let choices = connections(&config);
    let default = default_choice(&config, &choices);
    // Only the settings' client: a Codex default offers Codex models only.
    let driver = choices.iter().find(|c| c.id == default["connection_id"]).map(|c| c.driver.clone());
    let choices = choices
        .iter()
        .filter(|c| Some(&c.driver) == driver.as_ref())
        .map(|c| json!({"id": c.id, "name": c.name, "driver": c.driver, "models": c.models}))
        .collect::<Vec<_>>();
    Ok(json!({"suggested": task.suggested_project_id, "prompt": prompt, "connections": choices, "default": default}))
}

// ---------------------------------------------------------------- start

fn command(ctx: &Ctx, key: &str, task: &str, run: &str) -> String {
    let (cli, root) = (std::env::current_exe().unwrap_or_default(), ctx.host.install_root());
    let quote = |s: &str| s.replace('\'', "''");
    prompts::default(key)
        .replace("{cli}", &quote(&cli.to_string_lossy()))
        .replace("{root}", &quote(&root.to_string_lossy()))
        .replace("{task}", &quote(task))
        .replace("{run}", &quote(run))
}

/// The prompt of the first turn: the boundary, the editable policy, the
/// user's note, the task and how to read and write it through the CLI.
fn prompt(ctx: &Ctx, policy: &str, instructions: &str, task: &crate::tasks::model::Task) -> String {
    let writeback = command(ctx, "handling_writeback", &task.id, "");
    let task = json!({"id": task.id, "title": task.title, "description": task.description, "status": task.status});
    let note =
        if instructions.is_empty() { String::new() } else { format!("\n\n本次调查说明：\n{instructions}") };
    format!("{}\n\n{policy}{note}\n\n当前事项（资料）：\n{task}\n\n{writeback}", prompts::default("handling_boundary"))
}

async fn start(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let config = ctx.config.get();
    let mut connection = connection(&config, p.str_of("connection_id"))?;
    connection.models.clear();
    let (task, runs, policy) = ctx.db.handling_start(p.str_of("task_id").to_owned()).await?;
    let cwd = p.str_of("cwd");
    if !Path::new(cwd).is_dir() {
        crate::bail!("项目目录不存在，请在对应客户端中检查该项目");
    }
    let instructions = p.str_of("instructions").trim();
    let now = time::now();
    let mut run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task.id.clone(),
        connection,
        project_name: p.str_of("project_name").into(),
        cwd: cwd.into(),
        instructions: instructions.into(),
        status: "waiting".into(),
        created: now,
        updated: now,
        ..Default::default()
    };
    if run.connection.driver == "codex" {
        ctx.db.save_handling_run(run.clone()).await?;
        return Ok(json!({"run_id": run.id, "kickoff": open(ctx, &run).await?["kickoff"]}));
    }
    if runs.iter().any(|r| r.busy() || ctx.handling.live(&r.id).is_some()) {
        crate::bail!("该事项的处置正在启动，请稍候");
    }
    let text = prompt(ctx, &policy, instructions, &task);
    ctx.db.reserve_handling_call(config.clone()).await?;
    run.status = "running".into();
    run.model = p.str_of("model").into();
    run.effort = p.str_of("effort").into();
    ctx.db.save_handling_run(run.clone()).await?;
    let agent = ctx.handling.backends().agent(&run.connection, &ctx.root)?;
    ctx.handling.live.lock().unwrap().insert(run.id.clone(), run.clone());
    let turn = Turn {
        text,
        cwd: run.cwd.clone().into(),
        model: run.model.clone(),
        effort: run.effort.clone(),
        timeout: std::time::Duration::from_secs(config.ai_timeout_seconds.max(60) * 10),
    };
    let (c, id) = (ctx.clone(), run.id.clone());
    let started = time::now();
    tokio::spawn(async move {
        let (events, live, handle) = (c.clone(), id.clone(), tokio::runtime::Handle::current());
        let result =
            exec::blocking(move || agent.run(&turn, &mut |thread| handle.block_on(session(&events, &live, thread))))
                .await;
        finish(&c, &id, started, result).await;
    });
    Ok(json!({"run_id": run.id}))
}

/// The session id is known: keep it. The client gets it after the turn.
async fn session(ctx: &Arc<Ctx>, id: &str, thread: String) {
    let run = {
        let mut live = ctx.handling.live.lock().unwrap();
        let Some(run) = live.get_mut(id) else { return };
        run.thread_id = thread;
        run.updated = time::now();
        run.clone()
    };
    ctx.db.bump();
    if let Err(e) = ctx.db.save_handling_run(run).await {
        ctx.db.record_activity("handling_save_failed", json!({"run_id": id, "error": e.0})).await;
    }
}

async fn finish(ctx: &Arc<Ctx>, id: &str, started: i64, result: Result<agent::Outcome>) {
    let Some(mut run) = ctx.handling.live.lock().unwrap().remove(id) else { return };
    let (mut conclusion, mut usage) = (String::new(), Value::Null);
    match result {
        Ok(outcome) => {
            run.status = outcome.status.into();
            (conclusion, usage) = (outcome.conclusion, outcome.usage);
        }
        Err(e) => {
            run.status = "failed".into();
            run.error = e.0;
        }
    }
    run.updated = time::now();
    let log = json!({"run_id": run.id, "task_id": run.task_id, "thread_id": run.thread_id, "status": run.status,
        "source": {"driver": run.connection.driver, "connection_name": run.connection.name},
        "model": run.model, "reasoning_effort": run.effort, "question": clip(&run.instructions, 600),
        "conclusion": clip(&conclusion, 1200), "error": run.error,
        "elapsed_ms": run.updated - started, "usage": usage});
    // The session is free now: tell the user once, without taking focus.
    let client = if run.connection.driver == "dsh" { "DSH" } else { "Codex" };
    let body = match run.status.as_str() {
        "failed" => format!("首轮失败：{}", clip(&run.error, 120)),
        _ if run.thread_id.is_empty() => "首轮结束".into(),
        _ => format!("首轮结束，可在 {client} 中继续"),
    };
    let task_id = Some(run.task_id.clone());
    ctx.host.event(crate::HostEvent::Notify { title: "AI 处置".into(), body, task_id });
    if let Err(e) = ctx.db.finish_handling_run(run, usage, log).await {
        ctx.db.record_activity("handling_save_failed", json!({"run_id": id, "error": e.0})).await;
    }
}

// ---------------------------------------------------------------- view

/// `handling`: the latest run of every listed task, with first turns in
/// progress laid over from memory.
pub(crate) fn part(ctx: &Ctx, _: &Scope, out: &mut Map<String, Value>) {
    let Some(Value::Array(rows)) = out.get_mut("handling") else { return };
    let live = ctx.handling.live.lock().unwrap();
    for row in rows.iter_mut() {
        if let Some(run) = row["id"].as_str().and_then(|id| live.get(id)) {
            *row = head(run);
        }
    }
}

pub(crate) fn head(run: &Run) -> Value {
    json!({"id": run.id, "task_id": run.task_id, "status": run.status, "thread_id": run.thread_id,
        "driver": run.connection.driver, "connection_name": run.connection.name, "project_name": run.project_name,
        "error": run.error})
}
