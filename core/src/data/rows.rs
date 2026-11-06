//! Rows as serde values, so no table needs a hand-written mapping. A record
//! reads its columns by field name and writes back the fields its table
//! has; booleans are stored as 0/1, lists and objects as JSON text.

use std::collections::BTreeMap;

use rusqlite::types::{FromSql, Type, Value as Sql, ValueRef};
use rusqlite::{params_from_iter, Connection, OptionalExtension, Params, Row};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};

use crate::Result;

/// A row as a JSON object. `shape` says how to read a column: a boolean
/// there turns 0/1 into a boolean, an array, object or null parses the text
/// as JSON; any other column keeps its SQL value.
pub(crate) fn object(r: &Row, shape: &Value) -> rusqlite::Result<Value> {
    let mut out = Map::new();
    for (i, name) in r.as_ref().column_names().into_iter().enumerate() {
        let hint = shape.get(name);
        let value = match r.get_ref(i)? {
            ValueRef::Integer(n) if matches!(hint, Some(Value::Bool(_))) => Value::Bool(n != 0),
            ValueRef::Integer(n) => n.into(),
            ValueRef::Real(f) => f.into(),
            ValueRef::Text(t) => {
                let t = String::from_utf8_lossy(t);
                match hint {
                    Some(Value::Array(_) | Value::Object(_) | Value::Null) => {
                        serde_json::from_str(&t).unwrap_or_else(|_| t.into())
                    }
                    _ => t.into(),
                }
            }
            ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        };
        out.insert(name.into(), value);
    }
    Ok(Value::Object(out))
}

/// A row as a record shaped like `T`'s default; NULL columns keep the default.
pub(crate) fn record<T: Serialize + DeserializeOwned + Default>(r: &Row) -> rusqlite::Result<T> {
    let mut v = object(r, &serde_json::to_value(T::default()).unwrap_or_default())?;
    v.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    serde_json::from_value(v).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, e.into()))
}

pub(crate) fn all<T: Serialize + DeserializeOwned + Default>(
    conn: &Connection,
    sql: &str,
    p: impl Params,
) -> Result<Vec<T>> {
    Ok(conn.prepare_cached(sql)?.query_map(p, record)?.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn one<T: Serialize + DeserializeOwned + Default>(
    conn: &Connection,
    sql: &str,
    p: impl Params,
) -> Result<Option<T>> {
    Ok(conn.prepare_cached(sql)?.query_row(p, record).optional()?)
}

/// Rows as [`object`]s.
pub(crate) fn objects(conn: &Connection, sql: &str, p: impl Params, shape: &Value) -> Result<Vec<Value>> {
    Ok(conn.prepare_cached(sql)?.query_map(p, |r| object(r, shape))?.collect::<rusqlite::Result<_>>()?)
}

/// One column of every row.
pub(crate) fn column<T: FromSql>(conn: &Connection, sql: &str, p: impl Params) -> Result<Vec<T>> {
    Ok(conn.prepare_cached(sql)?.query_map(p, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Two columns of every row, as a map from the first to the second.
pub(crate) fn pairs<K: FromSql + Ord, V: FromSql>(
    conn: &Connection,
    sql: &str,
    p: impl Params,
) -> Result<BTreeMap<K, V>> {
    Ok(conn.prepare_cached(sql)?.query_map(p, |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

fn sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or_else(|| Sql::Integer(i64::MAX), Sql::Integer),
        Value::String(s) => Sql::Text(s.clone()),
        _ => Sql::Text(v.to_string()),
    }
}

/// The columns of `table` that `value` has, with their SQL values.
fn fields(conn: &Connection, table: &str, value: &impl Serialize) -> Result<Vec<(String, Sql)>> {
    let value = serde_json::to_value(value)?;
    let names: Vec<String> = column(conn, "SELECT name FROM pragma_table_info(?1)", [table])?;
    Ok(names.into_iter().filter_map(|c| Some((sql(value.get(&c)?), c))).map(|(v, c)| (c, v)).collect())
}

fn join(fields: &[(String, Sql)], sep: &str, f: impl Fn(usize, &str) -> String) -> String {
    fields.iter().enumerate().map(|(i, (c, _))| f(i + 1, c)).collect::<Vec<_>>().join(sep)
}

/// Insert the fields of `value` that are columns of `table`, or update them
/// in the row whose `key` conflicts.
pub(crate) fn put(conn: &Connection, table: &str, key: &str, value: &impl Serialize) -> Result<()> {
    let f = fields(conn, table, value)?;
    let sql = format!(
        "INSERT INTO {table}({}) VALUES ({}) ON CONFLICT({key}) DO UPDATE SET {}",
        join(&f, ", ", |_, c| format!("\"{c}\"")),
        join(&f, ", ", |i, _| format!("?{i}")),
        join(&f, ", ", |_, c| format!("\"{c}\" = excluded.\"{c}\""))
    );
    conn.prepare_cached(&sql)?.execute(params_from_iter(f.into_iter().map(|(_, v)| v)))?;
    Ok(())
}

/// Set the fields of `value` that are columns of `table` in the rows where
/// the `key` fields match; returns how many rows changed.
pub(crate) fn update(conn: &Connection, table: &str, key: &[&str], value: &impl Serialize) -> Result<usize> {
    let (keys, set): (Vec<_>, Vec<_>) =
        fields(conn, table, value)?.into_iter().partition(|(c, _)| key.contains(&c.as_str()));
    let filter = join(&keys, " AND ", |i, c| format!("\"{c}\" = ?{}", i + set.len()));
    let sql = format!("UPDATE {table} SET {} WHERE {filter}", join(&set, ", ", |i, c| format!("\"{c}\" = ?{i}")));
    Ok(conn.prepare_cached(&sql)?.execute(params_from_iter(set.into_iter().chain(keys).map(|(_, v)| v)))?)
}
