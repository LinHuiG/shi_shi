//! Retention of what archiving has processed: audit rows, archived tasks
//! and old archive history.

use std::collections::BTreeSet;

use rusqlite::{params, Connection};
use serde_json::Value;

use super::{attempts, groups, max_seq, outcome};
use crate::archive::ARCHIVE_CURSOR;
use crate::data::{kv, retention, rows};
use crate::{time, Result};

pub(crate) const HISTORY_DAYS: i64 = 90;

/// Audit rows something still cites: rule evidence (live and in archive
/// groups) and prompt review evidence.
fn protected_audit(conn: &Connection) -> Result<BTreeSet<i64>> {
    let mut cited: Vec<Value> = Vec::new();
    for rule in crate::data::rules::list(conn)?.iter().chain(groups(conn)?.iter().flat_map(|g| &g.active_rules)) {
        cited.extend(rule["evidence"].as_array().into_iter().flatten().cloned());
    }
    for text in rows::column::<String>(conn, "SELECT evidence FROM prompt_reviews", [])? {
        let evidence: Value = serde_json::from_str(&text).unwrap_or_default();
        for row in evidence["audit"].as_array().into_iter().flatten() {
            cited.extend([row["audit_index"].clone(), row["evidence_id"].clone()]);
        }
    }
    let seq = |v: &Value| v.as_i64().or_else(|| v.as_str()?.strip_prefix("audit:")?.parse().ok());
    Ok(cited.iter().filter_map(seq).collect())
}

/// Remove expired audit rows and archived tasks, then old archive history.
/// Never removes audit rows the archive has not processed, nor the newest
/// row (its seq would otherwise be reused and slip past the cursors).
pub(crate) fn prune(conn: &Connection, days: u32, at: i64) -> Result<bool> {
    let cutoff = at - i64::from(days) * time::DAY;
    let protected = protected_audit(conn)?;
    let rows: Vec<(i64, i64, String)> = conn
        .prepare_cached(
            "SELECT seq, time, kind FROM task_audit WHERE seq <= ?1 AND seq < ?2 AND time < ?3 ORDER BY seq",
        )?
        .query_map(params![kv::get_i64(conn, ARCHIVE_CURSOR)?, max_seq(conn)?, cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .filter(|row| row.as_ref().map_or(true, |(seq, _, _)| !protected.contains(seq)))
        .collect::<rusqlite::Result<_>>()?;
    for (seq, _, _) in &rows {
        conn.execute("DELETE FROM task_audit WHERE seq = ?1", [seq])?;
    }
    retention::add(conn, "records", "kinds", rows.iter().map(|(_, t, k)| (*t, k.as_str())))?;
    let at_sql = "CASE WHEN archived_at > 0 THEN archived_at ELSE updated END";
    let tasks: Vec<(String, i64, String)> = conn
        .prepare_cached(&format!(
            "SELECT id, {at_sql}, status FROM tasks WHERE archived = 1 AND {at_sql} < ?1 AND id NOT IN ({})",
            crate::data::tasks::TOOL_TASK_IDS
        ))?
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, _, _) in &tasks {
        conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
    }
    retention::add(conn, "tasks", "statuses", tasks.iter().map(|(_, t, s)| (*t, s.as_str())))?;
    let history = prune_history(conn, at)?;
    Ok(!rows.is_empty() || !tasks.is_empty() || history)
}

/// Keep groups of the last 90 days, the newest, those a report cites, and
/// everything they were restored from; attempts those groups name, open or
/// recent ones, the newest and the latest outcome; open, recent and
/// latest-outcome morning jobs.
pub(crate) fn prune_history(conn: &Connection, at: i64) -> Result<bool> {
    let cutoff = at - HISTORY_DAYS * time::DAY;
    let latest = outcome(conn)?.map(|o| (o.kind, o.id));
    let groups = groups(conn)?;
    let mut keep_groups: BTreeSet<String> =
        groups.iter().filter(|g| g.created >= cutoff).map(|g| g.id.clone()).collect();
    keep_groups.extend(groups.last().map(|g| g.id.clone()));
    let cited = "SELECT body ->> '$.archive_group_id' FROM reports WHERE json_type(body, '$.archive_group_id') = 'text'";
    keep_groups.extend(rows::column::<String>(conn, cited, [])?);
    // Restore chains, newest first: each kept group keeps its source.
    for g in groups.iter().rev() {
        if keep_groups.contains(&g.id) && !g.restored_from.is_empty() {
            keep_groups.insert(g.restored_from.clone());
        }
    }
    let attempts = attempts(conn)?;
    let mut keep_attempts: BTreeSet<String> =
        groups.iter().filter(|g| keep_groups.contains(&g.id)).map(|g| g.attempt_id.clone()).collect();
    keep_attempts.extend(attempts.iter().filter(|a| a.unfinished() || a.created >= cutoff).map(|a| a.id.clone()));
    keep_attempts.extend(attempts.last().map(|a| a.id.clone()));
    let mut changed = 0;
    for g in groups.iter().filter(|g| !keep_groups.contains(&g.id)) {
        changed += conn.execute("DELETE FROM archive_groups WHERE id = ?1", [&g.id])?;
    }
    for a in attempts.iter().filter(|a| !keep_attempts.contains(&a.id) && latest != Some(("archive", a.id.clone()))) {
        changed += conn.execute("DELETE FROM archive_attempts WHERE id = ?1", [&a.id])?;
    }
    let latest_job = latest.filter(|(kind, _)| *kind == "morning").map(|(_, id)| id);
    changed += conn.execute(
        "DELETE FROM morning_jobs WHERE status NOT IN ('queued', 'running') AND business_day < ?1 AND id IS NOT ?2",
        params![time::date_from(cutoff), latest_job],
    )?;
    Ok(changed > 0)
}
