//! Platform-neutral core of ShiShi 2.0: data thread, configuration,
//! executor and the business domains. The Windows host only forwards IPC
//! requests here and renders pushed events.

pub mod activity;
pub mod ai;
pub mod analysis;
pub mod archive;
pub mod config;
pub mod data;
pub mod error;
pub mod exec;
pub mod handling;
pub mod messages;
pub mod params;
pub mod process;
pub mod prompts;
pub mod settings;
pub mod sources;
pub mod state;
pub mod tasks;
#[cfg(test)]
pub(crate) mod testing;
pub mod time;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

pub use config::Config;
pub use data::{ConfigStore, Db};
pub use error::{Error, Result};
pub use exec::Executor;

/// Requests from the core to the native host.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    /// Show a desktop notification; `task_id` opens that task when clicked.
    Notify { title: String, body: String, task_id: Option<String> },
    /// Open a file, folder or URL with the system handler.
    Open(String),
    /// Leave the event loop, optionally starting the program again.
    Exit { restart: bool },
}

pub trait Host: Send + Sync {
    fn event(&self, event: HostEvent);

    /// Decode an image file into base64 JPEG for the preview window.
    fn image_preview(&self, _path: &std::path::Path) -> Result<String> {
        Err(Error::new("此平台不支持图片预览"))
    }

    /// Call one plugin method through the isolated plugin worker. `input` is
    /// the full request body; `settings` selects the settings entry point.
    /// A set `cancel` flag ends the call with an error.
    fn plugin_call(
        &self,
        _manifest: &std::path::Path,
        _settings: bool,
        _input: Value,
        _cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Value> {
        Err(Error::new("此平台不支持加载插件"))
    }

    /// Drop the plugin worker so no plugin file stays open.
    fn plugin_reset(&self) -> Result<()> {
        Ok(())
    }

    /// The installation or isolated root the CLI's `--root` takes; by
    /// default the program directory, which is also the CLI's own default.
    fn install_root(&self) -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap_or_default();
        exe.parent().map(Into::into).unwrap_or_default()
    }
}

/// A host that drops every event; used by tests and the CLI.
pub struct NoHost;

impl Host for NoHost {
    fn event(&self, _: HostEvent) {}
}

/// Everything a domain needs, shared by every request.
pub struct Ctx {
    /// Data directory: database, config, traces and plugin files.
    pub root: PathBuf,
    pub db: Db,
    pub config: ConfigStore,
    pub exec: Arc<Executor>,
    pub host: Arc<dyn Host>,
    pub flags: state::Flags,
    pub sources: sources::Runtime,
    pub tools: tools::Runtime,
    pub prompts: prompts::Runtime,
    pub archive: archive::Runtime,
    pub analysis: analysis::Runtime,
    pub ai: ai::provider::Runtime,
    pub handling: handling::Runtime,
}

impl Ctx {
    pub fn open(root: PathBuf, host: Arc<dyn Host>) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        let config = ConfigStore::load(root.join("config.json"))?;
        let db = Db::open(&root.join("shishi.db"))?;
        Ok(Self::new(root, db, config, host))
    }

    /// An in-memory context for tests.
    pub fn memory(config: Config) -> Result<Arc<Self>> {
        Self::memory_in(config, PathBuf::new())
    }

    /// An in-memory database with files under `root`.
    pub fn memory_in(config: Config, root: PathBuf) -> Result<Arc<Self>> {
        Ok(Self::new(root, Db::memory()?, ConfigStore::memory(config), Arc::new(NoHost)))
    }

    fn new(root: PathBuf, db: Db, config: ConfigStore, host: Arc<dyn Host>) -> Arc<Self> {
        Arc::new(Self {
            exec: Arc::new(Executor::new(db.clone())),
            root,
            db,
            config,
            host,
            flags: Default::default(),
            sources: Default::default(),
            tools: Default::default(),
            prompts: Default::default(),
            archive: Default::default(),
            analysis: Default::default(),
            ai: Default::default(),
            handling: Default::default(),
        })
    }
}

/// Methods `dispatch` answers before routing to a domain.
const CORE_METHODS: [&str; 7] =
    ["ai_job_start", "ai_job_result", "ai_interrupt", "state_version", "state", "ui_ready", "activity_query"];

/// Every method `dispatch` routes, for CLI discovery.
pub fn methods() -> Vec<&'static str> {
    [
        &CORE_METHODS[..],
        settings::METHODS,
        sources::METHODS,
        tools::METHODS,
        prompts::METHODS,
        tasks::METHODS,
        archive::METHODS,
        analysis::METHODS,
        ai::METHODS,
        handling::METHODS,
    ]
    .concat()
}

/// Long methods the page runs through `ai_job_start` and polls.
const JOB_METHODS: [&str; 11] = [
    "scan",
    "tool_test",
    "analyze",
    "advice_feedback",
    "schedule_format",
    "edit_ai",
    "report",
    "archive",
    "learn",
    "prompt_retry",
    "source_range",
];

type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>>;

fn dispatch_owned(ctx: Arc<Ctx>, method: String, params: Value) -> BoxFuture {
    Box::pin(async move { dispatch(&ctx, &method, params).await })
}

/// Route one IPC request to the domain that owns the method.
pub async fn dispatch(ctx: &Arc<Ctx>, method: &str, params: Value) -> Result<Value> {
    use params::Params;
    match method {
        "ai_job_start" => {
            let inner = params.str_of("method").to_string();
            if !JOB_METHODS.contains(&inner.as_str()) {
                bail!("不支持的后台操作");
            }
            let args = params.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
            let id = ctx.exec.job_start(dispatch_owned(ctx.clone(), inner, args))?;
            return Ok(serde_json::json!({ "job_id": id }));
        }
        "ai_job_result" => return ctx.exec.job_result(params.str_of("job_id")),
        "ai_interrupt" => {
            let id = params.str_of("request_id");
            return Ok(if ctx.exec.interrupt(Some(id)) {
                serde_json::json!({ "interrupted": true })
            } else {
                serde_json::json!({ "skipped": "该次 AI 分析已结束" })
            });
        }
        "state_version" => return Ok(ctx.db.revision().into()),
        "state" => return state::snapshot(ctx, &params).await,
        "ui_ready" => return Ok(true.into()),
        "activity_query" => return activity::query(ctx, params).await,
        _ => {}
    }
    macro_rules! route {
        ($($domain:ident),*) => {$(
            if $domain::METHODS.contains(&method) {
                return $domain::handle(ctx, method, params).await;
            }
        )*};
    }
    route!(settings, sources, tools, prompts, tasks, archive, analysis, ai, handling);
    Err(Error::new("未知请求"))
}

/// A console request: as [`dispatch`], plus one notice per task a task
/// method changed. The same edits made in the window stay silent.
pub async fn dispatch_cli(ctx: &Arc<Ctx>, method: &str, params: Value) -> Result<Value> {
    if !tasks::METHODS.contains(&method) {
        return dispatch(ctx, method, params).await;
    }
    let seq = ctx.db.audit_seq().await?;
    let result = dispatch(ctx, method, params).await?;
    let mut changes = analysis::apply::Applied::default();
    for (id, before, after, summary) in ctx.db.task_changes_since(seq).await.unwrap_or_default() {
        changes.record(&id, &before, &after, &summary);
    }
    for (id, title, body) in changes.notices() {
        ctx.host.event(HostEvent::Notify { title, body, task_id: Some(id) });
    }
    Ok(result)
}

/// Start the domain loops (scanning, scheduled work, archive boundaries).
/// Each loop sleeps until its next deadline or a wake-up; none polls.
pub fn spawn_background(ctx: &Arc<Ctx>) {
    sources::spawn(ctx);
    tasks::spawn(ctx);
    archive::spawn(ctx);
    analysis::spawn(ctx);
    ai::work::spawn(ctx);
    handling::spawn(ctx);
}

/// The executor thread: a current-thread runtime that serves IPC requests
/// concurrently and runs the background loops.
pub struct Runtime {
    tx: tokio::sync::mpsc::UnboundedSender<Request>,
    pub ctx: Arc<Ctx>,
}

type Reply = Box<dyn FnOnce(Result<Value>) + Send>;
type Watcher = Box<dyn FnMut(u64) + Send>;
enum Request {
    Call(String, Value, Reply),
    Cli(String, Value, Reply),
    Watch(Watcher),
}

impl Runtime {
    pub fn start(ctx: Arc<Ctx>, background: bool) -> Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            // Long provider calls hold a blocking thread for their whole run
            // and a pooled HTTP call needs one more for DNS; idle threads
            // exit after 10 s, so the cap costs nothing at rest.
            .max_blocking_threads(8)
            .thread_name("shishi-io")
            .build()?;
        shishi_api::http_client::use_runtime(runtime.handle().clone());
        let shared = ctx.clone();
        std::thread::Builder::new().name("shishi-exec".into()).spawn(move || {
            runtime.block_on(async move {
                if background {
                    spawn_background(&shared);
                }
                while let Some(request) = rx.recv().await {
                    let ctx = shared.clone();
                    match request {
                        Request::Call(method, params, reply) => {
                            tokio::spawn(async move { reply(dispatch(&ctx, &method, params).await) });
                        }
                        Request::Cli(method, params, reply) => {
                            tokio::spawn(async move { reply(dispatch_cli(&ctx, &method, params).await) });
                        }
                        Request::Watch(mut changed) => {
                            let mut pulse = ctx.db.subscribe();
                            tokio::spawn(async move {
                                while pulse.changed().await.is_ok() {
                                    let n = *pulse.borrow_and_update();
                                    changed(n);
                                }
                            });
                        }
                    }
                }
            })
        })?;
        Ok(Self { tx, ctx })
    }

    /// Queue a request; `reply` runs on the executor thread when it ends.
    pub fn request(&self, method: String, params: Value, reply: impl FnOnce(Result<Value>) + Send + 'static) {
        if let Err(e) = self.tx.send(Request::Call(method, params, Box::new(reply))) {
            if let Request::Call(_, _, reply) = e.0 {
                reply(Err(Error::new("后台已停止")));
            }
        }
    }

    /// Run `changed` on the executor thread after every state change
    /// (business revision or runtime poke), with the new pulse value. The
    /// host turns it into `window.stateChanged(n)`; nothing polls.
    pub fn watch(&self, changed: impl FnMut(u64) + Send + 'static) {
        let _ = self.tx.send(Request::Watch(Box::new(changed)));
    }

    /// Await a request from synchronous code (tests, the window).
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.wait(|reply| Request::Call(method.into(), params, reply))
    }

    /// Await a console request, which notifies the tasks it changed.
    pub fn call_cli(&self, method: &str, params: Value) -> Result<Value> {
        self.wait(|reply| Request::Cli(method.into(), params, reply))
    }

    fn wait(&self, request: impl FnOnce(Reply) -> Request) -> Result<Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        let reply: Reply = Box::new(move |r| {
            let _ = tx.send(r);
        });
        if let Err(e) = self.tx.send(request(reply)) {
            if let Request::Call(_, _, reply) | Request::Cli(_, _, reply) = e.0 {
                reply(Err(Error::new("后台已停止")));
            }
        }
        rx.recv().unwrap_or_else(|_| Err(Error::new("后台已停止")))
    }
}

#[cfg(test)]
mod tests;
