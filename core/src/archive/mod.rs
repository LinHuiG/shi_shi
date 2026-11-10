//! The archive domain: archive attempts and versions, reports and the
//! morning report, and retention of what archiving has processed.
//!
//! An archive runs in batches. Each batch reads the latest committed data,
//! asks the model outside any transaction, and commits its result in one
//! transaction. A task or the archived memory changed while the model ran is
//! skipped and logged; the batch's other effects still commit. Committed
//! batches are never rolled back, and a restart fails the attempt instead of
//! replaying it.
//!
//! Model calls go through the narrow [`Model`] interface; [`layer::model`]
//! builds the AI call layer: `measure` renders prompt plus payload and
//! measures it (archive batches grow their evidence, read page by page, up to
//! the input limit), and `invoke` sends it and returns the parsed JSON reply.
//! Tests use a stub [`Model`] and never a real model.

pub mod layer;
pub mod reports;
pub mod run;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub use crate::ai::invoke::Measured;
use crate::ai::prompt::require_purpose;
use crate::exec::Cancel;
use crate::exec::ARCHIVE_BUSY;
use crate::tasks::rules;
use crate::{time, Config, Ctx, Error, Result};

pub const METHODS: &[&str] =
    &["report", "archive", "archive_history", "archive_version", "archive_rollback", "report_detail"];

pub const RECOVERY_ERROR: &str = "重启中断：未完成批次保留，已提交批次不回滚；等待手动归档，不会自动重试";
pub const MORNING_RECOVERY_ERROR: &str = "重启中断，请手动生成报表";

/// Audit `seq` the archive model has processed up to (inclusive).
pub const ARCHIVE_CURSOR: &str = "archive_ai_cursor";
/// Audit `seq` the last report covered (inclusive).
pub const REPORT_CURSOR: &str = "report_cursor";
/// Audit `seq` learning has seen (inclusive); archive moves it forward.
pub const LEARNING_CURSOR: &str = "learning_cursor";
pub const ATTEMPT_DAY: &str = "archive_attempt_day";

/// How a run was started; decides the work-hours gate and the labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A button: never gated by work hours.
    Manual,
    /// The morning report.
    Automatic,
    /// The archive the work-end boundary queued with its claim.
    Nightly,
}

/// In-memory state of the archive domain.
#[derive(Default)]
pub struct Runtime {
    /// An archive attempt is running in this process.
    pub(crate) archive_busy: AtomicBool,
    /// A report summary is being generated.
    pub(crate) summary_busy: AtomicBool,
}

// ---- model interface -----------------------------------------------------

pub type ModelFuture<'a> = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;

/// One request the domain has fully prepared: the purpose selects the
/// system prompt and model route, the payload is the user part.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub purpose: &'static str,
    pub payload: Value,
}

/// What the archive domain needs from the AI call layer.
///
/// `invoke` returns the parsed model output. For `archive` it is an object
/// with `report` (`{summary, completed, attention, next_steps}`) and, when
/// the model rewrote the archived memory after reading it in full,
/// `_archived_memory_update: {expected, before, text}`; the layer reports a
/// tool-side memory failure as `_archived_memory_error` and may add
/// `_tool_run_id` / `tool_call_ids`. For `report` it is the summary object.
pub trait Model: Send + Sync {
    /// Render and measure a request. The default counts the payload only;
    /// the real layer renders prompt and payload and keeps the render.
    fn measure(&self, request: Prepared) -> Result<Measured<Prepared>> {
        let total = request.payload.to_string().chars().count();
        Ok(Measured::new(request, total))
    }

    /// Send one measured request as rendered. `cancel` is set when the
    /// user interrupts.
    fn invoke<'a>(&'a self, request: Measured<Prepared>, cancel: Cancel) -> ModelFuture<'a>;
}

/// Run one prepared request on the shared AI queue within `timeout`. A
/// running analysis round is waited for, so it never fails an archive.
pub(crate) async fn ask(ctx: &Ctx, model: &dyn Model, request: Measured<Prepared>, timeout: Duration) -> Result<Value> {
    let source = request.request.purpose;
    ctx.exec.wait_ai(source, timeout, move |cancel| model.invoke(request, cancel)).await
}

/// An archive or report is under way; automatic analysis yields to it.
pub fn running(ctx: &Ctx) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    ctx.archive.archive_busy.load(SeqCst) || ctx.archive.summary_busy.load(SeqCst)
}

/// Other AI work must not start while an archive runs or is queued.
pub async fn archive_blocks(ctx: &Ctx) -> Result<()> {
    let queued = ctx.db.first_queued_archive().await?.is_some();
    if ctx.archive.archive_busy.load(std::sync::atomic::Ordering::SeqCst) || queued {
        crate::bail!("{ARCHIVE_BUSY}");
    }
    Ok(())
}

// ---- shared reads ---------------------------------------------------------

/// The business day of `at`: the day of the most recent work start, so an
/// overnight shift archived after midnight still belongs to its start day.
pub fn business_day(config: &Config, at: i64) -> String {
    match time::parse_hm(&config.work_start) {
        Some(start) if time::minute_of_day(at) < start => time::date_from(at - time::DAY),
        _ => time::date_from(at),
    }
}

/// One archive run: queued by a work end or started by hand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attempt {
    pub id: String,
    pub trigger: String,
    pub status: String,
    pub business_day: String,
    pub created: i64,
    pub started: i64,
    pub finished: i64,
    pub error: String,
    pub report_id: String,
    pub reused: bool,
    pub batches: i64,
    pub processed: i64,
    pub remaining: i64,
    pub archived_tasks: i64,
}

impl Attempt {
    pub fn unfinished(&self) -> bool {
        ["queued", "running"].contains(&self.status.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MorningJob {
    pub id: String,
    pub business_day: String,
    pub status: String,
    pub error: String,
    pub created: i64,
    pub finished: i64,
}

/// The named fields of `value` that it has.
pub fn pick(value: &Value, names: &[&str]) -> Value {
    Value::Object(names.iter().filter_map(|n| value.get(*n).map(|v| ((*n).into(), v.clone()))).collect())
}

/// An audit row as the domain reads it.
#[derive(Clone, Debug, Default)]
pub struct Audit {
    pub seq: i64,
    pub time: i64,
    pub kind: String,
    pub task_id: String,
    pub detail: String,
    pub change_summary: String,
    pub before: Value,
    pub after: Value,
}

impl Audit {
    /// The row as reports keep it: what it changed, not the snapshots.
    pub fn change(&self) -> Value {
        json!({
            "time": self.time, "kind": self.kind, "task_id": self.task_id, "detail": self.detail,
            "change_summary": self.change_summary, "changes": changes(&self.task_id, &self.before, &self.after),
        })
    }

    pub fn learning(&self) -> bool {
        rules::learning_evidence(&self.kind, &self.detail, &self.after)
    }
}

/// The event the model reads of an [`Audit::change`] row.
pub fn event(change: &Value) -> Value {
    let mut event = change.clone();
    event["time"] = json!(time::stamp(change["time"].as_i64().unwrap_or(0)));
    event
}

/// Task fields whose changes the model reads; the rest is bookkeeping or
/// follows from these.
const CHANGED: [&str; 11] = [
    "title",
    "description",
    "status",
    "blocker",
    "waiting_for",
    "due",
    "tags",
    "evidence",
    "archive_note",
    "archived",
    "reason",
];

/// The fields a row changed in its task. `null` for a creation or removal,
/// whose result the task itself shows. A merge row holds every merged task;
/// only the row's own one counts.
fn changes(task_id: &str, before: &Value, after: &Value) -> Value {
    let own = |v: &Value| match v.as_array() {
        Some(rows) => rows.iter().find(|r| r["id"] == task_id).cloned().unwrap_or(Value::Null),
        None => v.clone(),
    };
    let (before, after) = (own(before), own(after));
    if before.is_null() || after.is_null() {
        return Value::Null;
    }
    fields(&CHANGED, &before, &after)
}

/// The changed ones of the named fields: lists as the items added and
/// removed, objects field by field, other values as both.
fn fields(names: &[&str], before: &Value, after: &Value) -> Value {
    let mut out = Map::new();
    for name in names {
        let (b, a) = (&before[*name], &after[*name]);
        let change = match (b, a) {
            (Value::Array(old), Value::Array(new)) => {
                let added: Vec<&Value> = new.iter().filter(|v| !old.contains(v)).collect();
                let removed: Vec<&Value> = old.iter().filter(|v| !new.contains(v)).collect();
                let mut change = Map::new();
                for (key, items) in [("added", added), ("removed", removed)] {
                    if !items.is_empty() {
                        change.insert(key.into(), json!(items));
                    }
                }
                Value::Object(change)
            }
            (Value::Object(old), Value::Object(new)) => {
                let inner: std::collections::BTreeSet<&str> =
                    old.keys().chain(new.keys()).map(String::as_str).collect();
                fields(&inner.into_iter().collect::<Vec<_>>(), b, a)
            }
            _ if b == a => continue,
            _ => json!({"before": b, "after": a}),
        };
        if change.as_object().is_none_or(|c| !c.is_empty()) {
            out.insert((*name).into(), change);
        }
    }
    Value::Object(out)
}

/// The task fields the model reads. Message evidence is ids only; a
/// summary that merely cuts the description short is left out.
pub fn task_brief(t: &Value) -> Value {
    let mut brief = pick(
        t,
        &[
            "id",
            "title",
            "description",
            "summary",
            "status",
            "tags",
            "due",
            "blocker",
            "waiting_for",
            "archive_note",
            "evidence",
            "updated",
        ],
    );
    if t["summary_fallback"] == true {
        brief.as_object_mut().map(|b| b.remove("summary"));
    }
    brief
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---- IPC ------------------------------------------------------------------

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    use crate::params::Params;
    let db = &ctx.db;
    match method {
        "archive_history" => db.archive_history().await,
        "archive_version" => db.archive_version(p.str_of("id").to_string()).await,
        "archive_rollback" => db.rollback_archive(p.str_of("id").to_string(), p.str_of("expected").to_string()).await,
        "report_detail" => db.report_detail(p).await,
        "report" => {
            require_purpose(&ctx.config.get(), "report")?;
            let m = layer::model(ctx).await?;
            reports::report(ctx, m.as_ref(), "手动报表", Mode::Manual).await
        }
        "archive" => {
            if ctx.exec.ai_busy() || ctx.archive.archive_busy.load(std::sync::atomic::Ordering::SeqCst) {
                crate::bail!("AI 正在处理，请稍后手动归档");
            }
            require_purpose(&ctx.config.get(), "archive")?;
            let m = layer::model(ctx).await?;
            run::archive(ctx, m.as_ref(), "manual_archive", Mode::Manual, None).await
        }
        _ => Err(Error::new("未知请求")),
    }
}

/// Retention once at startup and then hourly; queued archives and morning
/// jobs run as soon as the model is available.
pub fn spawn(ctx: &Arc<Ctx>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut changes = ctx.db.changes();
        let mut pruned: Option<i64> = None;
        loop {
            let at = time::now();
            if pruned.is_none_or(|p| at - p >= time::HOUR) {
                pruned = Some(at);
                let days = ctx.config.get().message_retention_days;
                let _ = ctx.db.prune_archive(days, at).await;
            }
            if let Ok(m) = layer::model(&ctx).await {
                let _ = run::archive_tick(&ctx, m.as_ref()).await;
                let _ = reports::morning_tick(&ctx, m.as_ref()).await;
            }
            let _ = tokio::time::timeout(Duration::from_millis(time::HOUR as u64), changes.changed()).await;
        }
    });
}
