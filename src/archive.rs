use crate::{
    context_budget,
    engine::{self, App},
    model::*,
    prompts,
    work_hours::AiMode,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, sync::atomic::Ordering};

fn group_snapshot(s: &State) -> ArchiveGroup {
    ArchiveGroup {
        active_rules: s
            .suggestions
            .iter()
            .filter(|rule| rule.active && rule.archived_at == 0)
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
        archived_version: s.analysis_memory.archived_version,
        ..ArchiveGroup::default()
    }
}

/// Archive groups keep the classify prompt by reference when an identical
/// prompt version exists. Prompt versions are never pruned, so the rollback
/// history stays complete without one full prompt copy per archived day.
pub(crate) fn stored_prompt<T: AsRef<str> + Default>(s: &State, version: &str, content: T) -> T {
    if s.prompt_versions
        .iter()
        .any(|v| v.id == version && v.content == content.as_ref())
    {
        T::default()
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
    let finished = s.next_report_finished(at);
    for attempt in &mut s.archive_attempts {
        if attempt.unfinished() {
            attempt.status = "failed".into();
            attempt.error = RECOVERY_ERROR.into();
            attempt.finished = finished;
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
    pub report: Value,
    pub tasks_to_archive: Vec<Task>,
    pub pending_count: usize,
    pub remaining_events: usize,
}
impl Candidate {
    fn has_ai_input(&self) -> bool {
        !self.evidence_ids.is_empty()
            || (self.trigger == "manual_archive"
                && self.payload["_archived_memory"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()))
    }
}

#[cfg(test)]
fn batch(s: &State, mode: AiMode, at: i64, attempt_id: &str, business_day: &str) -> Candidate {
    let mut candidate = batch_limited(
        s,
        mode,
        at,
        attempt_id,
        business_day,
        usize::MAX,
        true,
        crate::context_budget::pending_refs(s, false).count(),
    );
    crate::analysis_context::attach_records(
        &mut candidate.payload,
        crate::snapshots::records_from_state(s, at),
        s.config.retrieval_budget_chars,
    )
    .unwrap();
    candidate
}

fn batch_limited(
    s: &State,
    mode: AiMode,
    at: i64,
    attempt_id: &str,
    business_day: &str,
    max_events: usize,
    include_current_tasks: bool,
    pending_count: usize,
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
        "archive_note_sources": evidence_ids.iter().filter_map(|i| {
            let audit = &s.audit[*i];
            (audit.kind == "manual_archive").then_some(audit.after["archive_note"].as_str()).flatten()
                .filter(|note| !note.trim().is_empty())
                .map(|note| json!({"evidence_id":format!("audit:{i}"),"task_id":audit.task_id,"note":note.chars().take(2000).collect::<String>()}))
        }).collect::<Vec<_>>(),
        "learning_evidence_ids":learning_ids.iter().map(|i|format!("audit:{i}")).collect::<Vec<_>>(),
        "coverage":{"status_counts":counts,"remaining_events":remaining,"omitted_tasks":0},
        "tag_catalog":crate::tags::groups(s),
        "_trigger":trigger,
        "_archive_attempt_id":attempt_id,
        "_data_until":s.last_scan,
    });
    payload["report_tasks"] = Value::Array(tasks.iter().map(|t| context_budget::task(t)).collect());
    payload["current_tasks"] = Value::Array(if include_current_tasks {
        s.tasks
            .iter()
            .filter(|t| !t.archived && t.merged_into.is_empty() && !related.contains(t.id.as_str()))
            .map(context_budget::task)
            .collect()
    } else {
        Vec::new()
    });
    if !s.config.report_prompt.trim().is_empty() {
        payload["report_policy"] = json!(s.config.report_prompt);
    }
    let remaining_events = remaining;
    crate::analysis_context::attach_memory_text(
        &mut payload,
        &s.analysis_memory.archived_text(),
        &s.analysis_memory.temporary,
    );
    payload["_local_context"]["archived_memory"]["revision"] =
        json!(s.analysis_memory.archived_version);
    payload["_local_context"]["memory"]["writable"] = json!(false);
    payload["memory"]["writable"] = json!(false);
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
        report,
        tasks_to_archive,
        pending_count,
        remaining_events,
    }
}

fn apply_memory_update(
    s: &mut State,
    candidate: &Candidate,
    result: &Value,
    at: i64,
) -> Result<usize, String> {
    if let Some(error) = result["_archived_memory_error"].as_str() {
        return Err(format!("归档记忆未保存：{error}"));
    }
    let Some(update) = result.get("_archived_memory_update") else {
        if result["archived_memory"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        {
            return Err("归档记忆缺少完整读取校验，本批未提交".into());
        }
        return Ok(0);
    };
    let current = s.analysis_memory.archived_text();
    if s.analysis_memory.archived_version != candidate.memory_revision
        || update["expected"].as_u64() != Some(candidate.memory_revision)
        || update["before"].as_str() != Some(current.as_str())
    {
        return Err("归档记忆已被人工编辑，本批未提交，请核对后重新归档".into());
    }
    let text = update["text"].as_str().ok_or("归档记忆正文无效")?;
    if text.chars().count() > crate::analysis_context::ARCHIVED_MEMORY_LIMIT {
        return Err("归档记忆超过4000字符，保留原记忆".into());
    }
    if text == current {
        return Ok(0);
    }
    s.analysis_memory.replace_archived(text.into(), at);
    App::audit(
        s,
        "archive_memory",
        "",
        "归档更新记忆正文",
        Value::Null,
        json!({"revision":s.analysis_memory.archived_version,"chars":text.chars().count()}),
    );
    Ok(1)
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
    let finished = if candidate.remaining_events == 0 {
        s.next_report_finished(at)
    } else {
        0
    };
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
        attempt.finished = finished;
        attempt.error.clear();
    }
}

fn apply(s: &mut State, candidate: &Candidate, result: Value, at: i64) -> Result<Value, String> {
    let summary = &result["report"];
    // A validated full-document replacement is the only policy write allowed.
    let memory_applied = apply_memory_update(s, candidate, &result, at)?;

    // Completed tasks become archived only in the successful commit. Tasks
    // edited while the model was running are left for a later archive.
    let tool_audit_start = s.audit.len();
    let archived = finalize_tasks(s, candidate, at);
    let tool_tasks = s
        .audit
        .iter()
        .skip(tool_audit_start)
        .map(|audit| audit.task_id.clone())
        .collect::<Vec<_>>();
    crate::tool_calling::link_tasks(s, &result, &tool_tasks);

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
        ..group_snapshot(s)
    };
    report["archive_group_id"] = json!(group.id);
    s.archive_versions.push(group);
    s.reports.insert(0, report.clone());
    s.truncate_reports();
    App::audit(
        s,
        "archive_commit",
        "",
        "归档结果、成功状态、游标、事项归档标记和归档记忆同次保存",
        json!({"attempt":candidate.attempt_id,"cutoff":candidate.cutoff}),
        json!({"report":report["id"],"group":report["archive_group_id"],"archived_tasks":archived}),
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
        self.commit_state(|s| {
            if let Some(existing) = s
                .archive_attempts
                .iter_mut()
                .find(|existing| existing.id == attempt.id)
            {
                *existing = attempt;
            } else {
                s.archive_attempts.push(attempt);
            }
            Ok(())
        })
    }
    fn fail_attempt(&self, id: &str, error: &str) -> Result<(), String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let finished = s.next_report_finished(now());
        if let Some(attempt) = s.archive_attempts.iter_mut().find(|a| a.id == id) {
            attempt.status = "failed".into();
            attempt.finished = finished;
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
        let finished = lock.next_report_finished(now());
        if let Some(attempt) = lock.archive_attempts.iter_mut().find(|a| a.id == id) {
            attempt.status = "failed".into();
            attempt.finished = finished;
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
        snapshots: &mut crate::snapshots::Cache,
    ) -> Result<(Candidate, Option<engine::PreparedAiRequest>), String> {
        #[cfg(test)]
        let _gate = self.prepare_gate.lock().unwrap();
        let records = snapshots.records(frozen, at);
        let pending_count = crate::context_budget::pending_refs(frozen, false).count();
        let mut all = batch_limited(
            frozen,
            mode,
            at,
            &attempt.id,
            &attempt.business_day,
            usize::MAX,
            true,
            pending_count,
        );
        all.trigger = attempt.trigger.clone();
        all.payload["_trigger"] = json!(attempt.trigger);
        // Automatic archives with no new evidence reuse the report. An explicit
        // manual archive can also consolidate existing memory on its own.
        if !all.has_ai_input() {
            return Ok((all, None));
        }
        let mut context = self.capture_archive_context(frozen, records);
        let mut prepare = |candidate: &Candidate| {
            self.prepare_ai_candidate_with("archive", &candidate.payload, mode, &mut context)
        };
        let mut request = prepare(&all)?;
        if request.prepared.total <= request.config.max_input_chars {
            request.materialize_records();
            return Ok((all, Some(request)));
        }
        // Prefer new evidence over unrelated background summaries. Full records
        // remain in the frozen read-only tool snapshot.
        let omitted = all.payload["current_tasks"].as_array().map_or(0, Vec::len);
        all.payload["current_tasks"] = json!([]);
        all.payload["coverage"]["omitted_current_tasks"] = json!(omitted);
        let mut request = prepare(&all)?;
        if request.prepared.total <= request.config.max_input_chars {
            request.materialize_records();
            return Ok((all, Some(request)));
        }
        let accepted = context_budget::fit_prefix(all.evidence_ids.len(), |count| {
            let mut candidate = batch_limited(
                frozen,
                mode,
                at,
                &attempt.id,
                &attempt.business_day,
                count,
                false,
                pending_count,
            );
            candidate.trigger = attempt.trigger.clone();
            candidate.payload["_trigger"] = json!(attempt.trigger);
            candidate.payload["current_tasks"] = json!([]);
            candidate.payload["coverage"]["omitted_current_tasks"] = json!(omitted);
            let request = prepare(&candidate)?;
            Ok((request.prepared.total <= request.config.max_input_chars)
                .then_some((candidate, request)))
        })?;
        let (_, (candidate, mut request)) = accepted.ok_or_else(|| "归档的单条材料及必需上下文已超过单次输入上限，未调用 AI；请精简配置或提高输入上限后手动归档".to_string())?;
        request.materialize_records();
        Ok((candidate, Some(request)))
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
        let mut deadline_guard =
            crate::ai_control::DeadlineGuard::new(started + deadline, call.cancel.clone());
        let run = (|| -> Result<Value, String> {
            let mut snapshots = crate::snapshots::Cache::default();
            loop {
                crate::process::check_running()?;
                if call.cancel.load(Ordering::SeqCst) {
                    return Err(crate::ai_control::CANCELLED.into());
                }
                if started.elapsed() >= deadline {
                    return Err("归档总时限已到，后续批次未提交；请手动继续归档".into());
                }
                let (candidate, request) =
                    self.fitted_archive_batch(&frozen, mode, at, &attempt, &mut snapshots)?;
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
                let Some(mut request) = request else {
                    if candidate.remaining_events > 0 {
                        return Err("归档材料无法装入单批，游标和事项保留".into());
                    }
                    let _acceptance = self.active_ai.lock().unwrap();
                    if call.cancel.load(Ordering::SeqCst) {
                        return Err(crate::ai_control::CANCELLED.into());
                    }
                    if started.elapsed() >= deadline {
                        return Err("归档总时限已到，本批未提交；请手动继续归档".into());
                    }
                    return self.commit_without_ai(candidate, now());
                };
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
                // Retrieval exhaustion does not revoke evidence already delivered.
                // The candidate cutoff and memory receipt still bound the commit.
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
                frozen.analysis_memory.archived = live.analysis_memory.archived.clone();
                frozen.analysis_memory.archived_updated = live.analysis_memory.archived_updated;
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
        let timed_out = deadline_guard.finish();
        let run = if run.is_err() && timed_out {
            Err("归档总时限已到，本批未提交；已提交批次保留，请手动继续归档".into())
        } else {
            run
        };
        if let Err(error) = &run {
            self.fail_attempt(&attempt.id, error)?;
        }
        run
    }
    /// Derived files are best effort, after the business transaction commits.
    pub(crate) fn write_report_file(&self, report: &Value, error_event: &str) {
        let dir = crate::storage::data_dir(&self.root).join("reports");
        let result = fs::create_dir_all(&dir).and_then(|_| {
            fs::write(
                dir.join(format!(
                    "{}-{}.json",
                    report["date"].as_str().unwrap_or(""),
                    report["id"].as_str().unwrap_or("")
                )),
                serde_json::to_vec_pretty(report).unwrap(),
            )
        });
        if let Err(error) = result {
            self.log(
                error_event,
                json!({"error":error.to_string(),"report":report["id"]}),
            );
        }
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
            attempt.report_id = report["id"].as_str().unwrap_or("").into();
            attempt.reused = report["summary_status"] == "reused";
        }
        s.reports.insert(0, report.clone());
        s.truncate_reports();
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
        self.notifications
            .lock()
            .unwrap()
            .task_changes(&lock.tasks, &s.tasks);
        *lock = s;
        drop(lock);
        self.write_report_file(&report, "archive_report_file_failed");
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
        self.write_report_file(&output, "archive_report_file_failed");
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
            "current":s.archive_versions.last().map(|group| &group.id),
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
            "current":s.archive_versions.last().map(|g| &g.id),
            "memory":s.analysis_memory.archived_text(),
        })
    }
    /// Roll back prompt, rules, tags and archived memory as a NEW group. Tasks,
    /// message progress, analyzed fingerprints and later manual task edits are
    /// deliberately outside this operation.
    pub fn archive_rollback(&self, version_id: &str, expected: &str) -> Result<Value, String> {
        self.commit_state(|s| {
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
        let current_prompt_version = prompts::current(s, "classify")
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
            || prompts::content(&s.config, "classify") != archive_prompt(s, &latest)
            || s.rule_version != latest.rule_version
            || current_rules != latest_rules
            || current_tag_version != latest.tag_version;
        if conflict {
            return Err("最新归档之后提示词、规则或标签已有手动修改，整组回滚会覆盖这些修改，已取消；请先确认当前版本或生成新版本".into());
        }

        let restored_prompt = archive_prompt(s, &group).to_string();
        prompts::set_content(&mut s.config, "classify", restored_prompt.clone())?;
        let prompt_version = prompts::record(
            s,
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
            id: id(),
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
            prompt: stored_prompt(s, &prompt_version, restored_prompt),
            prompt_version,
            ..group_snapshot(s)
        };
        let response = json!({"group":next.id,"prompt_version":next.prompt_version});
        s.archive_versions.push(next);
        App::audit(
            s,
            "archive_rollback",
            "",
            "归档整组回滚：仅提示词、规则、标签和私有归档记忆；保留后续建议记录",
            json!({"restored_from":group.id}),
            json!({"group":response["group"]}),
        );
        Ok(response)
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn archive_app() -> (crate::test_support::TestRoot, App) {
        let (root, app) = crate::test_support::scoped_app("shishi-archive");
        app.state.lock().unwrap().config.model_profiles = None;
        (root, app)
    }
    fn result(report_summary: &str) -> Value {
        json!({
            "report":{"summary":report_summary,"completed":[],"attention":[],"next_steps":[]},
            "review":{"improved":false,"reason":"无需修改","proposed_prompt":"","evidence_ids":[]},
            "suggestions":[],
            "handling_review":[],
            "advice_decisions":[],
            "archived_memory":[],
            "tag_changes":[]
        })
    }
    fn manual_candidate(app: &App) -> Candidate {
        let attempt = app.start_attempt("manual_archive", now(), None).unwrap();
        batch(
            &app.state.lock().unwrap(),
            AiMode::Manual,
            now(),
            &attempt.id,
            &attempt.business_day,
        )
    }
    fn with_memory(candidate: &Candidate, value: &mut Value, text: &str) {
        let mut payload = candidate.payload.clone();
        let prepared = crate::ai::prepare(&Config::default(), "archive", &payload);
        crate::analysis_context::acknowledge_inline_memory(
            &mut payload,
            "archive",
            &prepared.prompt,
        );
        value["archived_memory"] = json!([text]);
        crate::ai_backend::api::attach_result(
            value,
            &payload["_local_context"],
            &Default::default(),
        );
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
                change_summary: String::new(),
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
        let (_root, app) = crate::test_support::scoped_app("shishi-archive");
        seed(&app, "合成归档依据");
        let candidate = {
            let state = app.state.lock().unwrap();
            batch(&state, AiMode::Manual, now(), "attempt", "day")
        };
        app.memory_save(&json!({"kind":"archived","expected":0,"text":"人工保存的最新内容"}))
            .unwrap();
        let before = app.memory_read();
        let mut output = result("合成结论");
        with_memory(&candidate, &mut output, "旧结果");
        assert!(app
            .commit(candidate, output, now())
            .unwrap_err()
            .contains("人工编辑"));
        assert_eq!(app.memory_read(), before);
    }
    #[test]
    fn archive_uses_full_rendered_budget_instead_of_fixed_event_or_task_caps() {
        let (_root, app) = archive_app();
        batch_fixture(&app, 60_000, 40);
        let frozen = app.state.lock().unwrap().clone();
        let attempt = ArchiveAttempt {
            id: "fixture".into(),
            trigger: "manual_archive".into(),
            ..Default::default()
        };
        let (candidate, request) = app
            .fitted_archive_batch(
                &frozen,
                AiMode::Manual,
                now(),
                &attempt,
                &mut crate::snapshots::Cache::default(),
            )
            .unwrap();
        let request = request.expect("new archive evidence requires an AI request");
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
    }

    #[test]
    fn manual_and_claimed_automatic_archive_finish_all_batches_in_one_attempt() {
        for automatic in [false, true] {
            let (_root, app) = archive_app();
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
        }
    }

    fn exhausted_result(request: &engine::PreparedAiRequest, replacement: Value) -> Value {
        let snapshot = &request.payload["_local_context"];
        let mut meter = crate::analysis_context::Meter {
            chars: request.config.retrieval_budget_chars,
            ..Default::default()
        };
        let notice: Value = serde_json::from_str(&crate::analysis_context::query(
            snapshot,
            &mut meter,
            &json!({"kind":"task"}),
        ))
        .unwrap();
        assert_eq!(notice["budget_exhausted"], true);
        let mut value = result("依据已交付材料完成合成归档");
        value["archived_memory"] = replacement;
        crate::ai_backend::api::attach_result(&mut value, snapshot, &meter);
        value
    }

    #[test]
    fn exhausted_retrieval_accepts_delivered_evidence_and_complete_inline_memory() {
        let (root, app) = archive_app();
        batch_fixture(&app, 60_000, 3);
        // This is a persistence check, not an expired-audit pruning fixture.
        // Keep evidence inside retention so reopening preserves its cursor.
        for event in app.state.lock().unwrap().audit.iter_mut() {
            event.time = now() - 1_000;
        }
        app.memory_save(&json!({"kind":"archived","expected":0,"text":"合成原记忆"}))
            .unwrap();
        let boundary = app.state.lock().unwrap().audit.len();
        let mut calls = 0;
        let report = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |request, _| {
                calls += 1;
                assert_eq!(request.payload["evidence"].as_array().unwrap().len(), 3);
                assert!(request.prepared.prompt.contains("合成原记忆"));
                let value = exhausted_result(request, json!(["整理后的合成记忆"]));
                assert!(value.get("_archived_memory_error").is_none());
                Ok(value)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(report["archive_complete"], true);
        let state = app.state.lock().unwrap();
        assert_eq!(state.archive_ai_cursor, Some(boundary));
        assert!(state.tasks.iter().all(|task| task.archived));
        assert_eq!(state.analysis_memory.archived_text(), "整理后的合成记忆");
        assert_eq!(state.analysis_memory.temporary, "合成临时记忆保留");
        assert_eq!(state.archive_attempts.last().unwrap().status, "succeeded");
        drop(state);
        drop(app);
        let restored = App::load(root.clone()).unwrap();
        assert_eq!(
            restored.state.lock().unwrap().archive_ai_cursor,
            Some(boundary)
        );
        assert_eq!(
            restored.memory_read()["archived"]["text"],
            "整理后的合成记忆"
        );
        drop(restored);
    }

    #[test]
    fn exhausted_retrieval_never_authorizes_a_partial_memory_replacement() {
        let (_root, app) = archive_app();
        batch_fixture(&app, 60_000, 3);
        let original = "合成".repeat(2_000);
        app.memory_save(&json!({"kind":"archived","expected":0,"text":original}))
            .unwrap();
        let before = app.state.lock().unwrap().clone();
        let error = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |request, _| {
                // Fault injection: this adapter did not deliver the inline body.
                // Reading only its first page must not authorize replacing it.
                let mut payload = request.payload.clone();
                crate::analysis_context::acknowledge_inline_memory(
                    &mut payload,
                    "archive",
                    &request.prepared.prompt.replace(&original, "省略后的摘要"),
                );
                let snapshot = &mut payload["_local_context"];
                snapshot["budget_chars"] = json!(crate::analysis_context::PAGE);
                let mut meter = crate::analysis_context::Meter::default();
                let page: Value = serde_json::from_str(&crate::analysis_context::query(
                    snapshot,
                    &mut meter,
                    &json!({"ids":["archived-memory"]}),
                ))
                .unwrap();
                let offset = page["records"][0]["next_offset"].as_u64().unwrap();
                let notice: Value = serde_json::from_str(&crate::analysis_context::query(
                    snapshot,
                    &mut meter,
                    &json!({"ids":["archived-memory"],"offset":offset}),
                ))
                .unwrap();
                assert_eq!(notice["budget_exhausted"], true);
                assert!(!meter.complete.contains("archived-memory"));
                let mut value = result("缺失原文的合成归档");
                value["archived_memory"] = json!(["不得覆盖原文"]);
                crate::ai_backend::api::attach_result(&mut value, snapshot, &meter);
                Ok(value)
            })
            .unwrap_err();
        assert!(error.contains("未完整读取原笔记"), "{error}");
        let state = app.state.lock().unwrap();
        assert_eq!(state.archive_ai_cursor, before.archive_ai_cursor);
        assert_eq!(json!(state.tasks), json!(before.tasks));
        assert_eq!(state.reports, before.reports);
        assert_eq!(json!(state.analysis_memory), json!(before.analysis_memory));
        assert_eq!(state.archive_attempts.last().unwrap().status, "failed");
        drop(state);
    }

    #[test]
    fn exhausted_retrieval_keeps_prior_batches_when_later_memory_is_stale() {
        let (_root, app) = archive_app();
        batch_fixture(&app, 10_000, 40);
        let mut calls = 0;
        let mut committed_cursor = 0;
        let mut committed_tasks = Value::Null;
        let error = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |request, _| {
                calls += 1;
                let value = exhausted_result(request, json!(["本批合成记忆"]));
                if calls == 2 {
                    let state = app.state.lock().unwrap();
                    committed_cursor = state.archive_ai_cursor.unwrap();
                    committed_tasks = json!(state.tasks);
                    assert!(committed_cursor > 0 && committed_cursor < 40);
                    // The first commit must not archive tasks whose latest
                    // evidence is still in this unprocessed tail.
                    assert!(state.tasks.iter().any(|task| task.archived));
                    assert!(state.tasks.iter().any(|task| !task.archived));
                    let revision = state.analysis_memory.archived_version;
                    drop(state);
                    app.memory_save(&json!({
                        "kind":"archived","expected":revision,"text":"人工最新记忆"
                    }))?;
                }
                Ok(value)
            })
            .unwrap_err();
        assert!(error.contains("人工编辑"), "{error}");
        assert_eq!(calls, 2);
        let state = app.state.lock().unwrap();
        let attempt = state.archive_attempts.last().unwrap();
        assert_eq!(attempt.status, "failed");
        assert_eq!(attempt.batches, 1);
        assert!(attempt.remaining > 0);
        assert_eq!(state.archive_ai_cursor, Some(committed_cursor));
        assert_eq!(json!(state.tasks), committed_tasks);
        assert_eq!(state.reports.len(), 1);
        assert_eq!(state.reports[0]["archive_complete"], false);
        assert_eq!(state.analysis_memory.archived_text(), "人工最新记忆");
        drop(state);
    }

    #[test]
    fn later_batch_failure_cancellation_and_total_timeout_preserve_prior_commit() {
        for stop in ["failure", "cancel", "timeout"] {
            let (_root, app) = archive_app();
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
        }
    }

    #[test]
    fn save_failure_leaves_every_business_effect_unchanged() {
        let (_root, app) = archive_app();
        seed(&app, "明确说明只做一次");
        let candidate = manual_candidate(&app);
        assert!(!candidate.evidence_ids.is_empty());
        // A final save failure must not apply the candidate in memory.
        let before_save = json!(*app.state.lock().unwrap());
        crate::test_support::persistence_failure(&app.root, true).unwrap();
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
        crate::test_support::persistence_failure(&app.root, false).unwrap();
        drop(after);
    }
    #[test]
    fn successful_commit_sets_success_cursor_and_effects_in_one_save() {
        let (root, app) = archive_app();
        seed(&app, "普通确认");
        let candidate = manual_candidate(&app);
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
                .find(|a| a.id == candidate.attempt_id)
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
    }
    #[test]
    fn restart_before_commit_fails_without_replay_and_success_survives() {
        let (root, app) = archive_app();
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
        let candidate = manual_candidate(&app);
        app.commit(candidate, result("成功"), now()).unwrap();
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_attempts.last().unwrap().status, "succeeded");
        drop(s);
    }
    #[test]
    fn no_new_input_ignores_ai_budget_and_reuses_report_or_fallback() {
        let (_root, app) = crate::test_support::scoped_app("shishi-archive");
        {
            let mut state = app.state.lock().unwrap();
            state.config.model_profiles = None;
            state.config.max_input_chars = 10_000;
            state.config.report_prompt = "Synthetic report policy. ".repeat(1000);
            state.analysis_memory.temporary = "合成临时记忆保留".into();
        }
        app.test_force_working();
        let empty = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |_, _| {
                panic!("empty archive must never invoke a model")
            })
            .unwrap();
        assert_eq!(empty["summary_status"], "fallback");
        assert_eq!(app.state.lock().unwrap().calls, 0);
        {
            let mut s = app.state.lock().unwrap();
            s.reports[0]["summary_status"] = json!("done");
            s.reports[0]["ai_summary"] =
                json!({"summary":"上次报表","completed":[],"attention":[],"next_steps":[]});
        }
        let reused = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |_, _| {
                panic!("reused archive must never invoke a model")
            })
            .unwrap();
        assert_eq!(reused["summary_status"], "reused");
        assert_eq!(
            reused["reused_from"],
            app.state.lock().unwrap().reports[1]["id"]
        );
        assert_eq!(app.state.lock().unwrap().calls, 0);
        assert_eq!(
            app.state.lock().unwrap().analysis_memory.temporary,
            "合成临时记忆保留"
        );
        assert!(app
            .state
            .lock()
            .unwrap()
            .archive_attempts
            .iter()
            .all(|a| a.status != "running"));
    }
    #[test]
    fn new_archive_evidence_still_requires_a_request_within_budget() {
        let (_root, app) = archive_app();
        batch_fixture(&app, 10_000, 1);
        app.state.lock().unwrap().config.report_prompt = "Synthetic report policy. ".repeat(1000);
        let error = app
            .run_archive_with("manual_archive", AiMode::Manual, None, &mut |_, _| {
                panic!("over-budget archive must never invoke a model")
            })
            .unwrap_err();
        assert!(error.contains("超过单次输入上限"), "{error}");
        let state = app.state.lock().unwrap();
        assert_eq!(state.archive_ai_cursor, Some(0));
        assert!(!state.tasks[0].archived);
        assert!(state.reports.is_empty());
        assert_eq!(state.calls, 0);
        assert_eq!(state.archive_attempts.last().unwrap().status, "failed");
        drop(state);
    }
    #[test]
    fn empty_archive_cancelled_during_preparation_has_no_business_commit() {
        let (_root, app) = archive_app();
        let before_cursor = app.state.lock().unwrap().archive_ai_cursor;
        std::thread::scope(|scope| {
            let snapshots = app.prepare_gate.lock().unwrap();
            let worker = scope.spawn(|| {
                app.run_archive_with("manual_archive", AiMode::Manual, None, &mut |_, _| {
                    panic!("empty archive must never invoke a model")
                })
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let request = loop {
                if let Some(id) = app.active_ai.lock().unwrap().as_ref().map(|a| a.id.clone()) {
                    break Some(id);
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let interrupted = request.map(|request| app.interrupt_ai(&request));
            drop(snapshots);
            assert!(interrupted.is_some_and(|result| result.is_ok()));
            assert!(worker.join().unwrap().unwrap_err().contains("中断"));
        });
        let state = app.state.lock().unwrap();
        assert!(state.reports.is_empty());
        assert_eq!(state.archive_ai_cursor, before_cursor);
        assert_eq!(state.calls, 0);
        assert_eq!(state.archive_attempts.last().unwrap().status, "failed");
        drop(state);
    }
    #[test]
    fn manual_archive_with_its_own_attempt_reaches_quota_outside_work_hours() {
        let (_root, app) = archive_app();
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
    }
    #[test]
    fn rollback_creates_a_new_group_without_rewinding_cursor_or_task_state() {
        let (_root, app) = archive_app();
        app.test_force_working();
        seed(&app, "整组回滚依据");
        let candidate = manual_candidate(&app);
        let mut value = result("完成");
        with_memory(&candidate, &mut value, "核对环境");
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
    }
    #[test]
    fn queued_archive_needs_the_matching_claim_and_failures_are_not_retried() {
        let (_root, app) = archive_app();
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
                change_summary: String::new(),
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
    }
    #[test]
    fn archive_only_maintains_archived_memory_even_with_legacy_auto_flags_and_outputs() {
        for enabled in [false, true] {
            let (root, app) = archive_app();
            seed(&app, "以后优先核对原始证据");
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
            let before = app.state.lock().unwrap().clone();
            let mut value = result("本批归档完成");
            with_memory(&candidate, &mut value, "优先核对原始证据");
            // Simulate an obsolete/unsolicited result reaching the commit boundary.
            value["review"] = json!({"improved":true,"proposed_prompt":"不应采用的新提示词"});
            value["suggestions"] = json!([{"title":"不应启用的规则","rule":"不应写入"}]);
            value["tag_changes"] =
                json!([{"action":"remove_group","tag":{"kind":"事项类型","value":""}}]);
            value["advice_decisions"] = json!([{"kind":"prompt","action":"enable"}]);
            value["handling_review"] = json!([{"improved":true,"policy":{"prompt":"不应采用"}}]);
            value["memory_update"] = json!(["不应覆盖临时记忆"]);
            crate::test_support::persistence_failure(&root, true).unwrap();
            assert!(app.commit(candidate.clone(), value.clone(), now()).is_err());
            assert_eq!(
                json!(app.state.lock().unwrap().analysis_memory),
                json!(before.analysis_memory)
            );
            crate::test_support::persistence_failure(&root, false).unwrap();
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
        }
    }
    #[test]
    fn note_only_archive_advances_shared_cursor_and_archives_after_success() {
        let (_root, app) = archive_app();
        seed(&app, "只有新增备注也构成新材料");
        let candidate = manual_candidate(&app);
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
    }
    #[test]
    fn archive_candidate_freezes_snapshot_and_version_checks_memory_and_task_edits() {
        let (_root, app) = archive_app();
        let task = seed(&app, "并发修改依据");
        {
            let mut s = app.state.lock().unwrap();
            s.analysis_memory.temporary = "候选时记忆".into();
            s.analysis_memory.temporary_version = 3;
            app.save(&s).unwrap();
        }
        let candidate = manual_candidate(&app);
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
                    private: true,
                    ..crate::test_support::message("late", "候选之后的新消息", now() + 1)
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
    }

    #[test]
    fn partial_archive_waits_for_latest_event_and_keeps_unprocessed_memory() {
        let (_root, app) = archive_app();
        batch_fixture(&app, 80_000, 80);
        {
            let mut s = app.state.lock().unwrap();
            s.analysis_memory.temporary_version = 1;
            let task = Task {
                id: "t".into(),
                title: "先有进展后有完成".into(),
                description: "核对".into(),
                status: "已完成".into(),
                updated: 2,
                created: 1,
                ..Default::default()
            };
            s.audit.insert(
                0,
                Audit {
                    time: 1,
                    kind: "ai_update".into(),
                    task_id: task.id.clone(),
                    detail: "早期进展".into(),
                    before: json!([task]),
                    after: json!([task]),
                    ..Default::default()
                },
            );
            s.audit.push(Audit {
                time: 3,
                kind: "manual_archive".into(),
                task_id: task.id.clone(),
                detail: "最终完成与备注".into(),
                before: json!([task]),
                after: json!([{"id":"t","archive_note":"最终才出现的备注","status":"已完成"}]),
                ..Default::default()
            });
            s.tasks.push(task);
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
                true,
                crate::context_budget::pending_refs(&s, false).count(),
            )
        };
        assert!(candidate.remaining_events > 0);
        assert!(candidate.cutoff < 80);
        assert!(!candidate.tasks_to_archive.is_empty());
        assert!(candidate.tasks_to_archive.len() < 80);
        assert!(candidate.tasks_to_archive.iter().all(|task| task.id != "t"));
        let memory = {
            let s = app.state.lock().unwrap();
            let covered = candidate
                .evidence_ids
                .iter()
                .map(|i| s.audit[*i].task_id.as_str())
                .collect::<BTreeSet<_>>();
            assert!(covered.contains("t"));
            assert!(candidate
                .tasks_to_archive
                .iter()
                .all(|task| covered.contains(task.id.as_str())));
            s.analysis_memory.clone()
        };
        let report = app
            .commit(candidate.clone(), result("部分处理"), now())
            .unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(s.archive_ai_cursor, Some(candidate.cutoff));
        assert_eq!(report["temporary_memory_cleared"], false);
        assert_eq!(s.analysis_memory.temporary, memory.temporary);
        assert_eq!(
            s.analysis_memory.temporary_version,
            memory.temporary_version
        );
        assert!(s.tasks.iter().filter(|t| t.archived).count() < 80);
        assert!(!s.tasks.iter().find(|t| t.id == "t").unwrap().archived);
    }

    #[test]
    fn rollback_conflicts_on_manual_prompt_rule_tag_or_memory_changes() {
        fn group_app() -> (crate::test_support::TestRoot, App, String) {
            let (root, app) = archive_app();
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
            let candidate = manual_candidate(&app);
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
            (root, app, group)
        }
        {
            let (_root, app, group) = group_app();
            app.state.lock().unwrap().config.classify_prompt = "手动新提示词".into();
            app.save(&app.state.lock().unwrap()).unwrap();
            let error = app.archive_rollback(&group, &group).unwrap_err();
            assert!(error.contains("手动修改"), "{error}");
            assert_eq!(
                app.state.lock().unwrap().config.classify_prompt,
                "手动新提示词"
            );
        }
        {
            let (_root, app, group) = group_app();
            app.state.lock().unwrap().suggestions[0].rule = "手动改规则".into();
            app.save(&app.state.lock().unwrap()).unwrap();
            let error = app.archive_rollback(&group, &group).unwrap_err();
            assert!(error.contains("手动修改"), "{error}");
            assert_eq!(app.state.lock().unwrap().suggestions[0].rule, "手动改规则");
        }
        {
            let (_root, app, group) = group_app();
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
        }
        {
            let (_root, app, group) = group_app();
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
        }
    }

    #[test]
    fn archive_groups_store_only_active_rules_and_current_tags_not_full_history() {
        let (_root, _app, group) = {
            let (root, app) = archive_app();
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
            let candidate = manual_candidate(&app);
            app.commit(candidate, result("完成"), now()).unwrap();
            let group = app
                .state
                .lock()
                .unwrap()
                .archive_versions
                .last()
                .cloned()
                .unwrap();
            (root, app, group)
        };
        let serialized = serde_json::to_value(&group).unwrap();
        assert!(serialized.get("suggestions").is_none());
        assert!(serialized.get("tag_state").is_none());
        assert_eq!(group.active_rules.len(), 1);
        assert_eq!(group.active_rules[0].id, "active");
        assert!(!group.tag_groups.is_empty());
        assert!(!group.tag_version.is_empty());
    }

    #[test]
    fn derived_report_file_failure_after_atomic_commit_is_a_success_with_diagnostic() {
        let (_root, app) = archive_app();
        seed(&app, "报表文件失败依据");
        let candidate = manual_candidate(&app);
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
    }
}

#[cfg(test)]
#[path = "archive/memory_tests.rs"]
mod memory_tests;
