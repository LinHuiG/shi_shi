//! Reports: the manual report and the morning report.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::{event, task_brief, Measured, Mode, Model, MorningJob, Prepared};
use crate::ai::prompt::require_purpose;
use crate::{exec, time, Ctx, Error, Result};

/// Days of changes a report keeps however large its delta grows.
const KEPT_DAYS: i64 = 7;

/// What the report model reads: the tasks and the delta from row `from` on.
/// It cannot read messages, so it gets no message ids.
pub fn request(r: &Value, mode: Mode, from: usize) -> Prepared {
    let rows = |section: &str| r[section].as_array().map_or(&[][..], Vec::as_slice);
    let tasks: Vec<Value> = rows("tasks").iter().map(|t| without(task_brief(t), "evidence")).collect();
    let delta: Vec<Value> = rows("delta")
        .iter()
        .skip(from)
        .map(|e| {
            let mut e = event(e);
            e["changes"] = without(e["changes"].take(), "evidence");
            e
        })
        .collect();
    Prepared {
        purpose: "report",
        payload: json!({
            "report_tasks": tasks, "delta": delta, "label": r["label"], "time": time::stamp(time::now()),
            "_trigger": if mode == Mode::Manual { "manual_report" } else { "scheduled_report" },
        }),
    }
}

fn without(mut v: Value, key: &str) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove(key);
    }
    v
}

/// The request with as much of the newest delta as fits `limit`: whole days
/// go oldest first, never the last [`KEPT_DAYS`]. One still too large is
/// sent as it is, for the call layer to reject and log. Returns it with the
/// number of rows left out.
fn fit(model: &dyn Model, r: &Value, mode: Mode, limit: usize) -> Result<(Measured<Prepared>, usize)> {
    let days: Vec<String> = r["delta"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|e| time::date_from(e["time"].as_i64().unwrap_or(0)))
        .collect();
    let kept = time::date_from(r["time"].as_i64().unwrap_or(0) - (KEPT_DAYS - 1) * time::DAY);
    let mut from = 0;
    loop {
        let measured = model.measure(request(r, mode, from))?;
        match days.get(from) {
            Some(day) if measured.total > limit && *day < kept => {
                from += days[from..].iter().take_while(|d| *d == day).count();
            }
            _ => return Ok((measured, from)),
        }
    }
}

/// Map the model result onto the report.
pub fn apply(r: &mut Value, result: Result<Value>) {
    match result {
        Ok(v) => {
            r["ai_summary"] = v;
            r["summary_status"] = json!("done");
            r.as_object_mut().map(|o| o.remove("summary_error"));
        }
        Err(e) if e.0 == exec::CANCELLED => {
            r["summary_status"] = json!("interrupted");
            r["summary_error"] = json!(e.0);
        }
        Err(e) => {
            r["summary_status"] = json!("failed");
            r["summary_error"] = json!(e.0);
        }
    }
}

async fn summarize(ctx: &Ctx, model: &dyn Model, mut r: Value, mode: Mode) -> Result<Value> {
    let Some(_release) = exec::Flag::claim(&ctx.archive.summary_busy) else { return Ok(r) };
    let result = match super::archive_blocks(ctx).await {
        Err(e) => Err(e),
        Ok(()) => {
            let config = ctx.config.get();
            let timeout = Duration::from_secs(config.ai_timeout_seconds.max(1));
            match fit(model, &r, mode, config.max_input_chars) {
                Ok((measured, omitted)) => {
                    if omitted > 0 {
                        r["delta_omitted"] = json!(omitted);
                    }
                    super::ask(ctx, model, measured, timeout).await
                }
                Err(e) => Err(e),
            }
        }
    };
    if result.as_ref().is_err_and(|e| e.0 == crate::ai::DEFERRED) {
        return Ok(r);
    }
    apply(&mut r, result);
    ctx.db.finish_report(r).await
}

/// A report with its summary attempt; failures stay in the report.
pub async fn report_result(ctx: &Ctx, model: &dyn Model, label: &str, mode: Mode) -> Result<Value> {
    require_purpose(&ctx.config.get(), "report")?;
    let r = ctx.db.prepare_report(label.to_string(), time::now()).await?;
    if r["summary_status"] == "pending" {
        return summarize(ctx, model, r, mode).await;
    }
    Ok(r)
}

/// `report`: an interrupted summary is an error, other failures are shown
/// in the report.
pub async fn report(ctx: &Ctx, model: &dyn Model, label: &str, mode: Mode) -> Result<Value> {
    let r = report_result(ctx, model, label, mode).await?;
    if r["summary_error"] == exec::CANCELLED {
        return Err(Error::new(exec::CANCELLED));
    }
    Ok(r)
}

/// Run the first queued morning job once; failures are not retried.
pub async fn morning_tick(ctx: &Arc<Ctx>, model: &dyn Model) -> Result<Option<MorningJob>> {
    if ctx.exec.ai_busy() {
        return Ok(None);
    }
    let job = ctx.db.claim_morning_job().await?;
    let Some(mut job) = job else {
        return Ok(None);
    };
    let result = report_result(ctx, model, "早报", Mode::Automatic).await;
    let (status, error, finished) = match &result {
        Ok(r) if ["done", "reused", "fallback"].contains(&r["summary_status"].as_str().unwrap_or("")) => {
            ("succeeded", String::new(), r["finished"].as_i64().unwrap_or(0))
        }
        Ok(r) => (
            "failed",
            r["summary_error"]
                .as_str()
                .filter(|e| !e.is_empty())
                .unwrap_or("早报已生成，但汇总未完成；不会自动重试")
                .to_string(),
            r["finished"].as_i64().unwrap_or(0),
        ),
        Err(e) => ("failed", e.0.clone(), 0),
    };
    job.status = status.into();
    job.error = error;
    let job = ctx.db.finish_morning_job(job, finished).await?;
    Ok(Some(job))
}
