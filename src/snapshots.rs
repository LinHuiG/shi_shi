//! Frozen, read-only projections of the authoritative state, cached per round.
use crate::model::{Audit, Message, State, Task};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const RETENTION_MS: i64 = 86_400_000;

fn message_record(m: &Message) -> Value {
    let mut row = json!({
        "id":m.id,"kind":"message","source":m.source_id,"chat":m.chat,"time":m.time,"data":m
    });
    row["data"]["truncated"] = json!(false);
    row["data"]["original_chars"] = json!(m.text.chars().count());
    row
}
fn task_update_kind(a: &Audit) -> bool {
    !a.task_id.is_empty()
        && [
            "ai_create",
            "ai_update",
            "ai_evidence",
            "ai_merge_duplicate",
            "manual_edit",
            "manual_ai_edit",
            "manual_status",
            "manual_archive",
            "merge",
            "undo",
            "archive_item",
            "deadline_update",
            "generation_done",
        ]
        .contains(&a.kind.as_str())
}

/// Only fields that exist in the current task schema may enter a derived update
/// snapshot. General settings, prompt bodies, logs and credentials never do.
const TASK_UPDATE_FIELDS: [&str; 36] = [
    "decision_context",
    "suggested_project_id",
    "suggested_project_reason",
    "tags",
    "id",
    "title",
    "description",
    "summary",
    "content_updated_at",
    "summary_updated_at",
    "summary_fallback",
    "evidence",
    "priority",
    "environment",
    "category",
    "generation",
    "generation_error",
    "status",
    "blocker",
    "waiting_for",
    "due",
    "created",
    "updated",
    "archived",
    "archive_note",
    "archived_at",
    "manual",
    "locked",
    "reason",
    "merged_into",
    "follow_deadline",
    "deadline_changed",
    "deferred_until",
    "resume_processing",
    "schedule_id",
    "scheduled_for",
];

fn whitelisted_task<'a>(value: &'a Value, task_id: &str) -> BTreeMap<&'static str, &'a Value> {
    let selected = match value {
        Value::Array(rows) => rows.iter().find(|row| row["id"].as_str() == Some(task_id)),
        Value::Object(_) => Some(value),
        _ => None,
    };
    TASK_UPDATE_FIELDS
        .into_iter()
        .filter_map(|field| selected?.get(field).map(|value| (field, value)))
        .collect()
}

fn update_record(index: usize, a: &Audit) -> Value {
    // A long note, description or other whitelisted field must remain fully
    // retrievable even when the task itself has been archived. The pageable
    // document holds the complete whitelisted before/after projection; only
    // small field/identity metadata accompanies it on every page.
    let before = whitelisted_task(&a.before, &a.task_id);
    let after = whitelisted_task(&a.after, &a.task_id);
    let fields = TASK_UPDATE_FIELDS
        .iter()
        .filter(|field| before.get(*field) != after.get(*field))
        .copied()
        .collect::<Vec<_>>();
    let document = json!({
        "task_id":a.task_id,
        "audit_kind":a.kind,
        "time":a.time,
        "before":before,
        "after":after,
    });
    json!({
        "id":format!("audit:{index}"),"kind":"update","source":"","chat":"","time":a.time,
        "data":{
            "task_id":a.task_id,"audit_kind":a.kind,"audit_index":index,
            "evidence_id":format!("audit:{index}"),"fields":fields,"detail":document.to_string(),
            "note":"detail 为完整白名单变更文档，可用 next_offset 续读；省略字段不代表清空"
        }
    })
}

fn referenced_messages(s: &State) -> BTreeSet<String> {
    s.tasks
        .iter()
        .filter(|t| !t.archived)
        .flat_map(|t| t.evidence.iter().map(str::to_owned))
        .collect()
}

fn message_records(s: &State, at: i64, referenced: &BTreeSet<String>) -> Vec<Value> {
    let cutoff = at - RETENTION_MS;
    s.messages
        .values()
        .filter(|m| {
            !m.withdrawn
                && (m.time >= cutoff
                    || referenced.contains(&m.id)
                    || s.analyzed.get(&m.id) != Some(&crate::model::fingerprint(m)))
        })
        .map(message_record)
        .collect()
}
fn task_records(s: &State) -> Vec<Value> {
    s.tasks
        .iter()
        .filter(|t| !t.archived)
        .map(|task| {
            // Only a single shared source/chat can describe a task; missing
            // evidence is ignored, and mixed conversations expose no scope.
            let mut sources = task
                .evidence
                .iter()
                .filter_map(|id| s.messages.get(id))
                .map(|m| (m.source_id.as_str(), m.chat.as_str()));
            let first = sources.next().unwrap_or_default();
            let (source, chat) = if sources.all(|scope| scope == first) {
                first
            } else {
                ("", "")
            };
            let mut record = json!({
                "id":task.id,"kind":"task","source":source,"chat":chat,
                "time":if task.updated > 0 { task.updated } else { task.created }
            });
            record["data"] = crate::context_budget::task_detail(task);
            record
        })
        .collect()
}
fn update_records(s: &State, at: i64) -> Vec<Value> {
    let cutoff = at - RETENTION_MS;
    let archive_start = s.archive_ai_cursor.unwrap_or(0).min(s.audit.len());
    s.audit
        .iter()
        .enumerate()
        .filter(|(index, a)| task_update_kind(a) && (a.time >= cutoff || *index >= archive_start))
        .map(|(index, a)| update_record(index, a))
        .collect()
}

/// A bounded cache owned by one analysis/archive round. Shared identities and
/// the explicit projection dependencies determine which section needs work;
/// records stay in memory and are frozen before crossing a process boundary.
#[derive(Default)]
pub(crate) struct Cache {
    at: Option<i64>,
    messages: Option<crate::shared::Shared<BTreeMap<String, Message>>>,
    tasks: Option<crate::shared::Shared<Vec<Task>>>,
    audit: Option<crate::shared::Shared<Vec<Audit>>>,
    analyzed: crate::shared::Shared<BTreeMap<String, String>>,
    archive_start: Option<usize>,
    referenced: BTreeSet<String>,
    parts: [Vec<Value>; 3],
    #[cfg(test)]
    builds: [usize; 3],
}
impl Cache {
    pub(crate) fn records(&mut self, s: &State, at: i64) -> Vec<Value> {
        let at = *self.at.get_or_insert(at);
        let messages_changed = !self
            .messages
            .as_ref()
            .is_some_and(|old| old.same_snapshot(&s.messages));
        let tasks_changed = !self
            .tasks
            .as_ref()
            .is_some_and(|old| old.same_snapshot(&s.tasks));
        let audit_changed = !self
            .audit
            .as_ref()
            .is_some_and(|old| old.same_snapshot(&s.audit));
        let updated_references = tasks_changed.then(|| referenced_messages(s));
        let referenced = updated_references.as_ref().unwrap_or(&self.referenced);
        let sources_changed = messages_changed
            && self.messages.as_ref().is_none_or(|old| {
                old.len() != s.messages.len()
                    || old
                        .iter()
                        .zip(s.messages.iter())
                        .any(|((id, a), (next_id, b))| {
                            id != next_id || a.source_id != b.source_id || a.chat != b.chat
                        })
            });
        let scope_changed = !messages_changed
            && (!self.analyzed.same_snapshot(&s.analyzed) || &self.referenced != referenced)
            && s.messages
                .values()
                .filter(|m| !m.withdrawn && m.time < at - RETENTION_MS)
                .any(|m| {
                    let was_referenced = self.referenced.contains(&m.id);
                    let is_referenced = referenced.contains(&m.id);
                    if was_referenced == is_referenced
                        && self.analyzed.get(&m.id) == s.analyzed.get(&m.id)
                    {
                        return false;
                    }
                    let fingerprint = crate::model::fingerprint(m);
                    (was_referenced || self.analyzed.get(&m.id) != Some(&fingerprint))
                        != (is_referenced || s.analyzed.get(&m.id) != Some(&fingerprint))
                });
        let refresh = [
            messages_changed || scope_changed,
            tasks_changed || sources_changed,
            audit_changed || self.archive_start != s.archive_ai_cursor,
        ];
        for position in 0..self.parts.len() {
            if refresh[position] {
                self.parts[position] = match position {
                    0 => message_records(s, at, referenced),
                    1 => task_records(s),
                    _ => update_records(s, at),
                };
                #[cfg(test)]
                {
                    self.builds[position] += 1;
                }
            }
        }
        self.messages = Some(s.messages.clone());
        self.tasks = Some(s.tasks.clone());
        self.audit = Some(s.audit.clone());
        self.analyzed.clone_from(&s.analyzed);
        self.archive_start = s.archive_ai_cursor;
        if let Some(referenced) = updated_references {
            self.referenced = referenced;
        }
        self.parts.iter().flatten().cloned().collect()
    }
}

/// Tests use the production projection, without maintaining another algorithm.
#[cfg(test)]
pub fn records_from_state(s: &State, at: i64) -> Vec<Value> {
    Cache::default().records(s, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::now;

    fn of_kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
        records
            .iter()
            .filter(|r| r["kind"] == kind)
            .map(|r| &r["data"])
            .collect()
    }

    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            source_id: "source".into(),
            chat: "room".into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }

    #[test]
    fn round_cache_reuses_unchanged_sections_and_keeps_frozen_records() {
        let at = now();
        let mut s = State::default();
        for n in 0..2000 {
            let id = format!("recent-{n}");
            s.messages
                .insert(id.clone(), message(&id, at - 1000, "synthetic body"));
        }
        s.messages.insert(
            "old".into(),
            message("old", at - 3 * RETENTION_MS, "old evidence"),
        );
        s.analyzed
            .insert("old".into(), crate::model::fingerprint(&s.messages["old"]));
        let mut cache = Cache::default();
        cache.records(&s, at);
        for n in 0..500 {
            let id = format!("recent-{n}");
            s.analyzed
                .insert(id.clone(), crate::model::fingerprint(&s.messages[&id]));
        }
        cache.records(&s, at + 1000);
        assert_eq!(cache.builds, [1, 1, 1]);
        s.tasks.push(crate::test_support::task("task", "new task"));
        let set = cache.records(&s, at + 2000);
        assert_eq!(cache.builds, [1, 2, 1]);
        assert!(set
            .iter()
            .any(|r| r["kind"] == "task" && r["data"]["title"] == "new task"));
        s.tasks[0].evidence.direct.push("old".into());
        let set = cache.records(&s, at + 3000);
        assert_eq!(cache.builds, [2, 3, 1]);
        assert!(set.iter().any(|r| r["id"] == "old"));
        s.messages.get_mut("recent-0").unwrap().text = "changed".into();
        let changed = cache.records(&s, at + 4000);
        assert_eq!(cache.builds, [3, 3, 1]);
        assert!(changed.iter().any(|r| r["data"]["text"] == "changed"));
        assert!(!set.iter().any(|r| r["data"]["text"] == "changed"));
        s.messages.get_mut("old").unwrap().source_id = "moved-source".into();
        let moved = cache.records(&s, at + 5000);
        assert_eq!(cache.builds, [4, 4, 1]);
        for id in ["old", "task"] {
            assert_eq!(
                moved.iter().find(|r| r["id"] == id).unwrap()["source"],
                "moved-source"
            );
            assert_eq!(
                set.iter().find(|r| r["id"] == id).unwrap()["source"],
                "source"
            );
        }
        s.messages.remove("old");
        let removed = cache.records(&s, at + 6000);
        assert_eq!(cache.builds, [5, 5, 1]);
        assert!(!removed.iter().any(|r| r["id"] == "old"));
        let task = removed.iter().find(|r| r["kind"] == "task").unwrap();
        assert_eq!(task["source"], "");
        assert_eq!(task["chat"], "");
    }

    #[test]
    fn snapshots_keep_recent_tasks_and_unarchived_old_evidence_but_not_unrelated_old_rows() {
        let at = now();
        let mut s = State::default();
        s.last_scan = at - 1000;
        s.messages
            .insert("recent".into(), message("recent", at - 1000, "近期材料"));
        s.messages.insert(
            "old-linked".into(),
            message("old-linked", at - 3 * RETENTION_MS, "旧但未归档依据"),
        );
        s.messages.insert(
            "old-unrelated".into(),
            message("old-unrelated", at - 3 * RETENTION_MS, "旧且无关"),
        );
        s.analyzed.insert(
            "old-unrelated".into(),
            crate::model::fingerprint(&s.messages["old-unrelated"]),
        );
        s.messages.insert(
            "pending".into(),
            message("pending", at - 3 * RETENTION_MS, "尚未分析"),
        );
        s.tasks.push(Task {
            id: "task".into(),
            title: "未归档事项".into(),
            description: "未归档事项".into(),
            status: "已完成".into(),
            evidence: crate::model::TaskEvidence {
                direct: vec!["old-linked".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        s.audit.push(crate::model::Audit {
            time: at - 3 * RETENTION_MS,
            kind: "manual_archive".into(),
            task_id: "task".into(),
            detail: "旧备注".into(),
            change_summary: String::new(),
            before: json!({}),
            after: json!({"archive_note":"长期要求"}),
        });
        s.archive_ai_cursor = Some(0);
        let records = records_from_state(&s, at);
        let messages = of_kind(&records, "message");
        let ids = messages
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect::<BTreeSet<_>>();
        assert!(ids.contains("recent"));
        assert!(ids.contains("old-linked"));
        assert!(ids.contains("pending"));
        assert!(!ids.contains("old-unrelated"));
        let tasks = of_kind(&records, "task");
        assert_eq!(tasks[0]["summary"], "未归档事项");
        let updates = of_kind(&records, "update");
        assert_eq!(updates[0]["evidence_id"], "audit:0");
        let document: Value = serde_json::from_str(updates[0]["detail"].as_str().unwrap()).unwrap();
        assert_eq!(document["after"]["archive_note"], "长期要求");
    }

    #[test]
    fn update_snapshot_projection_never_copies_settings_or_secret_bodies() {
        let at = now();
        let mut s = State::default();
        s.audit.push(Audit {
            time: at,
            kind: "manual_edit".into(),
            task_id: "t".into(),
            detail: "修改任务".into(),
            change_summary: String::new(),
            before: json!([{"id":"t","title":"旧标题","config":{"api_key":"TOP-SECRET"}}]),
            after: json!([{"id":"t","title":"新标题","config":{"api_key":"TOP-SECRET-2"}}]),
        });
        // Settings/log audits and unrelated secret bodies are not task updates.
        s.audit.push(Audit {
            time: at,
            kind: "settings".into(),
            task_id: String::new(),
            detail: "修改设置".into(),
            change_summary: String::new(),
            before: json!({"config":{"api_key":"TOP-SECRET"}}),
            after: json!({"config":{"api_key":"TOP-SECRET-2"}}),
        });
        let records = records_from_state(&s, at);
        assert!(!json!(records).to_string().contains("TOP-SECRET"));
        let updates = of_kind(&records, "update");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["audit_kind"], "manual_edit");
        assert!(updates[0].get("audit").is_none());
    }

    #[test]
    fn archived_task_long_note_and_description_are_fully_pageable_from_updates() {
        let at = now();
        let note = format!("{}归档备注尾部", "备".repeat(3_000));
        let description = format!("{}事项描述尾部", "述".repeat(3_500));
        let mut task = Task {
            id: "archived".into(),
            title: "已归档长文".into(),
            description: description.clone(),
            status: "已完成".into(),
            archived: true,
            archive_note: note.clone(),
            updated: at,
            ..Default::default()
        };
        let mut before = task.clone();
        before.description = "旧描述".into();
        before.archive_note.clear();
        task.archive_note = note.clone();
        task.steps.push(crate::model::TaskStep {
            id: "verified".into(),
            title: "人工验证事实".into(),
            status: "completed".into(),
            confirmed: true,
            ..Default::default()
        });
        let mut s = State::default();
        s.tasks.push(task.clone());
        s.audit.push(Audit {
            time: at,
            kind: "manual_archive".into(),
            task_id: task.id.clone(),
            detail: "归档完整长文".into(),
            change_summary: String::new(),
            before: json!([before]),
            after: json!([task]),
        });
        let records = records_from_state(&s, at);
        // The archived task is intentionally absent from the task snapshot.
        assert!(of_kind(&records, "task").is_empty());
        let rows = records;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "update");
        let document: Value =
            serde_json::from_str(rows[0]["data"]["detail"].as_str().unwrap()).unwrap();
        assert!(document["after"].get("steps").is_none());
        assert!(rows[0]["data"]["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("archive_note")));
        assert!(rows[0]["data"]["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("description")));
        let snapshot = json!({"records":rows,"expires":crate::model::now()+300_000});
        let mut meter = crate::analysis_context::Meter::default();
        let mut offset = 0u64;
        let mut joined = String::new();
        loop {
            let page: Value = serde_json::from_str(&crate::analysis_context::query(
                &snapshot,
                &mut meter,
                &json!({"ids":["audit:0"],"offset":offset}),
            ))
            .unwrap();
            assert!(page.to_string().chars().count() <= crate::analysis_context::PAGE);
            let Some(row) = page["records"].as_array().and_then(|rows| rows.first()) else {
                break;
            };
            joined.push_str(row["data"]["detail"].as_str().unwrap_or(""));
            // The repeated metadata is small; the whole page is not a second
            // full copy of the document.
            assert!(
                row["data"]["detail"].as_str().unwrap().chars().count()
                    <= crate::analysis_context::PAGE
            );
            match row["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert!(joined.contains(&note));
        assert!(joined.contains(&description));
        assert!(joined.contains("归档备注尾部"));
        assert!(joined.contains("事项描述尾部"));
    }

    #[test]
    fn task_records_have_time_and_unambiguous_source_chat_semantics() {
        let at = now();
        let mut s = State::default();
        s.messages.insert(
            "m1".into(),
            Message {
                id: "m1".into(),
                source_id: "source".into(),
                chat: "room".into(),
                text: "依据".into(),
                time: at,
                ..Default::default()
            },
        );
        s.tasks.push(Task {
            id: "t".into(),
            title: "任务".into(),
            description: "完整内容".into(),
            status: "待处理".into(),
            updated: at + 5,
            evidence: crate::model::TaskEvidence {
                direct: vec!["m1".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let records = records_from_state(&s, at);
        let rows = records;
        let task = rows.iter().find(|r| r["kind"] == "task").unwrap();
        assert_eq!(task["time"], at + 5);
        assert_eq!(task["source"], "source");
        assert_eq!(task["chat"], "room");
        let update = rows.iter().find(|r| r["kind"] == "update");
        assert!(update.is_none());
    }

    #[test]
    fn records_expose_full_bodies_and_stable_update_ids() {
        let at = now();
        let mut s = State::default();
        s.messages
            .insert("m".into(), message("m", at, &"长".repeat(5000)));
        s.tasks.push(Task {
            id: "t".into(),
            title: "任务".into(),
            description: "完整内容".into(),
            status: "待处理".into(),
            ..Default::default()
        });
        s.audit.push(crate::model::Audit {
            time: at,
            kind: "manual_edit".into(),
            task_id: "t".into(),
            detail: "修改".into(),
            change_summary: String::new(),
            before: json!({}),
            after: json!({}),
        });
        let records = records_from_state(&s, at);
        let rows = records;
        assert!(rows.iter().any(|r| r["kind"] == "message"
            && r["data"]["text"].as_str().unwrap().chars().count() == 5000));
        assert!(rows
            .iter()
            .any(|r| r["kind"] == "task" && r["data"]["description"] == "完整内容"));
        assert!(rows
            .iter()
            .any(|r| r["id"] == "audit:0" && r["kind"] == "update"));
    }
}
