//! The task record, its rows in `tasks`/`task_tags`/`task_evidence`, and the
//! pure rules every writer applies: summary fallback, focus and "today".

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tags::{self, Tag};
use crate::{Error, Result};

pub const STATUSES: [&str; 5] = ["待处理", "处理中", "已完成", "无需处理", "已取消"];
pub const ACTIVE: [&str; 2] = ["待处理", "处理中"];
pub const CLOSED: [&str; 3] = ["已完成", "无需处理", "已取消"];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub direct: Vec<String>,
    pub background: Vec<String>,
    pub completion: Vec<String>,
}

impl Evidence {
    pub fn all(&self) -> Vec<&str> {
        let mut ids: Vec<&str> =
            self.direct.iter().chain(&self.background).chain(&self.completion).map(String::as_str).collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

fn yes() -> bool {
    true
}

/// The task as the page and the audit snapshots see it. Unknown and retired
/// fields from the page are ignored; `number` and `status_changed_at` are
/// owned by the table and never read back from a submission.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    #[serde(skip_deserializing)]
    pub number: i64,
    pub title: String,
    pub description: String,
    pub summary: String,
    pub summary_fallback: bool,
    pub content_updated_at: i64,
    pub summary_updated_at: i64,
    pub status: String,
    pub blocker: String,
    pub waiting_for: String,
    pub due: String,
    #[serde(default = "yes")]
    pub follow_deadline: bool,
    pub deadline_changed: i64,
    pub deferred_until: i64,
    pub resume_processing: bool,
    pub generation: String,
    pub generation_error: String,
    pub manual: bool,
    pub archived: bool,
    pub archived_at: i64,
    pub archive_note: String,
    pub merged_into: String,
    pub reason: String,
    pub schedule_id: String,
    pub scheduled_for: String,
    pub suggested_project_id: String,
    pub suggested_project_reason: String,
    pub decision_context: Value,
    pub tags: Vec<Tag>,
    pub evidence: Evidence,
    #[serde(skip_deserializing)]
    pub status_changed_at: i64,
    pub created: i64,
    pub updated: i64,
}

impl Task {
    /// The textual fields a patch may name.
    pub fn text_field_mut(&mut self, field: &str) -> Option<&mut String> {
        Some(match field {
            "title" => &mut self.title,
            "description" => &mut self.description,
            "status" => &mut self.status,
            "blocker" => &mut self.blocker,
            "waiting_for" => &mut self.waiting_for,
            "due" => &mut self.due,
            "summary" => &mut self.summary,
            _ => return None,
        })
    }

    pub fn active(&self) -> bool {
        ACTIVE.contains(&self.status.as_str())
    }

    /// Mutually exclusive focus views; the stored status is unchanged.
    pub fn focus(&self, at: i64) -> Option<&'static str> {
        if self.archived || !self.merged_into.is_empty() || !self.active() {
            return None;
        }
        Some(if self.deferred_until > at {
            "延后处理"
        } else if !self.blocker.trim().is_empty() || !self.waiting_for.trim().is_empty() {
            "被阻塞"
        } else if self.status == "处理中" {
            "处理中"
        } else {
            "待处理"
        })
    }

    pub fn payload(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// Parse with the raw serde message.
pub fn from_json<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T> {
    T::deserialize(v).map_err(|e| Error(e.to_string()))
}

pub fn deadline_ms(due: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(due).ok().map(|d| d.timestamp_millis())
}

pub fn normalize(t: &mut Task) {
    if !STATUSES.contains(&t.status.as_str()) {
        t.status = "待处理".into();
    }
    t.tags.retain(tags::valid);
    t.tags.sort();
    t.tags.dedup();
    normalize_evidence(&mut t.evidence);
    refresh_summary(t);
}

/// Content-only fallback summary: whitespace collapsed, 240 characters.
pub fn summary_fallback(text: &str) -> String {
    let mut chars = text
        .split_whitespace()
        .enumerate()
        .flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()));
    let mut out = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn summary_content(t: &Task) -> String {
    [&t.description, &t.title]
        .into_iter()
        .map(|s| summary_fallback(s))
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "（未命名事项）".into())
}

pub fn refresh_summary(t: &mut Task) {
    if t.content_updated_at <= 0 {
        t.content_updated_at = t.updated.max(t.created);
    }
    if t.summary.trim().is_empty() || t.summary_updated_at < t.content_updated_at {
        t.summary = summary_content(t);
        t.summary_updated_at = t.content_updated_at.max(1);
        t.summary_fallback = true;
    }
}

pub fn mark_content_changed(t: &mut Task, at: i64) {
    t.content_updated_at = at.max(t.updated);
    t.summary_updated_at = 0;
    refresh_summary(t);
}

pub fn normalize_evidence(e: &mut Evidence) {
    for rows in [&mut e.direct, &mut e.background, &mut e.completion] {
        rows.retain(|id| !id.trim().is_empty());
        rows.sort();
        rows.dedup();
    }
}

pub fn merge_evidence(t: &mut Task, other: &Evidence) {
    t.evidence.direct.extend_from_slice(&other.direct);
    t.evidence.background.extend_from_slice(&other.background);
    t.evidence.completion.extend_from_slice(&other.completion);
    normalize_evidence(&mut t.evidence);
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
