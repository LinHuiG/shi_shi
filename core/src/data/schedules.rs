//! Recurring task templates (`schedules`) and the generation of today's
//! tasks from them.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::tasks::store;
use super::{audit, rows, Db};
use crate::tasks::model::{self, Task};
use crate::tasks::schedule::{due_today, next_occurrence, parse, Schedule};
use crate::{time, Result};

impl Db {
    pub async fn edit_schedule(&self, p: Value) -> Result<Value> {
        self.write(move |tx| edit(tx, &p)).await
    }

    pub async fn delete_schedule(&self, p: Value) -> Result<Value> {
        self.write(move |tx| delete(tx, &p)).await
    }
}

pub fn list(conn: &Connection) -> Result<Vec<Schedule>> {
    rows::all(conn, "SELECT * FROM schedules ORDER BY created, id", [])
}

fn get(conn: &Connection, id: &str) -> Result<Option<Schedule>> {
    rows::one(conn, "SELECT * FROM schedules WHERE id = ?1", [id])
}

fn save(conn: &Connection, s: &Schedule) -> Result<()> {
    rows::put(conn, "schedules", "id", s)
}

/// `schedules` of the snapshot, each with its `next_at`.
pub fn snapshot(conn: &Connection, at: i64) -> Result<Value> {
    Ok(list(conn)?
        .into_iter()
        .map(|s| {
            let mut v = json!(s);
            v["next_at"] = json!(next_occurrence(&s, at));
            v
        })
        .collect())
}

pub fn edit(conn: &Connection, v: &Value) -> Result<Value> {
    let mut schedule = parse(&v["schedule"])?;
    let at = time::now();
    let before = if schedule.id.is_empty() {
        schedule.id = model::new_id();
        schedule.created = at;
        schedule.updated = at;
        schedule.active_from = at;
        schedule.last_generated_on.clear();
        Value::Null
    } else {
        let old = get(conn, &schedule.id)?.ok_or("定时事项不存在")?;
        if old.updated != schedule.updated {
            crate::bail!("定时事项已更新，请重新打开编辑");
        }
        schedule.created = old.created;
        schedule.updated = at.max(old.updated + 1);
        schedule.last_generated_on = old.last_generated_on.clone();
        schedule.active_from = if schedule.frequency != old.frequency
            || schedule.weekdays != old.weekdays
            || schedule.time != old.time
            || (schedule.enabled && !old.enabled)
        {
            at
        } else {
            old.active_from
        };
        json!(old)
    };
    save(conn, &schedule)?;
    audit::write(conn, "schedule_edit", "", "保存定时事项", &before, &json!(schedule))?;
    Ok(json!(schedule))
}

pub fn delete(conn: &Connection, v: &Value) -> Result<Value> {
    let old = get(conn, v["id"].as_str().unwrap_or(""))?.ok_or("定时事项不存在")?;
    if Some(old.updated) != v["updated"].as_i64() {
        crate::bail!("定时事项已更新，请刷新后重试");
    }
    conn.execute("DELETE FROM schedules WHERE id = ?1", [&old.id])?;
    audit::write(conn, "schedule_delete", "", "删除定时事项，保留已生成的事项", &json!(old), &Value::Null)?;
    Ok(json!(true))
}

/// The earliest time a schedule falls due, for the background loop.
pub fn next_wake(conn: &Connection, at: i64) -> Result<Option<i64>> {
    Ok(list(conn)?.iter().filter_map(|s| next_occurrence(s, at)).min())
}

pub fn has_due(conn: &Connection, at: i64) -> Result<bool> {
    Ok(list(conn)?.iter().any(|s| due_today(s, at).is_some()))
}

/// Create today's task for every due schedule. The day marker and the
/// unique `(schedule_id, scheduled_for)` index keep it to one per day.
pub fn generate(conn: &Connection, at: i64) -> Result<usize> {
    let day = time::stamp(at)[..10].to_string();
    let mut count = 0;
    for mut s in list(conn)? {
        let Some(due) = due_today(&s, at) else { continue };
        s.last_generated_on = day.clone();
        save(conn, &s)?;
        if conn
            .prepare_cached("SELECT 1 FROM tasks WHERE schedule_id = ?1 AND scheduled_for = ?2")?
            .exists(params![s.id, day])?
        {
            continue;
        }
        let mut task = Task {
            id: model::new_id(),
            title: s.title.clone(),
            description: s.description.clone(),
            tags: s.tags.clone(),
            status: "待处理".into(),
            manual: true,
            follow_deadline: true,
            created: at,
            updated: at,
            schedule_id: s.id.clone(),
            scheduled_for: day.clone(),
            reason: format!("定时生成 · {}（上海时间）", time::stamp(due)),
            ..Default::default()
        };
        model::normalize(&mut task);
        let task = store(conn, &task)?;
        audit::write(conn, "schedule_generate", &task.id, "定时生成事项", &Value::Null, &task.payload())?;
        count += 1;
    }
    Ok(count)
}
