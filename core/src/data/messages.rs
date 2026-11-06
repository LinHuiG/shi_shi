//! Message storage: upsert from scans, the inbox and evidence lists, manual
//! deletion with tombstones and retention. Filtered or searched fields are
//! real columns; the rest of a message is `payload`.

use rusqlite::types::Value as Sql;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Map, Value};

use super::pages::Page;
use super::{audit, kv, retention, rows, Db};
use crate::messages::{cutoff, fingerprint, Message, REVISION};
use crate::params::Params;
use crate::{time, Error, Result};

impl Db {
    /// `messages_query`: the inbox list, newest first by default.
    pub async fn query_messages(&self, p: Value, days: u32) -> Result<Value> {
        self.read(move |c| query(c, &p, days)).await
    }

    /// `messages_delete`.
    pub async fn delete_messages(&self, p: Value) -> Result<Value> {
        self.write(move |tx| delete(tx, &p)).await
    }

    /// `evidence_query`.
    pub async fn task_evidence(&self, p: Value) -> Result<Value> {
        self.read(move |c| evidence(c, &p)).await
    }

    pub async fn prune_messages(&self, days: u32) -> Result<bool> {
        self.write(move |tx| prune(tx, days)).await
    }

    /// The file behind a still visible message image.
    pub async fn image_path(&self, image: String) -> Result<Option<String>> {
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT i.path FROM message_images i JOIN messages m ON m.id = i.message_id \
                     WHERE i.image_id = ?1 AND m.withdrawn = 0",
                    [image],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }
}

/// Message ids that retention and deletion must keep: evidence of live tasks.
pub(crate) const PROTECTED: &str =
    "SELECT e.message_id FROM task_evidence e JOIN tasks t ON t.id = e.task_id WHERE t.archived = 0";

/// The columns [`from_row`] reads, from `messages` aliased `m`; `images`
/// carries the message's image references in order.
const COLUMNS: &str = "m.*, (SELECT json_group_array(json_object('id', image_id, 'path', path)) \
     FROM (SELECT image_id, path FROM message_images WHERE message_id = m.id ORDER BY idx)) AS images";

/// The fields of a message that live in `payload` rather than in columns.
const PAYLOAD: [&str; 9] = ["from", "recipient", "to", "kind", "mention_me", "mention_all", "private", "mine", "reply"];

/// A message from a row selected with [`COLUMNS`].
fn from_row(r: &Row) -> Result<Message> {
    let mut v = rows::object(r, &json!({"images": [], "payload": {}, "withdrawn": false}))?;
    if let Value::Object(extra) = v["payload"].take() {
        v.as_object_mut().unwrap().extend(extra);
    }
    v.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    crate::tasks::model::from_json(&v)
}

pub(crate) fn get(conn: &Connection, id: &str) -> Result<Option<Message>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {COLUMNS} FROM messages m WHERE m.id = ?1"))?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(r) => Ok(Some(from_row(r)?)),
        None => Ok(None),
    }
}

/// Messages by id, in the given order; missing ids are skipped.
pub(crate) fn many(conn: &Connection, ids: &[String]) -> Result<Vec<Message>> {
    ids.iter().filter_map(|id| get(conn, id).transpose()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Unchanged,
}

/// Store a normalised message. `inserted_at` survives; `updated_at` only
/// advances when text or withdrawal changed. Writes nothing when equal.
pub(crate) fn upsert(conn: &Connection, m: &Message) -> Result<Change> {
    let hash = fingerprint(&m.text, m.withdrawn);
    let old = get(conn, &m.id)?;
    let old_hash = old.as_ref().map(|o| fingerprint(&o.text, o.withdrawn));
    let now = time::now();
    let (inserted, updated, change) = match &old {
        None => (now, now, Change::Added),
        Some(o) if old_hash.as_deref() != Some(hash.as_str()) => {
            (o.inserted_at, now.max(o.updated_at + 1), Change::Updated)
        }
        Some(o) => (o.inserted_at, o.updated_at, Change::Unchanged),
    };
    let mut stored = m.clone();
    stored.inserted_at = inserted;
    stored.updated_at = updated;
    if old.as_ref() == Some(&stored) {
        return Ok(Change::Unchanged);
    }
    let mut row = json!(stored);
    let payload: Map<String, Value> = PAYLOAD.iter().map(|k| (k.to_string(), row[*k].clone())).collect();
    (row["content_hash"], row["payload"]) = (json!(hash), json!(Value::Object(payload).to_string()));
    rows::put(conn, "messages", "id", &row)?;
    if old.as_ref().map(|o| &o.images) != Some(&m.images) {
        conn.execute("DELETE FROM message_images WHERE message_id = ?1", [&m.id])?;
        conn.execute(
            "INSERT INTO message_images (message_id, idx, image_id, path) \
             SELECT ?1, key, value ->> 'id', value ->> 'path' FROM json_each(?2) WHERE true \
             ON CONFLICT(image_id) DO UPDATE SET message_id=excluded.message_id, idx=excluded.idx, path=excluded.path",
            params![m.id, json!(m.images).to_string()],
        )?;
    }
    Ok(change)
}

pub(crate) fn tombstoned(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.prepare_cached("SELECT 1 FROM deleted_messages WHERE id = ?1")?.exists([id])?)
}

pub(crate) fn protected(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.prepare_cached(&format!("SELECT 1 FROM ({PROTECTED}) WHERE message_id = ?1"))?.exists([id])?)
}

/// Whether a scan may store this message: not deleted by hand, and inside
/// retention unless something still references it.
pub(crate) fn allowed(conn: &Connection, id: &str, at: i64, cutoff: i64) -> Result<bool> {
    Ok(!tombstoned(conn, id)? && (at >= cutoff || protected(conn, id)?))
}

fn json_row(r: &Row) -> Result<Value> {
    Ok(serde_json::to_value(from_row(r)?)?)
}

fn page_args(p: &Value) -> Result<(usize, usize)> {
    let page = match p.get("page") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().filter(|n| (1..=1_000_000).contains(n)).ok_or("分页参数无效")?,
    };
    let size = match p.get("page_size") {
        None | Some(Value::Null) => 50,
        Some(v) => v.as_u64().filter(|n| [20, 50, 100].contains(n)).ok_or("分页参数无效")?,
    };
    Ok((page as usize, size as usize))
}

fn endpoint(p: &Value, key: &str, default: i64) -> Result<i64> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_i64().filter(|n| *n >= 0).ok_or_else(|| Error(format!("无效的{key}"))),
    }
}

pub(crate) fn query(conn: &Connection, p: &Value, days: u32) -> Result<Value> {
    let (page, size) = page_args(p)?;
    let since = endpoint(p, "since", 0)?;
    let until = endpoint(p, "until", i64::MAX)?;
    if since > until {
        crate::bail!("开始时间应早于结束时间");
    }
    let q = p.str_of("query").trim();
    if q.chars().count() > 200 {
        crate::bail!("搜索词最多200字");
    }
    let descending = match p.get("sort") {
        None | Some(Value::Null) => true,
        Some(v) => match v.as_str() {
            Some("desc") => true,
            Some("asc") => false,
            _ => crate::bail!("时间排序参数无效"),
        },
    };
    let source = format!(
        "SELECT {COLUMNS}, time AS k0, id AS k1 FROM messages m WHERE withdrawn = 0 AND time >= ?1 AND time <= ?2 \
         AND (time >= ?3 OR id IN ({PROTECTED})) AND id NOT IN (SELECT id FROM deleted_messages) \
         AND (?4 = '' OR has_text(text, ?4) OR has_text(sender, ?4) OR has_text(\"group\", ?4) \
         OR has_text(chat, ?4) OR has_text(source_name, ?4))"
    );
    Page {
        source: &source,
        params: vec![Sql::Integer(since), Sql::Integer(until), Sql::Integer(cutoff(days)), Sql::Text(q.to_lowercase())],
        keys: 2,
        descending,
        cursor: p.get("cursor"),
        page,
        size,
    }
    .run(conn, json_row)
}

/// `messages_delete`: remove by hand and leave a tombstone so scans do not
/// import the message again; tasks keep their own copies.
pub(crate) fn delete(conn: &Connection, p: &Value) -> Result<Value> {
    let ids = p.get("ids").and_then(Value::as_array).ok_or("请选择消息")?;
    if ids.is_empty() || ids.len() > 1000 {
        crate::bail!("每次请选择1–1000条消息");
    }
    let mut ids: Vec<&str> = ids
        .iter()
        .map(|v| v.as_str().filter(|s| !s.is_empty() && s.len() <= 4096))
        .collect::<Option<_>>()
        .ok_or("无效消息编号")?;
    ids.sort_unstable();
    ids.dedup();
    let now = time::now();
    let mut count = 0;
    for id in ids {
        let Some(at) =
            conn.query_row("SELECT time FROM messages WHERE id = ?1", [id], |r| r.get::<_, i64>(0)).optional()?
        else {
            continue;
        };
        conn.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        conn.execute(
            "INSERT INTO deleted_messages (id, deleted_at, message_time) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET deleted_at = excluded.deleted_at",
            params![id, now, at],
        )?;
        count += 1;
    }
    if count > 0 {
        kv::bump(conn, REVISION)?;
        audit::write(
            conn,
            "messages_delete",
            "",
            &format!("手动删除消息，防止扫描重新导入，保留关联事项（{count}条）"),
            &Value::Null,
            &Value::Null,
        )?;
    }
    Ok(json!({ "count": count }))
}

/// Retention: drop unreferenced messages older than `days` and expire old
/// tombstones. Returns whether anything changed.
pub(crate) fn prune(conn: &Connection, days: u32) -> Result<bool> {
    let cutoff = cutoff(days);
    let sql = format!(
        "SELECT id, time, CASE WHEN source_name = '' THEN source_id ELSE source_name END FROM messages \
         WHERE (time < ?1 OR id IN (SELECT id FROM deleted_messages)) AND id NOT IN ({PROTECTED})"
    );
    let rows: Vec<(String, i64, String)> = conn
        .prepare(&sql)?
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare_cached("DELETE FROM messages WHERE id = ?1")?;
    for (id, _, _) in &rows {
        stmt.execute([id])?;
    }
    retention::add(conn, "messages", "sources", rows.iter().map(|(_, at, name)| (*at, name.as_str())))?;
    let expired = conn.execute(
        &format!("DELETE FROM deleted_messages WHERE message_time < ?1 AND id NOT IN ({PROTECTED})"),
        [cutoff],
    )?;
    let changed = !rows.is_empty() || expired > 0;
    if changed {
        kv::bump(conn, REVISION)?;
    }
    Ok(changed)
}

/// `evidence_query`: the messages a task cites, with their roles.
pub(crate) fn evidence(conn: &Connection, p: &Value) -> Result<Value> {
    let task = p.str_of("task_id");
    if task.is_empty() {
        crate::bail!("缺少事项编号");
    }
    let terms = crate::tasks::query::terms(p)?;
    let (since, until) = crate::tasks::query::record_range(p)?;
    let descending = !crate::tasks::query::record_ascending(p)?;
    if !conn.prepare_cached("SELECT 1 FROM tasks WHERE id = ?1")?.exists([task])? {
        crate::bail!("事项不存在或已清理");
    }
    let page = p.get("page").and_then(Value::as_u64).unwrap_or(1).clamp(1, 1_000_000) as usize;
    let mut params = vec![Sql::Text(task.into()), Sql::Integer(since), Sql::Integer(until)];
    let mut filter = String::new();
    for term in &terms {
        let n = params.len() + 1;
        filter += &format!(
            " AND (has_text(m.text, ?{n}) OR has_text(m.sender, ?{n}) OR has_text(m.\"group\", ?{n}) \
             OR has_text(m.chat, ?{n}) OR has_text(m.source_name, ?{n}))"
        );
        params.push(Sql::Text(term.clone()));
    }
    let source = format!(
        "SELECT {COLUMNS}, e.role AS evidence_role, m.time AS k0, m.id AS k1 FROM task_evidence e \
         JOIN messages m ON m.id = e.message_id WHERE e.task_id = ?1 AND m.withdrawn = 0 \
         AND m.time >= ?2 AND m.time <= ?3{filter}"
    );
    let all: i64 = conn.query_row(
        "SELECT count(*) FROM task_evidence e JOIN messages m ON m.id = e.message_id \
         WHERE e.task_id = ?1 AND m.withdrawn = 0",
        [task],
        |r| r.get(0),
    )?;
    let mut out = Page { source: &source, params, keys: 2, descending, cursor: p.get("cursor"), page, size: 50 }.run(
        conn,
        |r| {
            let mut v = json_row(r)?;
            v["evidence_role"] = r.get::<_, String>("evidence_role")?.into();
            Ok(v)
        },
    )?;
    out["all"] = all.into();
    Ok(out)
}
