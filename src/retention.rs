//! Retention shared by messages, audit history, archived tasks and trace files.
//! Audit indices are stable inside a state file, so deletion rebases every cursor
//! and evidence reference that points into the audit vector.
use crate::model::{json_chars, State, Task};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Expired records are replaced by a compact statistical digest. The cap keeps
/// the digest itself bounded even after years of cleanup.
pub const DIGEST_LIMIT: usize = 8_000;

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

fn merge_counts(section: &mut Value, field: &str, incoming: BTreeMap<&str, u64>, cap: usize) {
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
        *merged.entry(name.into()).or_default() += count;
    }
    let mut rows = merged.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    rows.truncate(cap);
    section[field] = json!(rows
        .into_iter()
        .map(|(name, count)| json!({"name":name,"count":count}))
        .collect::<Vec<_>>());
}

/// Summarize only timestamps and categories; record bodies stay borrowed.
pub fn record_pruned<'a>(
    digest: &mut Value,
    key: &str,
    field: &str,
    rows: impl Iterator<Item = (i64, &'a str)>,
) -> bool {
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    let mut count = 0;
    let mut groups = BTreeMap::new();
    for (at, name) in rows {
        first = first.min(at);
        last = last.max(at);
        count += 1;
        *groups.entry(name).or_insert(0) += 1;
    }
    if count == 0 {
        return false;
    }
    let section = section(digest, key);
    touch(section, first, last, count);
    merge_counts(section, field, groups, 8);
    true
}

fn archived_at(task: &Task) -> i64 {
    if task.archived_at > 0 {
        task.archived_at
    } else {
        task.updated
    }
}

fn audit_index(source: &str) -> Option<usize> {
    source
        .strip_prefix("audit:")
        .and_then(|value| value.parse().ok())
}

/// Keep the digest bounded: first drop per-source/kind/status detail, then
/// drop individual time ranges, and finally keep only the aggregate counts.
pub fn compact_digest(s: &mut State) {
    if s.retention_digest.is_null() || json_chars(&s.retention_digest) <= DIGEST_LIMIT {
        return;
    }
    for fields in [&["sources", "kinds", "statuses"][..], &["first", "last"]] {
        if let Some(object) = s.retention_digest.as_object_mut() {
            for key in ["messages", "records", "tasks"] {
                if let Some(section) = object.get_mut(key).and_then(Value::as_object_mut) {
                    for field in fields {
                        section.remove(*field);
                    }
                }
            }
            object.insert("compressed".into(), json!(true));
        }
        if json_chars(&s.retention_digest) <= DIGEST_LIMIT {
            return;
        }
    }
    let mut reduced = ["messages", "records", "tasks"]
        .into_iter()
        .map(|key| {
            (
                key.into(),
                json!({"count":s.retention_digest[key]["count"].as_u64().unwrap_or(0)}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    reduced.insert("compressed".into(), json!(true));
    s.retention_digest = Value::Object(reduced);
}

fn rebase_memory_entries(entries: &mut [crate::model::MemoryEntry], map: &[Option<usize>]) {
    for entry in entries {
        let had_audit = entry.source.iter().any(|item| item.starts_with("audit:"));
        entry.source.retain_mut(|item| {
            let Some(old) = audit_index(item) else {
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
        if crate::archive::stored_prompt(s, &group.prompt_version, group.prompt.as_str()).is_empty()
        {
            s.archive_versions[index].prompt = String::new();
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
/// and still-queued morning jobs (with their day-record claims), plus the latest
/// completed report outcome. Runs before
/// audit pruning, so audit evidence held only by dropped groups is released.
fn prune_history(s: &mut State, at: i64) -> bool {
    let latest = s
        .report_outcome()
        .map(|outcome| (outcome.kind, outcome.id.to_owned()));
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
    if let Some(("archive", id)) = &latest {
        keep_attempts.insert(id.clone());
    }
    let mut keep_jobs = s
        .morning_jobs
        .iter()
        .filter(|j| ["queued", "running"].contains(&j.status.as_str()))
        .map(|j| j.id.clone())
        .collect::<BTreeSet<_>>();

    if let Some(("morning", id)) = &latest {
        keep_jobs.insert(id.clone());
    }

    let before = (
        s.archive_versions.len(),
        s.archive_attempts.len(),
        s.morning_jobs.len(),
        s.work_scheduler.runtime.day_records.len(),
    );
    s.archive_versions.retain(|g| keep_groups.contains(&g.id));
    s.archive_attempts.retain(|a| keep_attempts.contains(&a.id));
    s.morning_jobs
        .retain(|j| keep_jobs.contains(&j.id) || j.business_day >= cutoff_day);
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
                .is_some_and(|c| keep_jobs.contains(&c.claim_id))
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
    for suggestion in s
        .suggestions
        .iter()
        .chain(s.archive_versions.iter().flat_map(|g| &g.active_rules))
    {
        protected.extend(suggestion.evidence.iter().copied());
    }
    for review in &s.prompt_reviews {
        for row in review.evidence["audit"].as_array().into_iter().flatten() {
            if let Some(index) = row["audit_index"].as_u64() {
                protected.insert(index as usize);
            }
        }
    }
    let memory = s
        .analysis_memory
        .archived_entries
        .iter()
        .chain(s.archive_versions.iter().flat_map(|g| &g.archived_entries));
    protected.extend(
        s.tag_state
            .proposals
            .iter()
            .flat_map(|p| &p.evidence)
            .chain(memory.flat_map(|entry| &entry.source))
            .filter_map(|source| audit_index(source)),
    );
    // The shared archive cursor defines unarchived evidence. Everything at or
    // after it must survive the 24h message/snapshot window and normal audit
    // retention so the next archive can still process it.
    let archive_start = s.archive_ai_cursor.unwrap_or(0).min(s.audit.len());
    let mut retained = 0;
    let map = s
        .audit
        .iter()
        .enumerate()
        .map(|(index, audit)| {
            (index >= archive_start || audit.time >= cutoff || protected.contains(&index)).then(
                || {
                    let new = retained;
                    retained += 1;
                    new
                },
            )
        })
        .collect::<Vec<_>>();
    let removed_audits = s.audit.len() - retained;

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
        .map(|run| run.id.as_str())
        .collect::<BTreeSet<_>>();
    let protected_tasks = s
        .handling
        .runs
        .iter()
        .filter(|run| {
            crate::handling::active(run)
                || run.updated >= cutoff
                || protected_runs.contains(run.id.as_str())
        })
        .map(|run| run.task_id.as_str())
        .collect::<BTreeSet<_>>();
    let removed_tasks = s
        .tasks
        .iter()
        .filter(|task| {
            task.archived
                && archived_at(task) < cutoff
                && !protected_tasks.contains(task.id.as_str())
        })
        .map(|task| task.id.clone())
        .collect::<BTreeSet<_>>();

    if removed_audits == 0 && removed_tasks.is_empty() {
        return compacted;
    }

    record_pruned(
        &mut s.retention_digest,
        "records",
        "kinds",
        s.audit
            .iter()
            .enumerate()
            .filter(|(index, _)| map[*index].is_none())
            .map(|(_, audit)| (audit.time, audit.kind.as_str())),
    );
    record_pruned(
        &mut s.retention_digest,
        "tasks",
        "statuses",
        s.tasks
            .iter()
            .filter(|task| removed_tasks.contains(&task.id))
            .map(|task| (archived_at(task), task.status.as_str())),
    );

    let mut positions = map.iter();
    s.audit.retain(|_| positions.next().unwrap().is_some());

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

    // Live rules and rollback snapshots use the same audit index space.
    for suggestion in s.suggestions.iter_mut().chain(
        s.archive_versions
            .iter_mut()
            .flat_map(|g| &mut g.active_rules),
    ) {
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
        (proposal.evidence, proposal.excerpts) = std::mem::take(&mut proposal.evidence)
            .into_iter()
            .zip(std::mem::take(&mut proposal.excerpts))
            .filter_map(|(mut item, excerpt)| {
                if let Some(old) = audit_index(&item) {
                    item = format!("audit:{}", map.get(old).copied().flatten()?);
                }
                Some((item, excerpt))
            })
            .unzip();
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
    for group in &mut s.archive_versions {
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
        for status in [
            "starting",
            "submitting",
            "inProgress",
            "waiting",
            "uncertain",
            "disconnected",
            "stopping",
        ] {
            s.tasks.push(task(status, true, old));
            s.handling.runs.push(run(status, status, status, old));
        }
        s.tasks.push(task("t-proposal", true, old));
        s.handling
            .runs
            .push(run("r-proposal", "t-proposal", "completed", old));
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
        assert_eq!(s.tasks.len(), 8);
        assert_eq!(s.handling.runs.len(), 8);
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
        for (id, time) in [("old", old), ("new", at)] {
            s.messages.insert(
                id.into(),
                Message {
                    id: id.into(),
                    source_name: "示例来源".into(),
                    time,
                    ..Default::default()
                },
            );
        }
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
            finished: 0,
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
        records.insert(day(old), crate::work_state::WorkDayRecord::default());
        records.insert(day(at), crate::work_state::WorkDayRecord::default());

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
