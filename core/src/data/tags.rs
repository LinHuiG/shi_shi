//! The tag catalog's rows: versions, proposals and saved filters.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::{audit, kv, rows, Db};
use crate::tasks::model::{from_json, new_id};
use crate::tasks::tags::{fill_priorities, known, valid, Assignment, Group, Tag, Version};
use crate::{time, Error, Result};

pub const FILTERS: &str = "tag_filters";
pub const PRIORITIES: &str = "tag_priorities";

impl Db {
    pub async fn set_tag_filters(&self, p: Value) -> Result<Value> {
        self.write(move |tx| tag_filters(tx, &p)).await
    }

    pub async fn decide_tag(&self, p: Value) -> Result<Value> {
        self.write(move |tx| tag_decide(tx, &p)).await
    }
}

pub fn versions(conn: &Connection) -> Result<Vec<Version>> {
    rows::all(conn, "SELECT * FROM tag_versions ORDER BY seq", [])
}

/// The current catalog: groups of the last version with display priorities
/// filled in (priority kind 10, 事项类型 20, others 100) and its priority kind.
pub fn catalog(conn: &Connection) -> Result<(Vec<Group>, String)> {
    let (groups, kind): (String, String) =
        conn.query_row("SELECT groups, priority_kind FROM tag_versions ORDER BY seq DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let mut groups: Vec<Group> = serde_json::from_str(&groups)?;
    fill_priorities(&mut groups, &kind);
    Ok((groups, kind))
}

/// Proposals matching `filter`, oldest first, as the page shows them.
fn proposals(conn: &Connection, filter: &str, p: impl rusqlite::Params) -> Result<Vec<Value>> {
    let sql = format!(
        "SELECT id, json_object('kind', kind, 'value', value) AS tag, reason, evidence, excerpts, status, created, \
         archived_at FROM tag_proposals WHERE {filter} ORDER BY seq"
    );
    rows::objects(conn, &sql, p, &json!({"tag": {}, "evidence": [], "excerpts": []}))
}

/// The pending, unarchived proposal `id`.
pub fn pending_proposal(conn: &Connection, id: &str) -> Result<Option<Value>> {
    Ok(proposals(conn, "id = ?1 AND status = 'pending' AND archived_at = 0", [id])?.pop())
}

fn filters(conn: &Connection) -> Result<BTreeMap<String, Vec<String>>> {
    Ok(kv::get(conn, FILTERS)?.unwrap_or_default())
}

/// `tag_state` of the snapshot.
pub fn snapshot(conn: &Connection) -> Result<Value> {
    let (groups, kind) = catalog(conn)?;
    Ok(json!({
        "unified": true, "priority_kind": kind, "groups": groups, "versions": versions(conn)?,
        "proposals": proposals(conn, "1", [])?, "filters": filters(conn)?,
        "priorities": kv::get::<Vec<String>>(conn, PRIORITIES)?.unwrap_or_default(), "category_aliases": {},
    }))
}

fn prune_filters(conn: &Connection, catalog: &[Group]) -> Result<()> {
    let mut saved = filters(conn)?;
    saved.retain(|kind, values| {
        values.retain(|v| known(catalog, kind, v));
        !values.is_empty()
    });
    kv::set(conn, FILTERS, &saved)?;
    kv::set(conn, PRIORITIES, &Vec::<String>::new())
}

pub fn tag_filters(conn: &Connection, v: &Value) -> Result<Value> {
    let filters: BTreeMap<String, Vec<String>> = from_json(&v["filters"])?;
    if filters.len() > 64
        || filters
            .iter()
            .any(|(k, vs)| vs.len() > 200 || vs.iter().any(|v| !valid(&Tag { kind: k.clone(), value: v.clone() })))
    {
        crate::bail!("标签筛选超出范围");
    }
    if let Some(priorities) = v.get("priorities") {
        let bad = || Error::new("事项等级筛选无效");
        let mut priorities: Vec<String> = from_json(priorities).map_err(|_| bad())?;
        let (catalog, kind) = catalog(conn)?;
        if priorities.len() > 200 || priorities.iter().any(|value| !known(&catalog, &kind, value)) {
            return Err(bad());
        }
        priorities.sort();
        priorities.dedup();
        kv::set(conn, PRIORITIES, &priorities)?;
    }
    let filters: BTreeMap<String, Vec<String>> = filters
        .into_iter()
        .map(|(k, vs)| (k, vs.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>()))
        .filter(|(_, vs)| !vs.is_empty())
        .collect();
    kv::set(conn, FILTERS, &filters)?;
    Ok(json!(filters))
}

/// Tags of every task and schedule, for a version's assignment diff.
fn assignments(conn: &Connection) -> Result<Vec<Assignment>> {
    let tasks = super::tasks::list(conn, "1")?.into_iter().map(|t| Assignment {
        id: t.id,
        before: t.tags,
        ..Default::default()
    });
    let schedules = super::schedules::list(conn)?.into_iter().map(|s| Assignment {
        id: s.id,
        schedule: true,
        before: s.tags,
        ..Default::default()
    });
    Ok(tasks.chain(schedules).collect())
}

fn set_tags(conn: &Connection, row: &Assignment, tags: &[Tag]) -> Result<()> {
    let at = time::now();
    if row.schedule {
        conn.execute(
            "UPDATE schedules SET tags = ?2, updated = max(?3, updated + 1) WHERE id = ?1",
            params![row.id, serde_json::to_string(tags)?, at],
        )?;
    } else if let Some(mut t) = super::tasks::get(conn, &row.id)? {
        t.tags = tags.to_vec();
        t.updated = at.max(t.updated + 1);
        super::tasks::save(conn, &t)?;
    }
    Ok(())
}

pub fn tag_decide(conn: &Connection, v: &Value) -> Result<Value> {
    let history = versions(conn)?;
    if v["expected"].as_str() != history.last().map(|r| r.id.as_str()) {
        crate::bail!("标签版本已变化，请刷新后重试");
    }
    let action = v["action"].as_str().unwrap_or("");
    let (mut catalog, mut priority) = catalog(conn)?;
    let mut assignments = Vec::new();
    let reason;
    if action == "display_priority" {
        let name = v["name"].as_str().ok_or("缺少标签类型")?;
        let value = v["display_priority"]
            .as_i64()
            .filter(|v| (-10000..=10000).contains(v))
            .ok_or("标签优先级需为 -10000 至 10000 的整数，数值越小越靠前")? as i32;
        let group = catalog.iter_mut().find(|g| g.name == name).ok_or("标签类型不存在")?;
        if group.display_priority == Some(value) {
            return snapshot(conn);
        }
        group.display_priority = Some(value);
        reason = format!("调整标签类型 {name} 的显示优先级为 {value}");
    } else if action == "rollback" {
        let target = history.iter().position(|r| Some(r.id.as_str()) == v["id"].as_str()).ok_or("标签版本不存在")?;
        let version = &history[target];
        catalog = version.groups.clone();
        priority = version.priority_kind.clone();
        let mut current: BTreeMap<(bool, String), Vec<Tag>> =
            self::assignments(conn)?.into_iter().map(|a| ((a.schedule, a.id), a.before)).collect();
        let before = current.clone();
        for later in history[target + 1..].iter().rev() {
            for row in later.assignments.iter().rev() {
                if let Some(tags) = current.get_mut(&(row.schedule, row.id.clone())) {
                    if *tags == row.after {
                        *tags = row.before.clone();
                    }
                }
            }
        }
        for ((schedule, id), tags) in current {
            let old = &before[&(schedule, id.clone())];
            if *old != tags {
                let row = Assignment { id, schedule, before: old.clone(), after: tags };
                set_tags(conn, &row, &row.after)?;
                assignments.push(row);
            }
        }
        reason = format!("回滚至 {}", version.id);
    } else {
        let id = v["id"].as_str().unwrap_or("");
        let row: Option<(String, String, String, String)> = conn
            .query_row("SELECT kind, value, reason, status FROM tag_proposals WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let (kind, value, why, status) = row.ok_or("标签建议不存在")?;
        if status != "pending" {
            crate::bail!("此建议已处理");
        }
        if action == "reject" {
            conn.execute("UPDATE tag_proposals SET status = 'rejected' WHERE id = ?1", [id])?;
            return Ok(json!(true));
        }
        if action != "adopt" {
            crate::bail!("不支持的标签操作");
        }
        let tag: Tag = if v.get("tag").is_some() { from_json(&v["tag"])? } else { Tag { kind, value } };
        if !valid(&tag) {
            crate::bail!("标签类型需1–24字，枚举值需1–40字");
        }
        if let Some(g) = catalog.iter_mut().find(|g| g.name == tag.kind) {
            if !g.values.contains(&tag.value) {
                if g.values.len() >= 200 {
                    crate::bail!("单种标签最多200个枚举值");
                }
                g.values.push(tag.value.clone());
            }
        } else {
            if catalog.len() >= 32 {
                crate::bail!("最多32种标签类型");
            }
            catalog.push(Group { display_priority: None, name: tag.kind.clone(), values: vec![tag.value.clone()] });
        }
        conn.execute("UPDATE tag_proposals SET status = 'adopted' WHERE id = ?1", [id])?;
        reason = format!("采用标签建议：{} · {}；{}", tag.kind, tag.value, why);
    }
    prune_filters(conn, &catalog)?;
    let version = Version {
        priority_kind: priority,
        assignments,
        id: new_id(),
        created: time::now(),
        action: action.into(),
        reason: reason.clone(),
        groups: catalog,
    };
    rows::put(conn, "tag_versions", "id", &version)?;
    audit::write(conn, "tag_version", "", &reason, &Value::Null, &json!(version))?;
    Ok(json!(version))
}

/// Queue a proposal; the analysis domain's intake calls this after it has
/// checked the evidence. A tag already proposed gains evidence instead.
pub fn propose(conn: &Connection, tag: &Tag, reason: &str, evidence: &[(String, String)]) -> Result<()> {
    let existing: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT id, status, evidence, excerpts FROM tag_proposals
             WHERE kind = ?1 AND value = ?2 AND status <> 'adopted' ORDER BY seq LIMIT 1",
            params![tag.kind, tag.value],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((id, status, ids, texts)) = existing {
        if status == "pending" {
            let mut ids: Vec<String> = serde_json::from_str(&ids)?;
            let mut texts: Vec<String> = serde_json::from_str(&texts)?;
            for (i, e) in evidence {
                if ids.len() < 20 && !ids.contains(i) {
                    ids.push(i.clone());
                    texts.push(e.clone());
                }
            }
            conn.execute(
                "UPDATE tag_proposals SET evidence = ?2, excerpts = ?3 WHERE id = ?1",
                params![id, serde_json::to_string(&ids)?, serde_json::to_string(&texts)?],
            )?;
        }
        return Ok(());
    }
    let pending: i64 =
        conn.query_row("SELECT count(*) FROM tag_proposals WHERE status = 'pending'", [], |r| r.get(0))?;
    if pending < 200 {
        let (ids, texts): (Vec<_>, Vec<_>) = evidence.iter().cloned().unzip();
        conn.execute(
            "INSERT INTO tag_proposals(id, kind, value, reason, evidence, excerpts, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                new_id(),
                tag.kind,
                tag.value,
                reason.chars().take(800).collect::<String>(),
                serde_json::to_string(&ids)?,
                serde_json::to_string(&texts)?,
                time::now()
            ],
        )?;
    }
    Ok(())
}
