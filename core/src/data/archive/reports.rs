//! Reports: their bodies with deduplicated display items, the catalogue,
//! the latest outcome and the first transaction of a report.

use rusqlite::{params, Connection, OptionalExtension, Params};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{audits, in_scope, max_seq, scope_since, scope_sql, status_counts};
use crate::archive::{new_id, pick, Attempt, Audit, MorningJob, REPORT_CURSOR};
use crate::data::{kv, rows};
use crate::{time, Error, Result};

pub(crate) const SECTIONS: [&str; 4] = ["tasks", "delta", "suggestions", "batch_summaries"];
pub(crate) const REPORT_LIMIT: usize = 60;
const REPORTS_REVISION: &str = "reports_revision";

/// The fields a report keeps of one section row.
pub(crate) fn display_row(section: &str, row: &Value) -> Value {
    pick(
        row,
        match section {
            "tasks" => &[
                "id",
                "updated",
                "title",
                "status",
                "archived",
                "merged_into",
                "generation",
                "blocker",
                "waiting_for",
                "deferred_until",
                "archive_note",
                "tags",
                "tag_catalog",
                "tag_snapshot",
                "effective_tags",
                "priority",
                "category",
                "environment",
            ],
            "delta" => &["time", "kind", "task_id", "detail", "change_summary"],
            "suggestions" => &["id", "title", "rule", "active"],
            _ => &["summary", "completed", "attention", "next_steps"],
        },
    )
}

pub(crate) fn item_key(section: &str, row: &Value) -> String {
    format!("{section}:{:x}", Sha256::digest(row.to_string().as_bytes()))
}

/// The item key a row is stored under, if it is one.
fn key_of<'a>(section: &str, v: &'a Value) -> Option<&'a str> {
    v.as_str().filter(|k| k.starts_with(&format!("{section}:")) && k.len() == section.len() + 65)
}

/// The item keys of a section in a stored or unstored report.
fn keys(report: &Value, section: &str) -> Vec<String> {
    let rows = report[section].as_array().into_iter().flatten();
    rows.map(|row| key_of(section, row).map_or_else(|| item_key(section, &display_row(section, row)), String::from))
        .collect()
}

/// Report bodies selected by `tail`.
pub(super) fn bodies(conn: &Connection, tail: &str, p: impl Params) -> Result<Vec<Value>> {
    let texts: Vec<String> = rows::column(conn, &format!("SELECT body FROM reports {tail}"), p)?;
    texts.iter().map(|b| Ok(serde_json::from_str(b)?)).collect()
}

/// Reports newest first.
#[cfg(test)]
pub(crate) fn reports(conn: &Connection) -> Result<Vec<Value>> {
    bodies(conn, "ORDER BY seq DESC", [])
}

/// The top-level `keys` of the report bodies selected by `tail`, read
/// without parsing whole bodies; an absent key reads as null.
fn report_fields(conn: &Connection, keys: &[&str], tail: &str) -> Result<Vec<Value>> {
    let fields: Vec<String> = keys.iter().map(|k| format!("'{k}', body -> '$.{k}'")).collect();
    let sql = format!("SELECT json_object({}) FROM reports {tail}", fields.join(", "));
    let texts: Vec<String> = rows::column(conn, &sql, [])?;
    texts.iter().map(|b| Ok(serde_json::from_str(b)?)).collect()
}

pub(crate) fn first_report(conn: &Connection) -> Result<Option<Value>> {
    Ok(bodies(conn, "ORDER BY seq DESC LIMIT 1", [])?.pop())
}

/// Store a report: replace the row with the same id in place, or insert it
/// as the newest, with its section rows as item keys. Changes the catalogue
/// revision. Returns the stored body.
pub(crate) fn put_report(conn: &Connection, report: &Value) -> Result<Value> {
    let mut body = report.clone();
    for section in SECTIONS {
        for row in body.get_mut(section).and_then(Value::as_array_mut).into_iter().flatten() {
            if key_of(section, row).is_none() {
                let value = display_row(section, row);
                let key = item_key(section, &value);
                conn.execute(
                    "INSERT OR IGNORE INTO report_items(key, row) VALUES (?1, ?2)",
                    params![key, value.to_string()],
                )?;
                *row = json!(key);
            }
        }
    }
    body["storage_version"] = json!(1);
    let (id, time, text) = (body["id"].as_str().unwrap_or(""), body["time"].as_i64().unwrap_or(0), body.to_string());
    if conn.execute("UPDATE reports SET time = ?2, body = ?3 WHERE id = ?1", params![id, time, text])? == 0 {
        conn.execute("INSERT INTO reports(id, time, body) VALUES (?1, ?2, ?3)", params![id, time, text])?;
    }
    kv::set(conn, REPORTS_REVISION, &new_id())?;
    Ok(body)
}

/// Keep [`REPORT_LIMIT`] reports. When the report that is the latest
/// outcome would fall off, keep it as the oldest instead. Drops display
/// items no report cites.
pub(crate) fn truncate(conn: &Connection) -> Result<()> {
    let keep = outcome(conn)?.filter(|o| o.kind == "report").map(|o| o.id);
    let ids: Vec<String> = rows::column(conn, "SELECT id FROM reports ORDER BY seq DESC", [])?;
    if ids.len() > REPORT_LIMIT {
        let kept: Vec<&String> = match keep.and_then(|k| ids.iter().position(|id| *id == k)) {
            Some(p) if p >= REPORT_LIMIT => ids[..REPORT_LIMIT - 1].iter().chain([&ids[p]]).collect(),
            _ => ids[..REPORT_LIMIT].iter().collect(),
        };
        conn.execute("DELETE FROM reports WHERE id NOT IN (SELECT value FROM json_each(?1))", [json!(kept).to_string()])?;
        kv::set(conn, REPORTS_REVISION, &new_id())?;
    }
    conn.execute(
        "DELETE FROM report_items WHERE key NOT IN (
            SELECT j.value FROM reports r, json_each(r.body, '$.tasks') j
            UNION SELECT j.value FROM reports r, json_each(r.body, '$.delta') j
            UNION SELECT j.value FROM reports r, json_each(r.body, '$.suggestions') j
            UNION SELECT j.value FROM reports r, json_each(r.body, '$.batch_summaries') j)",
        [],
    )?;
    Ok(())
}

/// Display rows `[start, end)` of a section; a missing item reads as
/// `{unavailable:true}`.
pub(super) fn section_rows(conn: &Connection, report: &Value, section: &str, start: usize, end: usize) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for row in report[section].as_array().into_iter().flatten().take(end).skip(start) {
        out.push(match key_of(section, row) {
            None => display_row(section, row),
            Some(key) => {
                let text: Option<String> =
                    conn.query_row("SELECT row FROM report_items WHERE key = ?1", [key], |r| r.get(0)).optional()?;
                text.map_or(Ok(json!({"unavailable": true})), |t| serde_json::from_str(&t))?
            }
        });
    }
    Ok(out)
}

pub(crate) fn reports_revision(conn: &Connection) -> Result<String> {
    Ok(kv::get::<String>(conn, REPORTS_REVISION)?.unwrap_or_default())
}

/// The most recent finished archive, report or morning report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Outcome {
    pub kind: &'static str,
    pub id: String,
    pub label: String,
    pub status: String,
    pub finished: i64,
    pub error: String,
}

/// Later `finished` wins; at the same time a success wins.
pub(crate) fn outcome(conn: &Connection) -> Result<Option<Outcome>> {
    const DONE: &str = "WHERE status IN ('succeeded', 'failed') ORDER BY seq";
    let mut all = Vec::new();
    for a in rows::all::<Attempt>(conn, &format!("SELECT * FROM archive_attempts {DONE}"), [])? {
        let finished = a.finished.max(a.started).max(a.created);
        all.push(Outcome {
            kind: "archive",
            finished,
            id: a.id,
            label: "归档".into(),
            status: a.status,
            error: a.error,
        });
    }
    let keys = [
        "id",
        "label",
        "summary_status",
        "attempt_id",
        "remaining_events",
        "archive_complete",
        "finished",
        "time",
        "summary_error",
    ];
    for r in report_fields(conn, &keys, "ORDER BY seq")? {
        let status = match r["summary_status"].as_str() {
            Some("done" | "reused" | "fallback") => "succeeded",
            Some("failed" | "interrupted") => "failed",
            _ => continue,
        };
        if r["attempt_id"].as_str().is_some_and(|id| !id.is_empty())
            || r["remaining_events"].as_u64().unwrap_or(0) > 0
            || r["archive_complete"] == false
        {
            continue;
        }
        all.push(Outcome {
            kind: "report",
            id: r["id"].as_str().unwrap_or("").into(),
            label: r["label"].as_str().unwrap_or("报表").into(),
            status: status.into(),
            finished: r["finished"].as_i64().filter(|t| *t > 0).unwrap_or_else(|| r["time"].as_i64().unwrap_or(0)),
            error: r["summary_error"].as_str().unwrap_or("").into(),
        });
    }
    for j in rows::all::<MorningJob>(conn, &format!("SELECT * FROM morning_jobs {DONE}"), [])? {
        // A job without a completion time sorts at the start of its day.
        let finished = if j.finished > 0 { j.finished } else { time::day_start(&j.business_day).unwrap_or(0) };
        all.push(Outcome {
            kind: "morning",
            id: j.id,
            label: "早报".into(),
            status: j.status,
            finished,
            error: j.error,
        });
    }
    // Of equal candidates the last one wins.
    Ok(all.into_iter().max_by_key(|o| (o.finished, o.status == "succeeded")))
}

/// A completion time that sorts after the current outcome, so a new result
/// always replaces the old one even within a millisecond or after a clock
/// step back.
pub(crate) fn next_finished(conn: &Connection, at: i64) -> Result<i64> {
    Ok(outcome(conn)?.map_or(at, |o| at.max(o.finished.saturating_add(1))))
}

fn summary(report: &Value) -> Value {
    let mut out = pick(
        report,
        &[
            "id",
            "label",
            "date",
            "time",
            "ai_summary",
            "summary_status",
            "summary_error",
            "archived_tasks",
            "skipped_tasks",
            "remaining_events",
            "data_until",
            "pending_analysis",
        ],
    );
    out["id"] = report["id"].clone();
    for (field, count) in [
        ("tasks", "task_count"),
        ("delta", "delta_count"),
        ("suggestions", "suggestion_count"),
        ("batch_summaries", "batch_count"),
    ] {
        out[count] = json!(report[field].as_array().map_or(0, Vec::len));
    }
    out["status_counts"] = report["status_counts"].clone();
    out["previous_report"] = pick(&report["previous_report"], &["time"]);
    out["learning"] = pick(&report["learning"], &["message", "skipped"]);
    out
}

/// `reports` of the snapshot: the newest in summary form, the rest as
/// catalogue rows (only the newest report is shown open).
pub(crate) fn catalog(conn: &Connection) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = first_report(conn)?.iter().map(summary).collect();
    let sql =
        "SELECT body -> '$.label' AS label, body -> '$.date' AS date, body -> '$.time' AS time, body -> '$.id' AS id
         FROM reports ORDER BY seq DESC LIMIT -1 OFFSET 1";
    for mut item in rows::objects(conn, sql, [], &json!({"label": null, "date": null, "time": null, "id": null}))? {
        item.as_object_mut().unwrap().retain(|k, v| k == "id" || !v.is_null());
        out.push(item);
    }
    Ok(out)
}

/// `report_detail {id, revision, section, offset, limit}`.
pub(super) fn detail(conn: &Connection, p: &Value) -> Result<Value> {
    let revision = reports_revision(conn)?;
    if p["revision"].as_str() != Some(revision.as_str()) {
        crate::bail!("报表已更新，请刷新后展开");
    }
    let id = p["id"].as_str().ok_or("缺少报表编号")?;
    let report = bodies(conn, "WHERE id = ?1", [id])?.pop().ok_or("报表已不在当前历史中")?;
    let section = p["section"].as_str().unwrap_or("summary");
    if section == "summary" {
        return Ok(json!({"revision": revision, "report": summary(&report)}));
    }
    if !SECTIONS.contains(&section) {
        crate::bail!("未知报表明细");
    }
    let len = report[section].as_array().map_or(0, Vec::len);
    // The first batch summary repeats `ai_summary`.
    let total = if section == "batch_summaries" { len.saturating_sub(1) } else { len };
    let offset = p["offset"].as_u64().unwrap_or(0).min(total as u64) as usize;
    let end = offset.saturating_add(p["limit"].as_u64().unwrap_or(50).clamp(1, 100) as usize).min(total);
    let rows = section_rows(conn, &report, section, offset, end).map_err(|e| Error(format!("报表明细读取失败：{e}")))?;
    Ok(json!({"revision": revision, "rows": rows, "total": total, "offset": offset, "next_offset": end}))
}

/// Transaction A of a report: collect the delta since the last report,
/// store a pending (or reused) report and move the report cursor. Returns
/// the report with its full rows.
pub(crate) fn prepare(conn: &Connection, label: &str, at: i64) -> Result<Value> {
    let since = scope_since(conn)?;
    let delta =
        audits(conn, &format!("seq > {} AND {} ORDER BY seq", kv::get_i64(conn, REPORT_CURSOR)?, in_scope(since)))?;
    let previous = first_report(conn)?;
    let mut r = json!({
        "id": new_id(),
        "previous_report": previous.as_ref().map(|p| json!({"id": p["id"], "time": p["time"], "label": p["label"]})),
        "delta": delta.iter().map(Audit::json).collect::<Vec<_>>(),
        "suggestions": crate::data::rules::list(conn)?,
        "time": at,
        "label": label,
        "date": time::date_from(at),
        "data_until": kv::get_i64(conn, "last_scan")?,
        "pending_analysis": crate::data::analysis::unanalyzed(conn)?,
        "tasks": crate::data::tasks::list(conn, &scope_sql(since))?.iter().map(|t| t.payload()).collect::<Vec<_>>(),
        "status_counts": status_counts(conn)?,
    });
    let reuse = previous.filter(|old| {
        delta.is_empty()
            && keys(old, "tasks") == keys(&r, "tasks")
            && keys(old, "suggestions") == keys(&r, "suggestions")
            && old["ai_summary"].is_object()
    });
    if let Some(old) = reuse {
        r["ai_summary"] = old["ai_summary"].clone();
        r["summary_status"] = json!("reused");
        r["finished"] = json!(next_finished(conn, at)?);
    } else {
        r["summary_status"] = json!("pending");
    }
    kv::set(conn, REPORT_CURSOR, &max_seq(conn)?)?;
    put_report(conn, &r)?;
    truncate(conn)?;
    Ok(r)
}
