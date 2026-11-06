//! Retained AI traces ([`crate::ai::trace`] decides what is recorded).

use rusqlite::{params, OptionalExtension};

use super::Db;
use crate::Result;

impl Db {
    /// Insert or replace entries of one request's trace.
    pub async fn save_ai_trace(&self, request_id: String, at: i64, entries: Vec<(String, String)>) -> Result<()> {
        self.write(move |tx| {
            let mut put = tx.prepare_cached(
                "INSERT INTO ai_traces(request_id, name, body, at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(request_id, name) DO UPDATE SET body = excluded.body",
            )?;
            for (name, body) in &entries {
                put.execute(params![request_id, name, body, at])?;
            }
            Ok(())
        })
        .await
    }

    /// All entries of one request's trace.
    pub async fn ai_trace(&self, request_id: String) -> Result<Vec<(String, String)>> {
        self.read(move |c| {
            let mut rows = c.prepare("SELECT name, body FROM ai_traces WHERE request_id = ?1")?;
            let rows = rows.query_map([request_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// The latest earlier request of the same identity: its id and input.
    pub async fn previous_ai_trace_input(&self, request_id: String) -> Result<Option<(String, String)>> {
        self.read(move |c| {
            Ok(c.query_row(
                "SELECT p.request_id, i.body FROM ai_traces c \
                 JOIN ai_traces p ON p.name = 'request.json' AND p.body = c.body AND p.at <= c.at AND p.request_id <> c.request_id \
                 JOIN ai_traces i ON i.request_id = p.request_id AND i.name = 'input.txt' \
                 WHERE c.request_id = ?1 AND c.name = 'request.json' \
                 ORDER BY p.at DESC, p.request_id DESC LIMIT 1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
    }

    /// Remove traces of requests started before `cutoff` (unix ms).
    pub async fn prune_ai_traces(&self, cutoff: i64) -> Result<usize> {
        self.write(move |tx| Ok(tx.execute("DELETE FROM ai_traces WHERE at < ?1", [cutoff])?)).await
    }
}
