//! The analysis domain: turn new message versions into task changes.
//!
//! A round takes the AI slot once, freezes the pending `(id, version)`
//! boundary and runs batches: assemble the newest data, fit it to the input
//! budget, ask the model outside any transaction and accept the result in
//! one transaction that rechecks versions and cancellation. Committed
//! batches stay; the rest stays pending. Progress lives in the message rows
//! (`analyzed_hash`, retry columns) so nothing is replayed after a restart.
//!
//! Model calls go through the narrow [`Model`] interface; [`layer::model`]
//! builds the AI call layer. Tests use a stub [`Model`].

pub mod apply;
pub mod backlog;
pub mod batch;
pub mod context;
pub mod filter;
pub mod layer;
pub mod progress;
pub mod round;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub use crate::ai::invoke::Measured;
use crate::exec::Cancel;
use crate::{time, Ctx, Error, Result};

pub const METHODS: &[&str] = &["analyze", "analysis_backlog"];

pub const FILTER_TIMEOUT: &str = "消息过滤总时限已到，未完成批次保留";
pub const COMMAND_RETRY_REASON: &str = "本批存在已执行或结果未知的命令，须核对命令记录后手动重试，未标为已分析";
pub const ROUND_DEADLINE: &str = "分析总时限已到，未提交的消息保留待下一轮处理";
pub const BATCH_DEADLINE: &str = "分析总时限已到，本批未调用 AI";
pub const LATE_RESULT: &str = "分析总时限已到，本批未提交";
pub const AUTHORITY_CHANGED: &str = "分析期间配置或来源已变化，本批结果未采用，消息仍待处理";
pub const BATCH_BUSY: &str = "消息分析正在处理，请稍后重试";
pub const DISABLED: &str = "自动 AI 已关闭，消息已保存";
pub const NOTHING: &str = "没有相关变化，不调用AI";
pub const NOT_DUE: &str = "等待分析间隔或单批体积阈值";

/// How a round was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The analyze button or a manual scan: no work-hours gate, sees
    /// messages that only a manual retry may take.
    Manual,
    /// The background loop.
    Automatic,
}

// ---- model interface -----------------------------------------------------

/// The filter decision a classify call takes and the versions it looks at;
/// recorded in the transaction that counts the call.
#[derive(Clone, Debug)]
pub struct FilterTaken {
    pub decision_at: Option<i64>,
    pub observed: BTreeMap<String, String>,
}

/// One prepared request. `purpose` selects prompt and route (`classify` or
/// `message_filter`); `payload` is the user part as this domain built it
/// (the layer renders and compacts it). `reader` answers the read-only
/// `read_context` tool for classify requests; `reserve` is recorded in the
/// reservation transaction before anything is sent.
#[derive(Clone)]
pub struct Request {
    pub purpose: &'static str,
    pub payload: Value,
    pub reader: Option<Arc<context::Reader>>,
    pub reserve: Option<FilterTaken>,
}

/// What one call produced. `commands_ran` is set when a tool call was not
/// rejected, so commands may have executed; `invalid_response` when a reply
/// arrived but failed validation (unknown short ids, schema); `refused`
/// when the provider refused or filtered the input. They decide what is
/// recorded against the batch's messages.
pub struct Reply {
    pub result: Result<Value>,
    pub commands_ran: bool,
    pub invalid_response: bool,
    pub refused: bool,
}

impl Reply {
    pub fn ok(value: Value) -> Self {
        Self { result: Ok(value), commands_ran: false, invalid_response: false, refused: false }
    }
    pub fn err(error: impl Into<String>) -> Self {
        Self { result: Err(Error(error.into())), commands_ran: false, invalid_response: false, refused: false }
    }
}

pub type ReplyFuture<'a> = Pin<Box<dyn Future<Output = Reply> + Send + 'a>>;
pub type GateFuture<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// What the analysis domain needs from the AI call layer.
///
/// `invoke` returns the parsed, id-decoded output: for classify
/// `{actions, tag_suggestions, no_action_reason, memory_update, ...}` plus
/// the layer's `_analysis_degraded`, `_tool_run_id`; for the filter
/// `{decision, reason}`. The layer keeps its own usage accounting, traces and
/// live phases.
pub trait Model: Send + Sync {
    /// Render and measure a request (prompt, schema, tools, wire). The
    /// default counts the payload without its `_` keys, which never reach
    /// the prompt; the real layer renders the request as it sends it and
    /// keeps the render, so the request chosen by fitting is sent as
    /// measured.
    fn measure(&self, request: Request) -> Result<Measured<Request>> {
        let mut payload = request.payload.clone();
        if let Some(o) = payload.as_object_mut() {
            o.retain(|k, _| !k.starts_with('_'));
        }
        let total = batch::chars(&payload);
        Ok(Measured::new(request, total))
    }

    /// Input room left for messages: `max_input_chars` minus the fixed
    /// prompt, schema and tool overhead. Only the due volume uses it.
    fn message_budget(&self, max_input_chars: usize) -> usize {
        max_input_chars
    }

    /// Work-hours gate of automatic work for `purpose`.
    fn work_gate<'a>(&'a self, _purpose: &'static str) -> GateFuture<'a> {
        Box::pin(async { true })
    }

    /// Send one measured request, as rendered, on the AI slot the caller
    /// holds; `_batch_metrics` set on the request after measuring is
    /// logged with it. The layer counts
    /// the call (daily limit, switches, work gate) and runs
    /// `request.reserve` in one transaction first; an error there ends the
    /// batch before anything is sent. `cancel` is set on interrupt or at
    /// the deadline.
    fn invoke<'a>(&'a self, request: Measured<Request>, cancel: Cancel) -> ReplyFuture<'a>;
}

/// Cancellation first, then the deadline with `deadline_error`.
pub(crate) fn check(cancel: &Cancel, deadline: i64, deadline_error: &str) -> Result<()> {
    if cancel.is_cancelled() {
        crate::bail!("{}", crate::exec::CANCELLED);
    }
    if time::now() >= deadline {
        crate::bail!("{deadline_error}");
    }
    Ok(())
}

/// Keep the first `max` characters, marking a cut with `…`.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

// ---- runtime ---------------------------------------------------------------

/// In-memory state of the analysis domain; a restart forgets it.
#[derive(Default)]
pub struct Runtime {
    /// The claimed batch: message id → version being analysed.
    pub(crate) batch: Mutex<BTreeMap<String, String>>,
}

impl Runtime {
    pub fn analyzing(&self) -> BTreeMap<String, String> {
        self.batch.lock().unwrap().clone()
    }
}

/// The claimed batch; dropping it puts the messages back in the queue.
pub struct Claim<'a> {
    ctx: &'a Ctx,
}

/// Claim a batch for analysis; the page sees it through `analyzing`.
pub fn claim(ctx: &Ctx, versions: BTreeMap<String, String>) -> Result<Claim<'_>> {
    {
        let mut batch = ctx.analysis.batch.lock().unwrap();
        if !batch.is_empty() {
            crate::bail!("{BATCH_BUSY}");
        }
        *batch = versions.clone();
    }
    ctx.flags.set_analyzing(ctx, versions.into_keys());
    Ok(Claim { ctx })
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.ctx.analysis.batch.lock().unwrap().clear();
        self.ctx.flags.set_analyzing(self.ctx, std::iter::empty());
    }
}

// ---- entries ---------------------------------------------------------------

pub async fn handle(ctx: &Arc<Ctx>, method: &str, params: Value) -> Result<Value> {
    match method {
        "analyze" => {
            let m = layer::model(ctx).await?;
            round::run(ctx, &m, Mode::Manual, false).await
        }
        "analysis_backlog" => backlog::handle(ctx, params).await,
        _ => Err(Error::new("未知请求")),
    }
}

/// Run (manual) or check (automatic) analysis after a scan committed.
pub async fn after_scan(ctx: &Arc<Ctx>, manual: bool) -> Result<Value> {
    if !manual {
        return Ok(json!({"queued": true}));
    }
    let m = layer::model(ctx).await?;
    round::run(ctx, &m, Mode::Manual, true).await
}

/// The background loop: the filter, then automatic analysis. It sleeps
/// until the next due moment or a data change; it never polls.
pub fn spawn(ctx: &Arc<Ctx>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut changes = ctx.db.changes();
        loop {
            let wait = match layer::model(&ctx).await {
                Ok(m) => tick(&ctx, &m).await,
                Err(e) => {
                    let _ = ctx.db.record_ai_error(e.0).await;
                    Duration::from_millis(time::MINUTE as u64)
                }
            };
            let _ = tokio::time::timeout(wait, changes.changed()).await;
        }
    });
}

/// One pass of the loop; returns how long to sleep at most.
pub async fn tick(ctx: &Arc<Ctx>, m: &Arc<dyn Model>) -> Duration {
    if let Err(e) = filter::tick(ctx, m.as_ref()).await {
        let _ = ctx.db.record_ai_error(e.0).await;
    }
    if let Err(e) = round::automatic(ctx, m).await {
        let _ = ctx.db.record_ai_error(e.0).await;
    }
    let next = ctx.db.analysis_next_due(ctx.config.get()).await.unwrap_or(0);
    let now = time::now();
    let wait = if next > now { next - now } else { time::MINUTE };
    Duration::from_millis(wait.clamp(1_000, time::HOUR) as u64)
}
