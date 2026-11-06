//! The runtime log. Domains append rows inside their own transaction when
//! the event belongs to a business change, or through
//! [`crate::Db::record_activity`] when it must not affect the outcome: a failed log
//! write never blocks cleanup.

use serde_json::{json, Value};

use crate::{bail, tasks, time, Result};

/// `activity_day_revision` for the logs page: changes when that day's rows
/// change. `live` moves only for today so history days do not reload while
/// today's log grows. An invalid day yields `null`.
pub async fn day_revision(ctx: &crate::Ctx, day: &str) -> Value {
    let Some(start) = time::day_start(day) else { return Value::Null };
    let [logs, last_log, audits, last_audit, tasks, task_updated] =
        ctx.db.activity_counts(day.to_string(), start).await.unwrap_or_default();
    let today = day == time::today();
    // Audit rows are immutable; labels follow current task titles. A history
    // day's log only changes by retention.
    let audit = format!("{audits}:{last_audit}:{tasks}:{task_updated}");
    json!({"day": day, "audit": if today { audit } else { format!("{audit}:{logs}:{last_log}") },
        "live": if today { last_log } else { 0 }})
}

/// AI and failure kinds shared by the runtime list and the AI filter.
const AI_KINDS: [&str; 10] = [
    "ai_start",
    "ai_success",
    "ai_failed",
    "ai_cancelled",
    "ai_rejected",
    "ai_resumed",
    "analysis_rejected",
    "archive_result",
    "archive_failed",
    "resume_pending",
];
const RUNTIME_KINDS: [&str; 7] =
    ["scan", "migration", "error", "request_error", "analysis_result", "analysis_skipped", "generation_failed"];
const ERROR_KINDS: [&str; 7] =
    ["error", "analysis_rejected", "request_error", "ai_failed", "ai_rejected", "generation_failed", "archive_failed"];
const EXTERNAL_ERRORS: [&str; 8] =
    ["failed", "uncertain", "disconnected", "rejected", "nonzero_exit", "start_failed", "output_error", "timed_out"];

fn external(kind: &str) -> bool {
    kind.starts_with("handling_") || kind.starts_with("tool_call_")
}

/// Runtime kinds the logs page lists; the rest only feed the status ring.
pub(crate) fn runtime(kind: &str) -> bool {
    external(kind) || kind == "codex_visibility" || AI_KINDS.contains(&kind) || RUNTIME_KINDS.contains(&kind)
}

pub(crate) fn matches_filter(row: &Value, filter: &str) -> bool {
    let kind = row["kind"].as_str().unwrap_or("");
    match filter {
        "all" => true,
        "changes" => row["audit"] == true,
        "errors" => {
            let detail = &row["detail"];
            ERROR_KINDS.contains(&kind)
                || (external(kind)
                    && (detail["error"].as_str().is_some_and(|s| !s.is_empty())
                        || detail["status"].as_str().is_some_and(|s| EXTERNAL_ERRORS.contains(&s))))
        }
        _ => external(kind) || ["advice_auto_decision", "codex_visibility"].contains(&kind) || AI_KINDS.contains(&kind),
    }
}

pub(crate) fn matches_purpose(row: &Value, purpose: &str) -> bool {
    let kind = row["detail"]["kind"].as_str().unwrap_or("");
    let row_kind = row["kind"].as_str().unwrap_or("");
    match purpose {
        "all" => true,
        "analysis" => kind == "classify" || (kind.is_empty() && row_kind.starts_with("ai_")),
        "tool_call" => kind == "tool_call" || kind == "tool_test" || row_kind.starts_with("tool_call_"),
        _ => kind == purpose,
    }
}

pub(crate) const FILTERS: [&str; 4] = ["ai", "all", "errors", "changes"];
pub(crate) const PURPOSES: [&str; 9] =
    ["all", "analysis", "message_filter", "report", "archive", "tool_call", "format", "edit", "tool_test"];

/// `activity_query`: one day of runtime log and task audit as a single
/// timeline, newest first, by page or by cursor.
pub async fn query(ctx: &crate::Ctx, p: Value) -> Result<Value> {
    let day = p["day"].as_str().map(str::trim).filter(|s| !s.is_empty()).map_or_else(time::today, str::to_owned);
    let Some(start) = time::day_start(&day) else { bail!("日志日期无效") };
    let revision = day_revision(ctx, &day).await;
    let mut out = ctx.db.activity_page(p, day, start).await?;
    out["revision"] = revision;
    Ok(out)
}

/// The row the filters see and the page shows, without its position.
pub(crate) fn timeline_row(
    audit: bool,
    kind: String,
    detail: Option<String>,
    before: Option<String>,
    after: Option<String>,
    task_id: String,
    label: Option<(String, i64)>,
) -> Value {
    let mut row = if audit {
        json!({"kind": kind, "task_id": task_id, "detail": detail.unwrap_or_default(), "audit": true,
            "before": tasks::query::snapshot_json(before), "after": tasks::query::snapshot_json(after)})
    } else {
        json!({"kind": kind, "detail": tasks::query::snapshot_json(detail)})
    };
    if !task_id.is_empty() {
        row["task_id"] = json!(task_id);
        if let Some((title, number)) = label {
            row["task_title"] = json!(title);
            row["task_number"] = json!(number);
            row["task_label"] = json!(format!("#{number}"));
        } else if let Some(title) = row["after"]["title"].as_str().or_else(|| row["before"]["title"].as_str()) {
            row["task_title"] = json!(title);
        }
    }
    row
}

#[cfg(test)]
mod tests;
