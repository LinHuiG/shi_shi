//! Learned judgement rules (`suggestions`) and the advice interface the
//! page uses for rules and tag proposals. Prompt and handling advice belong
//! to their own domains, which route those kinds before reaching here.

use rusqlite::{params, Connection, Params};
use serde_json::{json, Value};

use super::{audit, kv, rows, Db};
use crate::tasks::model::{from_json, new_id};
use crate::tasks::tags;
use crate::{time, Result};

pub const RULE_VERSION: &str = "rule_version";

impl Db {
    pub async fn decide_advice(&self, p: Value) -> Result<Value> {
        self.write(move |tx| decide(tx, &p)).await
    }

    pub async fn toggle_rule(&self, p: Value) -> Result<Value> {
        self.write(move |tx| toggle(tx, &p)).await
    }

    pub async fn prepare_feedback(&self, p: Value) -> Result<Feedback> {
        self.read(move |c| feedback_prepare(c, &p)).await
    }

    pub async fn complete_feedback(&self, feedback: Feedback, result: Value) -> Result<Value> {
        self.write(move |tx| feedback_complete(tx, &feedback, &result)).await
    }
}

/// The suggestions matching `filter`, oldest first.
fn select(conn: &Connection, filter: &str, p: impl Params) -> Result<Vec<Value>> {
    let sql = format!("SELECT id, title, rule, evidence, active, decision, created, updated, archived_at FROM suggestions WHERE {filter} ORDER BY seq");
    rows::objects(conn, &sql, p, &json!({"evidence": [], "active": false}))
}

fn get(conn: &Connection, id: &str) -> Result<Option<Value>> {
    Ok(select(conn, "id = ?1", [id])?.pop())
}

/// `suggestions` of the snapshot, oldest first.
pub fn list(conn: &Connection) -> Result<Vec<Value>> {
    select(conn, "true", [])
}

/// Store a learned suggestion: a new row, or the new title, rule and
/// evidence of a pending one. Learned rows start disabled, so the rule
/// version is unchanged.
pub fn save_learned(conn: &Connection, s: &crate::prompts::learning::Suggestion) -> Result<()> {
    conn.execute(
        "INSERT INTO suggestions(id, title, rule, evidence, active, decision, created, updated, archived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET title = excluded.title, rule = excluded.rule,
             evidence = excluded.evidence, updated = excluded.updated",
        params![
            s.id,
            s.title,
            s.rule,
            json!(s.evidence).to_string(),
            s.active,
            s.decision,
            s.created,
            s.updated,
            s.archived_at
        ],
    )?;
    Ok(())
}

/// Rules the analysis prompt applies.
pub fn active(conn: &Connection) -> Result<Vec<Value>> {
    select(conn, "active = 1 AND archived_at = 0", [])
}

/// `rule {id}`: switch an existing rule on or back to pending.
pub fn toggle(conn: &Connection, v: &Value) -> Result<Value> {
    let id = v["id"].as_str().unwrap_or("");
    let before = get(conn, id)?.ok_or("建议不存在")?;
    if before["archived_at"].as_i64() > Some(0) {
        crate::bail!("此建议已归档，请编辑系统提示词或回滚版本");
    }
    let on = !before["active"].as_bool().unwrap_or(false);
    conn.execute(
        "UPDATE suggestions SET active = ?2, decision = ?3 WHERE id = ?1",
        params![id, on, if on { "enabled" } else { "pending" }],
    )?;
    kv::bump(conn, RULE_VERSION)?;
    let after = get(conn, id)?.unwrap_or_default();
    audit::write(conn, "rule_change", "", "规则启用/撤销，仅影响后续判断", &before, &after)?;
    Ok(json!(true))
}

/// The pending, unarchived item an advice call acts on.
fn target(conn: &Connection, kind: &str, id: &str) -> Result<Value> {
    let found = match kind {
        "rule" => get(conn, id)?.filter(|r| r["archived_at"] == 0 && r["decision"] == "pending"),
        "tag" => super::tags::pending_proposal(conn, id)?,
        _ => None,
    };
    Ok(found.ok_or("建议已处理或不存在，请刷新")?)
}

pub fn decide(conn: &Connection, v: &Value) -> Result<Value> {
    let kind = v["kind"].as_str().unwrap_or("rule");
    let id = v["id"].as_str().ok_or("缺少建议编号")?;
    let action = v["action"].as_str().unwrap_or("");
    let before = target(conn, kind, id)?;
    if kind != "rule" {
        crate::bail!("不支持的建议操作");
    }
    if !["enable", "reject"].contains(&action) {
        crate::bail!("请选择启用或拒绝");
    }
    let on = action == "enable";
    conn.execute(
        "UPDATE suggestions SET active = ?2, decision = ?3, updated = ?4 WHERE id = ?1",
        params![id, on, if on { "enabled" } else { "rejected" }, time::now()],
    )?;
    kv::bump(conn, RULE_VERSION)?;
    let after = json!({"kind": kind, "id": id, "action": action});
    audit::write(conn, "advice_decision", "", "明确处理迭代建议，下次归档收起", &before, &after)?;
    Ok(json!(true))
}

/// What `advice_feedback` sends to the `advice_revision` prompt.
pub struct Feedback {
    pub kind: String,
    pub id: String,
    pub text: String,
    pub before: Value,
    pub request: Value,
}

/// `advice_feedback {kind, id, text}` before the AI call. A rule's evidence
/// is the audit rows its `seq` list names.
pub fn feedback_prepare(conn: &Connection, v: &Value) -> Result<Feedback> {
    let kind = v["kind"].as_str().unwrap_or("rule");
    let id = v["id"].as_str().ok_or("缺少建议编号")?;
    let text = v["text"].as_str().unwrap_or("").trim();
    if text.is_empty() || text.chars().count() > 4000 {
        crate::bail!("请输入1–4000字的批复意见");
    }
    let before = target(conn, kind, id)?;
    let evidence = if kind == "rule" {
        let sql = "SELECT a.seq AS \"index\", a.kind, a.task_id, a.detail FROM json_each(?1) j \
                   JOIN task_audit a ON a.seq = j.value WHERE j.type = 'integer' ORDER BY j.key";
        json!(rows::objects(conn, sql, [before["evidence"].to_string()], &json!({}))?)
    } else {
        before.get("evidence").or(before.get("excerpts")).cloned().unwrap_or(Value::Null)
    };
    let current = super::prompts::effective(conn, "classify")?;
    let request = json!({"kind": kind, "suggestion": before, "evidence": evidence, "current_prompt": current,
        "user_feedback": text});
    Ok(Feedback { kind: kind.into(), id: id.into(), text: text.into(), before, request })
}

/// Commit the AI's revision unless the item changed meanwhile. The revised
/// item stays pending until the user enables it.
pub fn feedback_complete(conn: &Connection, f: &Feedback, result: &Value) -> Result<Value> {
    if target(conn, &f.kind, &f.id)? != f.before {
        crate::bail!("AI 分析期间建议已变化，本次结果未覆盖；请重新批复");
    }
    let bounded = |key: &str, max: usize| -> Result<String> {
        let text = result[key].as_str().unwrap_or("").trim();
        if text.is_empty() || text.chars().count() > max {
            crate::bail!("AI 返回的 {key} 无效，原建议保留");
        }
        Ok(text.into())
    };
    let reason = bounded("reason", 4000)?;
    match f.kind.as_str() {
        "rule" => {
            conn.execute(
                "UPDATE suggestions SET title = ?2, rule = ?3, updated = ?4 WHERE id = ?1",
                params![f.id, bounded("title", 200)?, bounded("rule", 6000)?, time::now()],
            )?;
        }
        "tag" => {
            let tag: tags::Tag = from_json(&result["tag"])?;
            if !tags::valid(&tag) {
                crate::bail!("AI 返回的标签名称无效");
            }
            conn.execute(
                "UPDATE tag_proposals SET kind = ?2, value = ?3, reason = ?4 WHERE id = ?1",
                params![f.id, tag.kind, tag.value, reason],
            )?;
        }
        _ => crate::bail!("未知建议类型"),
    }
    let after = target(conn, &f.kind, &f.id)?;
    conn.execute(
        "INSERT INTO advice_revisions(id, kind, suggestion_id, time, feedback, reason, before, after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![new_id(), f.kind, f.id, time::now(), f.text, reason, f.before.to_string(), after.to_string()],
    )?;
    audit::write(conn, "advice_feedback", "", "按用户批复修改建议，仍待用户启用", &f.before, &after)?;
    Ok(after)
}
