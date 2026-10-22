use crate::{
    context_budget,
    engine::{self, App},
    model::*,
    prompts,
    work_hours::AiMode,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, sync::atomic::Ordering};

/// Archive groups keep the classify prompt by reference when an identical
/// prompt version exists. Prompt versions are never pruned, so the rollback
/// history stays complete without one full prompt copy per archived day.
pub(crate) fn stored_prompt(s: &State, version: &str, content: String) -> String {
    if s.prompt_versions
        .iter()
        .any(|v| v.id == version && v.content == content)
    {
        String::new()
    } else {
        content
    }
}

/// The prompt an archive group restores, resolving a by-reference copy.
pub(crate) fn archive_prompt<'a>(s: &'a State, group: &'a ArchiveGroup) -> &'a str {
    if group.prompt.is_empty() {
        s.prompt_versions
            .iter()
            .find(|v| v.id == group.prompt_version)
            .map_or("", |v| v.content.as_str())
    } else {
        &group.prompt
    }
}

pub const RECOVERY_ERROR: &str =
    "重启中断：未完成批次保留，已提交批次不回滚；等待手动归档，不会自动重试";

/// Business day follows the persisted work schedule (the most recent Shanghai
/// `work_start`, including overnight shifts). Calendar corrections and overtime
/// never move the business date; only an unparseable schedule falls back to the
/// legacy wall-clock calculation for diagnostics, and configure rejects that.
fn business_day(state: &State, at: i64) -> String {
    crate::work_state::WorkSchedule::parse(&state.config.work_start, &state.config.work_end)
        .and_then(|schedule| schedule.business_date(at))
        .map(|date| date.to_string())
        .unwrap_or_else(|_| crate::ai::business_date(&state.config, at))
}
pub fn initialize(s: &mut State) {
    if s.archive_ai_cursor.is_some() {
        return;
    }
    let pending = s
        .prompt_reviews
        .iter()
        .filter(|r| ["pending", "failed", "interrupted"].contains(&r.status.as_str()))
        .flat_map(|r| r.evidence["audit"].as_array().into_iter().flatten())
        .filter_map(|v| {
            v["evidence_id"]
                .as_str()?
                .strip_prefix("audit:")?
                .parse::<usize>()
                .ok()
        })
        .min()
        .unwrap_or(s.audit.len());
    s.archive_ai_cursor = Some(
        s.learning_cursor
            .min(s.report_cursor)
            .min(pending)
            .min(s.audit.len()),
    );
}
/// Startup recovery is deliberately one-way: queued/running attempts become
/// failed. A committed success is never replayed as failed.
pub fn recover(s: &mut State, at: i64) {
    for attempt in &mut s.archive_attempts {
        if attempt.unfinished() {
            attempt.status = "failed".into();
            attempt.error = RECOVERY_ERROR.into();
            attempt.finished = at;
        }
    }
}

fn evidence(a: &Audit) -> bool {
    !a.task_id.is_empty()
        && (engine::learning_evidence(a)
            || [
                "ai_create",
                "ai_update",
                "ai_evidence",
                "ai_merge_duplicate",
                "manual_status",
                "manual_archive",
                "generation_done",
                "archive_item",
                "deadline_update",
            ]
            .contains(&a.kind.as_str()))
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub memory_revision: u64,
    pub trigger: String,
    pub attempt_id: String,
    pub business_day: String,
    pub payload: Value,
    pub cutoff: usize,
    pub evidence_ids: Vec<usize>,
    pub learning_ids: Vec<usize>,
    pub report: Value,
    pub tasks_to_archive: Vec<Task>,
    pub referenced_messages: BTreeSet<String>,
    pub pending_count: usize,
    pub remaining_events: usize,
}
impl Candidate {
    fn has_ai_input(&self) -> bool {
        !self.evidence_ids.is_empty()
    }
    fn evidence_refs(&self) -> BTreeSet<String> {
        self.evidence_ids
            .iter()
            .map(|i| format!("audit:{i}"))
            .chain(self.learning_ids.iter().map(|i| format!("audit:{i}")))
            .chain(self.referenced_messages.iter().cloned())
            .collect()
    }
}

fn batch(s: &State, mode: AiMode, at: i64, attempt_id: &str, business_day: &str) -> Candidate {
    batch_limited(s, mode, at, attempt_id, business_day, usize::MAX)
}

fn batch_limited(
    s: &State,
    mode: AiMode,
    at: i64,
    attempt_id: &str,
    business_day: &str,
    max_events: usize,
) -> Candidate {
    let start = s.archive_ai_cursor.unwrap_or(0).min(s.audit.len());
    let mut cutoff = start;
    let mut audits = vec![];
    let mut evidence_ids = vec![];
    let mut learning_ids = vec![];
    for (i, a) in s.audit.iter().enumerate().skip(start) {
        if evidence(a) {
            let mut event = context_budget::audit(a);
            if let Some(note) = a.after["archive_note"]
                .as_str()
                .filter(|v| !v.trim().is_empty())
            {
                event["archive_note"] = json!(note.chars().take(2000).collect::<String>());
            }
            let row = json!({"audit_index":i,"evidence_id":format!("audit:{i}"),"event":event});
            if evidence_ids.len() >= max_events {
                break;
            }
            audits.push(row);
            evidence_ids.push(i);
            if engine::learning_evidence(a) {
                learning_ids.push(i);
            }
        }
        cutoff = i + 1;
    }
    let related = evidence_ids
        .iter()
        .map(|i| s.audit[*i].task_id.as_str())
        .collect::<BTreeSet<_>>();
    let tasks = s
        .tasks
        .iter()
        .filter(|t| related.contains(t.id.as_str()) && t.merged_into.is_empty())
        .collect::<Vec<_>>();
    let selected = context_budget::rows(
        tasks.iter().map(|t| context_budget::task(t)),
        usize::MAX,
        usize::MAX,
    );
    // Only finalize tasks whose latest relevant event was processed. An early
    // update included in this batch is not enough: a final completion or new
    // archive note may sit beyond the evidence cutoff and still be pending.
    let included_evidence = evidence_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut latest_relevant = std::collections::BTreeMap::<&str, usize>::new();
    for (index, audit) in s.audit.iter().enumerate() {
        if evidence(audit) && !audit.task_id.is_empty() {
            latest_relevant.insert(audit.task_id.as_str(), index);
        }
    }
    let tasks_to_archive = s
        .tasks
        .iter()
        .filter(|t| {
            !t.archived
                && t.merged_into.is_empty()
                && t.generation.is_empty()
                && ["已完成", "无需处理", "已取消"].contains(&t.status.as_str())
                && latest_relevant
                    .get(t.id.as_str())
                    .is_none_or(|latest| *latest < start || included_evidence.contains(latest))
        })
        .cloned()
        .collect::<Vec<_>>();
    let referenced_messages = tasks
        .iter()
        .flat_map(|task| task.evidence.all())
        .collect::<BTreeSet<_>>();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for t in &s.tasks {
        if !t.archived {
            *counts.entry(t.status.clone()).or_default() += 1;
        }
    }
    let remaining = s.audit.iter().skip(cutoff).filter(|a| evidence(a)).count();
    let trigger = if mode == AiMode::Manual {
        "manual_archive"
    } else {
        "nightly_archive"
    };
    let mut payload = json!({
        "evidence":audits,
        "archive_memory_sources": evidence_ids.iter().map(|i| {
            let audit=&s.audit[*i];
            json!({"evidence_id":format!("audit:{i}"),"task_id":audit.task_id,"kind":audit.kind})
        }).collect::<Vec<_>>(),
        "archive_note_sources": evidence_ids.iter().filter_map(|i| {
            let audit = &s.audit[*i];
            (audit.kind == "manual_archive").then_some(audit.after["archive_note"].as_str()).flatten()
                .filter(|note| !note.trim().is_empty())
                .map(|note| json!({"evidence_id":format!("audit:{i}"),"task_id":audit.task_id,"note":note.chars().take(2000).collect::<String>()}))
        }).collect::<Vec<_>>(),
        "learning_evidence_ids":learning_ids.iter().map(|i|format!("audit:{i}")).collect::<Vec<_>>(),
        "report_tasks":selected["items"],
        "current_tasks":context_budget::rows(s.tasks.iter().filter(|t|!t.archived&&t.merged_into.is_empty()&&!related.contains(t.id.as_str())).map(context_budget::task),usize::MAX,usize::MAX)["items"],
        "coverage":{"status_counts":counts,"remaining_events":remaining,"omitted_tasks":selected["omitted"]},
        "tag_catalog":crate::tags::groups(s),
        "_trigger":trigger,
        "_archive_attempt_id":attempt_id,
        "_data_until":s.last_scan,
    });
    if !s.config.report_prompt.trim().is_empty() {
        payload["report_policy"] = json!(s.config.report_prompt);
    }
    let pending_count = crate::context_budget::pending(s).len();
    let remaining_events = s.audit.iter().skip(cutoff).filter(|a| evidence(a)).count();
    crate::analysis_context::attach_memory_text(
        &mut payload,
        &s.analysis_memory.archived_text(),
        &s.analysis_memory.temporary,
    );
    payload["_local_context"]["memory"]["writable"] = json!(false);
    payload["memory"]["writable"] = json!(false);
    let records = crate::snapshots::records_from_state(s, at);
    let _ = crate::analysis_context::attach_records(
        &mut payload,
        records,
        s.config.retrieval_budget_chars,
    );
    // Freeze the exact candidate view: later scans/manual edits must not change
    // what the model sees while this archive is being generated.
    payload["_context_frozen"] = json!(true);
    let report = json!({
        "id":id(),
        "time":at,
        // The report belongs to the frozen business day, not the wall-clock
        // date. They differ for an overnight shift archived after midnight.
        "date":business_day,
        "label":if mode==AiMode::Manual{"手动归档"}else{"下班归档"},
        "trigger":trigger,
        "tasks":tasks,
        "status_counts":counts,
        "delta":evidence_ids.iter().map(|i|&s.audit[*i]).collect::<Vec<_>>(),
        "suggestions":[],
        "summary_status":"pending",
        "data_until":s.last_scan,
        "pending_analysis":pending_count,
        "remaining_events":remaining,
    });
    Candidate {
        memory_revision: s.analysis_memory.archived_version,
        trigger: trigger.into(),
        attempt_id: attempt_id.into(),
        business_day: business_day.into(),
        payload,
        cutoff,
        evidence_ids,
        learning_ids,
        report,
        tasks_to_archive,
        referenced_messages,
        pending_count,
        remaining_events,
    }
}

fn valid_summary(summary: &Value) -> bool {
    summary["summary"].is_string()
        && ["completed", "attention", "next_steps"].iter().all(|k| {
            summary[k]
                .as_array()
                .is_some_and(|a| a.len() <= 5 && a.iter().all(Value::is_string))
        })
}

fn archived_entries_text(entries: &[MemoryEntry]) -> String {
    entries
        .iter()
        .filter(|entry| !entry.text.trim().is_empty())
        .map(|entry| format!("- {}：{}", entry.key, entry.text.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_memory_changes(
    s: &mut State,
    changes: &[Value],
    allowed: &BTreeSet<String>,
    memory_sources: &BTreeSet<String>,
    at: i64,
) -> Result<usize, String> {
    if changes.len() > 16 {
        return Err("归档记忆变更超过16条，本次未保存".into());
    }
    // Validate and apply against local copies. A failure at any point leaves the
    // live archived memory and audit log untouched.
    let mut entries = s.analysis_memory.archived_entries.clone();
    let mut audits = Vec::new();
    let mut applied = 0;
    let base_version = s.analysis_memory.archived_version + 1;
    for change in changes {
        let action = change["action"].as_str().unwrap_or("");
        let scope = change["scope"].as_str().unwrap_or("lasting");
        let key = change["key"].as_str().unwrap_or("").trim();
        if key.is_empty()
            || key.chars().count() > 80
            || !["add", "update", "revoke"].contains(&action)
        {
            return Err("归档记忆变更格式无效".into());
        }
        if !["lasting", "once"].contains(&scope) {
            return Err("归档记忆范围无效".into());
        }
        let source = change["source"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .or_else(|| change["source"].as_str().map(|v| vec![v.to_owned()]))
            .unwrap_or_default();
        if source.is_empty()
            || source.len() > 8
            || source
                .iter()
                .any(|id| id.is_empty() || id.chars().count() > 256 || !allowed.contains(id))
        {
            return Err("归档记忆缺少有效来源".into());
        }
        if !source.iter().any(|id| memory_sources.contains(id)) {
            return Err("归档记忆必须引用本批操作记录或归档备注".into());
        }
        if scope == "once" {
            // A one-off exception stays with the task/note and is only recorded.
            audits.push(Audit {
                time: at,
                kind: "archive_memory_once".into(),
                task_id: change["task_id"].as_str().unwrap_or("").into(),
                detail: format!("一次性归档经验：{key}"),
                before: Value::Null,
                after: json!({"key":key,"text":change["text"],"source":source}),
            });
            applied += 1;
            continue;
        }
        let text = change["text"].as_str().unwrap_or("").trim().to_string();
        if action != "revoke" && text.is_empty() {
            return Err("归档记忆内容为空".into());
        }
        if text.chars().count() > crate::analysis_context::ARCHIVED_MEMORY_LIMIT {
            return Err("归档记忆超过长度上限".into());
        }
        let before = json!(entries);
        let existing = entries.iter().position(|entry| entry.key == key);
        match (action, existing) {
            ("revoke", Some(pos)) => {
                entries.remove(pos);
                applied += 1;
            }
            ("revoke", None) => {}
            ("add" | "update", Some(pos)) => {
                // Latest explicit correction in the same scope wins.
                entries[pos].text = text;
                entries[pos].source = source;
                entries[pos].updated = at;
                entries[pos].version = base_version;
                applied += 1;
            }
            ("add" | "update", None) => {
                entries.push(MemoryEntry {
                    id: id(),
                    key: key.into(),
                    text,
                    source,
                    version: base_version,
                    created: at,
                    updated: at,
                });
                applied += 1;
            }
            _ => return Err("归档记忆变更动作无效".into()),
        }
        audits.push(Audit {
            time: at,
            kind: "archive_memory".into(),
            task_id: change["task_id"].as_str().unwrap_or("").into(),
            detail: format!("归档维护长期记忆：{key}"),
            before,
            after: json!(entries),
        });
    }
    if archived_entries_text(&entries).chars().count()
        > crate::analysis_context::ARCHIVED_MEMORY_LIMIT
    {
        return Err("归档记忆超过长度上限，保留原记忆".into());
    }
    if applied > 0 {
        s.analysis_memory.archived_entries = entries;
        s.analysis_memory.archived_version += 1;
        s.audit.extend(audits);
        s.revision += 1;
    }
    Ok(applied)
}

fn finalize_tasks(s: &mut State, candidate: &Candidate, at: i64) -> usize {
    let mut archived = 0;
    for before in &candidate.tasks_to_archive {
        let Some(pos) = s.tasks.iter().position(|t| t.id == before.id) else {
            continue;
        };
        if s.tasks[pos].updated != before.updated || s.tasks[pos].archived {
            continue;
        }
        let old = s.tasks[pos].clone();
        s.tasks[pos].archived = true;
        s.tasks[pos].archived_at = at;
        s.tasks[pos].updated = at.max(old.updated + 1);
        archived += 1;
        App::audit(
            s,
            "archive_item",
            &old.id,
            "归档成功后归档已结束事项",
            json!(old),
            json!(s.tasks[pos]),
        );
    }
    archived
}

fn record_batch(s: &mut State, candidate: &Candidate, archived: usize, at: i64) {
    if let Some(attempt) = s
        .archive_attempts
        .iter_mut()
        .find(|a| a.id == candidate.attempt_id)
    {
        attempt.batches += usize::from(candidate.has_ai_input());
        attempt.processed += candidate.evidence_ids.len();
        attempt.remaining = candidate.remaining_events;
        attempt.archived_tasks += archived;
        attempt.status = if candidate.remaining_events == 0 {
            "succeeded"
        } else {
            "running"
        }
        .into();
        attempt.finished = if candidate.remaining_events == 0 {
            at
        } else {
            0
        };
        attempt.error.clear();
    }
}

fn apply(s: &mut State, candidate: &Candidate, result: Value, at: i64) -> Result<Value, String> {
    if s.analysis_memory.archived_version != candidate.memory_revision
        && result["memory_changes"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    {
        return Err("归档记忆已被人工编辑，本批未提交，请核对后重新归档".into());
    }
    let summary = &result["report"];
    if !valid_summary(summary) {
        return Err("归档汇总格式不正确".into());
    }
    let memory_changes = result["memory_changes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let allowed = candidate.evidence_refs();
    let memory_sources = candidate
        .evidence_ids
        .iter()
        .map(|index| format!("audit:{index}"))
        .collect::<BTreeSet<_>>();

    // The only AI-maintained policy data admitted by archive is archived memory.
    // Legacy/unsolicited policy outputs have no mutation path, even if old
    // automatic-adoption flags remain set in an upgraded configuration.
    let memory_applied = apply_memory_changes(s, &memory_changes, &allowed, &memory_sources, at)?;

    // Completed tasks become archived only in the successful commit. Tasks
    // edited while the model was running are left for a later archive.
    let archived = finalize_tasks(s, candidate, at);

    s.archive_ai_cursor = Some(candidate.cutoff);
    s.learning_cursor = s.learning_cursor.max(candidate.cutoff);
    s.archive_review_cursor = s.archive_review_cursor.max(candidate.cutoff);
    s.pending_learning = s
        .audit
        .iter()
        .skip(s.learning_cursor)
        .any(engine::learning_evidence);
    s.archive_attempt_day = candidate.business_day.clone();
    record_batch(s, candidate, archived, at);

    let mut report = candidate.report.clone();
    report["ai_summary"] = summary.clone();
    let mut summaries = s
        .reports
        .iter()
        .find(|r| r["attempt_id"] == candidate.attempt_id)
        .and_then(|r| r["batch_summaries"].as_array())
        .cloned()
        .unwrap_or_default();
    summaries.push(summary.clone());
    report["batch_summaries"] = json!(summaries);
    report["summary_status"] = json!("done");
    report["suggestions"] = json!(s
        .suggestions
        .iter()
        .filter(|v| v.archived_at == 0)
        .collect::<Vec<_>>());
    report["learning"] =
        json!({"added":0,"updated":0,"message":"归档仅维护归档记忆；系统提示词与临时记忆保留"});
    report["archived_tasks"] = json!(archived);
    report["archive_complete"] = json!(candidate.remaining_events == 0);
    report["skipped_tasks"] = json!(candidate.tasks_to_archive.len().saturating_sub(archived));
    report["attempt_id"] = json!(candidate.attempt_id);
    report["tags_changed"] = json!(0);
    report["memory_changes"] = json!(memory_applied);
    report["temporary_memory_cleared"] = json!(false);
    report["remaining_events"] = json!(candidate.remaining_events);
    report["memory_note"] = json!("临时记忆由消息分析自主维护，归档只读");
    report["report_id"] = report["id"].clone();
    if let Some(attempt) = s
        .archive_attempts
        .iter_mut()
        .find(|attempt| attempt.id == candidate.attempt_id)
    {
        attempt.report_id = report["id"].as_str().unwrap_or("").into();
    }

    let prompt_version = prompts::current(s, "classify")
        .map(|v| v.id.clone())
        .unwrap_or_default();
    let group = ArchiveGroup {
        id: id(),
        created: at,
        action: "archive".into(),
        restored_from: String::new(),
        attempt_id: candidate.attempt_id.clone(),
        report_id: report["id"].as_str().unwrap_or("").into(),
        reason: "归档成功：保存私有归档记忆版本，保留提示词、规则与标签快照".into(),
        prompt: stored_prompt(s, &prompt_version, prompts::content(&s.config, "classify")),
        prompt_version,
        active_rules: s
            .suggestions
            .iter()
            .filter(|suggestion| suggestion.active && suggestion.archived_at == 0)
            .cloned()
            .collect(),
        rule_version: s.rule_version,
        tag_groups: crate::tags::groups(s),
        tag_version: s
            .tag_state
            .versions
            .last()
            .map(|version| version.id.clone())
            .unwrap_or_default(),
        archived_entries: vec![],
        archived_version: s.analysis_memory.archived_version,
    };
    s.archive_versions.push(group.clone());
    report["archive_group_id"] = json!(group.id);
    s.reports.insert(0, report.clone());
    s.reports.truncate(60);
    App::audit(
        s,
        "archive_commit",
        "",
        "归档结果、成功状态、游标、事项归档标记和归档记忆同次保存",
        json!({"attempt":candidate.attempt_id,"cutoff":candidate.cutoff}),
        json!({"report":report["id"],"group":group.id,"archived_tasks":archived}),
    );
    Ok(report)
}

fn fallback_report(s: &State, candidate: &Candidate, at: i64) -> Value {
    let previous = s.reports.first().cloned();
    let mut report = candidate.report.clone();
    report["time"] = json!(at);
    report["id"] = json!(id());
    report["data_until"] = json!(s.last_scan);
    report["pending_analysis"] = json!(candidate.pending_count);
    report["archived_tasks"] = json!(0);
    report["tags_changed"] = json!(0);
    report["memory_changes"] = json!(0);
    if let Some(previous) = previous.filter(|old| {
        old["ai_summary"].is_object()
            && !["fallback"].contains(&old["summary_status"].as_str().unwrap_or(""))
    }) {
        report["ai_summary"] = previous["ai_summary"].clone();
        report["summary_status"] = json!("reused");
        report["reused_from"] = previous["id"].clone();
        report["reuse_note"] = json!("无新增归档材料，明确沿用上次报表，未调用 AI");
    } else {
        report["ai_summary"] = json!({
            "summary":"没有可归档的新材料，且尚无上次报表可沿用。",
            "completed":[],
            "attention":[],
            "next_steps":[]
        });
        report["summary_status"] = json!("fallback");
        report["fallback"] = json!(true);
        report["reuse_note"] = json!("首次运行无上次报表，生成不含 AI 的空报表兜底");
    }
    report
}

impl App {
    fn upsert_attempt(&self, attempt: ArchiveAttempt) -> Result<(), String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if let Some(existing) = s
            .archive_attempts
            .iter_mut()
            .find(|existing| existing.id == attempt.id)
        {
            *existing = attempt;
        } else {
            s.archive_attempts.push(attempt);
        }
        self.save(&s)?;
        *lock = s;
        Ok(())
    }
    fn fail_attempt(&self, id: &str, error: &str) -> Result<(), String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if let Some(attempt) = s.archive_attempts.iter_mut().find(|a| a.id == id) {
            attempt.status = "failed".into();
            attempt.finished = now();
            attempt.error = error.into();
        }
        let saved = self.save(&s);
        // Even when the disk save fails, never leave the live in-memory attempt
        // looking as if it were still running.
        *lock = s;
        drop(lock);
        if !error.starts_with("AI_UNCONFIGURED:") {
            self.log(
                "archive_failed",
                json!({"kind":"archive","attempt_id":id,"error":error}),
            );
        }
        saved
    }
    fn mark_attempt_failed_in_memory(&self, id: &str, error: &str) {
        let mut lock = self.state.lock().unwrap();
        if let Some(attempt) = lock.archive_attempts.iter_mut().find(|a| a.id == id) {
            attempt.status = "failed".into();
            attempt.finished = now();
            attempt.error = error.into();
        }
        drop(lock);
        self.log(
            "archive_failed",
            json!({"kind":"archive","attempt_id":id,"error":error}),
        );
    }
    /// Test fixture: persist a queued attempt the way the boundary would.
    #[cfg(test)]
    fn queue_attempt(&self, trigger: &str, at: i64) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let business_day = business_day(&s, at);
        if let Some(existing) = s.archive_attempts.iter().find(|a| {
            a.business_day == business_day
                && ["queued", "running", "failed"].contains(&a.status.as_str())
        }) {
            // A failed attempt deliberately blocks an automatic retry. Report
            // that truthfully instead of claiming the work is queued.
            let active = ["queued", "running"].contains(&existing.status.as_str());
            return Ok(json!({
                "queued": active,
                "deduplicated": true,
                "attempt": existing.id,
                "status": existing.status,
            }));
        }
        let attempt = ArchiveAttempt {
            id: id(),
            trigger: trigger.into(),
            status: "queued".into(),
            business_day,
            created: at,
            ..Default::default()
        };
        s.archive_attempts.push(attempt.clone());
        let after = json!(attempt);
        App::audit(
            &mut s,
            "archive_queued",
            "",
            "记录一次待归档，未推进游标",
            Value::Null,
            after,
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!({"queued":true,"attempt":attempt.id}))
    }
    fn start_attempt(
        &self,
        trigger: &str,
        at: i64,
        existing: Option<ArchiveAttempt>,
    ) -> Result<ArchiveAttempt, String> {
        let business_day = {
            let s = self.state.lock().unwrap();
            business_day(&s, at)
        };
        let attempt = if let Some(mut attempt) = existing {
            attempt.status = "running".into();
            attempt.started = at;
            attempt.finished = 0;
            attempt.error.clear();
            attempt
        } else {
            ArchiveAttempt {
                id: id(),
                trigger: trigger.into(),
                status: "running".into(),
                business_day,
                created: at,
                started: at,
                ..Default::default()
            }
        };
        self.upsert_attempt(attempt.clone())?;
        Ok(attempt)
    }
    /// A queued archive is allowed to run outside work hours only with the
    /// exact persisted end claim and its authoritative attempt. Missing or
    /// mismatched evidence fails closed; it is never treated as a manual retry.
    fn validate_queued_archive(&self, attempt: &ArchiveAttempt, at: i64) -> Result<(), String> {
        if !attempt.unfinished() {
            return Err("归档 attempt 已结束，不能自动续跑".into());
        }
        let (allowed, reason) = {
            let s = self.state.lock().unwrap();
            let claim =
                s.work_scheduler
                    .runtime
                    .find_end_claim(&attempt.id)
                    .filter(|(day, claim)| {
                        claim.attempt_id == attempt.id && day.to_string() == attempt.business_day
                    });
            if claim.is_none() {
                (
                    false,
                    "归档排队缺少匹配的持久化下班 claim；已停止自动执行，等待手动归档".to_string(),
                )
            } else {
                (
                    self.work_gate(&s, "archive", AiMode::Nightly, Some(&attempt.id), at),
                    "下班归档授权已失效；已停止自动执行，等待手动归档".to_string(),
                )
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(reason)
        }
    }
    fn fitted_archive_batch(
        &self,
        frozen: &State,
        mode: AiMode,
        at: i64,
        attempt: &ArchiveAttempt,
    ) -> Result<(Candidate, engine::PreparedAiRequest), String> {
        let prepare =
            |candidate: &Candidate| self.prepare_archive_request(&candidate.payload, mode);
        let mut all = batch(frozen, mode, at, &attempt.id, &attempt.business_day);
        all.trigger = attempt.trigger.clone();
        all.payload["_trigger"] = json!(attempt.trigger);
        let request = prepare(&all)?;
        if request.prepared.total <= request.config.max_input_chars {
            return Ok((all, request));
        }
        // Prefer new evidence over unrelated background summaries. Full records
        // remain in the frozen read-only tool snapshot.
        let omitted = all.payload["current_tasks"].as_array().map_or(0, Vec::len);
        all.payload["current_tasks"] = json!([]);
        all.payload["coverage"]["omitted_current_tasks"] = json!(omitted);
        let request = prepare(&all)?;
        if request.prepared.total <= request.config.max_input_chars {
            return Ok((all, request));
        }
        let (mut low, mut high) = (1, all.evidence_ids.len());
        let mut accepted = None;
        while low <= high {
            let count = low + (high - low) / 2;
            let mut candidate =
                batch_limited(frozen, mode, at, &attempt.id, &attempt.business_day, count);
            candidate.trigger = attempt.trigger.clone();
            candidate.payload["_trigger"] = json!(attempt.trigger);
            candidate.payload["current_tasks"] = json!([]);
            candidate.payload["coverage"]["omitted_current_tasks"] = json!(omitted);
            let request = prepare(&candidate)?;
            if request.prepared.total <= request.config.max_input_chars {
                accepted = Some((candidate, request));
                low = count + 1;
            } else {
                high = count - 1;
            }
        }
        accepted.ok_or_else(|| "归档的单条材料及必需上下文已超过单次输入上限，未调用 AI；请精简配置或提高输入上限后手动归档".into())
    }

    fn run_archive(
        &self,
        trigger: &str,
        mode: AiMode,
        existing: Option<ArchiveAttempt>,
    ) -> Result<Value, String> {
        self.run_archive_with(trigger, mode, existing, &mut |request, call| {
            self.invoke_prepared_request(request, mode, call, false)
        })
    }

    fn run_archive_with(
        &self,
        trigger: &str,
        mode: AiMode,
        existing: Option<ArchiveAttempt>,
        invoke: &mut dyn FnMut(
            &engine::PreparedAiRequest,
            &crate::ai_control::AiCall<'_>,
        ) -> Result<Value, String>,
    ) -> Result<Value, String> {
        // Serialize reservation with begin_ai_for: a manual/new analysis cannot
        // slip between our idle check and the archive's own model request.
        let busy = {
            let _active = self.active_ai.lock().unwrap();
            self.ai_busy.load(Ordering::SeqCst) || self.archive_busy.swap(true, Ordering::SeqCst)
        };
        if busy {
            if let Some(existing) = &existing {
                // The boundary transaction already persisted this queued
                // attempt and its claim. Never create a second attempt and never
                // report a success.
                return Ok(json!({"queued":true,"deduplicated":true,"attempt":existing.id}));
            }
            // Only manual runs arrive without a persisted attempt; automatic
            // archives always carry the boundary-claimed one handled above.
            return Ok(json!({"skipped":"AI 正在处理，请稍后手动归档"}));
        }
        struct Guard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = Guard(&self.archive_busy);
        let at = now();
        if let Some(existing) = &existing {
            if let Err(error) = self.validate_queued_archive(existing, at) {
                self.fail_attempt(&existing.id, &error)?;
                return Err(error);
            }
        } else {
            let allowed = {
                let s = self.state.lock().unwrap();
                self.work_gate(&s, "archive", mode, None, at)
            };
            if !allowed {
                return Err(crate::work_hours::DEFERRED.into());
            }
        }
        if let Err(error) = self.require_ai_purpose("archive") {
            if let Some(existing) = &existing {
                self.fail_attempt(&existing.id, &error)?;
            }
            return Err(error);
        }
        let attempt = self.start_attempt(trigger, at, existing)?;
        let mut call = self.begin_ai_for("archive").map_err(|error| {
            let _ = self.fail_attempt(&attempt.id, &error);
            error
        })?;
        let started = std::time::Instant::now();
        let mut frozen = self.state.lock().unwrap().clone();
        let deadline = std::time::Duration::from_secs(frozen.config.ai_timeout_seconds);
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct DeadlineGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DeadlineGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _deadline_guard = DeadlineGuard(done.clone());
        let (worker_done, worker_timeout, worker_cancel) =
            (done.clone(), timed_out.clone(), call.cancel.clone());
        std::thread::spawn(move || {
            while !worker_done.load(Ordering::SeqCst) {
                if started.elapsed() >= deadline {
                    worker_timeout.store(true, Ordering::SeqCst);
                    worker_cancel.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let run = (|| -> Result<Value, String> {
            {
                let _files = self.snapshot_files.lock().unwrap();
                crate::snapshots::ensure(&self.root, &frozen, at)
                    .map_err(|e| format!("分析快照不完整：{e}"))?;
            }
            loop {
                crate::process::check_running()?;
                if call.cancel.load(Ordering::SeqCst) {
                    return Err(crate::ai_control::CANCELLED.into());
                }
                if started.elapsed() >= deadline {
                    return Err("归档总时限已到，后续批次未提交；请手动继续归档".into());
                }
                let (candidate, mut request) =
                    self.fitted_archive_batch(&frozen, mode, at, &attempt)?;
                // Persist pending coverage before dispatch so a failure or
                // restart truthfully reports the uncommitted remainder.
                {
                    let mut lock = self.state.lock().unwrap();
                    let mut next = lock.clone();
                    if let Some(a) = next
                        .archive_attempts
                        .iter_mut()
                        .find(|a| a.id == attempt.id)
                    {
                        a.remaining = candidate.evidence_ids.len() + candidate.remaining_events;
                    }
                    self.save(&next)?;
                    *lock = next;
                }
                if !candidate.has_ai_input() {
                    if candidate.remaining_events > 0 {
                        return Err("归档材料无法装入单批，游标和事项保留".into());
                    }
                    let _acceptance = self.active_ai.lock().unwrap();
                    if call.cancel.load(Ordering::SeqCst) {
                        return Err(crate::ai_control::CANCELLED.into());
                    }
                    return self.commit_without_ai(candidate, now());
                }
                call.next_request()?;
                request.config.ai_timeout_seconds =
                    deadline.saturating_sub(started.elapsed()).as_secs().max(1);
                let result = invoke(&request, &call)?;
                if call.cancel.load(Ordering::SeqCst) {
                    return Err(crate::ai_control::CANCELLED.into());
                }
                if started.elapsed() >= deadline {
                    return Err("归档总时限已到，本批未提交；请手动继续归档".into());
                }
                if result["_context_usage"]["budget_exhausted"] == true {
                    return Err(
                        "补查预算已用尽，归档覆盖不完整，本批未提交；请调整预算后手动归档".into(),
                    );
                }
                let cutoff = candidate.cutoff;
                let remaining = candidate.remaining_events;
                let report = {
                    let _acceptance = self.active_ai.lock().unwrap();
                    if call.cancel.load(Ordering::SeqCst) {
                        return Err(crate::ai_control::CANCELLED.into());
                    }
                    self.commit(candidate, result, now())?
                };
                if remaining == 0 {
                    return Ok(report);
                }
                if cutoff <= frozen.archive_ai_cursor.unwrap_or(0) {
                    return Err("归档游标无进展，已停止后续批次".into());
                }
                frozen.archive_ai_cursor = Some(cutoff);
                // Preserve the original task/audit/message boundary. Only our
                // committed archive marks and memory carry into the next batch.
                let live = self.state.lock().unwrap();
                frozen.analysis_memory.archived_entries =
                    live.analysis_memory.archived_entries.clone();
                frozen.analysis_memory.archived_version = live.analysis_memory.archived_version;
                for task in &mut frozen.tasks {
                    if live.tasks.iter().any(|t| t.id == task.id && t.archived) {
                        task.archived = true;
                    }
                }
            }
        })();
        done.store(true, Ordering::SeqCst);
        let run = if run.is_err() && timed_out.load(Ordering::SeqCst) {
            Err("归档总时限已到，本批未提交；已提交批次保留，请手动继续归档".into())
        } else {
            run
        };
        if let Err(error) = &run {
            self.fail_attempt(&attempt.id, error)?;
        }
        run
    }
    pub(crate) fn write_report_file(&self, report: &Value) -> Result<(), String> {
        let dir = crate::storage::data_dir(&self.root).join("reports");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        fs::write(
            dir.join(format!(
                "{}-{}.json",
                report["date"].as_str().unwrap_or(""),
                report["id"].as_str().unwrap_or("")
            )),
            serde_json::to_vec_pretty(report).unwrap(),
        )
        .map_err(|e| e.to_string())
    }
    fn commit_without_ai(&self, candidate: Candidate, at: i64) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let mut report = fallback_report(&s, &candidate, at);
        let archived = finalize_tasks(&mut s, &candidate, at);
        record_batch(&mut s, &candidate, archived, at);
        report["archived_tasks"] = json!(archived);
        report["archive_complete"] = json!(true);
        report["attempt_id"] = json!(candidate.attempt_id);
        s.archive_ai_cursor = Some(candidate.cutoff);
        s.learning_cursor = s.learning_cursor.max(candidate.cutoff);
        s.archive_review_cursor = s.archive_review_cursor.max(candidate.cutoff);
        s.archive_attempt_day = candidate.business_day.clone();
        if let Some(attempt) = s
            .archive_attempts
            .iter_mut()
            .find(|a| a.id == candidate.attempt_id)
        {
            attempt.status = "succeeded".into();
            attempt.finished = at;
            attempt.report_id = report["id"].as_str().unwrap_or("").into();
            attempt.reused = report["summary_status"] == "reused";
        }
        s.reports.insert(0, report.clone());
        s.reports.truncate(60);
        App::audit(
            &mut s,
            "archive_reuse",
            "",
            "无新增材料，不调用 AI；沿用报表与成功状态同次保存",
            Value::Null,
            json!({"report":report["id"],"status":report["summary_status"]}),
        );
        if let Err(error) = self.save(&s) {
            drop(lock);
            let message = format!(
                "归档保存失败，未提交报表/游标/成功状态；内存已标记失败，等待手动归档：{error}"
            );
            self.mark_attempt_failed_in_memory(&candidate.attempt_id, &message);
            return Err(message);
        }
        *lock = s;
        drop(lock);
        if let Err(error) = self.write_report_file(&report) {
            self.log(
                "archive_report_file_failed",
                json!({"error":error,"report":report["id"]}),
            );
        }
        self.log(
            "archive_result",
            json!({"kind":"archive","trigger":candidate.trigger,"processed":0,"reused":true,"report":report["id"]}),
        );
        Ok(report)
    }
    fn commit(&self, candidate: Candidate, result: Value, at: i64) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let output = match apply(&mut s, &candidate, result, at) {
            Ok(report) => report,
            Err(error) => {
                drop(lock);
                self.fail_attempt(&candidate.attempt_id, &error)?;
                return Err(error);
            }
        };
        // The only place the success status and every business effect share a
        // single atomic save. On failure the committed business state is not
        // changed, and the live attempt is marked failed for truthful UI.
        if let Err(error) = self.save(&s) {
            drop(lock);
            let message = format!(
                "归档保存失败，结果、游标、事项归档标记和临时记忆均未提交；内存已标记失败，等待手动归档：{error}"
            );
            self.mark_attempt_failed_in_memory(&candidate.attempt_id, &message);
            return Err(message);
        }
        *lock = s;
        drop(lock);
        if let Err(error) = self.write_report_file(&output) {
            self.log(
                "archive_report_file_failed",
                json!({"error":error,"report":output["id"]}),
            );
        }
        self.log(
            "archive_result",
            json!({"kind":"archive","trigger":candidate.trigger,"processed":candidate.evidence_ids.len(),"remaining":output["remaining_events"],"report":output["id"],"attempt_id":candidate.attempt_id}),
        );
        Ok(output)
    }
    pub(crate) fn archive_tick(&self) -> Result<Value, String> {
        if self.ai_busy.load(Ordering::SeqCst) || self.archive_busy.load(Ordering::SeqCst) {
            return Ok(json!({"skipped":"归档已排队，等待当前 AI 结束"}));
        }
        let attempt = self
            .state
            .lock()
            .unwrap()
            .archive_attempts
            .iter()
            .find(|a| a.status == "queued")
            .cloned();
        if let Some(attempt) = attempt {
            return self.run_archive(&attempt.trigger.clone(), AiMode::Nightly, Some(attempt));
        }
        Ok(json!({"skipped":"归档失败或等待下次到点；失败仅手动重试"}))
    }
    pub fn archive(&self, mode: AiMode) -> Result<Value, String> {
        if mode != AiMode::Manual {
            return self.archive_tick();
        }
        {
            let s = self.state.lock().unwrap();
            if !self.work_gate(&s, "archive", mode, None, now()) {
                return Err(crate::work_hours::DEFERRED.into());
            }
        }
        if self.ai_busy.load(Ordering::SeqCst) || self.archive_busy.load(Ordering::SeqCst) {
            return Err("AI 正在处理，请稍后手动归档".into());
        }
        self.run_archive("manual_archive", mode, None)
    }
    /// Full, read-only view of one archived group for side-by-side comparison in
    /// the UI. It never mutates state and never reopens an old attempt.
    pub fn archive_version(&self, version_id: &str) -> Result<Value, String> {
        let s = self.state.lock().unwrap();
        let group = s
            .archive_versions
            .iter()
            .find(|group| group.id == version_id)
            .ok_or("归档版本不存在")?;
        Ok(json!({
            "id":group.id,
            "created":group.created,
            "action":group.action,
            "restored_from":group.restored_from,
            "attempt_id":group.attempt_id,
            "report_id":group.report_id,
            "reason":group.reason,
            "prompt":archive_prompt(&s, group),
            "prompt_version":group.prompt_version,
            "rules":group.active_rules,
            "tags":group.tag_groups,
            "tag_version":group.tag_version,
            "memory":group.archived_entries,
            "archived_version":group.archived_version,
            "current":s.archive_versions.last().map(|group|group.id.clone()),
        }))
    }
    pub fn archive_history(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({
            "attempts":s.archive_attempts,
            "versions":s.archive_versions.iter().map(|group|json!({
                "id":group.id,
                "created":group.created,
                "action":group.action,
                "restored_from":group.restored_from,
                "attempt_id":group.attempt_id,
                "report_id":group.report_id,
                "reason":group.reason,
                "prompt_version":group.prompt_version,
                "active_rules":group.active_rules.len(),
                "tag_version":group.tag_version,
                "archived_version":group.archived_version,
            })).collect::<Vec<_>>(),
            "current":s.archive_versions.last().map(|g|g.id.clone()),
            "memory":s.analysis_memory.archived_text(),
        })
    }
    /// Roll back prompt, rules, tags and archived memory as a NEW group. Tasks,
    /// message progress, analyzed fingerprints and later manual task edits are
    /// deliberately outside this operation.
    pub fn archive_rollback(&self, version_id: &str, expected: &str) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let current = s.archive_versions.last().map(|g| g.id.clone());
        if current.as_deref() != Some(expected) {
            return Err("归档版本已变化，请刷新后重试".into());
        }
        let Some(latest) = s.archive_versions.last().cloned() else {
            return Err("归档版本不存在".into());
        };
        let group = s
            .archive_versions
            .iter()
            .find(|group| group.id == version_id)
            .cloned()
            .ok_or("归档版本不存在")?;
        // Rollback is only safe if no component was manually changed after the
        // latest group. Group ids alone do not change on manual edits.
        let current_prompt_version = prompts::current(&s, "classify")
            .map(|version| version.id.clone())
            .unwrap_or_default();
        let current_tag_version = s
            .tag_state
            .versions
            .last()
            .map(|version| version.id.clone())
            .unwrap_or_default();
        let current_rules = s
            .suggestions
            .iter()
            .filter(|rule| rule.active && rule.archived_at == 0)
            .map(|rule| (rule.id.clone(), rule.title.clone(), rule.rule.clone()))
            .collect::<Vec<_>>();
        let latest_rules = latest
            .active_rules
            .iter()
            .map(|rule| (rule.id.clone(), rule.title.clone(), rule.rule.clone()))
            .collect::<Vec<_>>();
        let conflict = current_prompt_version != latest.prompt_version
            || prompts::content(&s.config, "classify") != archive_prompt(&s, &latest)
            || s.rule_version != latest.rule_version
            || current_rules != latest_rules
            || current_tag_version != latest.tag_version;
        if conflict {
            return Err("最新归档之后提示词、规则或标签已有手动修改，整组回滚会覆盖这些修改，已取消；请先确认当前版本或生成新版本".into());
        }

        let restored_prompt = archive_prompt(&s, &group).to_string();
        prompts::set_content(&mut s.config, "classify", restored_prompt.clone())?;
        let prompt_version = prompts::record(
            &mut s,
            "classify",
            restored_prompt.clone(),
            "archive_rollback",
            &format!("归档整组回滚至 {}", group.id),
        );
        // Restore the target active rules but never delete suggestions that were
        // added later. Non-target rules are disabled, not removed or archived.
        let target_rules = group
            .active_rules
            .iter()
            .map(|rule| (rule.id.clone(), rule))
            .collect::<std::collections::BTreeMap<_, _>>();
        for suggestion in &mut s.suggestions {
            if let Some(target) = target_rules.get(&suggestion.id) {
                suggestion.title = target.title.clone();
                suggestion.rule = target.rule.clone();
                suggestion.decision = "enabled".into();
                suggestion.active = true;
                suggestion.archived_at = 0;
                suggestion.updated = now();
            } else if suggestion.active {
                suggestion.active = false;
                suggestion.decision = "pending".into();
                suggestion.updated = now();
            }
        }
        for target in &group.active_rules {
            if !s.suggestions.iter().any(|rule| rule.id == target.id) {
                let mut restored = target.clone();
                restored.active = true;
                restored.archived_at = 0;
                restored.updated = now();
                s.suggestions.push(restored);
            }
        }
        s.tag_state.versions.push(crate::tags::Version {
            priority_kind: String::new(),
            assignments: vec![],
            id: model_id(),
            created: now(),
            action: "archive_rollback".into(),
            reason: format!("随归档版本回滚至 {}", group.id),
            groups: group.tag_groups.clone(),
        });
        // Memory is current editable state. Legacy snapshots remain readable
        // on disk but policy rollback must never replace current memory.
        s.rule_version += 1;
        let created = now();
        let next = ArchiveGroup {
            id: id(),
            created,
            action: "rollback".into(),
            restored_from: group.id.clone(),
            attempt_id: String::new(),
            report_id: String::new(),
            reason: format!("整组回滚至 {}，以新版本记录，不倒退消息/事项进度", group.id),
            prompt: stored_prompt(&s, &prompt_version, restored_prompt),
            prompt_version,
            active_rules: s
                .suggestions
                .iter()
                .filter(|rule| rule.active && rule.archived_at == 0)
                .cloned()
                .collect(),
            rule_version: s.rule_version,
            tag_groups: crate::tags::groups(&s),
            tag_version: s
                .tag_state
                .versions
                .last()
                .map(|version| version.id.clone())
                .unwrap_or_default(),
            archived_entries: vec![],
            archived_version: s.analysis_memory.archived_version,
        };
        s.archive_versions.push(next.clone());
        App::audit(
            &mut s,
            "archive_rollback",
            "",
            "归档整组回滚：仅提示词、规则、标签和私有归档记忆；保留后续建议记录",
            json!({"restored_from":group.id}),
            json!({"group":next.id}),
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!({"group":next.id,"prompt_version":next.prompt_version}))
    }
}
fn model_id() -> String {
    id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("shishi-archive-{}", id()))
    }
    fn result(report_summary: &str) -> Value {
        json!({
            "report":{"summary":report_summary,"completed":[],"attention":[],"next_steps":[]},
            "review":{"improved":false,"reason":"无需修改","proposed_prompt":"","evidence_ids":[]},
            "suggestions":[],
            "handling_review":[],
            "advice_decisions":[],
            "memory_changes":[],
            "tag_changes":[]
        })
    }
    fn seed(app: &App, note: &str) -> String {
        let task = app
            .task_edit(&json!({"task":{"title":"验证归档","description":"核对发布结果","status":"待处理"}}))
            .unwrap();
        let tid = task["id"].as_str().unwrap().to_string();
        let mut s = app.state.lock().unwrap();
        let pos = s.tasks.iter().position(|t| t.id == tid).unwrap();
        let before = json!(s.tasks[pos]);
        s.tasks[pos].status = "已完成".into();
        s.tasks[pos].archived = false;
        s.tasks[pos].archive_note = note.into();
        s.tasks[pos].updated = now();
        s.tasks[pos].manual = true;
        let after = json!(s.tasks[pos]);
        App::audit(
            &mut s,
            "manual_archive",
            &tid,
            "人工归档并记录说明",
            before,
            after,
        );
        s.pending_learning = true;
        app.save(&s).unwrap();
        tid
    }
    fn batch_fixture(app: &App, max_chars: usize, count: usize) {
        let mut s = app.state.lock().unwrap();
        s.config.max_input_chars = max_chars;
        s.analysis_memory.temporary = "合成临时记忆保留".into();
        for i in 0..count {
            let task = Task {
                id: format!("fixture-{i}"),
                title: format!("合成核对事项{i}"),
                description: "合成材料核对完成".repeat(22),
                status: "已完成".into(),
                updated: 2,
                created: 1,
                ..Default::default()
            };
            s.audit.push(Audit {
                time: 2,
                kind: "manual_status".into(),
                task_id: task.id.clone(),
                detail: "done".into(),
                before: json!({"status":"待处理"}),
                after: json!(task),
            });
            s.tasks.push(task);
        }
        s.archive_ai_cursor = Some(0);
        app.save(&s).unwrap();
    }

    #[test]
    fn archive_rejects_stale_memory_and_legacy_rollback_keeps_current_memory() {
        let app = App::load(root()).unwrap();
        seed(&app, "合成归档依据");
        let candidate = {
            let state = app.state.lock().unwrap();
            batch(&state, AiMode::Manual, now(), "attempt", "day")
        };
        app.memory_save(&json!({"kind":"archived","expected":0,"entries":[{"key":"人工主题","text":"人工保存的最新内容"}]})).unwrap();
        let before = app.memory_read();
        let mut output = result("合成结论");
        output["memory_changes"] = json!([{"action":"add","key":"旧主题","text":"旧结果"}]);
        assert!(app
            .commit(candidate, output, now())
            .unwrap_err()
            .contains("人工编辑"));
        assert_eq!(app.memory_read(), before);
    }
    #[test]
    fn archive_uses_full_rendered_budget_instead_of_fixed_event_or_task_caps() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        batch_fixture(&app, 60_000, 40);
        let frozen = app.state.lock().unwrap().clone();
        let attempt = ArchiveAttempt {
            id: "fixture".into(),
            trigger: "manual_archive".into(),
            ..Default::default()
        };
        let (candidate, request) = app
            .fitted_archive_batch(&frozen, AiMode::Manual, now(), &attempt)
            .unwrap();
        assert_eq!(candidate.evidence_ids.len(), 40);
        assert_eq!(candidate.remaining_events, 0);
        assert_eq!(
            candidate.payload["report_tasks"].as_array().unwrap().len(),
            40
        );
        assert!(
            candidate.payload["current_tasks"]
                .as_array()
                .unwrap()
                .is_empty(),
            "related tasks are not duplicated"
        );
        assert!(candidate.payload["evidence"].to_string().chars().count() > 9000);
        assert!(request.prepared.total <= 60_000);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manual_and_claimed_automatic_archive_finish_all_batches_in_one_attempt() {
        for automatic in [false, true] {
            let root = root();
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            app.test_force_working();
            batch_fixture(&app, 10_000, 40);
            let existing = if automatic {
                let queued = app.queue_attempt("work_end", now()).unwrap();
                let mut s = app.state.lock().unwrap();
                let day = crate::work_state::shanghai_date(now()).unwrap();
                s.work_scheduler.runtime.day_record_mut(day).end_claim =
                    Some(crate::work_state::EndArchiveClaim {
                        attempt_id: queued["attempt"].as_str().unwrap().into(),
                        claimed_at_ms: now(),
                        eligibility: crate::work_state::EndEligibility::WorkedBeforeEnd,
                    });
                s.archive_attempts.last().cloned()
            } else {
                None
            };
            let boundary = app.state.lock().unwrap().audit.len();
            let mut ids = BTreeSet::new();
            let mut calls = 0;
            app.run_archive_with(
                if automatic {
                    "work_end"
                } else {
                    "manual_archive"
                },
                if automatic {
                    AiMode::Nightly
                } else {
                    AiMode::Manual
                },
                existing,
                &mut |request, call| {
                    calls += 1;
                    assert!(ids.insert(call.id.clone()));
                    assert!(request.prepared.total <= 10_000);
                    assert_eq!(
                        app.state
                            .lock()
                            .unwrap()
                            .archive_attempts
                            .last()
                            .unwrap()
                            .status,
                        "running"
                    );
                    if calls == 1 {
                        app.state.lock().unwrap().audit.push(Audit {
                            kind: "manual_status".into(),
                            task_id: "new-arrival".into(),
                            ..Default::default()
                        });
                    }
                    Ok(result("合成分批汇总"))
                },
            )
            .unwrap();
            let s = app.state.lock().unwrap();
            let attempt = s.archive_attempts.last().unwrap();
            assert!(calls > 1);
            assert_eq!(s.archive_attempts.len(), 1);
            assert_eq!(attempt.status, "succeeded");
            assert_eq!(attempt.remaining, 0);
            assert_eq!(attempt.processed, 40);
            assert_eq!(attempt.batches, calls);
            assert_eq!(attempt.archived_tasks, 40);
            assert!(s.tasks.iter().all(|t| t.archived));
            assert_eq!(s.archive_ai_cursor, Some(boundary));
            assert_eq!(s.analysis_memory.temporary, "合成临时记忆保留");
            drop(s);
            assert!(!app.ai_busy.load(Ordering::SeqCst));
            assert!(!app.archive_busy.load(Ordering::SeqCst));
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn later_batch_failure_cancellation_and_total_timeout_preserve_prior_commit() {
        for stop in ["failure", "cancel", "timeout"] {
            let root = root();
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            batch_fixture(&app, 10_000, 40);
            if stop == "timeout" {
                app.state.lock().unwrap().config.ai_timeout_seconds = 1;
            }
            let mut calls = 0;
            let error = app
                .run_archive_with("manual_archive", AiMode::Manual, None, &mut |_, call| {
                    calls += 1;
                    if calls == 2 {
                        match stop {
                            "failure" => return Err("合成供应商失败".into()),
                            "cancel" => {
                                app.interrupt_ai(&call.id).unwrap();
                            }
                            _ => std::thread::sleep(std::time::Duration::from_millis(1100)),
                        }
                    }
                    Ok(result("合成汇总"))
                })
                .unwrap_err();
            assert!(error.contains(if stop == "failure" {
                "合成供应商失败"
            } else if stop == "cancel" {
                "中断"
            } else {
                "总时限"
            }));
            let s = app.state.lock().unwrap();
            let a = s.archive_attempts.last().unwrap();
            assert_eq!(calls, 2);
            assert_eq!(a.status, "failed");
            assert_eq!(a.batches, 1);
            assert!(a.remaining > 0);
            assert!(s.archive_ai_cursor.unwrap() > 0 && s.archive_ai_cursor.unwrap() < 40);
            assert!(s.tasks.iter().any(|t| t.archived) && s.tasks.iter().any(|t| !t.archived));
            drop(s);
            assert!(app.archive_tick().unwrap()["skipped"].is_string());
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn invalid_result_and_save_failure_leave_every_business_effect_unchanged() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = seed(&app, "明确说明只做一次");
        let latest = app.state.lock().unwrap().archive_attempts.last().cloned();
        assert!(latest.is_none());
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        assert!(!candidate.evidence_ids.is_empty());
        let before = json!(*app.state.lock().unwrap());
        assert!(app.commit(candidate.clone(), json!({}), now()).is_err());
        {
            let s = app.state.lock().unwrap();
            assert_eq!(json!(s.tasks), before["tasks"]);
            assert_eq!(json!(s.archive_ai_cursor), before["archive_ai_cursor"]);
            assert_eq!(json!(s.analysis_memory), before["analysis_memory"]);
            assert_eq!(s.archive_attempts.last().unwrap().status, "failed");
        }
        assert_eq!(app.state.lock().unwrap().tasks[0].id, task);
        // A final save failure must not apply the candidate in memory.
        let before_save = json!(*app.state.lock().unwrap());
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .commit(candidate.clone(), result("处理完毕"), now())
            .is_err());
        let after = app.state.lock().unwrap();
        assert_eq!(json!(after.tasks), before_save["tasks"]);
        assert_eq!(
            json!(after.archive_ai_cursor),
            before_save["archive_ai_cursor"]
        );
        assert_eq!(json!(after.analysis_memory), before_save["analysis_memory"]);
        assert!(!after.tasks[0].archived);
        // Save failure cannot persist a failed status, but the live in-memory
        // attempt must not remain "running" and pretend work is still active.
        let attempt = after.archive_attempts.last().unwrap();
        assert_eq!(attempt.status, "failed");
        assert!(attempt.error.contains("保存失败"));
        std::fs::remove_dir(app.root.join("data/state.next.json")).unwrap();
        drop(after);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn successful_commit_sets_success_cursor_and_effects_in_one_save() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        seed(&app, "普通确认");
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        let report = app
            .commit(candidate.clone(), result("处理完毕"), now())
            .unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(report["summary_status"], "done");
        assert_eq!(s.archive_ai_cursor, Some(candidate.cutoff));
        assert!(s.tasks[0].archived);
        assert_eq!(
            s.archive_attempts
                .iter()
                .find(|a| a.id == attempt.id)
                .unwrap()
                .status,
            "succeeded"
        );
        assert_eq!(s.archive_versions.len(), 1);
        assert_eq!(s.reports[0]["summary_status"], "done");
        assert!(s.analysis_memory.temporary.is_empty());
        drop(s);
        let reloaded = App::load(root.clone()).unwrap();
        assert!(!reloaded.state.lock().unwrap().archive_attempts[0].unfinished());
        assert_eq!(reloaded.state.lock().unwrap().archive_versions.len(), 1);
        drop(reloaded);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn restart_before_commit_fails_without_replay_and_success_survives() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        seed(&app, "重启前");
        let queued = app.queue_attempt("nightly_archive", now()).unwrap();
        assert_eq!(queued["queued"], true);
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_attempts.last().unwrap().status, "failed");
        assert!(s
            .archive_attempts
            .last()
            .unwrap()
            .error
            .contains("重启中断"));
        drop(s);
        assert!(app.archive_tick().unwrap()["skipped"].is_string());
        // A committed success is not turned into a failure on the next restart.
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        app.commit(candidate, result("成功"), now()).unwrap();
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_attempts.last().unwrap().status, "succeeded");
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn no_new_input_reuses_report_and_first_empty_uses_fallback() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        let empty = app.archive(AiMode::Manual).unwrap();
        assert_eq!(empty["summary_status"], "fallback");
        assert_eq!(app.state.lock().unwrap().calls, 0);
        {
            let mut s = app.state.lock().unwrap();
            s.reports[0]["summary_status"] = json!("done");
            s.reports[0]["ai_summary"] =
                json!({"summary":"上次报表","completed":[],"attention":[],"next_steps":[]});
        }
        let reused = app.archive(AiMode::Manual).unwrap();
        assert_eq!(reused["summary_status"], "reused");
        assert_eq!(
            reused["reused_from"],
            app.state.lock().unwrap().reports[1]["id"]
        );
        assert_eq!(app.state.lock().unwrap().calls, 0);
        assert!(app
            .state
            .lock()
            .unwrap()
            .archive_attempts
            .iter()
            .all(|a| a.status != "running"));
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn memory_changes_support_lasting_once_correction_and_revoke() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = seed(&app, "以后默认先核对环境，只此一次忽略版本");
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        let source = candidate.payload["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["event"]["kind"] == "manual_archive")
            .unwrap()["evidence_id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut value = result("完成");
        value["memory_changes"] = json!([
            {"action":"add","scope":"lasting","key":"职责","text":"先核对环境","source":[source],"task_id":task},
            {"action":"add","scope":"once","key":"本次例外","text":"忽略版本","source":[source],"task_id":task}
        ]);
        app.commit(candidate, value, now()).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.analysis_memory.archived_entries.len(), 1);
            assert_eq!(s.analysis_memory.archived_entries[0].text, "先核对环境");
            assert_eq!(s.analysis_memory.archived_version, 1);
            assert!(s.analysis_memory.archived_text().contains("先核对环境"));
            assert!(!s.analysis_memory.archived_text().contains("忽略版本"));
        }
        // Same-scope correction wins and revoke removes it.
        let s = app.state.lock().unwrap();
        let mut value = result("完成");
        value["memory_changes"] = json!([
            {"action":"update","scope":"lasting","key":"职责","text":"先核对环境再发布","source":["audit:0"]},
            {"action":"revoke","scope":"lasting","key":"职责","source":["audit:0"]}
        ]);
        let mut state = s.clone();
        drop(s);
        let allowed = (0..state.audit.len())
            .map(|i| format!("audit:{i}"))
            .collect::<BTreeSet<_>>();
        apply_memory_changes(
            &mut state,
            value["memory_changes"].as_array().unwrap(),
            &allowed,
            &allowed,
            now(),
        )
        .unwrap();
        assert!(state.analysis_memory.archived_entries.is_empty());
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn archive_contract_accepts_operation_learning_without_notes_and_rejects_unseen_sources() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = seed(&app, "");
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(&s, AiMode::Manual, now(), "attempt", "day")
        };
        assert!(candidate.payload["archive_note_sources"]
            .as_array()
            .unwrap()
            .is_empty());
        let sources = candidate.payload["archive_memory_sources"]
            .as_array()
            .unwrap();
        assert!(!sources.is_empty());
        let config = app.state.lock().unwrap().config.clone();
        let prepared = crate::ai::prepare(&config, "archive", &candidate.payload);
        let schema = &prepared.schema["properties"]["memory_changes"];
        assert_eq!(schema["maxItems"], 16);
        assert_eq!(
            schema["items"]["properties"]["source"]["items"]["enum"],
            json!(sources
                .iter()
                .map(|r| r["evidence_id"].clone())
                .collect::<Vec<_>>())
        );
        assert!(prepared.prompt.contains("archive_memory_sources"));
        let source = sources[0]["evidence_id"].clone();
        let mut value = result("汇总");
        value["memory_changes"] = json!([{"action":"add","scope":"lasting","key":"核对习惯","text":"优先核对合成证据","source":[source],"task_id":task}]);
        let mut state = app.state.lock().unwrap().clone();
        apply(&mut state, &candidate, value.clone(), now()).unwrap();
        assert_eq!(
            state.analysis_memory.archived_entries[0].text,
            "优先核对合成证据"
        );
        assert_eq!(
            state.analysis_memory.temporary,
            app.state.lock().unwrap().analysis_memory.temporary
        );
        let mut state = app.state.lock().unwrap().clone();
        let before = json!(state);
        value["memory_changes"][0]["source"] = json!(["audit:999999"]);
        assert!(apply(&mut state, &candidate, value.clone(), now()).is_err());
        assert_eq!(json!(state), before);
        value["memory_changes"][0]["source"] = json!(["unseen-message"]);
        assert!(apply(&mut state, &candidate, value, now()).is_err());
        assert_eq!(json!(state), before);
        let mut no_operations = candidate.payload.clone();
        no_operations["archive_memory_sources"] = json!([]);
        assert_eq!(
            crate::ai::prepare(&config, "archive", &no_operations).schema["properties"]
                ["memory_changes"]["maxItems"],
            0
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn manual_archive_with_its_own_attempt_reaches_quota_outside_work_hours() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        seed(&app, "合成归档备注");
        app.test_force_working();
        app.test_end_overtime();
        app.state.lock().unwrap().config.daily_calls = 0;
        assert!(!app.ai_allowed(AiMode::Automatic));
        let error = app.archive(AiMode::Manual).unwrap_err();
        assert!(error.starts_with("已达到每日AI调用上限"), "{error}");
        let s = app.state.lock().unwrap();
        assert_eq!(s.calls, 0);
        assert_eq!(s.archive_attempts.last().unwrap().status, "failed");
        assert!(s.reports.is_empty());
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn corrupt_snapshot_rebuild_failure_prevents_archive_side_effects() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        seed(&app, "快照失败也要保留");
        let before = json!(*app.state.lock().unwrap());
        std::fs::write(app.root.join(crate::snapshots::DIR), b"not-a-directory").unwrap();
        assert!(app.archive(AiMode::Manual).is_err());
        let after = app.state.lock().unwrap();
        assert_eq!(after.tasks[0].archived, before["tasks"][0]["archived"]);
        assert_eq!(json!(after.archive_ai_cursor), before["archive_ai_cursor"]);
        assert_eq!(after.analysis_memory.archived_version, 0);
        assert_eq!(after.tag_state.versions.len(), 1);
        drop(after);
        std::fs::remove_file(app.root.join(crate::snapshots::DIR)).unwrap();
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn rollback_creates_a_new_group_without_rewinding_cursor_or_task_state() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        let task = seed(&app, "整组回滚依据");
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        let source = candidate.payload["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["event"]["kind"] == "manual_archive")
            .unwrap()["evidence_id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut value = result("完成");
        value["memory_changes"] = json!([{"action":"add","scope":"lasting","key":"职责","text":"核对环境","source":[source],"task_id":task}]);
        value["tag_changes"] =
            json!([{"action":"add","tag":{"kind":"所属项目","value":"合成项目"}}]);
        app.commit(candidate, value, now()).unwrap();
        let cursor = app.state.lock().unwrap().archive_ai_cursor;
        let group = app
            .state
            .lock()
            .unwrap()
            .archive_versions
            .last()
            .unwrap()
            .id
            .clone();
        // A later manual task edit must survive the rollback.
        let t = app.state.lock().unwrap().tasks[0].clone();
        app.task_edit(&json!({"task":{"id":t.id,"title":"后改标题","description":"后改描述","status":t.status,"priority":t.priority,"category":t.category,"environment":t.environment,"created":t.created,"updated":t.updated}})).unwrap();
        let updated = app.state.lock().unwrap().tasks[0].updated;
        app.archive_rollback(&group, &group).unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_ai_cursor, cursor);
        assert_eq!(s.tasks[0].title, "后改标题");
        assert_eq!(s.tasks[0].updated, updated);
        assert_eq!(s.archive_versions.len(), 2);
        assert_eq!(s.archive_versions[1].action, "rollback");
        assert_eq!(s.archive_versions[1].restored_from, group);
        assert!(s.analysis_memory.archived_text().contains("核对环境"));
        assert!(!crate::tags::groups(&s)
            .iter()
            .any(|g| g.name == "所属项目" && g.values.contains(&"合成项目".into())));
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn queued_archive_needs_the_matching_claim_and_failures_are_not_retried() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.enabled = true;
            s.config.codex = "does-not-exist.exe".into();
            s.config.model_profiles = None;
            s.audit.push(Audit {
                time: now(),
                kind: "manual_archive".into(),
                task_id: "t".into(),
                detail: "人工归档".into(),
                before: json!({}),
                after: json!({"archive_note":"合成备注"}),
            });
            prompts::queue(&mut s, &[]);
        }
        // A queued attempt without a persisted end claim fails closed before any
        // AI call and is never auto-retried.
        let queued = app.queue_attempt("work_end", now()).unwrap();
        let attempt_id = queued["attempt"].as_str().unwrap().to_string();
        let error = app.archive_tick().unwrap_err();
        assert!(error.contains("持久化下班 claim"), "{error}");
        let s = app.state.lock().unwrap();
        let failed = s
            .archive_attempts
            .iter()
            .find(|a| a.id == attempt_id)
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(s.calls, 0);
        assert_eq!(s.archive_ai_cursor, Some(0));
        drop(s);
        assert_eq!(
            app.archive_tick().unwrap()["skipped"],
            "归档失败或等待下次到点；失败仅手动重试"
        );
        assert_eq!(app.state.lock().unwrap().calls, 0);

        // A later occurrence with the authoritative claim may cross the work
        // period boundary; the provider failure is recorded once.
        {
            let mut s = app.state.lock().unwrap();
            s.archive_attempts.retain(|a| a.id != attempt_id);
        }
        app.persist().unwrap();
        let queued = app.queue_attempt("work_end", now()).unwrap();
        let attempt_id = queued["attempt"].as_str().unwrap().to_string();
        {
            let mut s = app.state.lock().unwrap();
            let day = crate::work_state::shanghai_date(now()).unwrap();
            let record = s.work_scheduler.runtime.day_record_mut(day);
            record.end_claim = Some(crate::work_state::EndArchiveClaim {
                attempt_id: attempt_id.clone(),
                claimed_at_ms: now(),
                eligibility: crate::work_state::EndEligibility::WorkedBeforeEnd,
            });
        }
        app.persist().unwrap();
        let _ = app.archive_tick();
        let s = app.state.lock().unwrap();
        assert_eq!(s.calls, 1);
        assert_eq!(s.archive_ai_cursor, Some(0));
        assert_eq!(
            s.archive_attempts
                .iter()
                .find(|a| a.id == attempt_id)
                .unwrap()
                .status,
            "failed"
        );
        drop(s);
        assert_eq!(
            app.archive_tick().unwrap()["skipped"],
            "归档失败或等待下次到点；失败仅手动重试"
        );
        assert_eq!(app.state.lock().unwrap().calls, 1);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn archive_only_maintains_archived_memory_even_with_legacy_auto_flags_and_outputs() {
        for enabled in [false, true] {
            let root = root();
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            let task = seed(&app, "以后优先核对原始证据");
            {
                let mut s = app.state.lock().unwrap();
                s.config.auto_adopt_prompt = enabled;
                s.config.auto_adopt_advice = enabled;
                s.config.classify_prompt = "用户手动配置的分析规则".into();
                s.config.archive_prompt = "用户手动配置的归档规则".into();
                s.analysis_memory.temporary = "分析自主保留的交接内容".into();
                s.analysis_memory.temporary_version = 7;
                app.save(&s).unwrap();
            }
            let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
            let candidate = batch(
                &app.state.lock().unwrap(),
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            );
            assert!(candidate.payload.get("current_prompt").is_none());
            assert!(candidate.payload.get("pending_advice").is_none());
            assert!(candidate.payload.get("handling_learning").is_none());
            let source = candidate.payload["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["event"]["kind"] == "manual_archive")
                .unwrap()["evidence_id"]
                .clone();
            let before = app.state.lock().unwrap().clone();
            let mut value = result("本批归档完成");
            value["memory_changes"] = json!([{"action":"add","scope":"lasting","key":"核对习惯","text":"优先核对原始证据","source":[source],"task_id":task}]);
            // Simulate an obsolete/unsolicited result reaching the commit boundary.
            value["review"] = json!({"improved":true,"proposed_prompt":"不应采用的新提示词"});
            value["suggestions"] = json!([{"title":"不应启用的规则","rule":"不应写入"}]);
            value["tag_changes"] =
                json!([{"action":"remove_group","tag":{"kind":"事项类型","value":""}}]);
            value["advice_decisions"] = json!([{"kind":"prompt","action":"enable"}]);
            value["handling_review"] = json!([{"improved":true,"policy":{"prompt":"不应采用"}}]);
            value["memory_update"] = json!(["不应覆盖临时记忆"]);
            fs::create_dir(root.join("data/state.next.json")).unwrap();
            assert!(app.commit(candidate.clone(), value.clone(), now()).is_err());
            assert_eq!(
                json!(app.state.lock().unwrap().analysis_memory),
                json!(before.analysis_memory)
            );
            fs::remove_dir(root.join("data/state.next.json")).unwrap();
            let report = app.commit(candidate, value, now()).unwrap();
            let s = app.state.lock().unwrap();
            assert!(s
                .analysis_memory
                .archived_text()
                .contains("优先核对原始证据"));
            assert_eq!(
                s.analysis_memory.temporary,
                before.analysis_memory.temporary
            );
            assert_eq!(s.analysis_memory.temporary_version, 7);
            assert_eq!(json!(s.config), json!(before.config));
            assert_eq!(json!(s.prompt_versions), json!(before.prompt_versions));
            assert_eq!(json!(s.prompt_reviews), json!(before.prompt_reviews));
            assert_eq!(json!(s.suggestions), json!(before.suggestions));
            assert_eq!(json!(s.handling), json!(before.handling));
            assert_eq!(json!(s.tag_state), json!(before.tag_state));
            assert_eq!(s.rule_version, before.rule_version);
            assert_eq!(report["temporary_memory_cleared"], false);
            drop(s);
            let reload = App::load(root.clone()).unwrap();
            assert_eq!(
                reload.state.lock().unwrap().config.archive_prompt,
                before.config.archive_prompt
            );
            drop(reload);
            drop(app);
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn note_only_archive_advances_shared_cursor_and_archives_after_success() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        seed(&app, "只有新增备注也构成新材料");
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        assert!(!candidate.evidence_ids.is_empty());
        let cursor = app.state.lock().unwrap().archive_ai_cursor.unwrap_or(0);
        app.commit(candidate.clone(), result("备注已分析"), now())
            .unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.archive_ai_cursor.unwrap() > cursor);
        assert!(s.tasks[0].archived);
        assert!(s.analysis_memory.temporary.is_empty());
        assert!(s.reports[0]["summary_status"] == "done");
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn memory_changes_are_atomic_and_bounded_without_silent_truncation() {
        let mut s = State::default();
        s.analysis_memory.archived_entries.push(MemoryEntry {
            id: "old".into(),
            key: "已有".into(),
            text: "保留".into(),
            source: vec!["audit:0".into()],
            version: 1,
            ..Default::default()
        });
        let allowed = ["audit:0".to_string()].into_iter().collect::<BTreeSet<_>>();
        let notes = allowed.clone();
        let change = |key: &str, text: &str| json!({"action":"add","scope":"lasting","key":key,"text":text,"source":["audit:0"]});
        let too_many = (0..17)
            .map(|i| change(&format!("k{i}"), "值"))
            .collect::<Vec<_>>();
        assert!(apply_memory_changes(&mut s, &too_many, &allowed, &notes, now()).is_err());
        assert_eq!(s.analysis_memory.archived_entries.len(), 1);
        assert!(s.audit.is_empty());
        assert!(apply_memory_changes(
            &mut s,
            &[
                change("a", &"甲".repeat(3000)),
                change("b", &"乙".repeat(3000))
            ],
            &allowed,
            &notes,
            now()
        )
        .is_err());
        assert_eq!(s.analysis_memory.archived_entries.len(), 1);
        assert!(s.audit.is_empty());
        assert_eq!(
            apply_memory_changes(
                &mut s,
                &[change("a", "甲"), change("b", "乙")],
                &allowed,
                &notes,
                now()
            )
            .unwrap(),
            2
        );
        assert_eq!(s.analysis_memory.archived_entries.len(), 3);
        assert_eq!(s.analysis_memory.archived_version, 1);
        assert_eq!(s.audit.len(), 2);
    }

    #[test]
    fn archive_candidate_freezes_snapshot_and_version_checks_memory_and_task_edits() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = seed(&app, "并发修改依据");
        {
            let mut s = app.state.lock().unwrap();
            s.analysis_memory.temporary = "候选时记忆".into();
            s.analysis_memory.temporary_version = 3;
            app.save(&s).unwrap();
        }
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        let frozen = candidate.payload["_local_context"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect::<BTreeSet<_>>();
        assert!(frozen.contains(task.as_str()));
        // New arrivals after the candidate must not appear in its frozen view.
        {
            let mut s = app.state.lock().unwrap();
            s.messages.insert(
                "late".into(),
                Message {
                    id: "late".into(),
                    text: "候选之后的新消息".into(),
                    time: now() + 1,
                    private: true,
                    ..Default::default()
                },
            );
            s.tasks[0].updated += 1;
            s.analysis_memory.temporary = "分析期间的新记忆".into();
            s.analysis_memory.temporary_version += 1;
            app.save(&s).unwrap();
        }
        let report = app
            .commit(candidate.clone(), result("并发处理"), now())
            .unwrap();
        let s = app.state.lock().unwrap();
        assert!(!s.tasks[0].archived);
        assert_eq!(s.analysis_memory.temporary, "分析期间的新记忆");
        assert_eq!(report["temporary_memory_cleared"], false);
        assert!(report["memory_note"].as_str().is_some());
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_task_with_a_later_unprocessed_completion_or_note_is_not_archived_early() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            let mut task = Task {
                id: "t".into(),
                title: "先有进展后有完成".into(),
                description: "核对".into(),
                status: "已完成".into(),
                environment: "未指定".into(),
                updated: 2,
                created: 1,
                ..Default::default()
            };
            s.audit.push(Audit {
                time: 1,
                kind: "ai_update".into(),
                task_id: task.id.clone(),
                detail: "早期进展".into(),
                before: json!([task.clone()]),
                after: json!([task.clone()]),
            });
            for i in 0..100 {
                let filler = Task {
                    id: format!("f{i}"),
                    title: format!("填充{i}"),
                    description: "填充材料".into(),
                    status: "已完成".into(),
                    updated: 2,
                    created: 1,
                    ..Default::default()
                };
                s.audit.push(Audit {
                    time: 2,
                    kind: "manual_status".into(),
                    task_id: filler.id.clone(),
                    detail: "人工完成".into(),
                    before: json!([filler.clone()]),
                    after: json!([filler.clone()]),
                });
                s.tasks.push(filler);
            }
            task.status = "已完成".into();
            s.audit.push(Audit {
                time: 3,
                kind: "manual_archive".into(),
                task_id: task.id.clone(),
                detail: "最终完成与备注".into(),
                before: json!([task.clone()]),
                after: json!([{
                    "id":"t","archive_note":"最终才出现的备注","status":"已完成"
                }]),
            });
            s.tasks.push(task);
            s.archive_ai_cursor = Some(0);
            app.save(&s).unwrap();
        }
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch_limited(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
                10,
            )
        };
        assert!(candidate.remaining_events > 0);
        assert!(candidate.tasks_to_archive.iter().all(|task| task.id != "t"));
        // The earlier update was included, but the latest relevant event was not.
        assert!(candidate
            .evidence_ids
            .iter()
            .any(|index| app.state.lock().unwrap().audit[*index].task_id == "t"));
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn large_delta_only_finalizes_covered_tasks_and_keeps_unprocessed_temporary_memory() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            s.analysis_memory.temporary = "尚未完整处理的临时记忆".into();
            s.analysis_memory.temporary_version = 1;
            for i in 0..80 {
                let mut task = Task {
                    id: format!("t{i}"),
                    title: format!("合成事项{i}"),
                    description: "核对完成".into(),
                    status: "已完成".into(),
                    environment: "未指定".into(),
                    updated: 1,
                    created: 1,
                    ..Default::default()
                };
                let before = task.clone();
                task.updated = 2;
                s.audit.push(Audit {
                    time: i as i64 + 1,
                    kind: "manual_status".into(),
                    task_id: task.id.clone(),
                    detail: "人工完成".into(),
                    before: json!([before]),
                    after: json!([task.clone()]),
                });
                s.tasks.push(task);
            }
            s.archive_ai_cursor = Some(0);
            app.save(&s).unwrap();
        }
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch_limited(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
                10,
            )
        };
        assert!(candidate.remaining_events > 0);
        assert!(candidate.cutoff < 80);
        assert!(candidate.tasks_to_archive.len() < 80);
        let covered = candidate
            .evidence_ids
            .iter()
            .map(|i| app.state.lock().unwrap().audit[*i].task_id.clone())
            .collect::<BTreeSet<_>>();
        assert!(candidate
            .tasks_to_archive
            .iter()
            .all(|task| covered.contains(&task.id)));
        let report = app
            .commit(candidate.clone(), result("部分处理"), now())
            .unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_ai_cursor, Some(candidate.cutoff));
        assert_eq!(report["temporary_memory_cleared"], false);
        assert_eq!(s.analysis_memory.temporary, "尚未完整处理的临时记忆");
        assert!(s.tasks.iter().filter(|t| t.archived).count() < 80);
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_conflicts_on_manual_prompt_rule_tag_or_memory_changes() {
        fn group_app() -> (App, String, std::path::PathBuf) {
            let root = root();
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            seed(&app, "回滚冲突依据");
            {
                let mut s = app.state.lock().unwrap();
                s.suggestions.push(Suggestion {
                    id: "rule".into(),
                    title: "规则".into(),
                    rule: "先核对环境".into(),
                    active: true,
                    decision: "enabled".into(),
                    created: 1,
                    ..Default::default()
                });
                app.save(&s).unwrap();
            }
            let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
            let candidate = {
                let s = app.state.lock().unwrap();
                batch(
                    &s,
                    AiMode::Manual,
                    now(),
                    &attempt.id,
                    &attempt.business_day,
                )
            };
            app.commit(candidate, result("完成"), now()).unwrap();
            let group = app
                .state
                .lock()
                .unwrap()
                .archive_versions
                .last()
                .unwrap()
                .id
                .clone();
            (app, group, root)
        }
        {
            let (app, group, root) = group_app();
            app.state.lock().unwrap().config.classify_prompt = "手动新提示词".into();
            app.save(&app.state.lock().unwrap()).unwrap();
            let error = app.archive_rollback(&group, &group).unwrap_err();
            assert!(error.contains("手动修改"), "{error}");
            assert_eq!(
                app.state.lock().unwrap().config.classify_prompt,
                "手动新提示词"
            );
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
        {
            let (app, group, root) = group_app();
            app.state.lock().unwrap().suggestions[0].rule = "手动改规则".into();
            app.save(&app.state.lock().unwrap()).unwrap();
            let error = app.archive_rollback(&group, &group).unwrap_err();
            assert!(error.contains("手动修改"), "{error}");
            assert_eq!(app.state.lock().unwrap().suggestions[0].rule, "手动改规则");
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
        {
            let (app, group, root) = group_app();
            app.state
                .lock()
                .unwrap()
                .tag_state
                .versions
                .push(crate::tags::Version {
                    priority_kind: String::new(),
                    assignments: vec![],
                    id: "manual-tag".into(),
                    created: now(),
                    action: "manual".into(),
                    reason: "手动标签".into(),
                    groups: vec![],
                });
            app.save(&app.state.lock().unwrap()).unwrap();
            let error = app.archive_rollback(&group, &group).unwrap_err();
            assert!(error.contains("手动修改"), "{error}");
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
        {
            let (app, group, root) = group_app();
            app.state.lock().unwrap().analysis_memory.archived_version += 1;
            app.state
                .lock()
                .unwrap()
                .analysis_memory
                .archived_entries
                .push(MemoryEntry {
                    id: id(),
                    key: "人工记忆".into(),
                    text: "保留最新内容".into(),
                    ..Default::default()
                });
            app.save(&app.state.lock().unwrap()).unwrap();
            let before = serde_json::to_value(&app.state.lock().unwrap().analysis_memory).unwrap();
            app.archive_rollback(&group, &group).unwrap();
            assert_eq!(
                serde_json::to_value(&app.state.lock().unwrap().analysis_memory).unwrap(),
                before
            );
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn archive_groups_store_only_active_rules_and_current_tags_not_full_history() {
        let (app, group, root) = {
            let root = root();
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            seed(&app, "紧凑组依据");
            {
                let mut s = app.state.lock().unwrap();
                s.suggestions.push(Suggestion {
                    id: "active".into(),
                    title: "规则".into(),
                    rule: "规则内容".into(),
                    active: true,
                    decision: "enabled".into(),
                    ..Default::default()
                });
                s.suggestions.push(Suggestion {
                    id: "pending".into(),
                    title: "待定".into(),
                    rule: "待定内容".into(),
                    ..Default::default()
                });
                app.save(&s).unwrap();
            }
            let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
            let candidate = {
                let s = app.state.lock().unwrap();
                batch(
                    &s,
                    AiMode::Manual,
                    now(),
                    &attempt.id,
                    &attempt.business_day,
                )
            };
            app.commit(candidate, result("完成"), now()).unwrap();
            let group = app
                .state
                .lock()
                .unwrap()
                .archive_versions
                .last()
                .cloned()
                .unwrap();
            (app, group, root)
        };
        let serialized = serde_json::to_value(&group).unwrap();
        assert!(serialized.get("suggestions").is_none());
        assert!(serialized.get("tag_state").is_none());
        assert_eq!(group.active_rules.len(), 1);
        assert_eq!(group.active_rules[0].id, "active");
        assert!(!group.tag_groups.is_empty());
        assert!(!group.tag_version.is_empty());
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn derived_report_file_failure_after_atomic_commit_is_a_success_with_diagnostic() {
        let root = root();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        seed(&app, "报表文件失败依据");
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        let candidate = {
            let s = app.state.lock().unwrap();
            batch(
                &s,
                AiMode::Manual,
                now(),
                &attempt.id,
                &attempt.business_day,
            )
        };
        let report_id = candidate.report["id"].as_str().unwrap().to_string();
        let report_dir = app.root.join("data/reports");
        fs::create_dir_all(&report_dir).unwrap();
        let blocked = report_dir.join(format!(
            "{}-{report_id}.json",
            candidate.report["date"].as_str().unwrap()
        ));
        fs::create_dir_all(&blocked).unwrap();
        let report = app
            .commit(candidate.clone(), result("已提交"), now())
            .unwrap();
        assert_eq!(report["summary_status"], "done");
        assert_eq!(
            app.state.lock().unwrap().archive_ai_cursor,
            Some(candidate.cutoff)
        );
        assert!(app
            .activity
            .lock()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "archive_report_file_failed"));
        let _ = fs::remove_dir_all(&blocked);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}
