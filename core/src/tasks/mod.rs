//! The task domain: tasks, batches, merge and undo, change history,
//! schedules, the tag catalog and learned rules.
//!
//! Methods that need a model (`edit_ai`, `generate`, `schedule_format`,
//! `advice_feedback`) prepare their request here, ask through
//! [`crate::ai::ask`] and commit the answer; `generate` returns the pending
//! draft at once and formats it in the background.

pub mod model;
pub mod query;
pub mod rules;
pub mod schedule;
pub mod tags;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::ai::ask::ask;
use crate::ai::Mode;
use crate::{time, Ctx, Error, Result};

pub const METHODS: &[&str] = &[
    "tasks_query",
    "task_get",
    "archived_tasks_query",
    "active_tasks_query",
    "merge_candidates",
    "task_batch_action",
    "edit",
    "edit_ai",
    "action",
    "merge",
    "undo",
    "archive_task",
    "task_changes",
    "generate",
    "schedule_edit",
    "schedule_delete",
    "schedule_format",
    "tag_filters",
    "tag_decide",
    "advice_decide",
    "advice_feedback",
    "rule",
];

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    let db = &ctx.db;
    match method {
        "tasks_query" => db.query_tasks(p).await,
        "task_get" => db.task_payload(p).await,
        "archived_tasks_query" => db.archived_tasks(p).await,
        "active_tasks_query" => db.active_tasks(p).await,
        "merge_candidates" => db.merge_candidates(p).await,
        "task_changes" => db.task_changes(p).await,
        "edit" => db.edit_task(p).await,
        "action" => db.task_action(p, ctx.config.get().work_start.clone()).await,
        "task_batch_action" => db.batch_task_action(p, ctx.config.get().work_start.clone()).await,
        "merge" => db.merge_tasks(p).await,
        "undo" => db.undo_task_change(p).await,
        "archive_task" => db.archive_task(p).await,
        "schedule_edit" => db.edit_schedule(p).await,
        "schedule_delete" => db.delete_schedule(p).await,
        "tag_filters" => db.set_tag_filters(p).await,
        "tag_decide" => db.decide_tag(p).await,
        "advice_decide" => db.decide_advice(p).await,
        "rule" => db.toggle_rule(p).await,
        "edit_ai" => {
            let edit = db.prepare_ai_edit(p).await?;
            let result = ask(ctx, "edit", edit.request.clone(), Mode::Manual).await?;
            db.complete_ai_edit(edit, result).await
        }
        "advice_feedback" => {
            let feedback = db.prepare_feedback(p).await?;
            let result = ask(ctx, "advice_revision", feedback.request.clone(), Mode::Manual).await?;
            db.complete_feedback(feedback, result).await
        }
        "schedule_format" => {
            let text = p["text"].as_str().unwrap_or("").to_string();
            schedule::format_input(&text)?;
            let result = ask(ctx, "schedule", json!({"user_description": text}), Mode::Manual).await?;
            Ok(json!(schedule::parse_draft(&result)?))
        }
        "generate" => {
            crate::ai::ask::require(ctx, "format").await?;
            let draft = db.start_generation(p).await?;
            let (worker, pending) = (ctx.clone(), draft.clone());
            tokio::spawn(async move { generate(&worker, pending).await });
            Ok(draft.payload())
        }
        _ => Err(Error::new("未知请求")),
    }
}

/// How long a manual generation waits for another AI request to finish.
const GENERATION_WAIT: Duration = Duration::from_secs(600);
pub const GENERATION_WAIT_TIMEOUT: &str = "等待其他AI请求超时，请重试";

/// Format a pending draft in the background: wait while the slot or the
/// archive queue is taken, stop silently when the draft changed or the
/// work gate defers, then commit the formatted tasks or the failure.
async fn generate(ctx: &Arc<Ctx>, draft: model::Task) {
    let started = tokio::time::Instant::now();
    let result = loop {
        if started.elapsed() > GENERATION_WAIT {
            break Err(Error::new(GENERATION_WAIT_TIMEOUT));
        }
        if !ctx.db.generation_pending(&draft).await.unwrap_or(false) {
            return;
        }
        let result = ask(ctx, "format", json!({"user_description": draft.description}), Mode::Manual).await;
        match &result {
            Err(e) if e.0 == crate::ai::DEFERRED => return,
            Err(e) if crate::ai::ask::busy(e) => {
                let _ = tokio::time::timeout(Duration::from_secs(1), ctx.exec.released()).await;
            }
            _ => break result,
        }
    };
    let _ = ctx.db.complete_generation(draft, result).await;
}

/// Scheduled generation and deferred resume. The loop sleeps until the
/// next due time, at most a minute (the calendar day may change), and wakes
/// early on every commit.
pub fn spawn(ctx: &Arc<Ctx>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let _ = ctx.db.interrupt_generations().await;
        let mut changes = ctx.db.subscribe();
        loop {
            let at = time::now();
            let wake = match ctx.db.task_wake(at).await {
                Ok((true, _)) => {
                    let _ = ctx.db.run_due_tasks(at).await;
                    continue;
                }
                Ok((false, wake)) => wake,
                Err(_) => None,
            };
            let wait = wake.map_or(time::MINUTE, |w| (w - time::now()).clamp(200, time::MINUTE));
            let _ = tokio::time::timeout(Duration::from_millis(wait as u64), changes.changed()).await;
        }
    });
}
