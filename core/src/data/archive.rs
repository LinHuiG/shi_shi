//! Archive data: attempts, morning jobs, the evidence an archive batch
//! reads and the batch commits. Reports, archive groups with their
//! rollback and retention live in the submodules.

use std::collections::BTreeMap;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::{audit, kv, rows, Db};
use crate::archive::run::{Batch, Material, Page, PAGE};
use crate::archive::{
    new_id, Attempt, Audit, MorningJob, ARCHIVE_CURSOR, ATTEMPT_DAY, LEARNING_CURSOR, MORNING_RECOVERY_ERROR,
    RECOVERY_ERROR,
};
use crate::config::Config;
use crate::prompts::memory;
use crate::state::Scope;
use crate::tasks::model as tasks;
use crate::{time, Result};

mod groups;
mod pruning;
mod reports;

// The submodules are this module's own parts; their items read as its.
pub(crate) use groups::*;
pub(crate) use pruning::*;
pub(crate) use reports::*;

// ---- attempts and morning jobs ---------------------------------------------

pub(crate) fn attempts(conn: &Connection) -> Result<Vec<Attempt>> {
    rows::all(conn, "SELECT * FROM archive_attempts ORDER BY seq", [])
}

pub(crate) fn attempt(conn: &Connection, id: &str) -> Result<Option<Attempt>> {
    rows::one(conn, "SELECT * FROM archive_attempts WHERE id = ?1", [id])
}

pub(crate) fn last_attempt(conn: &Connection) -> Result<Option<Attempt>> {
    rows::one(conn, "SELECT * FROM archive_attempts ORDER BY seq DESC LIMIT 1", [])
}

fn first_queued(conn: &Connection) -> Result<Option<Attempt>> {
    rows::one(conn, "SELECT * FROM archive_attempts WHERE status = 'queued' ORDER BY seq LIMIT 1", [])
}

/// Insert or replace an attempt by id; a new one goes to the end.
pub(crate) fn put_attempt(conn: &Connection, a: &Attempt) -> Result<()> {
    rows::put(conn, "archive_attempts", "id", a)
}

pub(crate) fn jobs(conn: &Connection) -> Result<Vec<MorningJob>> {
    rows::all(conn, "SELECT * FROM morning_jobs ORDER BY seq", [])
}

pub(crate) fn put_job(conn: &Connection, j: &MorningJob) -> Result<()> {
    rows::put(conn, "morning_jobs", "id", j)
}

/// Queue the work-end archive in the boundary transaction. The claim's
/// attempt id is the attempt id, so a claimed day queues once.
pub(crate) fn queue_end_archive(conn: &Connection, attempt_id: &str, business_day: &str, at: i64) -> Result<bool> {
    if attempt(conn, attempt_id)?.is_some() {
        return Ok(false);
    }
    let a = Attempt {
        id: attempt_id.into(),
        trigger: "work_end".into(),
        status: "queued".into(),
        business_day: business_day.into(),
        created: at,
        ..Default::default()
    };
    put_attempt(conn, &a)?;
    Ok(true)
}

/// Queue a morning report in the work-boundary transaction. The claim id is
/// the job id, so a claimed day queues once.
pub(crate) fn queue_morning(conn: &Connection, id: &str, business_day: &str, at: i64) -> Result<()> {
    if conn.prepare_cached("SELECT 1 FROM morning_jobs WHERE id = ?1")?.exists([id])? {
        return Ok(());
    }
    put_job(
        conn,
        &MorningJob {
            id: id.into(),
            business_day: business_day.into(),
            status: "queued".into(),
            created: at,
            ..Default::default()
        },
    )
}

// ---- shared reads ----------------------------------------------------------

/// Task evidence as a condition on `task_audit`: the archive kinds plus
/// every kind [`rules::learning_evidence`] can accept.
const EVIDENCE: &str = "task_audit.task_id <> '' AND task_audit.kind IN ('ai_create', 'ai_update', 'ai_evidence', \
    'ai_merge_duplicate', 'manual_status', 'manual_archive', 'generation_done', 'archive_item', 'deadline_update', \
    'manual_edit', 'manual_ai_edit', 'undo', 'merge')";

/// Audit rows matching `tail`, the SQL after `WHERE`.
pub(super) fn audits(conn: &Connection, tail: &str) -> Result<Vec<Audit>> {
    let sql =
        format!("SELECT seq, time, kind, task_id, detail, change_summary, before, after FROM task_audit WHERE {tail}");
    let mut stmt = conn.prepare_cached(&sql)?;
    let parse = |text: Option<String>| text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let rows = stmt.query_map([], |r| {
        Ok(Audit {
            seq: r.get(0)?,
            time: r.get(1)?,
            kind: r.get(2)?,
            task_id: r.get(3)?,
            detail: r.get(4)?,
            change_summary: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            before: parse(r.get(6)?),
            after: parse(r.get(7)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn max_seq(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(seq), 0) FROM task_audit", [], |r| r.get(0))?)
}

/// The start of the last successful archive, less a second: tasks edited
/// after it stay in scope even when archived.
fn scope_since(conn: &Connection) -> Result<i64> {
    let sql = "SELECT coalesce(max(CASE WHEN started > 0 THEN started ELSE finished END), 0) FROM archive_attempts WHERE status = 'succeeded'";
    Ok((conn.query_row(sql, [], |r| r.get::<_, i64>(0))? - 1).max(0))
}

/// Tasks archiving and reports still look at, as a condition on `tasks`:
/// unmerged, not generating, and either open or edited after `since` by
/// something other than the archive itself.
fn scope_sql(since: i64) -> String {
    format!(
        "tasks.merged_into = '' AND tasks.generation = '' AND (tasks.archived = 0 OR tasks.updated > {since}
         AND (SELECT coalesce(json_extract(x.after, '$.updated'), 0) FROM task_audit x
              WHERE x.task_id = tasks.id AND x.kind = 'archive_item' ORDER BY x.seq DESC LIMIT 1) IS NOT tasks.updated)"
    )
}

/// The tasks of [`scope_sql`], by number.
#[cfg(test)]
pub(crate) fn task_scope(conn: &Connection) -> Result<Vec<tasks::Task>> {
    super::tasks::list(conn, &scope_sql(scope_since(conn)?))
}

fn in_scope(since: i64) -> String {
    format!("EXISTS (SELECT 1 FROM tasks WHERE tasks.id = task_audit.task_id AND {})", scope_sql(since))
}

/// Unarchived tasks by status.
fn status_counts(conn: &Connection) -> Result<Value> {
    let sql = "SELECT json_group_object(status, n) FROM (SELECT status, count(*) AS n FROM tasks WHERE archived = 0 GROUP BY status)";
    Ok(serde_json::from_str(&conn.query_row(sql, [], |r| r.get::<_, String>(0))?)?)
}

// ---- archive batches -------------------------------------------------------

/// One page of evidence after `after` with the tasks it links.
fn page(conn: &Connection, cursor: i64, since: i64, after: i64, limit: usize) -> Result<Page> {
    let scope = in_scope(since);
    let rows = audits(conn, &format!("seq > {after} AND {EVIDENCE} AND {scope} ORDER BY seq LIMIT {limit}"))?;
    let linked = match rows.last() {
        Some(last) => {
            let ids =
                format!("SELECT task_id FROM task_audit WHERE seq > {after} AND seq <= {} AND {EVIDENCE}", last.seq);
            super::tasks::list(conn, &format!("id IN ({ids}) AND {}", scope_sql(since)))?
        }
        None => Vec::new(),
    };
    let total = conn.query_row(
        &format!("SELECT count(*) FROM task_audit WHERE seq > {cursor} AND {EVIDENCE} AND {scope}"),
        [],
        |r| r.get(0),
    )?;
    Ok(Page { exhausted: rows.len() < limit, rows, linked, total, max_seq: max_seq(conn)? })
}

/// The condition on `tasks` for tasks in scope without evidence after the
/// cursor up to the read `max_seq`.
fn unrelated(since: i64, cursor: i64, max_seq: i64) -> String {
    format!(
        "{} AND NOT EXISTS (SELECT 1 FROM task_audit WHERE task_audit.task_id = tasks.id AND seq > {cursor} AND seq <= {max_seq} AND {EVIDENCE})",
        scope_sql(since)
    )
}

/// The base of an archive batch and its first evidence page.
pub(crate) fn load(conn: &Connection) -> Result<Material> {
    let cursor = kv::get_i64(conn, ARCHIVE_CURSOR)?;
    let since = scope_since(conn)?;
    let closed: Vec<String> = tasks::CLOSED.iter().map(|s| format!("'{s}'")).collect();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, updated, (SELECT max(seq) FROM task_audit WHERE task_audit.task_id = tasks.id AND seq > ?1 AND {EVIDENCE})
         FROM tasks WHERE archived = 0 AND merged_into = '' AND generation = '' AND status IN ({}) ORDER BY number",
        closed.join(", ")
    ))?;
    let closed =
        stmt.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut m = Material {
        cursor,
        max_seq: 0,
        since,
        evidence: Vec::new(),
        exhausted: false,
        total: 0,
        linked: BTreeMap::new(),
        current: Vec::new(),
        omitted: 0,
        closed,
        status_counts: status_counts(conn)?,
        tag_catalog: json!(super::tags::catalog(conn)?.0),
        memory: super::prompts::memory(conn, "archived")?,
        temporary: super::prompts::memory(conn, "temporary")?,
        data_until: kv::get_i64(conn, "last_scan")?,
        pending: super::analysis::unanalyzed(conn)?,
        report_policy: super::prompts::current(conn, "report")?.map(|v| v.content).unwrap_or_default(),
    };
    m.add(page(conn, cursor, since, cursor, PAGE)?);
    m.omitted = conn.query_row(
        &format!("SELECT count(*) FROM tasks WHERE {}", unrelated(since, cursor, m.max_seq)),
        [],
        |r| r.get(0),
    )?;
    Ok(m)
}

fn conflict(conn: &Connection, b: &Batch, kind: &str, detail: Value) -> Result<()> {
    super::activity::log(conn, "archive_conflict", &json!({"kind": kind, "attempt_id": b.attempt_id, "detail": detail}))
        .map(drop)
}

/// Archive the closed tasks of the batch that nobody changed since it was
/// built, then move the cursors. Returns the archived ids; changed tasks are
/// logged and left.
fn finalize(conn: &Connection, b: &Batch, at: i64) -> Result<Vec<String>> {
    let (mut archived, mut skipped) = (Vec::new(), Vec::new());
    for (id, updated) in &b.tasks_to_archive {
        let Some(mut t) = super::tasks::get(conn, id)? else { continue };
        if t.updated != *updated || t.archived {
            skipped.push(id.clone());
            continue;
        }
        let before = t.payload();
        t.archived = true;
        t.archived_at = at;
        t.updated = at.max(t.updated + 1);
        let t = super::tasks::store(conn, &t)?;
        audit::write(conn, "archive_item", id, "归档成功后归档已结束事项", &before, &t.payload())?;
        archived.push(id.clone());
    }
    if !skipped.is_empty() {
        conflict(conn, b, "tasks", json!(skipped))?;
    }
    kv::set(conn, ARCHIVE_CURSOR, &b.cutoff)?;
    let learning = kv::get_i64(conn, LEARNING_CURSOR)?.max(b.cutoff);
    kv::set(conn, LEARNING_CURSOR, &learning)?;
    let pending = super::prompts::learning_pending(conn, learning)?;
    kv::set(conn, super::tasks::PENDING_LEARNING, &pending)?;
    kv::set(conn, ATTEMPT_DAY, &b.business_day)?;
    Ok(archived)
}

fn record_batch(conn: &Connection, b: &Batch, archived: usize, report_id: &str, at: i64) -> Result<Attempt> {
    let mut a = attempt(conn, &b.attempt_id)?.ok_or("归档 attempt 不存在")?;
    a.batches += i64::from(b.has_ai_input());
    a.processed += b.evidence.len() as i64;
    a.remaining = b.remaining;
    a.archived_tasks += archived as i64;
    a.status = if b.remaining == 0 { "succeeded" } else { "running" }.into();
    a.finished = if b.remaining == 0 { next_finished(conn, at)? } else { 0 };
    a.error.clear();
    a.report_id = report_id.into();
    put_attempt(conn, &a)?;
    Ok(a)
}

/// Apply the archived-memory part of a result. `Err` texts reject the
/// whole batch; a memory edited while the model ran is skipped and logged,
/// and `None` tells the caller its merge did not land.
fn apply_memory(conn: &Connection, b: &Batch, result: &Value) -> Result<std::result::Result<Option<usize>, String>> {
    if let Some(error) = result["_archived_memory_error"].as_str() {
        return Ok(Err(format!("归档记忆未保存：{error}")));
    }
    let Some(update) = result.get("_archived_memory_update").filter(|u| !u.is_null()) else {
        if result["archived_memory"].as_array().is_some_and(|r| !r.is_empty()) {
            return Ok(Err("归档记忆缺少完整读取校验，本批未提交".into()));
        }
        return Ok(Ok(Some(0)));
    };
    if update["expected"].as_i64() != Some(b.memory_revision)
        || update["before"].as_str() != Some(b.memory_text.as_str())
    {
        return Ok(Err("归档记忆已被人工编辑，本批未提交，请核对后重新归档".into()));
    }
    let Some(text) = update["text"].as_str() else { return Ok(Err("归档记忆正文无效".into())) };
    if text.chars().count() > memory::ARCHIVED_LIMIT {
        return Ok(Err("归档记忆超过4000字符，保留原记忆".into()));
    }
    let current = super::prompts::memory(conn, "archived")?;
    if current.revision != b.memory_revision {
        conflict(conn, b, "memory", json!({"expected": b.memory_revision, "current": current.revision}))?;
        return Ok(Ok(None));
    }
    if text == current.text || !super::prompts::replace_memory(conn, "archived", b.memory_revision, text)? {
        return Ok(Ok(Some(0)));
    }
    let after = json!({"revision": b.memory_revision + 1, "chars": text.chars().count()});
    audit::write(conn, "archive_memory", "", "归档更新记忆正文", &Value::Null, &after)?;
    Ok(Ok(Some(1)))
}

/// Empty the temporary memory the batch merged; one written while the model
/// ran is kept and logged.
fn clear_temporary(conn: &Connection, b: &Batch) -> Result<bool> {
    if b.temporary_text.trim().is_empty() {
        return Ok(false);
    }
    if !super::prompts::replace_memory(conn, "temporary", b.temporary_revision, "")? {
        let current = super::prompts::memory(conn, "temporary")?.revision;
        conflict(conn, b, "temporary_memory", json!({"expected": b.temporary_revision, "current": current}))?;
        return Ok(false);
    }
    Ok(true)
}

/// Store a report, trim the history and log the commit in the audit.
fn finish_batch(
    conn: &Connection,
    report: &Value,
    kind: &str,
    detail: &str,
    before: Value,
    after: Value,
) -> Result<()> {
    put_report(conn, report)?;
    truncate(conn)?;
    audit::write(conn, kind, "", detail, &before, &after).map(drop)
}

/// Commit one batch with its model result in one transaction. The inner
/// `Err` rejects the result before anything is written.
fn commit(conn: &Connection, b: &Batch, result: &Value, at: i64) -> Result<std::result::Result<Value, String>> {
    let (memory_changes, cleared) = match apply_memory(conn, b, result)? {
        Ok(Some(n)) => (n, clear_temporary(conn, b)?),
        // The temporary memory stays for the next archive to merge.
        Ok(None) => (0, false),
        Err(e) => return Ok(Err(e)),
    };
    let archived = finalize(conn, b, at)?;
    super::tools::link_tool_calls(conn, result, &archived)?;
    let summary = result["report"].clone();
    let same = bodies(conn, "WHERE body ->> '$.attempt_id' = ?1 ORDER BY seq DESC LIMIT 1", [&b.attempt_id])?;
    let mut summaries = match same.into_iter().next() {
        Some(r) => section_rows(conn, &r, "batch_summaries", 0, usize::MAX)?,
        None => Vec::new(),
    };
    summaries.push(summary.clone());
    let mut report = b.report.clone();
    let suggestions: Vec<Value> = super::rules::list(conn)?.into_iter().filter(|s| s["archived_at"] == 0).collect();
    for (key, value) in [
        ("ai_summary", summary),
        ("batch_summaries", json!(summaries)),
        ("summary_status", json!("done")),
        ("suggestions", json!(suggestions)),
        ("learning", json!({"added": 0, "updated": 0, "message": "归档仅维护归档记忆，合并后清空临时记忆；系统提示词保留"})),
        ("temporary_memory_cleared", json!(cleared)),
        ("archived_tasks", json!(archived.len())),
        ("archive_complete", json!(b.remaining == 0)),
        ("skipped_tasks", json!(b.tasks_to_archive.len().saturating_sub(archived.len()))),
        ("attempt_id", json!(b.attempt_id)),
        ("memory_changes", json!(memory_changes)),
        ("remaining_events", json!(b.remaining)),
    ] {
        report[key] = value;
    }
    let report_id = report["id"].as_str().unwrap_or("").to_string();
    record_batch(conn, b, archived.len(), &report_id, at)?;
    let group = Group {
        id: new_id(),
        created: at,
        action: "archive".into(),
        attempt_id: b.attempt_id.clone(),
        report_id: report_id.clone(),
        reason: "归档成功：保存私有归档记忆版本，保留提示词、规则与标签快照".into(),
        ..snapshot(conn)?
    };
    rows::put(conn, "archive_groups", "id", &group)?;
    report["archive_group_id"] = json!(group.id);
    finish_batch(
        conn,
        &report,
        "archive_commit",
        "归档结果、成功状态、游标、事项归档标记、归档记忆和临时记忆清空同次保存",
        json!({"attempt": b.attempt_id, "cutoff": b.cutoff}),
        json!({"report": report_id, "group": group.id, "archived_tasks": archived.len()}),
    )?;
    let log = json!({"kind": "archive", "trigger": b.trigger, "processed": b.evidence.len(), "remaining": b.remaining,
                     "report": report_id, "attempt_id": b.attempt_id});
    super::activity::log(conn, "archive_result", &log)?;
    Ok(Ok(report))
}

/// Commit a batch that needs no model call: its report reuses the previous
/// AI summary when there is one, otherwise it is an empty fallback.
fn commit_without_ai(conn: &Connection, b: &Batch, at: i64) -> Result<Value> {
    let mut report = b.report.clone();
    report["time"] = json!(at);
    report["id"] = json!(new_id());
    report["memory_changes"] = json!(0);
    match first_report(conn)?.filter(|old| old["ai_summary"].is_object() && old["summary_status"] != "fallback") {
        Some(prev) => {
            report["ai_summary"] = prev["ai_summary"].clone();
            report["summary_status"] = json!("reused");
            report["reused_from"] = prev["id"].clone();
        }
        None => {
            report["ai_summary"] = json!({"summary": "没有可归档的新材料，且尚无上次报表可沿用。", "completed": [], "attention": [], "next_steps": []});
            report["summary_status"] = json!("fallback");
        }
    }
    let archived = finalize(conn, b, at)?;
    report["archived_tasks"] = json!(archived.len());
    report["archive_complete"] = json!(true);
    report["attempt_id"] = json!(b.attempt_id);
    let report_id = report["id"].as_str().unwrap_or("").to_string();
    let mut a = record_batch(conn, b, archived.len(), &report_id, at)?;
    a.status = "succeeded".into();
    a.reused = report["summary_status"] == "reused";
    put_attempt(conn, &a)?;
    let after = json!({"report": report_id, "status": report["summary_status"]});
    finish_batch(
        conn,
        &report,
        "archive_reuse",
        "无新增材料，不调用 AI；沿用报表与成功状态同次保存",
        Value::Null,
        after,
    )?;
    let log = json!({"kind": "archive", "trigger": b.trigger, "processed": 0, "reused": true, "report": report_id});
    super::activity::log(conn, "archive_result", &log)?;
    Ok(report)
}

/// Mark an attempt failed; only configuration errors stay out of the log.
fn fail(conn: &Connection, id: &str, error: &str) -> Result<()> {
    if let Some(mut a) = attempt(conn, id)? {
        a.status = "failed".into();
        a.finished = next_finished(conn, time::now())?;
        a.error = error.into();
        put_attempt(conn, &a)?;
    }
    if !error.starts_with("AI_UNCONFIGURED:") {
        super::activity::log(conn, "archive_failed", &json!({"kind": "archive", "attempt_id": id, "error": error}))?;
    }
    Ok(())
}

fn start(conn: &Connection, trigger: &str, business_day: &str, at: i64, existing: Option<Attempt>) -> Result<Attempt> {
    let fresh = || Attempt {
        id: new_id(),
        trigger: trigger.into(),
        business_day: business_day.into(),
        created: at,
        ..Default::default()
    };
    let a = Attempt {
        status: "running".into(),
        started: at,
        finished: 0,
        error: String::new(),
        ..existing.unwrap_or_else(fresh)
    };
    put_attempt(conn, &a)?;
    Ok(a)
}

// ---- startup and snapshot --------------------------------------------------

/// Startup recovery, in the caller's transaction: unfinished attempts and
/// morning jobs of a previous process fail; nothing is replayed.
pub(crate) fn recover(conn: &Connection) -> Result<()> {
    let at = next_finished(conn, time::now())?;
    for (table, error) in [("archive_attempts", RECOVERY_ERROR), ("morning_jobs", MORNING_RECOVERY_ERROR)] {
        let sql = format!(
            "UPDATE {table} SET status = 'failed', error = ?1, finished = ?2 WHERE status IN ('queued', 'running')"
        );
        conn.execute(&sql, params![error, at])?;
    }
    Ok(())
}

/// Snapshot keys of the archive domain.
pub(crate) fn part(conn: &Connection, scope: &Scope, out: &mut Map<String, Value>) -> Result<()> {
    out.insert("archive_attempt".into(), json!(last_attempt(conn)?));
    out.insert("morning_jobs".into(), json!(jobs(conn)?));
    out.insert("report_outcome".into(), json!(outcome(conn)?));
    if scope.page == "reports" {
        out.insert("reports_revision".into(), reports_revision(conn)?.into());
        out.insert("reports".into(), catalog(conn)?.into());
    }
    if ["prompts", "learning"].contains(&scope.page.as_str()) {
        let count: i64 = conn.query_row("SELECT count(*) FROM advice_revisions", [], |r| r.get(0))?;
        out.entry("evolution").or_insert_with(|| json!({}))["revision_count"] = count.into();
    }
    Ok(())
}

// ---- business operations ---------------------------------------------------

impl Db {
    /// Whether a work-end archive waits in the queue.
    pub async fn first_queued_archive(&self) -> Result<Option<Attempt>> {
        self.read(first_queued).await
    }

    pub async fn archive_attempt(&self, id: String) -> Result<Option<Attempt>> {
        self.read(move |c| attempt(c, &id)).await
    }

    pub async fn archive_history(&self) -> Result<Value> {
        self.read(history).await
    }

    pub async fn archive_version(&self, id: String) -> Result<Value> {
        self.read(move |c| version(c, &id)).await
    }

    pub async fn rollback_archive(&self, id: String, expected: String) -> Result<Value> {
        self.write(move |tx| rollback(tx, &id, &expected)).await
    }

    pub async fn report_detail(&self, p: Value) -> Result<Value> {
        self.read(move |c| detail(c, &p)).await
    }

    /// Hourly retention of what archiving has processed.
    pub async fn prune_archive(&self, days: u32, at: i64) -> Result<bool> {
        self.write(move |tx| prune(tx, days, at)).await
    }

    /// The base of an archive batch with its first evidence page.
    pub async fn archive_material(&self) -> Result<Material> {
        self.read(load).await
    }

    /// The next evidence page after `after`.
    pub async fn archive_page(&self, cursor: i64, since: i64, after: i64, limit: usize) -> Result<Page> {
        self.read(move |c| page(c, cursor, since, after, limit)).await
    }

    /// Up to `limit` tasks in scope without evidence after the cursor up to
    /// `max_seq`, numbered after `after`.
    pub async fn archive_current_tasks(
        &self,
        since: i64,
        cursor: i64,
        max_seq: i64,
        after: i64,
        limit: usize,
    ) -> Result<Vec<tasks::Task>> {
        let filter =
            format!("{} AND number > {after} ORDER BY number LIMIT {limit}", unrelated(since, cursor, max_seq));
        self.read(move |c| super::tasks::select(c, &filter, [])).await
    }

    /// The check of a queued work-end attempt against its persisted claim
    /// and the work gate; the inner error is the text the attempt fails with.
    pub async fn check_archive_claim(
        &self,
        id: String,
        config: std::sync::Arc<Config>,
        at: i64,
    ) -> Result<std::result::Result<(), String>> {
        self.read(move |c| {
            Ok(match attempt(c, &id)? {
                Some(live) if live.unfinished() => super::ai::claim_check(c, &config, &live, at),
                _ => Err("归档 attempt 已结束，不能自动续跑".to_string()),
            })
        })
        .await
    }

    pub async fn start_archive(
        &self,
        trigger: String,
        day: String,
        at: i64,
        existing: Option<Attempt>,
    ) -> Result<Attempt> {
        self.write(move |tx| start(tx, &trigger, &day, at, existing)).await
    }

    pub async fn fail_archive(&self, id: String, error: String) -> Result<()> {
        self.write(move |tx| fail(tx, &id, &error)).await
    }

    pub async fn set_archive_remaining(&self, id: String, remaining: i64) -> Result<()> {
        self.write(move |tx| {
            Ok(tx
                .execute("UPDATE archive_attempts SET remaining = ?2 WHERE id = ?1", params![id, remaining])
                .map(drop)?)
        })
        .await
    }

    pub async fn commit_archive_batch(
        &self,
        batch: Batch,
        result: Value,
        at: i64,
    ) -> Result<std::result::Result<Value, String>> {
        self.write(move |tx| commit(tx, &batch, &result, at)).await
    }

    pub async fn commit_archive_without_ai(&self, batch: Batch, at: i64) -> Result<Value> {
        self.write(move |tx| commit_without_ai(tx, &batch, at)).await
    }

    pub async fn prepare_report(&self, label: String, at: i64) -> Result<Value> {
        self.write(move |tx| prepare(tx, &label, at)).await
    }

    /// Transaction B of a report: the finished report replaces its pending
    /// row, or comes back as the newest when concurrent reports displaced it.
    /// Only a summary moves the report cursor past its delta.
    pub async fn finish_report(&self, mut r: Value) -> Result<Value> {
        self.write(move |tx| {
            if r["summary_status"] == "done" {
                kv::set(tx, crate::archive::REPORT_CURSOR, &r["audit_until"])?;
            }
            r["finished"] = json!(next_finished(tx, time::now())?);
            put_report(tx, &r)?;
            truncate(tx)?;
            Ok(r)
        })
        .await
    }

    /// Mark the first queued morning job running.
    pub async fn claim_morning_job(&self) -> Result<Option<MorningJob>> {
        self.write(|tx| {
            let sql = "SELECT * FROM morning_jobs WHERE status = 'queued' ORDER BY seq LIMIT 1";
            let Some(mut job) = rows::one::<MorningJob>(tx, sql, [])? else { return Ok(None) };
            job.status = "running".into();
            put_job(tx, &job)?;
            Ok(Some(job))
        })
        .await
    }

    /// Store a finished morning job; without a report time it finishes now.
    pub async fn finish_morning_job(&self, mut job: MorningJob, finished: i64) -> Result<MorningJob> {
        self.write(move |tx| {
            job.finished = if finished > 0 { finished } else { next_finished(tx, time::now())? };
            put_job(tx, &job)?;
            Ok(job)
        })
        .await
    }
}
