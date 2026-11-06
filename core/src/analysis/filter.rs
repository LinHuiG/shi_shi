//! The early message filter: a cheap model looks at pending messages and
//! decides whether full analysis should run before its interval.
//!
//! It never marks a message analysed. What it has looked at lives in
//! `messages.filter_hash` (current when equal to `content_hash`, so an edit
//! makes the message new again); its last decision in the one-row
//! `filter_state` table. Call and token counts belong to the AI layer's
//! daily usage.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::{batch, Model, Request, FILTER_TIMEOUT};
use crate::ai::{AUTOMATION_DISABLED, INPUT_LIMIT_ERROR, UNCONFIGURED_PREFIX};
use crate::exec::{Call, Cancel, BUSY, CANCELLED};
use crate::{time, Ctx, Error, Result};

/// Minimum gap between two filter runs.
pub const COOLDOWN: i64 = time::MINUTE;
/// Body characters of a filtered message.
pub const BODY_CHARS: usize = 2000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterState {
    pub last_run: i64,
    /// Full analysis should run now.
    pub early: bool,
    /// Why it runs early: `message_filter` or `message_filter_failed`.
    pub trigger: String,
    pub reason: String,
}

/// Inputs fixed when the filter decides whether to start.
pub(crate) struct Gate {
    pub open: bool,
    pub message_budget: usize,
}

/// A batch picked from the queue (see `Db::pick_filter_batch`).
pub(crate) struct Picked {
    pub queue: VecDeque<(String, String)>,
    pub rows: Vec<Value>,
    pub versions: Vec<(String, String)>,
    pub bounded: bool,
}

/// One filter pass: drain the pending versions in batches until a batch
/// asks for full analysis, fails or is cancelled. Busy AI is not an error.
pub async fn tick(ctx: &Arc<Ctx>, m: &dyn Model) -> Result<()> {
    if ctx.exec.ai_busy() {
        return Ok(());
    }
    let config = ctx.config.get();
    let first = gate(ctx, m).await;
    let start = time::now();
    let found = ctx.db.filter_boundary(config, first, start).await?;
    if found.is_none() || ctx.exec.ai_busy() {
        return Ok(());
    }
    let timeout = Duration::from_secs(ctx.config.get().ai_timeout_seconds.max(1));
    let deadline = time::now() + timeout.as_millis() as i64;
    // The batch whose call is in flight; committed outside when the queue
    // drops the work on interruption or at its timeout.
    let in_flight: Arc<Mutex<Option<Flight>>> = Arc::default();
    let call = match ctx.exec.try_call("message_filter") {
        Ok(call) => call,
        Err(e) if e.0 == BUSY => return Ok(()),
        Err(e) => return Err(e),
    };
    let cancel = call.cancel.clone();
    let call = Mutex::new(call);
    let out = tokio::select! {
        result = run(ctx, m, deadline, cancel.clone(), in_flight.clone(), &call) => result,
        _ = cancel.cancelled() => Err(Error::new(CANCELLED)),
        _ = tokio::time::sleep(timeout) => {
            cancel.cancel();
            Err(Error(format!("AI 调用超时（{} 秒）", timeout.as_secs())))
        }
    };
    let out = if call.lock().unwrap().finish() && out.is_ok() { Err(Error::new(CANCELLED)) } else { out };
    let flight = in_flight.lock().unwrap().take();
    match (out, flight) {
        (Err(e), Some(flight)) => commit(ctx, flight, Err(e), deadline, &cancel).await.map(|_| ()),
        (out, _) => out,
    }
}

/// The gate as of now: work hours and message budget.
async fn gate(ctx: &Ctx, m: &dyn Model) -> Gate {
    Gate {
        open: m.work_gate("message_filter").await,
        message_budget: m.message_budget(ctx.config.get().max_input_chars),
    }
}

/// A batch on its way: what a commit needs.
#[derive(Clone)]
pub(crate) struct Flight {
    pub authority: String,
    pub versions: BTreeMap<String, String>,
}

async fn run(
    ctx: &Arc<Ctx>,
    m: &dyn Model,
    deadline: i64,
    cancel: Cancel,
    in_flight: Arc<Mutex<Option<Flight>>>,
    call: &Mutex<Call>,
) -> Result<()> {
    // Recheck now the slot is ours; from here the cooldown runs.
    let config = ctx.config.get();
    let g = gate(ctx, m).await;
    let started = ctx.db.start_filter(config, g).await?;
    let Some((mut boundary, fixed)) = started else { return Ok(()) };
    let check = || super::check(&cancel, deadline, FILTER_TIMEOUT);
    let mut index = 0usize;
    loop {
        let config = ctx.config.get();
        let open = gate(ctx, m).await.open;
        let max_input = config.max_input_chars;
        let picked = ctx.db.pick_filter_batch(config, open, fixed.clone(), boundary.clone()).await?;
        let Some(Picked { queue, rows, versions, bounded }) = picked else { return Ok(()) };
        boundary = queue;
        if rows.is_empty() {
            return Ok(());
        }
        let pending = boundary.len();
        let request = |n: usize| Request {
            purpose: "message_filter",
            payload: json!({"changes": &rows[..n], "_trigger": "scan_batch"}),
            reader: None,
            reserve: None,
        };
        let fitted = check().and_then(|_| {
            let all = m.measure(request(rows.len()))?;
            if all.total <= max_input {
                return Ok((rows.len(), all));
            }
            let found = batch::fit_prefix(rows.len().saturating_sub(1), |n| {
                check()?;
                let r = m.measure(request(n))?;
                Ok((r.total <= max_input).then_some(r))
            })?;
            check()?;
            found.ok_or_else(|| {
                Error(format!("{INPUT_LIMIT_ERROR}：单条消息及必需过滤上下文超过单次输入上限，未调用 AI"))
            })
        });
        let (selected, mut request) = match fitted {
            Ok(found) => found,
            Err(e) => {
                let flight = Flight { authority: fixed.clone(), versions: BTreeMap::new() };
                commit(ctx, flight, Err(e), deadline, &cancel).await?;
                return Ok(());
            }
        };
        let chosen: BTreeMap<String, String> = versions[..selected].iter().cloned().collect();
        let stale = ctx.db.filter_batch_stale(chosen.clone()).await?;
        if stale {
            continue;
        }
        let last = &versions[selected - 1].0;
        while let Some((id, _)) = boundary.pop_front() {
            if &id == last {
                break;
            }
        }
        index += 1;
        let total = request.total;
        request.request.payload["_batch_metrics"] = json!({
            "index": index, "pending": pending, "selected": selected, "remaining": boundary.len(),
            "initial_selected": rows.len(), "message_chars": batch::chars(&json!(&rows[..selected])),
            "total_input_chars": total, "max_input_chars": max_input,
            "reduced_by": if bounded || selected < rows.len() { vec!["input_budget"] } else { vec![] },
        });
        if index > 1 {
            call.lock().unwrap().next_request()?;
        }
        let flight = Flight { authority: fixed.clone(), versions: chosen };
        *in_flight.lock().unwrap() = Some(flight.clone());
        let remaining = Duration::from_millis((deadline - time::now()).max(1_000) as u64);
        let result = match check() {
            Err(e) => Err(e),
            Ok(()) => match tokio::time::timeout(remaining, m.invoke(request, cancel.clone())).await {
                Ok(reply) => reply.result,
                Err(_) => Err(Error::new(FILTER_TIMEOUT)),
            },
        };
        let go_on = commit(ctx, flight, result, deadline, &cancel).await?;
        *in_flight.lock().unwrap() = None;
        if !go_on {
            return Ok(());
        }
    }
}

/// Commit one batch's decision; returns whether to filter the next one.
async fn commit(ctx: &Ctx, flight: Flight, result: Result<Value>, deadline: i64, cancel: &Cancel) -> Result<bool> {
    if let Err(e) = &result {
        if e.0.starts_with(UNCONFIGURED_PREFIX) || e.0 == AUTOMATION_DISABLED || e.0 == crate::ai::DEFERRED {
            return Ok(false);
        }
    }
    let config = ctx.config.get();
    let interrupted = ctx.exec.last_interrupt();
    ctx.db.commit_filter(config, flight, result, deadline, interrupted, cancel.is_cancelled()).await
}
