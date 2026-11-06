//! Archive groups: the prompt version, rules and tags an archive or a
//! rollback left, and the whole-group rollback.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::attempts;
use crate::data::{audit, kv, rows};
use crate::tasks::model as tasks;
use crate::{time, Result};

const CONFLICT: &str =
    "最新归档之后提示词、规则或标签已有手动修改，整组回滚会覆盖这些修改，已取消；请先确认当前版本或生成新版本";

/// The prompt version, enabled rules and tags an archive or rollback left.
/// Memory, cursors and task state never move back.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Group {
    pub id: String,
    pub created: i64,
    pub action: String,
    pub restored_from: String,
    pub attempt_id: String,
    pub report_id: String,
    pub reason: String,
    pub prompt_version: String,
    pub active_rules: Vec<Value>,
    pub rule_version: i64,
    pub tag_groups: Value,
    pub tag_version: String,
    pub archived_version: i64,
}

pub(crate) fn groups(conn: &Connection) -> Result<Vec<Group>> {
    rows::all(conn, "SELECT * FROM archive_groups ORDER BY seq", [])
}

pub(crate) fn group(conn: &Connection, id: &str) -> Result<Option<Group>> {
    rows::one(conn, "SELECT * FROM archive_groups WHERE id = ?1", [id])
}

pub(crate) fn latest_group(conn: &Connection) -> Result<Option<Group>> {
    rows::one(conn, "SELECT * FROM archive_groups ORDER BY seq DESC LIMIT 1", [])
}

/// The current tag catalog as stored, its version id and priority kind.
fn tag_state(conn: &Connection) -> Result<(Value, String, String)> {
    let sql = "SELECT groups, id, priority_kind FROM tag_versions ORDER BY seq DESC LIMIT 1";
    let (groups, id, kind): (String, String, String) =
        conn.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok((serde_json::from_str(&groups)?, id, kind))
}

/// Prompt version, enabled rules and tags as they are now.
pub(super) fn snapshot(conn: &Connection) -> Result<Group> {
    let (tag_groups, tag_version, _) = tag_state(conn)?;
    Ok(Group {
        prompt_version: crate::data::prompts::current(conn, "classify")?.map(|v| v.id).unwrap_or_default(),
        active_rules: crate::data::rules::active(conn)?,
        rule_version: kv::get_i64(conn, crate::data::rules::RULE_VERSION)?,
        tag_groups,
        tag_version,
        archived_version: crate::data::prompts::memory(conn, "archived")?.revision,
        ..Default::default()
    })
}

fn prompt_of(conn: &Connection, version: &str) -> Result<String> {
    let sql = "SELECT content FROM prompt_versions WHERE id = ?1";
    Ok(conn.query_row(sql, [version], |r| r.get(0)).optional()?.unwrap_or_default())
}

/// `archive_history`: group summaries; `archive_version` loads a version's
/// rules and tags.
pub(super) fn history(conn: &Connection) -> Result<Value> {
    let sql = "SELECT id, created, action, restored_from, attempt_id, report_id, reason, prompt_version,
            json_array_length(active_rules) AS active_rules, tag_version, archived_version FROM archive_groups ORDER BY seq";
    let versions = rows::objects(conn, sql, [], &Value::Null)?;
    Ok(json!({
        "attempts": attempts(conn)?,
        "current": versions.last().map(|v| v["id"].clone()),
        "versions": versions,
        "memory": crate::data::prompts::memory(conn, "archived")?.text,
    }))
}

/// `archive_version {id}`. Groups keep no memory text, so `memory` is an
/// empty list.
pub(super) fn version(conn: &Connection, id: &str) -> Result<Value> {
    let g = group(conn, id)?.ok_or("归档版本不存在")?;
    Ok(json!({
        "id": g.id, "created": g.created, "action": g.action, "restored_from": g.restored_from,
        "attempt_id": g.attempt_id, "report_id": g.report_id, "reason": g.reason,
        "prompt": prompt_of(conn, &g.prompt_version)?, "prompt_version": g.prompt_version,
        "rules": g.active_rules, "tags": g.tag_groups, "tag_version": g.tag_version,
        "memory": [], "archived_version": g.archived_version,
        "current": latest_group(conn)?.map(|l| l.id),
    }))
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn rule_keys(rules: &[Value]) -> Vec<[&str; 3]> {
    rules.iter().map(|r| [text(r, "id"), text(r, "title"), text(r, "rule")]).collect()
}

/// `archive_rollback {id, expected}`: restore the prompt, rules and tags
/// of a group as a new group.
pub(super) fn rollback(conn: &Connection, id: &str, expected: &str) -> Result<Value> {
    let latest = latest_group(conn)?.filter(|g| g.id == expected).ok_or("归档版本已变化，请刷新后重试")?;
    let target = group(conn, id)?.ok_or("归档版本不存在")?;
    let now = snapshot(conn)?;
    let current_prompt = crate::data::prompts::current(conn, "classify")?.map(|v| v.content).unwrap_or_default();
    if now.prompt_version != latest.prompt_version
        || current_prompt != prompt_of(conn, &latest.prompt_version)?
        || now.rule_version != latest.rule_version
        || rule_keys(&now.active_rules) != rule_keys(&latest.active_rules)
        || now.tag_version != latest.tag_version
    {
        crate::bail!("{CONFLICT}");
    }
    let at = time::now();
    let restored = prompt_of(conn, &target.prompt_version)?;
    let prompt_version = crate::data::prompts::record(
        conn,
        "classify",
        &restored,
        "archive_rollback",
        &format!("归档整组回滚至 {}", target.id),
    )?;
    // Restore the target's rules; disable, never delete, the others.
    let wanted: BTreeMap<&str, &Value> = target.active_rules.iter().map(|r| (text(r, "id"), r)).collect();
    for rule in crate::data::rules::list(conn)? {
        let rid = text(&rule, "id");
        if let Some(t) = wanted.get(rid) {
            conn.execute(
                "UPDATE suggestions SET title = ?2, rule = ?3, decision = 'enabled', active = 1, archived_at = 0, updated = ?4 WHERE id = ?1",
                params![rid, text(t, "title"), text(t, "rule"), at],
            )?;
        } else if rule["active"] == true {
            conn.execute(
                "UPDATE suggestions SET active = 0, decision = 'pending', updated = ?2 WHERE id = ?1",
                params![rid, at],
            )?;
        }
    }
    for t in &target.active_rules {
        conn.execute(
            "INSERT OR IGNORE INTO suggestions(id, title, rule, evidence, active, decision, created, updated, archived_at)
             VALUES (?1, ?2, ?3, ?4, 1, 'enabled', ?5, ?6, 0)",
            params![text(t, "id"), text(t, "title"), text(t, "rule"), t.get("evidence").unwrap_or(&json!([])).to_string(), t["created"].as_i64().unwrap_or(at), at],
        )?;
    }
    // A rollback keeps the current priority kind; an empty one would read as
    // "no priority group".
    let (_, _, priority_kind) = tag_state(conn)?;
    conn.execute(
        "INSERT INTO tag_versions(id, created, action, reason, priority_kind, groups, assignments)
         VALUES (?1, ?2, 'archive_rollback', ?3, ?4, ?5, '[]')",
        params![
            tasks::new_id(),
            at,
            format!("随归档版本回滚至 {}", target.id),
            priority_kind,
            target.tag_groups.to_string()
        ],
    )?;
    kv::bump(conn, crate::data::rules::RULE_VERSION)?;
    let next = Group {
        id: tasks::new_id(),
        created: at,
        action: "rollback".into(),
        restored_from: target.id.clone(),
        reason: format!("整组回滚至 {}，以新版本记录，不倒退消息/事项进度", target.id),
        ..snapshot(conn)?
    };
    rows::put(conn, "archive_groups", "id", &next)?;
    let detail = "归档整组回滚：仅提示词、规则、标签和私有归档记忆；保留后续建议记录";
    audit::write(
        conn,
        "archive_rollback",
        "",
        detail,
        &json!({"restored_from": target.id}),
        &json!({"group": next.id}),
    )?;
    Ok(json!({"group": next.id, "prompt_version": prompt_version}))
}
