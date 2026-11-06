//! The runtime log table and the logs page timeline over it and the task
//! audit.

use rusqlite::types::Value as Sql;
use rusqlite::{functions::Context, params, Connection};
use serde_json::{json, Value};

use super::pages::Page;
use super::Db;
use crate::activity::{matches_filter, matches_purpose, runtime, timeline_row, FILTERS, PURPOSES};
use crate::{bail, tasks, time, Result};

/// Append a log row in the caller's transaction.
pub fn log(conn: &Connection, kind: &str, detail: &Value) -> Result<i64> {
    let at = time::now();
    conn.execute(
        "INSERT INTO activity(time, day, kind, detail) VALUES (?1, ?2, ?3, ?4)",
        params![at, time::date_from(at), kind, detail.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The newest `limit` rows, newest first, as `{id,time,kind,detail}`.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare_cached("SELECT id, time, kind, detail FROM activity ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        let detail: String = r.get(3)?;
        Ok(json!({
            "id": r.get::<_, i64>(0)?.to_string(),
            "time": r.get::<_, i64>(1)?,
            "kind": r.get::<_, String>(2)?,
            "detail": serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null),
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl Db {
    /// Append a log row on its own; errors are dropped on purpose.
    pub async fn record_activity(&self, kind: &'static str, detail: Value) {
        let _ = self.log(kind, detail).await;
    }

    /// Append a log row on its own.
    pub async fn log(&self, kind: &'static str, detail: Value) -> Result<()> {
        self.write(move |tx| log(tx, kind, &detail).map(drop)).await
    }

    /// [`Db::log`] from a blocking thread.
    pub fn log_blocking(&self, kind: &'static str, detail: Value) -> Result<()> {
        self.write_blocking(move |tx| log(tx, kind, &detail).map(drop))
    }

    /// Row counts and last ids behind `activity_day_revision`.
    pub async fn activity_counts(&self, day: String, start: i64) -> Result<[i64; 6]> {
        self.read(move |c| revision_counts(c, &day, start)).await
    }

    /// One page of `activity_query` for `day` starting at `start`.
    pub async fn activity_page(&self, p: Value, day: String, start: i64) -> Result<Value> {
        self.read(move |c| page(c, &p, &day, start)).await
    }
}

fn revision_counts(c: &Connection, day: &str, start: i64) -> Result<[i64; 6]> {
    let (logs, last_log) =
        c.query_row("SELECT count(*), coalesce(max(id), 0) FROM activity WHERE day = ?1", [day], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let (audits, last_audit) = c.query_row(
        "SELECT count(*), coalesce(max(seq), 0) FROM task_audit WHERE time >= ?1 AND time < ?2",
        params![start, start + time::DAY],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (tasks, updated) =
        c.query_row("SELECT count(*), coalesce(max(updated), 0) FROM tasks", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok([logs, last_log, audits, last_audit, tasks, updated])
}

/// One day's timeline: runtime log rows (an AI start only while no outcome
/// for the same request exists) and task audit rows, with the labels of the
/// tasks they name. `?1` is the day, `?2..?3` its time range.
const TIMELINE: &str = "WITH timeline AS (
    SELECT a.time, '0:' || printf('%020d', a.id) AS position, 0 AS audit, a.id AS seq, '' AS token, a.kind,
        a.detail, NULL AS change_summary, NULL AS before, NULL AS after,
        CASE json_type(a.detail, '$.task_id') WHEN 'text' THEN json_extract(a.detail, '$.task_id') ELSE '' END AS task_id
    FROM activity AS a
    WHERE a.day = ?1 AND activity_runtime(a.kind)
      AND NOT (a.kind = 'ai_start' AND json_type(a.detail, '$.request_id') = 'text' AND EXISTS(SELECT 1 FROM activity AS o
          WHERE o.day = ?1 AND o.kind IN ('ai_success', 'ai_failed', 'ai_cancelled')
            AND json_extract(o.detail, '$.request_id') = json_extract(a.detail, '$.request_id')))
    UNION ALL
    SELECT time, '1:' || token, 1, seq, token, kind, detail, change_summary, before, after, task_id
    FROM task_audit WHERE time >= ?2 AND time < ?3
)
SELECT l.*, k.title AS task_title, k.number AS task_number
FROM timeline AS l LEFT JOIN tasks AS k ON k.id = l.task_id
WHERE l.time >= ?2 AND l.time < ?3
  AND activity_shown(l.audit, l.kind, l.detail, l.before, l.after, l.task_id, k.title, k.number, ?4, ?5, ?6)";

pub(super) fn runtime_kind(ctx: &Context) -> rusqlite::Result<bool> {
    Ok(runtime(&ctx.get::<String>(0)?))
}

/// SQL `activity_shown(audit, kind, detail, before, after, task_id, title,
/// number, filter, purpose, terms)`: whether the logs page lists the row.
pub(super) fn shown(ctx: &Context) -> rusqlite::Result<bool> {
    let label = match (ctx.get::<Option<String>>(6)?, ctx.get::<Option<i64>>(7)?) {
        (Some(title), Some(number)) => Some((title, number)),
        _ => None,
    };
    let row = timeline_row(ctx.get(0)?, ctx.get(1)?, ctx.get(2)?, ctx.get(3)?, ctx.get(4)?, ctx.get(5)?, label);
    let filter: String = ctx.get(8)?;
    let terms: Vec<String> = ctx.get::<String>(10)?.split_whitespace().map(str::to_owned).collect();
    Ok(matches_filter(&row, &filter)
        && (filter != "ai" || matches_purpose(&row, &ctx.get::<String>(9)?))
        && tasks::query::matches(&row, &terms))
}

fn page(conn: &Connection, p: &Value, day: &str, start: i64) -> Result<Value> {
    let filter = p["filter"].as_str().unwrap_or("ai");
    if !FILTERS.contains(&filter) {
        bail!("日志筛选无效");
    }
    let purpose = p["purpose"].as_str().unwrap_or("all");
    if !PURPOSES.contains(&purpose) {
        bail!("AI 用途筛选无效");
    }
    let terms = tasks::query::terms(p)?.join(" ");
    let source = format!("SELECT *, time AS k0, position AS k1 FROM ({TIMELINE})");
    let params: Vec<Sql> = vec![
        day.to_string().into(),
        start.into(),
        (start + time::DAY).into(),
        filter.to_string().into(),
        purpose.to_string().into(),
        terms.into(),
    ];
    let mut out = Page {
        source: &source,
        params,
        keys: 2,
        descending: true,
        cursor: p.get("cursor"),
        page: p["page"].as_u64().unwrap_or(1).min(1000) as usize,
        size: p["page_size"].as_u64().unwrap_or(50).clamp(1, 100) as usize,
    }
    .run(conn, |r| {
        let audit: bool = r.get("audit")?;
        let label = match (r.get::<_, Option<String>>("task_title")?, r.get::<_, Option<i64>>("task_number")?) {
            (Some(title), Some(number)) => Some((title, number)),
            _ => None,
        };
        let mut row =
            timeline_row(audit, r.get("kind")?, r.get("detail")?, r.get("before")?, r.get("after")?, r.get("task_id")?, label);
        let seq: i64 = r.get("seq")?;
        row["time"] = json!(r.get::<_, i64>("time")?);
        row["position"] = json!(r.get::<_, String>("position")?);
        if audit {
            row["index"] = json!(seq);
            row["id"] = json!(r.get::<_, String>("token")?);
            row["kind_label"] = json!(tasks::query::label(row["kind"].as_str().unwrap_or("")));
            row["change_summary"] = json!(r.get::<_, Option<String>>("change_summary")?);
        } else {
            row["id"] = json!(seq.to_string());
        }
        Ok(row)
    })?;
    out["day"] = json!(day);
    Ok(out)
}
