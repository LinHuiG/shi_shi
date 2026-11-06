//! The retention digest (`state.retention_digest`): what retention removed,
//! kept as counts, time ranges and the top categories only. Messages,
//! audit records and archived tasks each add their section.

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use super::kv;
use crate::Result;

pub const DIGEST_LIMIT: usize = 8_000;
const KEY: &str = "retention_digest";
const SECTIONS: [&str; 3] = ["messages", "records", "tasks"];

fn chars(value: &Value) -> usize {
    value.to_string().chars().count()
}

fn section<'a>(digest: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !digest.is_object() {
        *digest = json!({"messages": {}, "records": {}, "tasks": {}});
    }
    let object = digest.as_object_mut().unwrap();
    if !object.get(key).is_some_and(Value::is_object) {
        object.insert(key.into(), json!({}));
    }
    object.get_mut(key).unwrap().as_object_mut().unwrap()
}

/// Add removed rows `(time, category)` to `key`; categories go to `field`
/// with the eight largest kept. Returns whether anything was added.
pub fn record<S: AsRef<str>>(
    digest: &mut Value,
    key: &str,
    field: &str,
    rows: impl IntoIterator<Item = (i64, S)>,
) -> bool {
    let (mut first, mut last, mut count) = (i64::MAX, i64::MIN, 0u64);
    let mut groups = BTreeMap::<String, u64>::new();
    for (at, name) in rows {
        first = first.min(at);
        last = last.max(at);
        count += 1;
        *groups.entry(name.as_ref().to_owned()).or_default() += 1;
    }
    if count == 0 {
        return false;
    }
    let s = section(digest, key);
    let total = s.get("count").and_then(Value::as_u64).unwrap_or(0) + count;
    s.insert("count".into(), total.into());
    let first = s.get("first").and_then(Value::as_i64).map_or(first, |v| v.min(first));
    s.insert("first".into(), first.into());
    let last = s.get("last").and_then(Value::as_i64).map_or(last, |v| v.max(last));
    s.insert("last".into(), last.into());
    let mut merged: BTreeMap<String, u64> = s
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["name"].as_str()?.to_string(), row["count"].as_u64()?)))
        .collect();
    for (name, n) in groups {
        *merged.entry(name).or_default() += n;
    }
    let mut rows: Vec<_> = merged.into_iter().collect();
    rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    rows.truncate(8);
    s.insert(field.into(), rows.into_iter().map(|(name, count)| json!({"name": name, "count": count})).collect());
    true
}

/// Keep the digest under [`DIGEST_LIMIT`]: drop categories, then time
/// ranges, then everything but the totals.
pub fn compact(digest: &mut Value) {
    if digest.is_null() || chars(digest) <= DIGEST_LIMIT {
        return;
    }
    for fields in [&["sources", "kinds", "statuses"][..], &["first", "last"]] {
        if let Some(object) = digest.as_object_mut() {
            for key in SECTIONS {
                if let Some(s) = object.get_mut(key).and_then(Value::as_object_mut) {
                    for field in fields {
                        s.remove(*field);
                    }
                }
            }
            object.insert("compressed".into(), true.into());
        }
        if chars(digest) <= DIGEST_LIMIT {
            return;
        }
    }
    let mut reduced: Map<String, Value> = SECTIONS
        .into_iter()
        .map(|key| (key.into(), json!({"count": digest[key]["count"].as_u64().unwrap_or(0)})))
        .collect();
    reduced.insert("compressed".into(), true.into());
    *digest = Value::Object(reduced);
}

pub fn load(conn: &Connection) -> Result<Value> {
    Ok(kv::get(conn, KEY)?.unwrap_or(Value::Null))
}

/// Record removed rows in the stored digest, in the caller's transaction.
pub fn add<S: AsRef<str>>(
    conn: &Connection,
    key: &str,
    field: &str,
    rows: impl IntoIterator<Item = (i64, S)>,
) -> Result<()> {
    let mut digest = load(conn)?;
    if record(&mut digest, key, field, rows) {
        compact(&mut digest);
        kv::set(conn, KEY, &digest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
