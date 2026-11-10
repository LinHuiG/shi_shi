//! Applying a classify result in the accept transaction: task actions,
//! temporary memory and tag suggestions.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::analysis::apply::{
    accepted_change_summary, confidence, materialize, preserve_model_summary, strings, Applied, MEMORY_CHANGED,
    MEMORY_PURPOSE,
};
use crate::data::audit;
use crate::messages::Message;
use crate::tasks::model::{self, Evidence, Task};
use crate::tasks::tags::{self, Group, Tag};
use crate::{time, Error, Result};

/// The versions the decision depended on, stamped on every `ai_*` audit and
/// the task it touched.
pub(crate) fn decision_context(conn: &Connection) -> Result<Value> {
    let rules: Vec<Value> = crate::data::rules::active(conn)?.into_iter().map(|r| r["id"].clone()).collect();
    Ok(json!({
        "prompt_version": crate::data::prompts::current(conn, "classify")?.map(|v| v.id),
        "rule_version": crate::data::kv::get_i64(conn, crate::data::rules::RULE_VERSION)?,
        "rule_ids": rules,
        "archived_memory_version": crate::data::prompts::memory(conn, "archived")?.revision,
        "temporary_memory_version": crate::data::prompts::memory(conn, "temporary")?.revision,
    }))
}

/// Apply `actions` all-or-nothing for whole-batch errors.
pub(crate) fn apply_actions(
    conn: &Connection,
    result: &Value,
    allowed: &BTreeMap<String, Message>,
    task_versions: &BTreeMap<String, i64>,
    catalog: &[Group],
    context: &Value,
) -> Result<Applied> {
    conn.execute_batch("SAVEPOINT analysis_apply")?;
    let out = Applier { conn, result, allowed, task_versions, catalog, context }.run();
    if out.is_err() {
        conn.execute_batch("ROLLBACK TO analysis_apply")?;
    }
    conn.execute_batch("RELEASE analysis_apply")?;
    out
}

struct Applier<'a> {
    conn: &'a Connection,
    result: &'a Value,
    allowed: &'a BTreeMap<String, Message>,
    task_versions: &'a BTreeMap<String, i64>,
    catalog: &'a [Group],
    context: &'a Value,
}

impl Applier<'_> {
    fn run(&self) -> Result<Applied> {
        let empty = Vec::new();
        let actions = self.result["actions"].as_array().unwrap_or(&empty);
        let mut out = Applied::default();
        // Versions written earlier in this batch replace the frozen ones.
        let mut applied_versions: BTreeMap<String, i64> = BTreeMap::new();
        for action in actions {
            self.action(action, &mut applied_versions, &mut out)?;
        }
        Ok(out)
    }

    /// The live message equals the delivered one and is not withdrawn.
    fn live(&self, id: &str) -> Result<bool> {
        let Some(sent) = self.allowed.get(id) else { return Ok(false) };
        Ok(!sent.withdrawn && crate::data::messages::get(self.conn, id)?.as_ref() == Some(sent))
    }

    /// Valid evidence ids, or none at all when any entry is invalid.
    fn valid_evidence(&self, rows: &Value) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for v in rows.as_array().into_iter().flatten() {
            let Some(id) = v.as_str() else { return Ok(vec![]) };
            if !self.live(id)? {
                return Ok(vec![]);
            }
            out.push(id.to_string());
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Time of the last manual correction of the task (`manual*`, and with
    /// `merges` also merge and undo), else its creation.
    fn last_manual(&self, task: &Task, merges: bool) -> Result<i64> {
        let sql = if merges {
            "SELECT max(time) FROM task_audit WHERE task_id = ?1 AND (kind LIKE 'manual%' OR kind IN ('merge', 'undo'))"
        } else {
            "SELECT max(time) FROM task_audit WHERE task_id = ?1 AND kind LIKE 'manual%'"
        };
        let at: Option<i64> = self.conn.query_row(sql, [&task.id], |r| r.get(0))?;
        Ok(at.unwrap_or(task.created))
    }

    /// Whether a source is newer than every message the task already cites:
    /// an older backlog batch must not roll a task back to a previous stage.
    fn has_new_progress(&self, old: &Task, sources: &[String]) -> Result<bool> {
        let sql = "SELECT EXISTS (SELECT 1 FROM messages n WHERE n.withdrawn = 0 \
                   AND n.id IN (SELECT value FROM json_each(?2)) AND n.id NOT IN (SELECT value FROM json_each(?1)) \
                   AND n.time >= (SELECT coalesce(max(o.time), 0) FROM messages o WHERE o.withdrawn = 0 \
                                  AND o.id IN (SELECT value FROM json_each(?1))))";
        let existing = json!(old.evidence.all()).to_string();
        Ok(self.conn.query_row(sql, params![existing, json!(sources).to_string()], |r| r.get(0))?)
    }

    /// The lowest-numbered live task citing any of `sources`.
    fn dedup_target(&self, sources: &[String]) -> Result<Option<Task>> {
        let sql = "SELECT t.id FROM tasks t JOIN task_evidence e ON e.task_id = t.id \
                   WHERE t.archived = 0 AND t.merged_into = '' AND t.generation = '' \
                   AND e.message_id IN (SELECT value FROM json_each(?1)) ORDER BY t.number LIMIT 1";
        let id: Option<String> = self.conn.query_row(sql, [json!(sources).to_string()], |r| r.get(0)).optional()?;
        id.map_or(Ok(None), |id| crate::data::tasks::get(self.conn, &id))
    }

    fn stamp(&self, t: &mut Task) {
        if !self.context.is_null() {
            t.decision_context = self.context.clone();
        }
    }

    /// Store `t` with its audit and count it as applied.
    fn write(
        &self,
        kind: &str,
        detail: &str,
        before: &Value,
        t: &Task,
        summary: &str,
        versions: &mut BTreeMap<String, i64>,
        out: &mut Applied,
    ) -> Result<()> {
        let stored = crate::data::tasks::store(self.conn, t)?;
        let after = json!(stored);
        let seq = audit::write(self.conn, kind, &stored.id, detail, before, &after)?;
        out.record(&stored.id, before, &after, summary);
        if !summary.is_empty() {
            self.conn.execute("UPDATE task_audit SET change_summary = ?2 WHERE seq = ?1", params![seq, summary])?;
        }
        crate::data::tools::link_tool_calls(self.conn, self.result, std::slice::from_ref(&stored.id))?;
        versions.insert(stored.id.clone(), stored.updated);
        out.tasks.push(stored.id);
        out.applied += 1;
        Ok(())
    }

    fn action(&self, action: &Value, applied_versions: &mut BTreeMap<String, i64>, out: &mut Applied) -> Result<()> {
        let kind = action["action"].as_str().unwrap_or("");
        if kind == "ignore" {
            return Ok(());
        }
        if kind != "create" && kind != "update" {
            crate::bail!("AI动作类型无效");
        }
        if action.get("tags").is_some() {
            crate::bail!("AI 分析必须使用 tag_changes 明确增删标签，不能整组替换；事项未保存");
        }
        if !action["sources"].is_array() {
            return Err(Error::new("AI动作缺少来源"));
        }
        let sources = strings(&action["sources"]);
        let mut stale = sources.is_empty();
        for id in &sources {
            stale |= !self.live(id)?;
        }
        if stale {
            out.retry.extend(sources);
            return Ok(());
        }
        // A redirected create keeps create-time tag permissions.
        let create_tags = (kind == "create")
            .then(|| tags::apply_changes(&[], &action["tag_changes"], self.catalog, false))
            .transpose()?;
        let (old, deduplicated) = if create_tags.is_some() {
            let t = self.dedup_target(&sources)?;
            let dedup = t.is_some();
            (t, dedup)
        } else {
            (crate::data::tasks::get(self.conn, action["task_id"].as_str().unwrap_or(""))?, false)
        };
        let updating = kind == "update" || deduplicated;
        let background: Vec<String> =
            strings(&action["background_evidence"]).into_iter().filter(|id| self.allowed.contains_key(id)).collect();
        let completion = self.valid_evidence(&action["completion_evidence"])?;
        let Ok(mut t) = materialize(action, old.as_ref().filter(|_| updating)) else {
            out.retry.extend(sources);
            return Ok(());
        };
        t.tags = match create_tags {
            Some(tags) if !updating => tags,
            _ => tags::apply_changes(&t.tags, &action["tag_changes"], self.catalog, !deduplicated)?,
        };
        t.suggested_project_id = action["suggested_project_id"].as_str().unwrap_or("").into();
        t.suggested_project_reason = action["suggested_project_reason"].as_str().unwrap_or("").into();
        let wants_completion = t.status == "已完成"
            && (!updating || action["fields"].as_array().into_iter().flatten().any(|c| c["field"] == "status"));
        let mut summary_allowed = !deduplicated;
        if wants_completion && (confidence(action) < 0.95 || completion.is_empty()) {
            summary_allowed = false;
            t.status = old.as_ref().map(|o| o.status.clone()).unwrap_or_else(|| "待处理".into());
            out.retry.extend(sources.iter().cloned());
        }
        let now = time::now();
        let added =
            Evidence { direct: sources.clone(), background: background.clone(), completion: completion.clone() };
        t.updated = now;
        t.evidence = Evidence::default();
        model::merge_evidence(&mut t, &added);
        preserve_model_summary(&mut t);
        model::normalize(&mut t);

        if !updating {
            if !t.evidence.direct.iter().any(|id| self.allowed.get(id).is_some_and(Message::relevant)) {
                out.retry.extend(sources);
                return Ok(());
            }
            t.id = model::new_id();
            t.number = 0;
            t.deferred_until = 0;
            t.resume_processing = false;
            t.created = now;
            t.follow_deadline = true;
            t.deadline_changed = 0;
            if !t.due.is_empty() && model::deadline_ms(&t.due).is_none() {
                t.due.clear();
            }
            t.content_updated_at = t.updated;
            t.summary_updated_at = 0;
            preserve_model_summary(&mut t);
            tags::sanitize(&mut t, None, self.catalog);
            model::normalize(&mut t);
            self.stamp(&mut t);
            let summary =
                if summary_allowed { accepted_change_summary(action, &Value::Null, &json!(t)) } else { String::new() };
            let reason = t.reason.clone();
            return self.write("ai_create", &reason, &Value::Null, &t, &summary, applied_versions, out);
        }

        let Some(old) = old else {
            out.retry.extend(sources);
            return Ok(());
        };
        let tid = old.id.clone();
        if applied_versions.get(&tid).or_else(|| self.task_versions.get(&tid)) != Some(&old.updated) {
            out.retry.extend(sources);
            return Ok(());
        }
        let before = json!(old);
        if !(self.has_new_progress(&old, &sources)? || !completion.is_empty()) {
            // Evidence-only update: keep every state field, append the newly
            // observed references.
            let mut evidence = old.clone();
            model::merge_evidence(&mut evidence, &added);
            if evidence.evidence == old.evidence {
                crate::data::tools::link_tool_calls(self.conn, self.result, std::slice::from_ref(&tid))?;
                return Ok(());
            }
            evidence.updated = now.max(old.updated + 1);
            self.stamp(&mut evidence);
            let detail = format!("追加未展示过的依据：{}", action["reason"].as_str().unwrap_or(""));
            return self.write("ai_evidence", &detail, &before, &evidence, "", applied_versions, out);
        }

        let deadline_proof = strings(&action["deadline_evidence"]);
        let mut deadline_valid = !deadline_proof.is_empty();
        for id in &deadline_proof {
            deadline_valid &= self.live(id)? && self.allowed[id].time > old.deadline_changed;
        }
        let rescheduled =
            t.due != old.due && (t.due.is_empty() || model::deadline_ms(&t.due).is_some()) && deadline_valid;
        if old.manual {
            let last = self.last_manual(&old, true)?;
            if confidence(action) < 0.9
                || t.reason.trim().is_empty()
                || !sources.iter().any(|id| self.allowed.get(id).is_some_and(|m| m.time > last))
            {
                out.retry.extend(sources);
                return Ok(());
            }
        }
        let proposed_deferral = t.deferred_until;
        let mut candidate = t;
        candidate.updated = now.max(old.updated + 1);
        candidate.evidence = old.evidence.clone();
        model::merge_evidence(&mut candidate, &added);
        candidate.deferred_until = old.deferred_until;
        candidate.resume_processing = old.resume_processing;
        if proposed_deferral != old.deferred_until {
            let proof = self.valid_evidence(&action["deferral_evidence"])?;
            let last = self.last_manual(&old, false)?;
            let reason = action["deferral_reason"].as_str().filter(|r| !r.trim().is_empty());
            match reason {
                Some(reason)
                    if confidence(action) >= 0.9
                        && proof.iter().any(|id| self.allowed.get(id).is_some_and(|m| m.time > last))
                        && (proposed_deferral == 0 || proposed_deferral > now) =>
                {
                    candidate.deferred_until = proposed_deferral;
                    candidate.reason = format!("{}；延后调整：{reason}", candidate.reason);
                }
                _ => out.retry.extend(sources.iter().cloned()),
            }
        }
        // A status-only patch must not bypass a deferral.
        if candidate.deferred_until > now && candidate.status == "处理中" {
            candidate.status = "待处理".into();
        }
        if !model::ACTIVE.contains(&candidate.status.as_str()) {
            candidate.deferred_until = 0;
        }
        if candidate.deferred_until == 0 || candidate.status != "待处理" {
            candidate.resume_processing = false;
        }
        if rescheduled {
            candidate.deadline_changed =
                deadline_proof.iter().filter_map(|id| self.allowed.get(id).map(|m| m.time)).max().unwrap_or(now);
        } else {
            candidate.due = old.due.clone();
        }
        if candidate.description != old.description || candidate.title != old.title {
            candidate.content_updated_at = candidate.updated;
            candidate.summary_updated_at = 0;
        }
        preserve_model_summary(&mut candidate);
        model::normalize(&mut candidate);
        tags::sanitize(&mut candidate, Some(&old), self.catalog);
        self.stamp(&mut candidate);
        let summary =
            if summary_allowed { accepted_change_summary(action, &before, &json!(candidate)) } else { String::new() };
        let reason = candidate.reason.clone();
        self.write("ai_update", &reason, &before, &candidate, &summary, applied_versions, out)
    }
}

// ---- temporary memory (§2.9) -----------------------------------------------

/// Write the classify result's `_memory_update` in the accept transaction.
/// A failure is logged as `memory_skipped` and leaves the rest of the batch.
pub(crate) fn write_memory(conn: &Connection, purpose: &str, update: &Value) -> Result<bool> {
    if update.is_null() {
        return Ok(false);
    }
    let outcome =
        if purpose != "classify" { Err(MEMORY_PURPOSE.to_string()) } else { apply_memory_update(conn, update)? };
    match outcome {
        Ok(()) => Ok(true),
        Err(error) => {
            crate::data::activity::log(conn, "memory_skipped", &json!({"error": error}))?;
            Ok(false)
        }
    }
}

fn apply_memory_update(conn: &Connection, update: &Value) -> Result<std::result::Result<(), String>> {
    let (Some(before), Some(text)) = (update["before"].as_str(), update["text"].as_str()) else {
        return Ok(Err("笔记内容无效".into()));
    };
    if text.chars().count() > crate::prompts::memory::TEMPORARY_LIMIT {
        return Ok(Err(format!("笔记超过{}字符，未保存", crate::prompts::memory::TEMPORARY_LIMIT)));
    }
    let current = crate::data::prompts::memory(conn, "temporary")?;
    if current.text != before || update["expected"].as_i64().is_some_and(|n| n != current.revision) {
        return Ok(Err(MEMORY_CHANGED.into()));
    }
    if !crate::data::prompts::replace_memory(conn, "temporary", current.revision, text)? {
        return Ok(Err(MEMORY_CHANGED.into()));
    }
    Ok(Ok(()))
}

// ---- tag suggestions ---------------------------------------------------------

/// Queue the result's tag suggestions (at most 8) as proposals. Evidence is
/// the payload's changes and context still in `allowed`; a suggestion that
/// is invalid, already known, lacks a reason or cites anything else is
/// dropped. Returns how many were queued.
pub(crate) fn collect_tags(
    conn: &Connection,
    result: &Value,
    payload: &Value,
    allowed: &BTreeMap<String, Message>,
    catalog: &[Group],
) -> Result<usize> {
    let mut evidence: BTreeMap<String, String> = BTreeMap::new();
    for key in ["changes", "context"] {
        for m in payload[key].as_array().into_iter().flatten() {
            if let Some(id) = m["id"].as_str().filter(|id| allowed.contains_key(*id)) {
                evidence.insert(id.into(), m["text"].as_str().unwrap_or("").chars().take(350).collect());
            }
        }
    }
    let mut queued = 0;
    for v in result["tag_suggestions"].as_array().into_iter().flatten().take(8) {
        let Ok(tag) = serde_json::from_value::<Tag>(json!({"kind": v["kind"], "value": v["value"]})) else {
            continue;
        };
        let reason = v["reason"].as_str().unwrap_or("").trim();
        let ids = strings(&v["evidence_ids"]);
        if !tags::valid(&tag)
            || tags::known(catalog, &tag.kind, &tag.value)
            || reason.is_empty()
            || ids.is_empty()
            || ids.iter().any(|id| !evidence.contains_key(id))
        {
            continue;
        }
        let proof: Vec<(String, String)> = ids.iter().map(|id| (id.clone(), evidence[id].clone())).collect();
        crate::data::tags::propose(conn, &tag, reason, &proof)?;
        queued += 1;
    }
    Ok(queued)
}
