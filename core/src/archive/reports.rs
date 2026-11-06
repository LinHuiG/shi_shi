//! Reports: the manual report and the morning report.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::{pick, Mode, Model, MorningJob, Prepared};
use crate::ai::prompt::require_purpose;
use crate::{exec, time, Ctx, Error, Result};

/// What the report model reads.
pub fn request(r: &Value, mode: Mode) -> Prepared {
    let tasks: Vec<Value> = r["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            pick(t, &["id", "title", "description", "status", "tags", "due", "blocker", "waiting_for", "archive_note"])
        })
        .collect();
    Prepared {
        purpose: "report",
        payload: json!({
            "report_tasks": tasks, "delta": r["delta"], "label": r["label"], "time": time::stamp(time::now()),
            "_trigger": if mode == Mode::Manual { "manual_report" } else { "scheduled_report" },
        }),
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
            let timeout = Duration::from_secs(ctx.config.get().ai_timeout_seconds.max(1));
            match model.measure(request(&r, mode)) {
                Ok(measured) => super::ask(ctx, model, measured, timeout).await,
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
