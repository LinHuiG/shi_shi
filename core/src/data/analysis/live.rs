//! The live source behind `read_context`: messages, tasks and updates
//! read from current data within the request's scope.

use std::cell::RefCell;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::analysis::context::{self, query_from, Live, Meter, QueryFilter, ReadSet, Records};
use crate::data::{kv, rows, Db};
use crate::messages::Message;
use crate::tasks::model::Task;
use crate::{Error, Result};

impl Db {
    /// One `read_context` page over the current data; off the executor.
    pub(crate) fn analysis_context_blocking(
        &self,
        live: Live,
        snapshot: Value,
        mut meter: Meter,
        args: Value,
        limit: Option<usize>,
    ) -> Result<(String, Meter, ReadSet)> {
        self.read_blocking(move |c| Ok(live_query(c, &live, &snapshot, &mut meter, &args, limit)))
    }
}

fn live_query(
    conn: &Connection,
    live: &Live,
    snapshot: &Value,
    meter: &mut Meter,
    args: &Value,
    limit: Option<usize>,
) -> (String, Meter, ReadSet) {
    let source = Source {
        conn,
        live,
        pending: RefCell::default(),
        error: RefCell::default(),
        archive_cursor: kv::get_i64(conn, crate::archive::ARCHIVE_CURSOR).unwrap_or(0),
    };
    let result = query_from(snapshot, meter, args, limit, &source);
    (result, meter.clone(), source.pending.into_inner())
}

struct Source<'a> {
    conn: &'a Connection,
    live: &'a Live,
    pending: RefCell<ReadSet>,
    error: RefCell<Option<String>>,
    archive_cursor: i64,
}

impl Source<'_> {
    fn checked<T>(&self, result: Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                *self.error.borrow_mut() = Some(e.0);
                None
            }
        }
    }

    /// Readable: not withdrawn, and recent, unanalysed, evidence of an open
    /// task, one of the request's records or their reply target, or the
    /// reply target of an accepted read.
    fn allowed(&self, m: &Message) -> Result<bool> {
        if m.withdrawn {
            return Ok(false);
        }
        if m.time >= self.live.cutoff
            || self
                .live
                .records
                .iter()
                .any(|r| r["kind"] == "message" && (r["id"] == m.id.as_str() || r["data"]["reply"] == m.id.as_str()))
            || self.live.accepted.messages.values().any(|read| read.reply == m.id)
        {
            return Ok(true);
        }
        Ok(self
            .conn
            .prepare_cached(
                "SELECT 1 FROM messages m WHERE m.id = ?1 AND (m.analyzed_hash IS NOT m.content_hash \
                 OR EXISTS (SELECT 1 FROM task_evidence e JOIN tasks t ON t.id = e.task_id \
                            WHERE e.message_id = m.id AND t.archived = 0))",
            )?
            .exists([&m.id])?)
    }

    fn message(&self, id: &str) -> Option<Message> {
        if let Some(m) = self.live.accepted.messages.get(id) {
            return Some(m.clone());
        }
        let m = self.checked(crate::data::messages::get(self.conn, id))??;
        self.checked(self.allowed(&m))?.then_some(m)
    }

    fn task(&self, id: &str) -> Option<Task> {
        self.checked(crate::data::tasks::get(self.conn, id)).flatten().filter(|t| !t.archived)
    }

    /// `(source, chat)` when every evidence message shares one room.
    fn task_scope(&self, t: &Task) -> Option<(String, String)> {
        let ids: Vec<String> = t.evidence.all().into_iter().map(str::to_string).collect();
        let mut scope: Option<(String, String)> = None;
        for m in self.checked(crate::data::messages::many(self.conn, &ids))? {
            let room = (m.source_id, m.chat);
            match &scope {
                None => scope = Some(room),
                Some(first) if *first != room => return Some(Default::default()),
                _ => {}
            }
        }
        Some(scope.unwrap_or_default())
    }

    fn update(&self, seq: i64) -> Option<Value> {
        let sql = "SELECT time, kind, task_id, before, after FROM task_audit WHERE seq = ?1";
        let row = self.conn.query_row(sql, [seq], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?))
        });
        let (at, kind, task_id, before, after) = self.checked(row.optional().map_err(Error::from))??;
        if !crate::tasks::query::task_update_kind(&kind, &task_id)
            || (at < self.live.cutoff && seq <= self.archive_cursor)
        {
            return None;
        }
        Some(context::update_row(seq, at, &kind, &task_id, before, after))
    }

    fn task_row(&self, t: &Task) -> Option<Value> {
        let (source, chat) = self.task_scope(t)?;
        let at = if t.updated > 0 { t.updated } else { t.created };
        Some(
            json!({"id": t.id, "kind": "task", "source": source, "chat": chat, "time": at, "data": context::task_detail(t)}),
        )
    }

    fn memory(&self) -> impl Iterator<Item = &Value> {
        self.live.records.iter().filter(|r| r["kind"] == "memory")
    }

    fn message_ids(&self, filter: &QueryFilter) -> Vec<String> {
        let sql = "SELECT id FROM messages WHERE withdrawn = 0 AND ?1 IN ('', source_id) AND ?2 IN ('', chat) \
                   AND time >= ?3 AND time <= ?4 AND (?5 IS NULL OR id IN (SELECT value FROM json_each(?5))) ORDER BY time, id";
        let ids = (!filter.ids.is_empty()).then(|| json!(filter.ids).to_string());
        let range = (filter.since.unwrap_or(i64::MIN), filter.until.unwrap_or(i64::MAX));
        self.checked(rows::column(self.conn, sql, params![filter.source, filter.chat, range.0, range.1, ids]))
            .unwrap_or_default()
    }

    fn update_seqs(&self, filter: &QueryFilter) -> Vec<i64> {
        let sql = "SELECT seq FROM task_audit WHERE task_id <> '' AND (time >= ?1 OR seq > ?2) \
                   AND time >= ?3 AND time <= ?4 ORDER BY seq";
        let range = (filter.since.unwrap_or(i64::MIN), filter.until.unwrap_or(i64::MAX));
        self.checked(rows::column(self.conn, sql, params![self.live.cutoff, self.archive_cursor, range.0, range.1]))
            .unwrap_or_default()
    }
}

impl Records for Source<'_> {
    fn contains(&self, id: &str) -> bool {
        if self.memory().any(|r| r["id"] == id) {
            return true;
        }
        if let Some(seq) = id.strip_prefix("audit:") {
            return seq.parse().is_ok_and(|seq| self.update(seq).is_some());
        }
        self.task(id).is_some() || self.message(id).is_some()
    }

    fn has_room(&self, source: &str, chat: &str, except: Option<(&str, &str)>) -> bool {
        let matches = |account: &str, room: &str| {
            room == chat && (source.is_empty() || account == source) && except != Some((account, room))
        };
        if self.live.accepted.messages.values().any(|m| matches(&m.source_id, &m.chat)) {
            return true;
        }
        let filter = QueryFilter { source: source.into(), chat: chat.into(), ..Default::default() };
        self.message_ids(&filter).iter().filter_map(|id| self.message(id)).any(|m| matches(&m.source_id, &m.chat))
    }

    fn rows<'a>(&'a self, filter: &'a QueryFilter) -> Box<dyn Iterator<Item = Value> + 'a> {
        let kind = |k: &str| filter.kind.is_empty() || filter.kind == k;
        let wanted = |id: &str| filter.ids.is_empty() || filter.ids.contains(id);
        let fixed =
            self.memory().filter(move |r| filter.kind.is_empty() && wanted(r["id"].as_str().unwrap_or(""))).cloned();
        // Each kind is read only when the consumer gets that far.
        let messages = kind("message")
            .then_some(())
            .into_iter()
            .flat_map(move |_| {
                let accepted: Vec<String> = self.live.accepted.messages.keys().cloned().collect();
                let rest =
                    self.message_ids(filter).into_iter().filter(|id| !self.live.accepted.messages.contains_key(id));
                accepted.into_iter().chain(rest).collect::<Vec<_>>()
            })
            .filter(move |id| wanted(id))
            .filter_map(move |id| self.message(&id))
            .map(move |m| context::message_row(&m));
        let tasks = kind("task")
            .then_some(())
            .into_iter()
            .flat_map(move |_| {
                if filter.ids.is_empty() {
                    self.checked(crate::data::tasks::list(self.conn, "archived = 0")).unwrap_or_default()
                } else {
                    filter.ids.iter().filter_map(|id| self.task(id)).collect()
                }
            })
            .filter_map(move |t| self.task_row(&t));
        let updates = kind("update")
            .then_some(())
            .into_iter()
            .flat_map(move |_| self.update_seqs(filter))
            .filter(move |seq| wanted(&format!("audit:{seq}")))
            .filter_map(move |seq| self.update(seq));
        Box::new(fixed.chain(messages).chain(tasks).chain(updates))
    }

    fn delivered(&self, row: &Value) {
        let id = row["id"].as_str().unwrap_or("");
        let mut pending = self.pending.borrow_mut();
        match row["kind"].as_str() {
            Some("message") if !self.live.accepted.messages.contains_key(id) => {
                if let Some(m) = self.message(id) {
                    pending.messages.insert(id.into(), m);
                }
            }
            Some("task") => {
                if let Some(t) = self.task(id) {
                    pending.task_versions.insert(id.into(), t.updated);
                }
            }
            Some("update") => {
                if let Some(seq) = row["data"]["audit_index"].as_i64() {
                    pending.updates.insert(seq);
                }
            }
            _ => {}
        }
        pending.records.push(row.clone());
    }

    fn error(&self) -> Option<String> {
        self.error.borrow().clone()
    }
}
