//! Prompt persistence: versions, stored rewrite suggestions, the two AI
//! memories and the learning pass over correction audit rows.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::{audit, kv};
use super::{rows, Db};
use crate::analysis::batch;
use crate::archive::run::PAGE;
use crate::archive::{Audit, LEARNING_CURSOR};
use crate::prompts::learning::{self, Batch, Evidence, Suggestion};
use crate::prompts::memory::{Memory, ARCHIVED_LIMIT, TEMPORARY_LIMIT};
use crate::prompts::{default, has_result, Review, Version, RULE_VERSION, TARGETS};
use crate::state::Scope;
use crate::tasks::model;
use crate::{time, Error, Result};

impl Db {
    /// Save `text` as the next version of `target` unless it is already in
    /// effect; `expected` is the version the editor opened.
    pub async fn save_prompt(&self, target: String, text: String, expected: Option<String>) -> Result<()> {
        self.write(move |tx| {
            if current(tx, &target)?.map(|v| v.id) != expected {
                crate::bail!("提示词已更新，请重新打开编辑");
            }
            if !TARGETS.contains(&target.as_str()) {
                crate::bail!("未知提示词类型");
            }
            if effective(tx, &target)? != text {
                record(tx, &target, &text, "manual", "手动保存系统提示词")?;
            }
            Ok(())
        })
        .await
    }

    /// One stored version of `target`.
    pub async fn prompt_version(&self, target: String, id: String) -> Result<Option<Version>> {
        self.read(move |conn| Ok(version(conn, &id)?.filter(|v| v.target == target))).await
    }

    /// The versions of `target` newest first, without their text, and the
    /// current one in full.
    pub async fn prompt_history(&self, target: String) -> Result<Value> {
        self.read(move |conn| {
            let sql = "SELECT id, target, previous_id, action, reason, created FROM prompt_versions WHERE target = ?1 ORDER BY rowid DESC";
            let versions = rows::objects(conn, sql, [&target], &json!({}))?;
            Ok(json!({"versions": versions, "current": current(conn, &target)?}))
        })
        .await
    }

    /// Adopt a ready suggestion as the next classify version; returns its id.
    pub async fn adopt_prompt_review(&self, id: String) -> Result<String> {
        self.write(move |tx| {
            let r = review(tx, &id)?.ok_or_else(|| Error::new("建议不存在"))?;
            if r.status != "ready" {
                crate::bail!("该建议尚不能采用");
            }
            if current(tx, "classify")?.map(|v| v.id) != Some(r.base_version.clone())
                || effective(tx, "classify")? != r.base_prompt
            {
                crate::bail!("当前提示词已变化，请重新生成建议，避免覆盖新版本");
            }
            let version = record(tx, "classify", &r.proposed_prompt, "adopt", &r.reason)?;
            tx.execute("UPDATE prompt_reviews SET status = 'adopted' WHERE id = ?1", [&id])?;
            kv::bump(tx, RULE_VERSION)?;
            audit::write(
                tx,
                "prompt_adopt",
                "",
                &r.reason,
                &json!({"version": r.base_version}),
                &json!({"version": version}),
            )?;
            Ok(version)
        })
        .await
    }

    /// Restore any target's earlier version as a new version; returns its id.
    pub async fn rollback_prompt(&self, id: String, expected: String) -> Result<String> {
        self.write(move |tx| {
            let old = version(tx, &id)?.ok_or_else(|| Error::new("版本不存在"))?;
            if current(tx, &old.target)?.map(|v| v.id).as_deref() != Some(expected.as_str()) {
                crate::bail!("提示词已更新，请刷新版本列表");
            }
            if old.id == expected {
                crate::bail!("当前已是此版本");
            }
            let version = record(tx, &old.target, &old.content, "rollback", &format!("回滚到 {}", old.id))?;
            kv::bump(tx, RULE_VERSION)?;
            audit::write(
                tx,
                "prompt_rollback",
                "",
                "回滚系统提示词",
                &json!({"version": expected}),
                &json!({"version": version, "restored_from": id}),
            )?;
            Ok(version)
        })
        .await
    }

    /// The suggestion to regenerate and its pending successor against the
    /// current classify prompt.
    pub async fn prompt_retry(&self, id: String) -> Result<(Review, Review)> {
        self.read(move |conn| {
            let old = review(conn, &id)?.ok_or_else(|| Error::new("建议不存在"))?;
            if old.archived_at > 0 || ["adopted", "rejected"].contains(&old.status.as_str()) {
                crate::bail!("建议已处理，请刷新后选择可重新评估的记录");
            }
            let now = time::now();
            let next = Review {
                id: uuid::Uuid::new_v4().to_string(),
                day: old.day.clone(),
                created: now,
                status: "pending".into(),
                base_version: current(conn, "classify")?.map(|v| v.id).unwrap_or_default(),
                base_prompt: effective(conn, "classify")?,
                evidence: old.evidence.clone(),
                last_attempt: now,
                ..Default::default()
            };
            Ok((old, next))
        })
        .await
    }

    /// Store an answered retry in place of `old` when neither the suggestion
    /// nor the classify prompt changed meanwhile.
    pub async fn save_prompt_retry(&self, old: Review, next: Review) -> Result<Review> {
        self.write(move |tx| {
            if review(tx, &old.id)?.as_ref() != Some(&old)
                || current(tx, "classify")?.map(|v| v.id).unwrap_or_default() != next.base_version
                || effective(tx, "classify")? != next.base_prompt
            {
                crate::bail!("建议或提示词版本已变化，本次结果未保存");
            }
            insert_review(tx, &next)?;
            tx.execute("UPDATE prompt_reviews SET archived_at = ?2 WHERE id = ?1", params![old.id, time::now()])?;
            Ok(next)
        })
        .await
    }

    /// Both memories with their limits.
    pub async fn memories(&self) -> Result<Value> {
        self.read(|conn| memories(conn)).await
    }

    pub async fn ai_accuracy(&self, since: i64) -> Result<Value> {
        self.read(move |conn| ai_accuracy(conn, since)).await
    }

    /// A manual memory save; `expected` is the revision the editor read.
    pub async fn save_memory(
        &self,
        kind: &'static str,
        name: &'static str,
        expected: i64,
        text: String,
    ) -> Result<Value> {
        self.write(move |tx| {
            if !replace_memory(tx, kind, expected, &text)? {
                crate::bail!("{name}记忆已更新，草稿已保留；请重新读取后合并");
            }
            // Record the operation, never a second copy of the private text.
            audit::write(tx, "memory_manual", "", "手动保存 AI 记忆", &Value::Null, &json!({"kind": kind}))?;
            memories(tx)
        })
        .await
    }

    /// Open a learning batch after the cursor; with no evidence the cursor
    /// moves to the end and `None` is returned.
    pub(crate) async fn open_learning(&self) -> Result<Option<Batch>> {
        self.write(|tx| open_learning(tx)).await
    }

    /// Apply an answer to the first `count` rows of `b`; returns (added, updated).
    pub(crate) async fn commit_learning(&self, b: Batch, count: usize, result: Value) -> Result<(usize, usize)> {
        self.write(move |tx| commit_learning(tx, &b, count, &result)).await
    }
}

/// The version in effect for `target`.
pub(crate) fn current(conn: &Connection, target: &str) -> Result<Option<Version>> {
    rows::one(conn, "SELECT * FROM prompt_versions WHERE target = ?1 ORDER BY rowid DESC LIMIT 1", [target])
}

fn version(conn: &Connection, id: &str) -> Result<Option<Version>> {
    rows::one(conn, "SELECT * FROM prompt_versions WHERE id = ?1", [id])
}

/// The policy text a request of kind `target` uses: the adopted version for
/// an editable target, the factory text for everything else. Analysis,
/// archive and handling append their fixed protocol fragments themselves.
pub(crate) fn effective(conn: &Connection, target: &str) -> Result<String> {
    if !TARGETS.contains(&target) {
        return Ok(default(target).into());
    }
    Ok(current(conn, target)?
        .map(|v| v.content)
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| default(target).into()))
}

/// Append a version and return its id.
pub(crate) fn record(conn: &Connection, target: &str, content: &str, action: &str, reason: &str) -> Result<String> {
    let parent = current(conn, target)?.map(|v| v.id).unwrap_or_default();
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO prompt_versions(id, target, content, previous_id, action, reason, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, target, content, parent, action, reason, time::now()],
    )?;
    Ok(id)
}

/// Record the built-in text of every target on each start: the first
/// version, or an upgrade while the target is still on a built-in version.
/// A version the user saved, adopted or rolled back to is kept.
pub(super) fn seed(conn: &Connection) -> Result<()> {
    for target in TARGETS {
        let text = default(target);
        match current(conn, target)? {
            None => record(conn, target, text, "initial", "内置提示词")?,
            Some(v) if ["initial", "upgrade"].contains(&v.action.as_str()) && v.content != text => {
                record(conn, target, text, "upgrade", "内置提示词随版本更新")?
            }
            Some(_) => continue,
        };
    }
    Ok(())
}

fn review(conn: &Connection, id: &str) -> Result<Option<Review>> {
    rows::one(conn, "SELECT * FROM prompt_reviews WHERE id = ?1", [id])
}

pub(crate) fn reviews(conn: &Connection) -> Result<Vec<Review>> {
    rows::all(conn, "SELECT * FROM prompt_reviews ORDER BY rowid", [])
}

pub(crate) fn insert_review(conn: &Connection, r: &Review) -> Result<()> {
    let mut row = json!(r);
    row["evidence"] = r.evidence.to_string().into();
    rows::put(conn, "prompt_reviews", "id", &row)
}

/// One memory; `kind` is `temporary` or `archived`.
pub(crate) fn memory(conn: &Connection, kind: &str) -> Result<Memory> {
    Ok(conn.query_row("SELECT text, revision, updated FROM memory WHERE kind = ?1", [kind], |r| {
        Ok(Memory { text: r.get(0)?, revision: r.get(1)?, updated: r.get(2)? })
    })?)
}

/// Replace a memory's text if `expected` is still its revision. Used by
/// manual saves and by the analysis and archive commits.
pub(crate) fn replace_memory(conn: &Connection, kind: &str, expected: i64, text: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE memory SET text = ?3, revision = revision + 1, updated = ?4 WHERE kind = ?1 AND revision = ?2",
        params![kind, expected, text, time::now()],
    )? == 1)
}

fn memories(conn: &Connection) -> Result<Value> {
    let entry = |kind: &str, limit: usize| -> Result<Value> {
        let m = memory(conn, kind)?;
        Ok(json!({"text": m.text, "revision": m.revision, "updated": m.updated, "max_chars": limit}))
    };
    Ok(json!({"temporary": entry("temporary", TEMPORARY_LIMIT)?, "archived": entry("archived", ARCHIVED_LIMIT)?}))
}

/// Both memory revisions, as the snapshot and the analysis base show them.
pub(crate) fn memory_revisions(conn: &Connection) -> Result<Value> {
    Ok(json!([memory(conn, "temporary")?.revision, memory(conn, "archived")?.revision]))
}

/// Audit kinds [`rules::learning_evidence`] can accept; rows of other kinds
/// are never read for learning.
const LEARNING_KINDS: &str =
    "kind IN ('manual_edit', 'manual_ai_edit', 'undo', 'merge', 'manual_status', 'manual_archive')";

/// Learning evidence rows matching `condition`, in `order`.
fn learning_rows(conn: &Connection, condition: &str, order: &str) -> Result<Vec<Audit>> {
    let rows = super::archive::audits(conn, &format!("{condition} AND {LEARNING_KINDS} ORDER BY seq {order}"))?;
    Ok(rows.into_iter().filter(Audit::learning).collect())
}

/// Whether learning evidence waits after `cursor`, read a page at a time.
pub(super) fn learning_pending(conn: &Connection, cursor: i64) -> Result<bool> {
    let mut after = cursor;
    loop {
        let page = super::archive::audits(conn, &format!("seq > {after} AND {LEARNING_KINDS} ORDER BY seq LIMIT {PAGE}"))?;
        if page.iter().any(Audit::learning) {
            return Ok(true);
        }
        match page.last() {
            Some(last) if page.len() == PAGE => after = last.seq,
            _ => return Ok(false),
        }
    }
}

fn suggestions(conn: &Connection) -> Result<Vec<Suggestion>> {
    Ok(super::rules::list(conn)?.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

fn open_learning(conn: &Connection) -> Result<Option<Batch>> {
    let cursor = kv::get_i64(conn, LEARNING_CURSOR)?;
    let end = super::archive::max_seq(conn)?.max(cursor);
    let eligible = learning_rows(conn, &format!("seq > {cursor}"), "ASC")?;
    kv::set(conn, super::tasks::PENDING_LEARNING, &!eligible.is_empty())?;
    if eligible.is_empty() {
        kv::set(conn, LEARNING_CURSOR, &end)?;
        return Ok(None);
    }
    let relevant: BTreeSet<&str> = eligible.iter().map(|a| a.task_id.as_str()).collect();
    let kinds: BTreeSet<&str> = eligible.iter().map(|a| a.kind.as_str()).collect();
    let history = batch::rows(
        learning_rows(conn, &format!("seq <= {cursor}"), "DESC")?
            .iter()
            .filter(|a| relevant.contains(a.task_id.as_str()) || kinds.contains(a.kind.as_str()))
            .map(|a| json!({"audit_index": a.seq, "event": learning::event(a)})),
        2800,
    );
    let history_ids =
        history["items"].as_array().into_iter().flatten().filter_map(|r| r["audit_index"].as_i64()).collect();
    let mut tasks = BTreeMap::new();
    for id in &relevant {
        if let Some(t) = super::tasks::get(conn, id)? {
            tasks.insert(id.to_string(), t);
        }
    }
    let mut audit = Vec::new();
    for a in &eligible {
        let mut messages = Vec::new();
        if let Some(task) = tasks.get(&a.task_id) {
            for id in task.evidence.all().into_iter().rev() {
                if let Some(m) = super::messages::get(conn, id)?.filter(|m| !m.withdrawn) {
                    messages.push(json!({"id": m.id, "sender": m.sender, "mine": m.mine,
                        "text": batch::message(&m, 400)["text"]}));
                    break;
                }
            }
        }
        audit.push(json!({"audit_index": a.seq, "event": learning::event(a), "messages": messages}));
    }
    let mut current: Vec<&model::Task> = tasks.values().collect();
    current.sort_by_key(|t| (t.created, t.id.clone()));
    let base = json!({
        "history": history["items"], "omitted_history": history["omitted"],
        "existing_suggestions": learning::existing(&suggestions(conn)?, &json!(audit).to_string()),
        "current_prompt": effective(conn, "classify")?,
        "current_tasks": batch::rows(current.into_iter().map(batch::task), 3000),
    });
    Ok(Some(Batch { eligible, end, history_ids, audit, base }))
}

/// Move the cursor past the first `count` rows (never back: an archive run
/// may have moved it further).
fn commit_learning(conn: &Connection, b: &Batch, count: usize, result: &Value) -> Result<(usize, usize)> {
    let eligible: Vec<i64> = b.eligible[..count].iter().map(|a| a.seq).collect();
    let cutoff = if count < b.eligible.len() { eligible[count - 1] } else { b.end };
    let rows = result["suggestions"].as_array().cloned().unwrap_or_default();
    // Only the rows the answer cites can be looked up.
    let cited: BTreeSet<i64> =
        rows.iter().flat_map(|r| r["evidence"].as_array().into_iter().flatten()).filter_map(Value::as_i64).collect();
    let list = cited.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
    let audit: BTreeMap<i64, Audit> =
        super::archive::audits(conn, &format!("seq IN ({list})"))?.into_iter().map(|a| (a.seq, a)).collect();
    let before = suggestions(conn)?;
    let mut after = before.clone();
    let lookup = |seq: i64| audit.get(&seq).map(|a| Evidence { kind: &a.kind, detail: &a.detail, after: &a.after });
    let counts = learning::apply(&mut after, &rows, &eligible, &b.history_ids, lookup, time::now());
    for s in after.iter().filter(|s| !before.contains(s)) {
        super::rules::save_learned(conn, s)?;
    }
    let cursor = kv::get_i64(conn, LEARNING_CURSOR)?.max(cutoff);
    kv::set(conn, LEARNING_CURSOR, &cursor)?;
    kv::set(conn, super::tasks::PENDING_LEARNING, &learning_pending(conn, cursor)?)?;
    super::activity::log(
        conn,
        "learning_result",
        &json!({"added": counts.0, "updated": counts.1, "processed": count}),
    )?;
    Ok(counts)
}

/// Snapshot keys of the prompts domain.
pub(crate) fn part(conn: &Connection, scope: &Scope, out: &mut Map<String, Value>) -> Result<()> {
    let versions: i64 = conn.query_row("SELECT count(*) FROM prompt_versions", [], |r| r.get(0))?;
    let reviews = reviews(conn)?;
    out.insert(
        "prompt_state".into(),
        json!({"versions": versions, "reviews": reviews.iter().map(|r| json!([r.id, r.status, r.error])).collect::<Vec<_>>()}),
    );
    let pending = reviews.iter().filter(|r| r.archived_at == 0 && r.status == "ready" && has_result(r)).count();
    out.insert("prompt_pending".into(), pending.into());
    out.insert(RULE_VERSION.into(), kv::get_i64(conn, RULE_VERSION)?.into());
    out.insert("memory_state".into(), memory_revisions(conn)?);
    if ["prompts", "learning"].contains(&scope.page.as_str()) {
        let shown: Vec<Value> = reviews
            .iter()
            .filter(|r| r.archived_at == 0 && has_result(r))
            .map(|r| {
                let mut v = json!(r);
                v.as_object_mut().unwrap().remove("evidence");
                v
            })
            .collect();
        out.entry("evolution").or_insert_with(|| json!({}))["prompts"] = shown.into();
    }
    Ok(())
}

/// AI task changes since `since` and how many the user then corrected
/// before the AI touched the task again, per change kind and per field.
/// A correction is an edit (manual or by one-sentence AI) or a merge;
/// status changes and archiving are ordinary progress, not corrections.
fn ai_accuracy(conn: &Connection, since: i64) -> Result<Value> {
    const AI: &str = "'ai_create', 'ai_update', 'ai_merge_duplicate'";
    const FIELDS: [&str; 8] = ["title", "description", "status", "blocker", "waiting_for", "due", "tags", "evidence"];
    let sql = format!(
        "SELECT a.kind, m.kind, m.before, m.after FROM task_audit a
         LEFT JOIN task_audit m ON m.seq = (
             SELECT min(x.seq) FROM task_audit x
             WHERE x.task_id = a.task_id AND x.seq > a.seq AND x.kind IN ('manual_edit', 'manual_ai_edit', 'merge')
               AND NOT EXISTS (SELECT 1 FROM task_audit n WHERE n.task_id = a.task_id
                               AND n.seq > a.seq AND n.seq < x.seq AND n.kind IN ({AI})))
         WHERE a.kind IN ({AI}) AND a.task_id != '' AND a.time >= ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([since])?;
    let mut kinds: BTreeMap<String, (u64, u64, BTreeMap<&str, u64>)> = BTreeMap::new();
    while let Some(r) = rows.next()? {
        let entry = kinds.entry(r.get(0)?).or_default();
        entry.0 += 1;
        let Some(fix) = r.get::<_, Option<String>>(1)? else { continue };
        entry.1 += 1;
        if fix == "merge" {
            *entry.2.entry("merge").or_default() += 1;
            continue;
        }
        let json = |i| -> Result<Value> {
            Ok(r.get::<_, Option<String>>(i)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
        };
        let (before, after) = (json(2)?, json(3)?);
        for field in FIELDS.into_iter().filter(|f| before[f] != after[f]) {
            *entry.2.entry(field).or_default() += 1;
        }
    }
    Ok(kinds
        .into_iter()
        .map(|(kind, (total, corrected, fields))| json!({"kind": kind, "total": total, "corrected": corrected, "fields": fields}))
        .collect())
}
