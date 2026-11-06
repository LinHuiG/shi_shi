//! The change log shared by every domain (`task_audit`). Rows are written in
//! the caller's transaction so a change and its record commit together.

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::{time, Result};

fn snapshot(value: &Value) -> Option<String> {
    (!value.is_null()).then(|| value.to_string())
}

/// Append an audit row and return its `seq`.
pub fn write(conn: &Connection, kind: &str, task_id: &str, detail: &str, before: &Value, after: &Value) -> Result<i64> {
    conn.execute(
        "INSERT INTO task_audit(token, time, kind, task_id, detail, before, after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            uuid::Uuid::new_v4().to_string(),
            time::now(),
            kind,
            task_id,
            detail,
            snapshot(before),
            snapshot(after)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}
