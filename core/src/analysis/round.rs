//! One analysis round: slot, frozen boundary, batches.
//!
//! The round holds the AI slot for all its batches. Every batch is
//! assembled from current data (read-only, never calls the model), sent
//! outside any transaction and accepted in one transaction that rechecks
//! the authority, the message versions and the task versions. Progress the
//! final bookkeeping needs is shared with the caller, because the queue
//! drops the work as soon as it is interrupted or times out.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::batch::{self, Fit};
use super::context::Reader;
use super::progress::{self, Failure};
use super::{claim, FilterTaken, Mode, Model, BATCH_DEADLINE, DISABLED, LATE_RESULT, NOTHING, NOT_DUE, ROUND_DEADLINE};
use crate::ai::prompt::require_purpose;
use crate::ai::{AUTOMATION_DISABLED, DEFERRED};
use crate::exec::{Call, Cancel, BUSY, CANCELLED};
use crate::messages::Message;
use crate::{time, Config, Ctx, Error, Result};

pub const AI_QUEUED: &str = "AI 正在运行，新消息已排队";
pub const PARTIAL_COMMANDS: &str = "部分动作因证据或事项版本变化未采用";

fn skipped(reason: &str) -> Value {
    json!({"skipped": reason})
}

/// What the round did so far; read after the queue returns.
#[derive(Default)]
struct Progress {
    /// The boundary was not empty: the round really ran.
    ran: bool,
    decision_at: Option<i64>,
    applied: usize,
    processed: usize,
    batches: usize,
}

/// Run one round. `after_scan` in manual mode waits for the AI slot
/// instead of answering busy; in automatic mode it rechecks due-ness.
pub async fn run(ctx: &Arc<Ctx>, m: &Arc<dyn Model>, mode: Mode, after_scan: bool) -> Result<Value> {
    let config = ctx.config.get();
    if mode == Mode::Automatic && !config.automatic_analysis_enabled() {
        return Ok(skipped(DISABLED));
    }
    if mode == Mode::Automatic && !m.work_gate("classify").await {
        return Ok(skipped(DEFERRED));
    }
    let timeout = Duration::from_secs(config.ai_timeout_seconds.max(1));
    let deadline = time::now() + timeout.as_millis() as i64;
    let call = if after_scan && mode == Mode::Manual {
        ctx.exec.wait_call("classify", std::time::Instant::now() + timeout).await?
    } else {
        ctx.exec.try_call("classify")?
    };
    let cancel = call.cancel.clone();
    let progress = Arc::new(Mutex::new(Progress::default()));
    let call = Mutex::new(call);
    let until = tokio::time::Instant::now() + Duration::from_millis((deadline - time::now()).max(0) as u64);
    let out = tokio::select! {
        result = body(ctx, m, mode, after_scan, deadline, cancel.clone(), progress.clone(), &call) => result,
        _ = cancel.cancelled() => Err(Error::new(CANCELLED)),
        _ = tokio::time::sleep_until(until) => {
            cancel.cancel();
            Err(Error::new(ROUND_DEADLINE))
        }
    };
    let out = if call.lock().unwrap().finish() && out.is_ok() { Err(Error::new(CANCELLED)) } else { out };
    finish(ctx, deadline, &cancel, out, &progress).await
}

/// Error rewriting and the final bookkeeping of a round that ran.
async fn finish(
    ctx: &Ctx,
    deadline: i64,
    cancel: &Cancel,
    out: Result<Value>,
    progress: &Mutex<Progress>,
) -> Result<Value> {
    let (ran, decision_at, applied, processed, batches) = {
        let p = progress.lock().unwrap();
        (p.ran, p.decision_at, p.applied, p.processed, p.batches)
    };
    if !ran {
        return out;
    }
    let interrupted = ctx.exec.last_interrupt();
    let out = match out {
        Err(_) if cancel.is_cancelled() && interrupted.is_some() => Err(Error::new(CANCELLED)),
        Err(_) if time::now() >= deadline => Err(Error::new(ROUND_DEADLINE)),
        out => out,
    };
    // The trigger time was set when the round started and stays, whatever
    // the outcome; a cancellation only records the interruption.
    let cancelled = out.as_ref().is_err_and(|e| e.0 == CANCELLED);
    if let Err(e) = ctx.db.finish_analysis_round(cancelled, decision_at, interrupted).await {
        let _ = ctx.db.log("analysis_finish_failed", json!({"error": e.0})).await;
    }
    match out {
        Err(e) if batches > 0 && e.0 != CANCELLED => {
            let detail = format!(
                "已提交 {batches} 批、处理 {processed} 条消息、采用 {applied} 个动作，已提交结果保留，其余消息仍待处理"
            );
            let row =
                json!({"error": e.0, "detail": detail, "batches": batches, "processed": processed, "applied": applied});
            let _ = ctx.db.log("analysis_partial", row).await;
            Err(Error(format!("{}；{detail}", e.0)))
        }
        Err(e) if batches == 0 && e.0 == AUTOMATION_DISABLED => Ok(skipped(DISABLED)),
        out => out,
    }
}

pub(crate) enum Start {
    Skip(&'static str),
    Go { boundary: VecDeque<(String, String)>, decision_at: Option<i64> },
}

/// The round inside the AI slot.
#[allow(clippy::too_many_arguments)]
async fn body(
    ctx: &Arc<Ctx>,
    m: &Arc<dyn Model>,
    mode: Mode,
    after_scan: bool,
    deadline: i64,
    cancel: Cancel,
    progress: Arc<Mutex<Progress>>,
    call: &Mutex<Call>,
) -> Result<Value> {
    let config = ctx.config.get();
    let gate = mode == Mode::Manual || m.work_gate("classify").await;
    let budget = m.message_budget(config.max_input_chars);
    require_purpose(&config, "classify")?;
    let start = ctx.db.start_analysis_round(config, mode, gate, after_scan, budget).await?;
    let (mut boundary, decision_at) = match start {
        Start::Skip(reason) => return Ok(skipped(reason)),
        Start::Go { boundary, decision_at } => (boundary, decision_at),
    };
    if boundary.is_empty() {
        return Ok(skipped(NOTHING));
    }
    {
        let mut p = progress.lock().unwrap();
        p.ran = true;
        p.decision_at = decision_at;
    }
    let mut steps = Steps { ctx, m, mode, deadline, cancel: &cancel, decision_at, call, first: true };
    while !boundary.is_empty() {
        if time::now() >= deadline {
            crate::bail!("{ROUND_DEADLINE}");
        }
        let Some((applied, processed)) = steps.batch(&mut boundary).await? else { break };
        let mut p = progress.lock().unwrap();
        p.applied += applied;
        p.processed += processed;
        p.batches += 1;
    }
    let remaining = ctx.db.analysis_remaining(ctx.config.get()).await?;
    let p = progress.lock().unwrap();
    Ok(json!({"applied": p.applied, "processed": p.processed, "batches": p.batches, "remaining": remaining}))
}

/// What the accept transaction decided.
pub(crate) enum Accepted {
    /// `notices`: `(task id, title, summary)` of the committed changes.
    Done {
        applied: usize,
        processed: usize,
        notices: Vec<(String, String, String)>,
    },
    Rejected(String),
}

/// A prepared batch read from current data.
pub(crate) struct Prepared {
    pub authority: String,
    pub boundary: VecDeque<(String, String)>,
    pub batch: Option<(batch::Batch, Vec<String>)>,
}

struct Steps<'a> {
    ctx: &'a Arc<Ctx>,
    m: &'a Arc<dyn Model>,
    mode: Mode,
    deadline: i64,
    cancel: &'a Cancel,
    decision_at: Option<i64>,
    /// The held slot; every request after the first gets a new id.
    call: &'a Mutex<Call>,
    first: bool,
}

impl Steps<'_> {
    fn check(&self, deadline_error: &str) -> Result<()> {
        super::check(self.cancel, self.deadline, deadline_error)
    }

    /// One batch; `None` when the boundary has nothing eligible left.
    async fn batch(&mut self, boundary: &mut VecDeque<(String, String)>) -> Result<Option<(usize, usize)>> {
        let ctx = self.ctx;
        let m = self.m;
        loop {
            self.check(BATCH_DEADLINE)?;
            if self.mode == Mode::Automatic && !m.work_gate("classify").await {
                crate::bail!("{DEFERRED}");
            }
            let config = ctx.config.get();
            let budget = m.message_budget(config.max_input_chars);
            let prepared = ctx.db.prepare_analysis_batch(config.clone(), self.mode, boundary.clone(), budget).await?;
            *boundary = prepared.boundary;
            let authority = prepared.authority;
            let Some((mut fresh, ids)) = prepared.batch else { return Ok(None) };
            fresh.payload["_filter_decision_at"] = json!(self.decision_at);
            let check = || self.check(BATCH_DEADLINE);
            let (fitted, mut request) = match batch::fit(m.as_ref(), fresh, config.max_input_chars, None, &check) {
                Ok(found) => found,
                // Only this message is too large: skip it this round, nothing
                // is recorded against it; the next round measures it again.
                Err(Fit::TooLarge { error, alone: true, .. }) => {
                    self.check(BATCH_DEADLINE)?;
                    let Some(first) = ids.first().cloned() else { return Ok(None) };
                    let row = json!({"id": first, "error": error});
                    ctx.db.log("analysis_input_skipped", row).await?;
                    while let Some((id, _)) = boundary.pop_front() {
                        if id == first {
                            break;
                        }
                    }
                    continue;
                }
                Err(Fit::TooLarge { error, alone: false, log }) => {
                    ctx.db.log("analysis_input_limit", log).await?;
                    return Err(Error(error));
                }
                Err(Fit::Stop(e)) => return Err(e),
            };
            let remaining: Vec<String> =
                ids.iter().filter(|id| !fitted.fingerprints.contains_key(*id)).cloned().collect();
            let logs: Vec<(&'static str, Value)> = batch::trim_logs(&fitted.payload, &remaining);
            let current = ctx
                .db
                .analysis_batch_current(
                    config,
                    logs,
                    authority.clone(),
                    fitted.fingerprints.clone(),
                    fitted.versions.clone(),
                )
                .await?;
            if !current || fitted.fingerprints.is_empty() {
                continue;
            }
            let _claim = claim(ctx, fitted.fingerprints.clone())?;
            self.check(LATE_RESULT)?;
            if !std::mem::take(&mut self.first) {
                self.call.lock().unwrap().next_request()?;
            }
            request.request.reserve =
                Some(FilterTaken { decision_at: self.decision_at, observed: fitted.fingerprints.clone() });
            let config = ctx.config.get();
            let reader = Arc::new(Reader::new(ctx.db.clone(), &config, &request.request.payload));
            request.request.reader = Some(reader.clone());
            let wait = Duration::from_millis((self.deadline - time::now()).max(1_000) as u64);
            let reply = match tokio::time::timeout(wait, m.invoke(request, self.cancel.clone())).await {
                Ok(reply) => reply,
                Err(_) => super::Reply::err(LATE_RESULT),
            };
            let read = reader.accepted();
            let mut allowed = fitted.allowed.clone();
            allowed.extend(read.messages);
            let mut versions = fitted.versions.clone();
            versions.extend(read.task_versions);
            let request_id = self.call.lock().unwrap().id().to_string();
            let value = match reply.result {
                Ok(value) => value,
                Err(e) => {
                    let settled = !self.cancel.is_cancelled() && time::now() < self.deadline;
                    // Only a refusal of one message is an input-data error;
                    // a refused batch cannot be attributed to any message.
                    let kind = if reply.commands_ran {
                        Some(Failure::CommandUnconfirmed)
                    } else if !settled {
                        None
                    } else if reply.refused && fitted.fingerprints.len() == 1 {
                        Some(Failure::InputData)
                    } else if reply.invalid_response {
                        Some(Failure::InvalidResult)
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        let (fps, error) = (fitted.fingerprints.clone(), e.0.clone());
                        let saved = ctx
                            .db
                            .record_batch_failure(config, authority.clone(), fps, kind, error, reply.commands_ran)
                            .await;
                        if let (Err(save), true) = (saved, reply.commands_ran) {
                            return Err(Error(format!("{}；命令重试保护未能保存：{}", e.0, save.0)));
                        }
                    }
                    return Err(e);
                }
            };
            let accept = Accept {
                config,
                authority,
                payload: fitted.payload.clone(),
                fingerprints: fitted.fingerprints.clone(),
                allowed,
                versions,
                value,
                commands_ran: reply.commands_ran,
                cancel: self.cancel.clone(),
                deadline: self.deadline,
                request_id,
            };
            match ctx.db.accept_analysis_batch(accept).await? {
                Accepted::Rejected(e) => return Err(Error(e)),
                Accepted::Done { applied, processed, notices } => {
                    for (id, title, body) in notices {
                        ctx.host.event(crate::HostEvent::Notify { title, body, task_id: Some(id) });
                    }
                    let last = fitted.fingerprints.keys().cloned().collect::<BTreeSet<_>>();
                    if let Some(at) = boundary.iter().rposition(|(id, _)| last.contains(id)) {
                        boundary.drain(..=at);
                    }
                    return Ok(Some((applied, processed)));
                }
            }
        }
    }
}

/// The accept transaction of one batch (§3.2, §4.3).
pub(crate) struct Accept {
    pub config: Arc<Config>,
    pub authority: String,
    pub payload: Value,
    pub fingerprints: BTreeMap<String, String>,
    pub allowed: BTreeMap<String, Message>,
    pub versions: BTreeMap<String, i64>,
    pub value: Value,
    pub commands_ran: bool,
    pub cancel: Cancel,
    pub deadline: i64,
    pub request_id: String,
}

impl Accept {
    pub(crate) fn check(&self) -> Result<()> {
        super::check(&self.cancel, self.deadline, LATE_RESULT)
    }
}

/// Check automatic analysis and run it when due. The round rechecks
/// due-ness against the latest trigger time when it starts; a busy slot is
/// a skip, never an error, so the next trigger is not held up.
pub async fn automatic(ctx: &Arc<Ctx>, m: &Arc<dyn Model>) -> Result<Value> {
    let config = ctx.config.get();
    if !config.automatic_analysis_enabled() {
        return Ok(skipped(DISABLED));
    }
    if !m.work_gate("classify").await {
        return Ok(skipped(DEFERRED));
    }
    let budget = m.message_budget(config.max_input_chars);
    let (s, backlog, early) = ctx.db.analysis_probe(config.clone()).await?;
    let pending = backlog.count > 0;
    let now = time::now();
    if s.pending != pending || (pending && s.pending_since == 0) {
        ctx.db.mark_analysis_pending(pending, now).await?;
    }
    if ctx.exec.ai_busy() || crate::archive::running(ctx) {
        return Ok(skipped(AI_QUEUED));
    }
    if !progress::automatic_due(&s, early.is_some(), &config, backlog, budget, now) {
        return Ok(skipped(NOT_DUE));
    }
    match run(ctx, m, Mode::Automatic, true).await {
        Err(e) if e.0 == BUSY => Ok(skipped(AI_QUEUED)),
        out => out,
    }
}
