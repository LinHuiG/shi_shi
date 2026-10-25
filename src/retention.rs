//! Retention shared by messages, audit history, archived tasks and trace files.
//! Audit indices are stable inside a state file, so deletion rebases every cursor
//! and evidence reference that points into the audit vector.
use crate::model::{Audit, Message, State, Task};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const ACTIVE_RUN: [&str; 7] = [
    "starting",
    "submitting",
    "inProgress",
    "waiting",
    "uncertain",
    "disconnected",
    "stopping",
];

/// Expired records are replaced by a compact statistical digest. The cap keeps
/// the digest itself bounded even after years of cleanup.
pub const DIGEST_LIMIT: usize = 8_000;

fn is_active(status: &str) -> bool {
    ACTIVE_RUN.contains(&status)
}

fn digest_len(value: &Value) -> usize {
    value.to_string().chars().count()
}

fn section<'a>(digest: &'a mut Value, key: &str) -> &'a mut Value {
    if !digest.is_object() {
        *digest = json!({"messages":{},"records":{},"tasks":{}});
    }
    let object = digest.as_object_mut().unwrap();
    if !object.get(key).is_some_and(Value::is_object) {
        object.insert(key.into(), json!({}));
    }
    object.get_mut(key).unwrap()
}

fn touch(section: &mut Value, first: i64, last: i64, count: u64) {
    let object = section.as_object_mut().unwrap();
    let total = object.get("count").and_then(Value::as_u64).unwrap_or(0) + count;
    object.insert("count".into(), json!(total));
    let first = object
        .get("first")
        .and_then(Value::as_i64)
        .map_or(first, |value| value.min(first));
    object.insert("first".into(), json!(first));
    let last = object
        .get("last")
        .and_then(Value::as_i64)
        .map_or(last, |value| value.max(last));
    object.insert("last".into(), json!(last));
}

fn merge_counts(section: &mut Value, field: &str, incoming: BTreeMap<String, u64>, cap: usize) {
    if incoming.is_empty() {
        return;
    }
    let mut merged = section[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["name"].as_str()?.to_string(), row["count"].as_u64()?)))
        .collect::<BTreeMap<_, _>>();
    for (name, count) in incoming {
        *merged.entry(name).or_default() += count;
    }
    let mut rows = merged.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    rows.truncate(cap);
    section[field] = json!(rows
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>());
}

pub fn add_messages(s: &mut State, messages: &[Message]) {
    if messages.is_empty() {
        return;
    }
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    let mut sources = BTreeMap::new();
    for message in messages {
        first = first.min(message.time);
        last = last.max(message.time);
        let name = if message.source_name.trim().is_empty() {
            message.source_id.clone()
        } else {
            message.source_name.clone()
        };
        *sources.entry(name).or_insert(0) += 1;
    }
    let section = section(&mut s.retention_digest, "messages");
    touch(section, first, last, messages.len() as u64);
    merge_counts(section, "sources", sources, 8);
}

pub fn add_records(s: &mut State, audits: &[Audit]) {
    if audits.is_empty() {
        return;
    }
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    let mut kinds = BTreeMap::new();
    for audit in audits {
        first = first.min(audit.time);
        last = last.max(audit.time);
        *kinds.entry(audit.kind.clone()).or_insert(0) += 1;
    }
    let section = section(&mut s.retention_digest, "records");
    touch(section, first, last, audits.len() as u64);
    merge_counts(section, "kinds", kinds, 8);
}

pub fn add_tasks(s: &mut State, tasks: &[Task]) {
    if tasks.is_empty() {
        return;
    }
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    let mut statuses = BTreeMap::new();
    for task in tasks {
        let at = if task.archived_at > 0 {
            task.archived_at
        } else {
            task.updated
        };
        first = first.min(at);
        last = last.max(at);
        *statuses.entry(task.status.clone()).or_insert(0) += 1;
    }
    let section = section(&mut s.retention_digest, "tasks");
    touch(section, first, last, tasks.len() as u64);
    merge_counts(section, "statuses", statuses, 8);
}

/// Keep the digest bounded: first drop per-source/kind/status detail, then
/// drop individual time ranges, and finally keep only the aggregate counts.
pub fn compact_digest(s: &mut State) {
    if s.retention_digest.is_null() || digest_len(&s.retention_digest) <= DIGEST_LIMIT {
        return;
    }
    if let Some(object) = s.retention_digest.as_object_mut() {
        for key in ["messages", "records", "tasks"] {
            if let Some(section) = object.get_mut(key).and_then(Value::as_object_mut) {
                section.remove("sources");
                section.remove("kinds");
                section.remove("statuses");
            }
        }
        object.insert("compressed".into(), json!(true));
    }
    if digest_len(&s.retention_digest) <= DIGEST_LIMIT {
        return;
    }
    if let Some(object) = s.retention_digest.as_object_mut() {
        for key in ["messages", "records", "tasks"] {
            if let Some(section) = object.get_mut(key).and_then(Value::as_object_mut) {
                section.remove("first");
                section.remove("last");
            }
        }
    }
    if digest_len(&s.retention_digest) <= DIGEST_LIMIT {
        return;
    }
    let counts = ["messages", "records", "tasks"]
        .iter()
        .map(|key| {
            (
                *key,
                s.retention_digest[*key]["count"].as_u64().unwrap_or(0),
            )
        })
        .collect::<Vec<_>>();
    let mut reduced = serde_json::Map::new();
    for (key, count) in counts {
        reduced.insert(key.into(), json!({"count":count}));
    }
    reduced.insert("compressed".into(), json!(true));
    s.retention_digest = Value::Object(reduced);
}

fn rebase_memory_entries(entries: &mut [crate::model::MemoryEntry], map: &[Option<usize>]) {
    for entry in entries {
        let had_audit = entry.source.iter().any(|item| item.starts_with("audit:"));
        entry.source.retain_mut(|item| {
            let Some(old) = item
                .strip_prefix("audit:")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                return true;
            };
            match map.get(old).copied().flatten() {
                Some(new) => {
                    *item = format!("audit:{new}");
                    true
                }
                None => false,
            }
        });
        if had_audit && entry.source.is_empty() {
            // Keep the entry text but make it explicit that its audit lineage
            // was pruned; a dangling audit:N is worse than an empty source.
            entry.source.push("source:pruned".into());
        }
    }
}

/// Replace archive-group prompt copies that equal their referenced prompt
/// version with the by-reference form. History is unchanged; only size drops.
fn compact_archive_prompts(s: &mut State) -> bool {
    let mut changed = false;
    for index in 0..s.archive_versions.len() {
        let group = &s.archive_versions[index];
        if group.prompt.is_empty() {
            continue;
        }
        let stored = crate::archive::stored_prompt(s, &group.prompt_version, group.prompt.clone());
        if stored.is_empty() {
            s.archive_versions[index].prompt = stored;
            changed = true;
        }
    }
    changed
}

/// Archive groups, archive attempts, morning jobs and work-day records keep
/// this many days of history.
pub const HISTORY_DAYS: i64 = 90;

/// Trim expired, finished history records. The protected set is computed
/// first: the latest archive group, recent groups and every group they were
/// restored from, groups referenced by retained reports, unfinished attempts
/// and still-queued morning jobs (with their day-record claims). Runs before
/// audit pruning, so audit evidence held only by dropped groups is released.
fn prune_history(s: &mut State, at: i64) -> bool {
    let cutoff = at - HISTORY_DAYS * 86_400_000;
    let cutoff_day = crate::model::date_from(cutoff);
    let mut keep_groups = s
        .archive_versions
        .iter()
        .filter(|g| g.created >= cutoff)
        .map(|g| g.id.clone())
        .collect::<BTreeSet<_>>();
    keep_groups.extend(s.archive_versions.last().map(|g| g.id.clone()));
    keep_groups.extend(
        s.reports
            .iter()
            .filter_map(|r| r["archive_group_id"].as_str().map(str::to_owned)),
    );
    loop {
        let sources = s
            .archive_versions
            .iter()
            .filter(|g| keep_groups.contains(&g.id) && !g.restored_from.is_empty())
            .map(|g| g.restored_from.clone())
            .filter(|id| !keep_groups.contains(id))
            .collect::<Vec<_>>();
        if sources.is_empty() {
            break;
        }
        keep_groups.extend(sources);
    }
    let mut keep_attempts = s
        .archive_versions
        .iter()
        .filter(|g| keep_groups.contains(&g.id))
        .map(|g| g.attempt_id.clone())
        .collect::<BTreeSet<_>>();
    keep_attempts.extend(
        s.archive_attempts
            .iter()
            .filter(|a| a.unfinished() || a.created >= cutoff)
            .map(|a| a.id.clone()),
    );
    keep_attempts.extend(s.archive_attempts.last().map(|a| a.id.clone()));
    let queued_jobs = s
        .morning_jobs
        .iter()
        .filter(|j| ["queued", "running"].contains(&j.status.as_str()))
        .map(|j| j.id.clone())
        .collect::<BTreeSet<_>>();

    let before = (
        s.archive_versions.len(),
        s.archive_attempts.len(),
        s.morning_jobs.len(),
        s.work_scheduler.runtime.day_records.len(),
    );
    s.archive_versions.retain(|g| keep_groups.contains(&g.id));
    s.archive_attempts.retain(|a| keep_attempts.contains(&a.id));
    s.morning_jobs
        .retain(|j| queued_jobs.contains(&j.id) || j.business_day >= cutoff_day);
    s.work_scheduler.runtime.day_records.retain(|day, record| {
        day.to_string() >= cutoff_day
            || s.work_scheduler
                .runtime
                .active_overtime
                .as_ref()
                .is_some_and(|active| active.business_day == *day)
            || record
                .end_claim
                .as_ref()
                .is_some_and(|c| keep_attempts.contains(&c.attempt_id))
            || record
                .morning_claim
                .as_ref()
                .is_some_and(|c| queued_jobs.contains(&c.claim_id))
    });
    before
        != (
            s.archive_versions.len(),
            s.archive_attempts.len(),
            s.morning_jobs.len(),
            s.work_scheduler.runtime.day_records.len(),
        )
}

pub fn prune(s: &mut State, at: i64) -> bool {
    let compacted = compact_archive_prompts(s) | prune_history(s, at);
    let days = s.config.message_retention_days.max(1);
    let cutoff = at - i64::from(days) * 86_400_000;

    // Evidence that can still be shown or acted on keeps its audit record even
    // after the retention period; everything else is removed and rebased.
    let mut protected = BTreeSet::new();
    for suggestion in &s.suggestions {
        protected.extend(suggestion.evidence.iter().copied());
    }
    for review in &s.prompt_reviews {
        for row in review.evidence["audit"].as_array().into_iter().flatten() {
            if let Some(index) = row["audit_index"].as_u64() {
                protected.insert(index as usize);
            }
        }
    }
    for proposal in &s.tag_state.proposals {
        for evidence in &proposal.evidence {
            if let Some(index) = evidence
                .strip_prefix("audit:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                protected.insert(index);
            }
        }
    }
    for entry in &s.analysis_memory.archived_entries {
        for evidence in &entry.source {
            if let Some(index) = evidence
                .strip_prefix("audit:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                protected.insert(index);
            }
        }
    }
    for group in &s.archive_versions {
        for rule in &group.active_rules {
            protected.extend(rule.evidence.iter().copied());
        }
        for entry in &group.archived_entries {
            for evidence in &entry.source {
                if let Some(index) = evidence
                    .strip_prefix("audit:")
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    protected.insert(index);
                }
            }
        }
    }
    // The shared archive cursor defines unarchived evidence. Everything at or
    // after it must survive the 24h message/snapshot window and normal audit
    // retention so the next archive can still process it.
    let archive_start = s.archive_ai_cursor.unwrap_or(0).min(s.audit.len());
    protected.extend(archive_start..s.audit.len());

    let keep = s
        .audit
        .iter()
        .enumerate()
        .map(|(index, audit)| audit.time >= cutoff || protected.contains(&index))
        .collect::<Vec<_>>();
    let removed_audits = keep.iter().filter(|kept| !**kept).count();

    let evidence_runs = s
        .handling
        .proposals
        .iter()
        .flat_map(|proposal| proposal.evidence.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let protected_runs = s
        .handling
        .runs
        .iter()
        .filter(|run| {
            evidence_runs.contains(run.id.as_str())
                || run
                    .rounds
                    .iter()
                    .any(|round| evidence_runs.contains(round.id.as_str()))
        })
        .map(|run| run.id.clone())
        .collect::<BTreeSet<_>>();
    let active_tasks = s
        .handling
        .runs
        .iter()
        .filter(|run| is_active(&run.status))
        .map(|run| run.task_id.clone())
        .collect::<BTreeSet<_>>();
    let recent_tasks = s
        .handling
        .runs
        .iter()
        .filter(|run| run.updated >= cutoff || protected_runs.contains(&run.id))
        .map(|run| run.task_id.clone())
        .collect::<BTreeSet<_>>();
    let removed_tasks = s
        .tasks
        .iter()
        .filter(|task| {
            task.archived
                && (if task.archived_at > 0 {
                    task.archived_at
                } else {
                    task.updated
                }) < cutoff
                && !active_tasks.contains(&task.id)
                && !recent_tasks.contains(&task.id)
        })
        .map(|task| task.id.clone())
        .collect::<BTreeSet<_>>();

    if removed_audits == 0 && removed_tasks.is_empty() {
        return compacted;
    }

    let removed_records = s
        .audit
        .iter()
        .enumerate()
        .filter(|(index, _)| !keep[*index])
        .map(|(_, audit)| audit.clone())
        .collect::<Vec<_>>();
    add_records(s, &removed_records);
    let removed_task_rows = s
        .tasks
        .iter()
        .filter(|task| removed_tasks.contains(&task.id))
        .cloned()
        .collect::<Vec<_>>();
    add_tasks(s, &removed_task_rows);

    let mut map = Vec::with_capacity(s.audit.len());
    let mut next = 0;
    for kept in &keep {
        if *kept {
            map.push(Some(next));
            next += 1;
        } else {
            map.push(None);
        }
    }
    let old = std::mem::take(&mut s.audit);
    s.audit = old
        .into_iter()
        .zip(keep)
        .filter_map(|(audit, kept)| kept.then_some(audit))
        .collect();

    let rebase = |cursor: usize| {
        map.iter()
            .take(cursor.min(map.len()))
            .filter(|mapped| mapped.is_some())
            .count()
    };
    s.learning_cursor = rebase(s.learning_cursor);
    s.report_cursor = rebase(s.report_cursor);
    s.archive_review_cursor = rebase(s.archive_review_cursor);
    s.archive_ai_cursor = s.archive_ai_cursor.map(rebase);

    for suggestion in &mut s.suggestions {
        suggestion.evidence = suggestion
            .evidence
            .iter()
            .filter_map(|index| map.get(*index).copied().flatten())
            .collect();
    }
    for review in &mut s.prompt_reviews {
        if let Some(rows) = review.evidence["audit"].as_array_mut() {
            rows.retain_mut(|row| {
                let Some(old) = row["audit_index"].as_u64() else {
                    return true;
                };
                match map.get(old as usize).copied().flatten() {
                    Some(new) => {
                        row["audit_index"] = json!(new);
                        if row["evidence_id"]
                            .as_str()
                            .is_some_and(|value| value.starts_with("audit:"))
                        {
                            row["evidence_id"] = json!(format!("audit:{new}"));
                        }
                        true
                    }
                    None => false,
                }
            });
        }
    }
    for proposal in &mut s.tag_state.proposals {
        let mut evidence = Vec::new();
        let mut excerpts = Vec::new();
        for (item, excerpt) in proposal.evidence.iter().zip(proposal.excerpts.iter()) {
            match item
                .strip_prefix("audit:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                Some(old) => {
                    if let Some(new) = map.get(old).copied().flatten() {
                        evidence.push(format!("audit:{new}"));
                        excerpts.push(excerpt.clone());
                    }
                }
                None => {
                    evidence.push(item.clone());
                    excerpts.push(excerpt.clone());
                }
            }
        }
        proposal.evidence = evidence;
        proposal.excerpts = excerpts;
    }
    for report in &mut s.reports {
        if let Some(suggestions) = report["suggestions"].as_array_mut() {
            for suggestion in suggestions {
                if let Some(values) = suggestion["evidence"].as_array_mut() {
                    values.retain_mut(|value| {
                        let Some(old) = value.as_u64() else {
                            return true;
                        };
                        match map.get(old as usize).copied().flatten() {
                            Some(new) => {
                                *value = json!(new);
                                true
                            }
                            None => false,
                        }
                    });
                }
            }
        }
    }
    // Archived group snapshots are rollback material. Keep their audit
    // references aligned with the pruned source just like live suggestions.
    for group in &mut s.archive_versions {
        for suggestion in &mut group.active_rules {
            suggestion.evidence = suggestion
                .evidence
                .iter()
                .filter_map(|index| map.get(*index).copied().flatten())
                .collect();
        }
        rebase_memory_entries(&mut group.archived_entries, &map);
    }
    // Live archived memory carries the same audit:N evidence references and is
    // not protected by a group snapshot, so it must be rebased too.
    rebase_memory_entries(&mut s.analysis_memory.archived_entries, &map);

    if !removed_tasks.is_empty() {
        s.tasks.retain(|task| !removed_tasks.contains(&task.id));
        s.handling
            .runs
            .retain(|run| !removed_tasks.contains(&run.task_id));
    }
    compact_digest(s);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::tags::{Proposal, Tag};

    fn audit(time: i64, task: &str) -> Audit {
        Audit {
            time,
            kind: "manual_archive".into(),
            task_id: task.into(),
            detail: "归档".into(),
            change_summary: String::new(),
            before: json!({}),
            after: json!({}),
        }
    }

    fn task(id: &str, archived: bool, archived_at: i64) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            status: "已完成".into(),
            archived,
            archived_at,
            updated: archived_at,
            ..Default::default()
        }
    }

    fn run(id: &str, task: &str, status: &str, updated: i64) -> crate::handling::Run {
        crate::handling::Run {
            id: id.into(),
            task_id: task.into(),
            status: status.into(),
            updated,
            ..Default::default()
        }
    }

    #[test]
    fn removes_expired_audit_and_archived_tasks_and_rebases_every_reference() {
        let at = 1_800_000_000_000;
        let old = at - 100 * 86_400_000;
        let mut s = State::default();
        s.config.message_retention_days = 90;
        *s.audit = vec![
            audit(old, "t-old"),
            audit(old + 1, "t-keep"),
            audit(at - 1, "t-new"),
        ];
        s.learning_cursor = 3;
        s.report_cursor = 1;
        s.archive_review_cursor = 3;
        s.archive_ai_cursor = Some(3);
        s.suggestions.push(Suggestion {
            id: "s".into(),
            title: "规则".into(),
            rule: "规则".into(),
            evidence: vec![0, 2],
            ..Default::default()
        });
        s.prompt_reviews.push(PromptReview {
            id: "r".into(),
            status: "pending".into(),
            evidence: json!({"audit":[{"audit_index":2,"evidence_id":"audit:2","event":{}}],"logs":[]}),
            ..Default::default()
        });
        s.tag_state.proposals.push(Proposal {
            id: "p".into(),
            tag: Tag {
                kind: "事项类型".into(),
                value: "测试".into(),
            },
            evidence: vec!["audit:2".into(), "input:req".into()],
            excerpts: vec!["一".into(), "二".into()],
            status: "pending".into(),
            ..Default::default()
        });
        s.reports
            .push(json!({"suggestions":[{"id":"s","evidence":[2]}]}));
        *s.tasks = vec![
            task("t-old", true, old),
            task("t-keep", true, at - 86_400_000),
            task("t-open", false, 0),
            task("t-run", true, old),
        ];
        *s.handling.runs = vec![
            run("r-old", "t-old", "completed", old),
            run("r-recent", "t-run", "completed", at - 86_400_000),
        ];

        assert!(prune(&mut s, at));
        assert_eq!(s.audit.len(), 2);
        assert_eq!(s.audit[0].task_id, "t-old");
        assert_eq!(s.audit[1].task_id, "t-new");
        assert_eq!(s.learning_cursor, 2);
        assert_eq!(s.report_cursor, 1);
        assert_eq!(s.archive_review_cursor, 2);
        assert_eq!(s.archive_ai_cursor, Some(2));
        assert_eq!(s.suggestions[0].evidence, vec![0, 1]);
        assert_eq!(s.prompt_reviews[0].evidence["audit"][0]["audit_index"], 1);
        assert_eq!(
            s.prompt_reviews[0].evidence["audit"][0]["evidence_id"],
            "audit:1"
        );
        assert_eq!(
            s.tag_state.proposals[0].evidence,
            vec!["audit:1".to_string(), "input:req".to_string()]
        );
        assert_eq!(s.tag_state.proposals[0].excerpts, vec!["一", "二"]);
        assert_eq!(s.reports[0]["suggestions"][0]["evidence"], json!([1]));
        let ids = s.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["t-keep", "t-open", "t-run"]);
        assert_eq!(s.handling.runs.len(), 1);
        assert_eq!(s.handling.runs[0].id, "r-recent");
        assert!(!prune(&mut s, at));
    }

    #[test]
    fn live_and_group_memory_sources_are_protected_and_rebased_without_dangling_audit_refs() {
        let at = 1_800_000_000_000;
        let old = at - 100 * 86_400_000;
        let mut s = State::default();
        s.config.message_retention_days = 90;
        *s.audit = vec![audit(old, "remove"), audit(old + 1, "keep")];
        s.archive_ai_cursor = Some(2);
        s.analysis_memory.archived_entries.push(MemoryEntry {
            id: "live".into(),
            key: "scope".into(),
            text: "偏好".into(),
            source: vec!["audit:1".into(), "message:1".into()],
            version: 1,
            ..Default::default()
        });
        s.archive_versions.push(ArchiveGroup {
            id: "g".into(),
            created: at,
            action: "archive".into(),
            archived_entries: vec![MemoryEntry {
                id: "group".into(),
                key: "scope2".into(),
                text: "旧偏好".into(),
                source: vec!["audit:1".into()],
                version: 1,
                ..Default::default()
            }],
            active_rules: vec![Suggestion {
                id: "rule".into(),
                evidence: vec![1],
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(prune(&mut s, at));
        assert_eq!(s.audit.len(), 1);
        assert_eq!(s.analysis_memory.archived_entries[0].source[0], "audit:0");
        assert_eq!(
            s.archive_versions[0].archived_entries[0].source,
            vec!["audit:0"]
        );
        assert_eq!(s.archive_versions[0].active_rules[0].evidence, vec![0]);
        assert!(s
            .analysis_memory
            .archived_entries
            .iter()
            .flat_map(|entry| entry.source.iter())
            .all(|source| source == "message:1" || source == "audit:0"));
    }

    #[test]
    fn active_or_protected_work_keeps_its_task_and_run() {
        let at = 1_800_000_000_000;
        let old = at - 100 * 86_400_000;
        let mut s = State::default();
        s.config.message_retention_days = 90;
        *s.tasks = vec![task("t-active", true, old), task("t-proposal", true, old)];
        *s.handling.runs = vec![
            run("r-active", "t-active", "inProgress", old),
            run("r-proposal", "t-proposal", "completed", old),
        ];
        s.handling.proposals.push(crate::handling::Proposal {
            id: "p".into(),
            base: "b".into(),
            reason: "r".into(),
            evidence: vec!["r-proposal".into()],
            prompt: String::new(),
            legacy_policy: None,
            adopted: false,
            created: old,
            archived_at: 0,
            rejected: false,
        });
        assert!(!prune(&mut s, at));
        assert_eq!(s.tasks.len(), 2);
        assert_eq!(s.handling.runs.len(), 2);
    }

    #[test]
    fn audit_after_archive_cursor_survives_retention_until_processed() {
        let at = 1_800_000_000_000;
        let old = at - 100 * 86_400_000;
        let mut s = State::default();
        s.config.message_retention_days = 90;
        *s.audit = vec![audit(old, "t-open")];
        s.archive_ai_cursor = Some(0);
        assert!(!prune(&mut s, at));
        assert_eq!(s.audit.len(), 1);
        s.archive_ai_cursor = Some(1);
        assert!(prune(&mut s, at));
        assert!(s.audit.is_empty());
        assert_eq!(s.retention_digest["records"]["count"], 1);
    }

    #[test]
    fn expired_content_is_reduced_to_a_count_only_digest() {
        let at = crate::model::now();
        let old = at - 100 * 86_400_000;
        let mut s = State::default();
        s.config.message_retention_days = 90;
        s.messages.insert(
            "old".into(),
            crate::model::Message {
                id: "old".into(),
                source_name: "示例来源".into(),
                time: old,
                ..Default::default()
            },
        );
        s.messages.insert(
            "new".into(),
            crate::model::Message {
                id: "new".into(),
                source_name: "示例来源".into(),
                time: at,
                ..Default::default()
            },
        );
        crate::messages::prune(&mut s);
        assert!(!s.messages.contains_key("old"));
        assert_eq!(s.retention_digest["messages"]["count"], 1);
        assert_eq!(
            s.retention_digest["messages"]["sources"][0]["name"],
            "示例来源"
        );
        *s.audit = vec![audit(old, "t-old")];
        *s.tasks = vec![task("t-old", true, old)];
        s.archive_ai_cursor = Some(s.audit.len());
        assert!(prune(&mut s, at));
        assert_eq!(s.retention_digest["records"]["count"], 1);
        assert_eq!(s.retention_digest["tasks"]["count"], 1);
        assert!(s.retention_digest.to_string().contains("manual_archive"));
        assert!(!s.retention_digest.to_string().contains("t-old"));
    }

    #[test]
    fn oversized_digest_is_compressed_without_losing_totals() {
        let mut s = State::default();
        s.retention_digest = json!({
            "messages":{"count":7,"sources":(0..2000).map(|i|json!({"name":format!("来源{i}"),"count":1})).collect::<Vec<_>>()},
            "records":{"count":3,"kinds":[{"name":"manual_archive","count":3}]},
            "tasks":{"count":2,"statuses":[{"name":"已完成","count":2}]}
        });
        compact_digest(&mut s);
        assert!(s.retention_digest.to_string().chars().count() <= DIGEST_LIMIT);
        assert_eq!(s.retention_digest["messages"]["count"], 7);
        assert_eq!(s.retention_digest["records"]["count"], 3);
        assert_eq!(s.retention_digest["tasks"]["count"], 2);
        assert_eq!(s.retention_digest["compressed"], true);
        assert!(s.retention_digest["messages"].get("sources").is_none());
    }

    #[test]
    fn identical_archive_prompts_are_stored_by_reference_without_losing_history() {
        let mut s = State::default();
        let version = crate::prompts::record(&mut s, "classify", "规则 A".into(), "manual", "初始");
        let group = |id: &str, prompt: &str, version: &str| ArchiveGroup {
            id: id.into(),
            created: 1_800_000_000_000,
            prompt: prompt.into(),
            prompt_version: version.into(),
            ..Default::default()
        };
        *s.archive_versions = vec![
            group("same", "规则 A", &version),
            group("edited", "规则 A（手改）", &version),
            group("orphan", "规则 B", "missing"),
        ];
        assert!(prune(&mut s, 1_800_000_000_000));
        let stored = s
            .archive_versions
            .iter()
            .map(|g| g.prompt.as_str())
            .collect::<Vec<_>>();
        assert_eq!(stored, vec!["", "规则 A（手改）", "规则 B"]);
        let resolved = s
            .archive_versions
            .iter()
            .map(|g| crate::archive::archive_prompt(&s, g))
            .collect::<Vec<_>>();
        assert_eq!(resolved, vec!["规则 A", "规则 A（手改）", "规则 B"]);
        assert!(!prune(&mut s, 1_800_000_000_000), "already compact");
    }

    #[test]
    fn history_keeps_ninety_days_plus_every_protected_reference() {
        let at = 1_800_000_000_000;
        let old = at - 200 * 86_400_000;
        let mut s = State::default();
        let group = |id: &str, created: i64, restored_from: &str, attempt: &str| ArchiveGroup {
            id: id.into(),
            created,
            restored_from: restored_from.into(),
            attempt_id: attempt.into(),
            ..Default::default()
        };
        *s.archive_versions = vec![
            group("old-unreferenced", old, "", "a-old"),
            group("old-rollback-source", old, "", "a-source"),
            group("old-in-report", old, "", ""),
            group("recent-rollback", at, "old-rollback-source", ""),
            group("current", old + 1, "", ""),
        ];
        *s.reports = vec![json!({"archive_group_id":"old-in-report"})];
        let attempt = |id: &str, created: i64, status: &str| ArchiveAttempt {
            id: id.into(),
            created,
            status: status.into(),
            ..Default::default()
        };
        s.archive_attempts = vec![
            attempt("a-old", old, "succeeded"),
            attempt("a-source", old, "succeeded"),
            attempt("a-stuck", old, "queued"),
            attempt("a-recent", at, "failed"),
        ];
        let job = |id: &str, day: &str, status: &str| crate::calendar_service::MorningJob {
            id: id.into(),
            business_day: day.into(),
            status: status.into(),
            error: String::new(),
        };
        s.morning_jobs = vec![
            job("m-old", &crate::model::date_from(old), "done"),
            job("m-queued", &crate::model::date_from(old), "queued"),
            job("m-new", &crate::model::date_from(at), "done"),
        ];
        let day = |t: i64| {
            chrono::NaiveDate::parse_from_str(&crate::model::date_from(t), "%Y-%m-%d").unwrap()
        };
        let records = &mut s.work_scheduler.runtime.day_records;
        records.insert(day(old), crate::work_state::WorkDayRecord::empty());
        records.insert(day(at), crate::work_state::WorkDayRecord::empty());

        assert!(prune(&mut s, at));
        let ids = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            s.archive_versions
                .iter()
                .map(|g| g.id.clone())
                .collect::<Vec<_>>(),
            ids(vec![
                "old-rollback-source",
                "old-in-report",
                "recent-rollback",
                "current"
            ])
        );
        assert_eq!(
            s.archive_attempts
                .iter()
                .map(|a| a.id.clone())
                .collect::<Vec<_>>(),
            ids(vec!["a-source", "a-stuck", "a-recent"])
        );
        assert_eq!(
            s.morning_jobs
                .iter()
                .map(|j| j.id.clone())
                .collect::<Vec<_>>(),
            ids(vec!["m-queued", "m-new"])
        );
        assert_eq!(
            s.work_scheduler
                .runtime
                .day_records
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![day(at)]
        );
        assert!(s.work_scheduler.validate().is_ok());
        assert!(!prune(&mut s, at), "already trimmed");
    }
}
