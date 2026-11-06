//! Typed scalars in the `kv` table: scan checkpoints, revision counters and
//! daily markers. Values are JSON text so a key can hold any small value.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

use crate::Result;

pub fn get<T: DeserializeOwned>(conn: &Connection, key: &str) -> Result<Option<T>> {
    let text: Option<String> = conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional()?;
    Ok(match text {
        Some(t) => serde_json::from_str(&t).ok(),
        None => None,
    })
}

pub fn get_i64(conn: &Connection, key: &str) -> Result<i64> {
    Ok(get::<i64>(conn, key)?.unwrap_or(0))
}

pub fn set<T: Serialize + ?Sized>(conn: &Connection, key: &str, value: &T) -> Result<()> {
    conn.execute(
        "INSERT INTO kv(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

/// Increment a counter and return the new value.
pub fn bump(conn: &Connection, key: &str) -> Result<i64> {
    let next = get_i64(conn, key)? + 1;
    set(conn, key, &next)?;
    Ok(next)
}

#[cfg(test)]
mod tests;
