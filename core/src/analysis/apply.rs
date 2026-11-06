//! Apply AI actions through the tasks domain.
//!
//! The writes run in `data::analysis` inside the accept transaction. Whole-batch errors (an unknown action
//! type, a whole `tags` field, missing sources, an invalid tag patch) roll
//! back every write of the batch through a savepoint; an action that is only
//! not adopted puts its sources into `retry` and the rest of the batch still
//! lands. Tasks are read and written with `tasks::model`, tags go through
//! `tasks::tags`, every change writes its `task_audit` row.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::tasks::model::Task;
use crate::tasks::tags::Tag;

pub const MEMORY_CHANGED: &str = "临时记忆已被修改，本次不覆盖";
pub const MEMORY_PURPOSE: &str = "此用途不可修改临时记忆";

/// What one batch adopted.
#[derive(Debug, Default)]
pub struct Applied {
    pub applied: usize,
    /// Sources of actions that were not adopted.
    pub retry: BTreeSet<String>,
    /// Tasks created or changed, in order.
    pub tasks: Vec<String>,
    /// Per task: the state before its first write, after its last, and the
    /// last change summary; one notice each once the batch is committed.
    pub changes: BTreeMap<String, (Value, Value, String)>,
}

impl Applied {
    pub(crate) fn record(&mut self, id: &str, before: &Value, after: &Value, summary: &str) {
        let change = self.changes.entry(id.into()).or_insert_with(|| (before.clone(), Value::Null, String::new()));
        (change.1, change.2) = (after.clone(), summary.into());
    }

    /// `(task id, title, summary)` of every change worth telling the user.
    pub fn notices(&self) -> Vec<(String, String, String)> {
        let short = |text: &str| {
            let text = text.trim();
            (!text.is_empty() && text.chars().count() <= 30 && !text.chars().any(char::is_control)).then(|| text.into())
        };
        let mut out = Vec::new();
        for (id, (before, after, summary)) in &self.changes {
            let changed = |key: &str| before[key] != after[key];
            let fallback = if before.is_null() {
                "新增事项".into()
            } else if changed("status") {
                format!("状态改为{}", after["status"].as_str().unwrap_or("未设置"))
            } else if changed("blocker") {
                if after["blocker"].as_str().unwrap_or("").is_empty() {
                    "阻塞已解除"
                } else {
                    "阻塞信息已更新"
                }
                .into()
            } else if changed("due") {
                "截止时间已调整".into()
            } else if changed("tags") {
                "事项标签已调整".into()
            } else if changed("waiting_for") {
                "等待对象已更新".into()
            } else if changed("title") || changed("description") || changed("summary") {
                "事项内容已更新".into()
            } else if changed("evidence") {
                "补充了消息依据".into()
            } else {
                continue;
            };
            let title = after["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("未命名事项");
            out.push((id.clone(), title.into(), short(summary).unwrap_or(fallback)));
        }
        out
    }
}

pub(crate) fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
}

pub(crate) fn confidence(action: &Value) -> f64 {
    action["confidence"].as_f64().unwrap_or(0.)
}

/// Apply the action's field patch to a copy of `old` (or a blank task).
pub fn materialize(action: &Value, old: Option<&Task>) -> std::result::Result<Task, String> {
    let mut task = old.cloned().unwrap_or_default();
    let fields = action["fields"].as_array().ok_or("AI 动作缺少字段差异")?;
    let mut seen = BTreeSet::new();
    for change in fields {
        let key = change["field"].as_str().ok_or("AI 字段无效")?;
        let value = change["value"].as_str().ok_or("AI 字段重复或无效")?;
        if !seen.insert(key) {
            return Err("AI 字段重复或无效".into());
        }
        if key == "deferred_until" {
            task.deferred_until = if value == "0" {
                0
            } else {
                chrono::DateTime::parse_from_rfc3339(value).map_err(|_| "延后时间无效")?.timestamp_millis()
            };
        } else {
            *task.text_field_mut(key).ok_or("AI 字段重复或无效")? = value.into();
            if key == "summary" {
                task.summary_fallback = false;
            }
        }
    }
    task.reason = action["reason"].as_str().ok_or("AI 动作缺少理由")?.into();
    Ok(task)
}

/// A model summary is kept unless the content changed after it; a stored
/// fallback is recomputed by `normalize`.
pub(crate) fn preserve_model_summary(t: &mut Task) {
    if !t.summary.trim().is_empty() && !t.summary_fallback {
        t.content_updated_at = t.content_updated_at.max(t.updated);
        t.summary_updated_at = t.content_updated_at;
    }
}

/// Presentation metadata is best-effort: kept only when every requested
/// field landed as sent, every tag change took effect and something changed.
pub fn accepted_change_summary(action: &Value, before: &Value, after: &Value) -> String {
    let Some(summary) = action["change_summary"].as_str() else { return String::new() };
    if summary.trim().is_empty()
        || summary.chars().count() > 30
        || summary.chars().any(char::is_control)
        || summary.contains(['\u{2028}', '\u{2029}'])
    {
        return String::new();
    }
    let Some(fields) = action["fields"].as_array() else { return String::new() };
    if fields.iter().any(|c| c["field"].as_str().is_none_or(|f| after.get(f) != Some(&c["value"]))) {
        return String::new();
    }
    let tag_set =
        |v: &Value| serde_json::from_value::<Vec<Tag>>(v.clone()).ok().map(|t| t.into_iter().collect::<BTreeSet<_>>());
    let Some(assigned) = tag_set(&after["tags"]) else { return String::new() };
    if action["tag_changes"].as_array().is_none_or(|changes| {
        changes.iter().any(|c| {
            let tag =
                Tag { kind: c["kind"].as_str().unwrap_or("").into(), value: c["value"].as_str().unwrap_or("").into() };
            match c["action"].as_str() {
                Some("add") => !assigned.contains(&tag),
                Some("remove") => assigned.contains(&tag),
                _ => true,
            }
        })
    }) {
        return String::new();
    }
    if !before.is_null()
        && fields.iter().all(|c| {
            let key = c["field"].as_str().unwrap_or("");
            before[key] == after[key]
        })
        && tag_set(&before["tags"]) == tag_set(&after["tags"])
    {
        return String::new();
    }
    summary.trim().to_owned()
}
