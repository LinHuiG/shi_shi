//! One archive attempt: batches prepared from the latest committed data,
//! one model request per batch outside any transaction, and one commit per
//! batch. Conflicting edits made while the model ran are skipped and logged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Attempt, Audit, Measured, Mode, Model, Prepared};
use crate::ai::prompt::require_purpose;
use crate::prompts::memory;
use crate::tasks::model as tasks;
use crate::{exec, time, Ctx, Error, Result};

pub const BUSY_SKIP: &str = "AI 正在处理，请稍后手动归档";
const INPUT_LIMIT: &str = "归档的单条材料及必需上下文已超过单次输入上限，未调用 AI；请精简配置或提高输入上限后手动归档";
const TOTAL_TIMEOUT_NEXT: &str = "归档总时限已到，后续批次未提交；请手动继续归档";
const TOTAL_TIMEOUT_BATCH: &str = "归档总时限已到，本批未提交；请手动继续归档";
const TOTAL_TIMEOUT_KEPT: &str = "归档总时限已到，本批未提交；已提交批次保留，请手动继续归档";
const NOTE_LIMIT: usize = 2000;

/// Evidence rows read per page at first; each later page doubles what is
/// read so far.
#[cfg(not(test))]
pub(crate) const PAGE: usize = 64;
#[cfg(test)]
pub(crate) const PAGE: usize = 2;

/// What a batch is built from. The base and the first evidence page are
/// read in one short transaction; further pages are read only while the
/// batch still fits the input budget.
pub struct Material {
    pub(crate) cursor: i64,
    pub(crate) max_seq: i64,
    pub(crate) since: i64,
    /// Evidence rows of tasks in scope after the cursor read so far, oldest first.
    pub(crate) evidence: Vec<Audit>,
    /// No further evidence rows exist after the read ones.
    pub(crate) exhausted: bool,
    /// All evidence rows of tasks in scope after the cursor.
    pub(crate) total: i64,
    /// The tasks of the read evidence rows.
    pub(crate) linked: BTreeMap<String, tasks::Task>,
    /// Tasks in scope without evidence after the cursor, read only for a
    /// batch that takes all evidence.
    pub(crate) current: Vec<tasks::Task>,
    pub(crate) omitted: i64,
    /// Closed, unarchived tasks with their `updated` and the seq of their
    /// latest evidence after the cursor.
    pub(crate) closed: Vec<(String, i64, Option<i64>)>,
    pub(crate) status_counts: Value,
    pub(crate) tag_catalog: Value,
    pub(crate) memory: memory::Memory,
    pub(crate) temporary: String,
    pub(crate) data_until: i64,
    pub(crate) pending: i64,
    pub(crate) report_policy: String,
}

/// One page of evidence after `after` with the tasks it links.
pub struct Page {
    pub(crate) rows: Vec<Audit>,
    pub(crate) linked: Vec<tasks::Task>,
    pub(crate) exhausted: bool,
    pub(crate) total: i64,
    pub(crate) max_seq: i64,
}

impl Material {
    pub(crate) fn add(&mut self, page: Page) {
        self.evidence.extend(page.rows);
        self.linked.extend(page.linked.into_iter().map(|t| (t.id.clone(), t)));
        self.exhausted = page.exhausted;
        self.total = page.total;
        self.max_seq = page.max_seq;
    }
}

/// Read the next evidence page, as large as what is read so far.
async fn more(ctx: &Ctx, m: &mut Material) -> Result<()> {
    let (cursor, since, limit) = (m.cursor, m.since, m.evidence.len().max(PAGE));
    let after = m.evidence.last().map_or(cursor, |a| a.seq);
    m.add(ctx.db.archive_page(cursor, since, after, limit).await?);
    Ok(())
}

/// Read the next page of tasks without evidence, as large as what is read
/// so far; `true` once none are left.
async fn current(ctx: &Ctx, m: &mut Material) -> Result<bool> {
    let (after, limit) = (m.current.last().map_or(i64::MIN, |t| t.number), m.current.len().max(PAGE));
    let page = ctx.db.archive_current_tasks(m.since, m.cursor, m.max_seq, after, limit).await?;
    let exhausted = page.len() < limit;
    m.current.extend(page);
    Ok(exhausted)
}

/// One batch: the model request and what its commit may change.
#[derive(Clone, Debug)]
pub struct Batch {
    pub manual: bool,
    pub trigger: String,
    pub attempt_id: String,
    pub business_day: String,
    pub payload: Value,
    pub start: i64,
    pub cutoff: i64,
    pub evidence: Vec<i64>,
    pub report: Value,
    /// Task ids with the `updated` the batch saw.
    pub tasks_to_archive: Vec<(String, i64)>,
    pub remaining: i64,
    pub memory_revision: i64,
    pub memory_text: String,
}

impl Batch {
    pub fn has_ai_input(&self) -> bool {
        !self.evidence.is_empty()
            || ["report_tasks", "current_tasks"]
                .iter()
                .any(|k| self.payload[*k].as_array().is_some_and(|r| !r.is_empty()))
            || self.manual && !self.memory_text.trim().is_empty()
    }
}

fn note(a: &Audit) -> Option<String> {
    a.after["archive_note"].as_str().filter(|n| !n.trim().is_empty()).map(|n| n.chars().take(NOTE_LIMIT).collect())
}

/// Build a batch of the first `count` read evidence rows.
pub fn build(m: &Material, attempt: &Attempt, mode: Mode, at: i64, count: usize, include_current: bool) -> Batch {
    let taken = &m.evidence[..count];
    let cutoff = match m.evidence.get(count) {
        Some(next) => next.seq - 1,
        None if m.exhausted => m.max_seq.max(taken.last().map_or(m.cursor, |a| a.seq)),
        None => taken.last().map_or(m.cursor, |a| a.seq),
    };
    let mut rows = Vec::new();
    let mut learning = Vec::new();
    for a in taken {
        let mut event = a.event();
        if let Some(n) = note(a) {
            event["archive_note"] = json!(n);
        }
        rows.push(json!({"audit_index": a.seq, "evidence_id": format!("audit:{}", a.seq), "event": event}));
        if a.learning() {
            learning.push(a.seq);
        }
    }
    let evidence: Vec<i64> = taken.iter().map(|a| a.seq).collect();
    let last = taken.last().map(|a| a.seq);
    let ids: BTreeSet<&str> = taken.iter().map(|a| a.task_id.as_str()).collect();
    let mut related: Vec<&tasks::Task> = ids.iter().filter_map(|id| m.linked.get(*id)).collect();
    related.sort_by_key(|t| t.number);
    // A task waits until its latest event after the cursor is in the batch.
    let tasks_to_archive: Vec<(String, i64)> = m
        .closed
        .iter()
        .filter(|(_, _, latest)| latest.is_none_or(|s| last.is_some_and(|l| s <= l)))
        .map(|(id, updated, _)| (id.clone(), *updated))
        .collect();
    let remaining = (m.total - count as i64).max(0);
    let current: &[tasks::Task] = if include_current { &m.current } else { &[] };
    let mut reported: Vec<&tasks::Task> = related.iter().copied().chain(current).collect();
    reported.sort_by_key(|t| t.number);
    let manual = mode == Mode::Manual;
    let trigger = if manual { "manual_archive" } else { "nightly_archive" };
    let notes: Vec<Value> = taken
        .iter()
        .filter_map(|a| {
            (a.kind == "manual_archive")
                .then(|| note(a))
                .flatten()
                .map(|n| json!({"evidence_id": format!("audit:{}", a.seq), "task_id": a.task_id, "note": n}))
        })
        .collect();
    let mut payload = json!({
        "evidence": rows,
        "archive_note_sources": notes,
        "learning_evidence_ids": learning.iter().map(|s| format!("audit:{s}")).collect::<Vec<_>>(),
        "coverage": {"status_counts": m.status_counts, "remaining_events": remaining, "omitted_tasks": 0},
        "tag_catalog": m.tag_catalog,
        "_trigger": attempt.trigger,
        "_archive_attempt_id": attempt.id,
        "_data_until": m.data_until,
        "report_tasks": related.iter().map(|t| super::task_brief(t)).collect::<Vec<_>>(),
        "current_tasks": current.iter().map(super::task_brief).collect::<Vec<_>>(),
        "_archived_memory": m.memory.text,
        "_temporary_memory": m.temporary,
        "_local_context": {"archived_memory": {"revision": m.memory.revision}, "memory": {"writable": false}},
        "memory": {"writable": false},
        "_context_frozen": true,
    });
    if !m.report_policy.trim().is_empty() {
        payload["report_policy"] = json!(m.report_policy);
    }
    let report = json!({
        "id": super::new_id(),
        "time": at,
        "date": attempt.business_day,
        "label": if manual { "手动归档" } else { "下班归档" },
        "trigger": trigger,
        "tasks": reported.iter().map(|t| t.payload()).collect::<Vec<_>>(),
        "status_counts": m.status_counts,
        "delta": taken.iter().map(Audit::json).collect::<Vec<_>>(),
        "suggestions": [],
        "summary_status": "pending",
        "data_until": m.data_until,
        "pending_analysis": m.pending,
        "remaining_events": remaining,
    });
    Batch {
        manual,
        trigger: attempt.trigger.clone(),
        attempt_id: attempt.id.clone(),
        business_day: attempt.business_day.clone(),
        payload,
        start: m.cursor,
        cutoff,
        evidence,
        report,
        tasks_to_archive,
        remaining,
        memory_revision: m.memory.revision,
        memory_text: m.memory.text.clone(),
    }
}

fn prepared(b: &Batch) -> Prepared {
    Prepared { purpose: "archive", payload: b.payload.clone() }
}

/// The largest batch whose rendered request fits `max_chars`. Evidence is
/// read page by page while the batch still fits; a batch that takes all
/// evidence also takes the unrelated current tasks when they fit. The
/// request comes back measured, to be sent as rendered; `None` means there
/// is nothing for the model.
pub async fn fit(
    ctx: &Ctx,
    model: &dyn Model,
    attempt: &Attempt,
    mode: Mode,
    at: i64,
    max_chars: usize,
) -> Result<(Batch, Option<Measured<Prepared>>)> {
    let mut m = ctx.db.archive_material().await?;
    let narrow = |m: &Material, count: usize| {
        let mut b = build(m, attempt, mode, at, count, false);
        b.payload["coverage"]["omitted_current_tasks"] = json!(m.omitted);
        b
    };
    let mut best: Option<(usize, Batch, Measured<Prepared>)> = None;
    let mut over = None;
    loop {
        let n = m.evidence.len();
        if n > best.as_ref().map_or(0, |b| b.0) {
            let b = narrow(&m, n);
            let measured = model.measure(prepared(&b))?;
            if measured.total > max_chars {
                over = Some(n);
                break;
            }
            best = Some((n, b, measured));
        }
        if m.exhausted {
            break;
        }
        more(ctx, &mut m).await?;
    }
    let Some(over) = over else {
        // Current tasks are read page by page and only while all of them
        // can still fit; they join the batch only when all of them do.
        loop {
            let exhausted = current(ctx, &mut m).await?;
            let all = build(&m, attempt, mode, at, m.evidence.len(), true);
            if exhausted && !all.has_ai_input() {
                return Ok((all, None));
            }
            let measured = model.measure(prepared(&all))?;
            if measured.total > max_chars {
                break;
            }
            if exhausted {
                return Ok((all, Some(measured)));
            }
        }
        let (b, measured) = match best {
            Some((_, b, measured)) => (b, measured),
            None => {
                let b = narrow(&m, 0);
                let measured = model.measure(prepared(&b))?;
                if measured.total > max_chars {
                    crate::bail!("{INPUT_LIMIT}");
                }
                (b, measured)
            }
        };
        return Ok((b, Some(measured)));
    };
    let (mut low, mut high) = (best.as_ref().map_or(0, |b| b.0) + 1, over - 1);
    while low <= high {
        let mid = low + (high - low) / 2;
        let b = narrow(&m, mid);
        let measured = model.measure(prepared(&b))?;
        if measured.total <= max_chars {
            best = Some((mid, b, measured));
            low = mid + 1;
        } else {
            high = mid - 1;
        }
    }
    let (_, b, measured) = best.ok_or_else(|| Error::new(INPUT_LIMIT))?;
    Ok((b, Some(measured)))
}

async fn fail_attempt(ctx: &Ctx, id: &str, error: &str) -> Result<()> {
    ctx.db.fail_archive(id.into(), error.into()).await
}

/// Run one attempt through all its batches. `existing` is a queued
/// work-end attempt; without it this is a manual archive.
pub async fn archive(
    ctx: &Ctx,
    model: &dyn Model,
    trigger: &str,
    mode: Mode,
    existing: Option<Attempt>,
) -> Result<Value> {
    let claim = if ctx.exec.ai_busy() { None } else { exec::Flag::claim(&ctx.archive.archive_busy) };
    let Some(_release) = claim else {
        return Ok(match &existing {
            Some(a) => json!({"queued": true, "deduplicated": true, "attempt": a.id}),
            None => json!({"skipped": BUSY_SKIP}),
        });
    };
    let at = time::now();
    if let Some(a) = &existing {
        let checked = ctx.db.check_archive_claim(a.id.clone(), ctx.config.get(), at).await?;
        if let Err(e) = checked {
            fail_attempt(ctx, &a.id, &e).await?;
            return Err(Error(e));
        }
    } else if mode != Mode::Manual {
        return Err(Error::new(crate::ai::DEFERRED));
    }
    let config = ctx.config.get();
    if let Err(e) = require_purpose(&config, "archive") {
        if let Some(a) = &existing {
            fail_attempt(ctx, &a.id, &e.0).await?;
        }
        return Err(e);
    }
    let (trigger, day) = (trigger.to_string(), super::business_day(&config, at));
    let attempt = ctx.db.start_archive(trigger, day, at, existing).await?;
    let result = batches(ctx, model, &attempt, mode, at).await;
    if let Err(e) = &result {
        fail_attempt(ctx, &attempt.id, &e.0).await?;
    }
    result
}

async fn batches(ctx: &Ctx, model: &dyn Model, attempt: &Attempt, mode: Mode, at: i64) -> Result<Value> {
    let config = ctx.config.get();
    let started = Instant::now();
    let deadline = Duration::from_secs(config.ai_timeout_seconds.max(1));
    loop {
        let live = ctx.db.archive_attempt(attempt.id.clone()).await?;
        if live.is_none_or(|a| a.status != "running") {
            crate::bail!("{}", exec::CANCELLED);
        }
        if started.elapsed() >= deadline {
            crate::bail!("{TOTAL_TIMEOUT_NEXT}");
        }
        let (batch, request) = fit(ctx, model, attempt, mode, at, config.max_input_chars).await?;
        ctx.db.set_archive_remaining(attempt.id.clone(), batch.evidence.len() as i64 + batch.remaining).await?;
        let Some(request) = request else {
            if batch.remaining > 0 {
                crate::bail!("归档材料无法装入单批，游标和事项保留");
            }
            if started.elapsed() >= deadline {
                crate::bail!("{TOTAL_TIMEOUT_BATCH}");
            }
            return ctx.db.commit_archive_without_ai(batch, time::now()).await.map_err(|e| {
                Error(format!("归档保存失败，未提交报表/游标/成功状态；内存已标记失败，等待手动归档：{e}"))
            });
        };
        let left = deadline.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
        let result = match super::ask(ctx, model, request, left).await {
            Err(_) if started.elapsed() >= deadline => crate::bail!("{TOTAL_TIMEOUT_KEPT}"),
            other => other?,
        };
        if started.elapsed() >= deadline {
            crate::bail!("{TOTAL_TIMEOUT_BATCH}");
        }
        let (cutoff, start, remaining) = (batch.cutoff, batch.start, batch.remaining);
        let committed = ctx.db.commit_archive_batch(batch, result, time::now()).await.map_err(|e| {
            Error(format!(
                "归档保存失败，结果、游标、事项归档标记和临时记忆均未提交；内存已标记失败，等待手动归档：{e}"
            ))
        })?;
        let report = committed.map_err(Error)?;
        if remaining == 0 {
            return Ok(report);
        }
        if cutoff <= start {
            crate::bail!("归档游标无进展，已停止后续批次");
        }
    }
}

/// Run the first queued work-end archive once the AI queue is idle.
pub async fn archive_tick(ctx: &Ctx, model: &dyn Model) -> Result<Value> {
    if ctx.exec.ai_busy() || ctx.archive.archive_busy.load(Ordering::SeqCst) {
        return Ok(json!({"skipped": "归档已排队，等待当前 AI 结束"}));
    }
    match ctx.db.first_queued_archive().await? {
        Some(a) => archive(ctx, model, &a.trigger.clone(), Mode::Nightly, Some(a)).await,
        None => Ok(json!({"skipped": "归档失败或等待下次到点；失败仅手动重试"})),
    }
}
