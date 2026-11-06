//! Private per-request traces, kept only when
//! `retain_ai_traces` is on, never published, pruned with the message
//! retention period. Drivers record into a [`Trace`] in memory; the caller
//! saves it to SQLite through the data module when the request is built and
//! when it ends. A save failure is a diagnostic of the call, never its
//! failure.
//!
//! Entries (the names of the former files): `input.txt` (rendered prompt),
//! `schema.json`, `tools.json`, `request.json` (the comparison identity),
//! `output.txt` (raw provider reply), `error.txt`, `events.jsonl` (bounded
//! phase/provider events), `tool-failures.jsonl`
//! and `round-NNNN/…` per native round.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::prompt::{Prepared, Route};
use crate::{Db, Error, Result};

/// `events.jsonl` stops growing at this size.
pub const EVENTS_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Entries {
    text: BTreeMap<String, String>,
    /// Names changed since the last save.
    dirty: BTreeSet<String>,
}

/// The trace of one request, or of one of its rounds (a name prefix over
/// the same entries).
#[derive(Clone, Default)]
pub struct Trace {
    entries: Arc<Mutex<Entries>>,
    prefix: String,
    /// The request's start (unix ms): the retention and comparison time of
    /// all its entries.
    at: i64,
}

impl Trace {
    pub fn new(at: i64) -> Self {
        Trace { at, ..Default::default() }
    }

    /// The trace of native round `n`.
    pub fn round(&self, n: usize) -> Trace {
        Trace { entries: self.entries.clone(), prefix: format!("{}round-{n:04}/", self.prefix), at: self.at }
    }

    pub fn put(&self, name: &str, text: impl Into<String>) {
        let name = format!("{}{name}", self.prefix);
        let mut e = self.entries.lock().unwrap();
        e.text.insert(name.clone(), text.into());
        e.dirty.insert(name);
    }

    /// Append one line, bounded per entry by `limit`.
    pub fn append(&self, name: &str, line: &str, limit: Option<usize>) {
        let name = format!("{}{name}", self.prefix);
        let mut e = self.entries.lock().unwrap();
        let text = e.text.entry(name.clone()).or_default();
        if limit.is_some_and(|l| text.len() >= l) {
            return;
        }
        text.push_str(line);
        text.push('\n');
        e.dirty.insert(name);
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<String> {
        self.entries.lock().unwrap().text.get(&format!("{}{name}", self.prefix)).cloned()
    }

    pub fn event(&self, event: &Value) {
        self.append("events.jsonl", &event.to_string(), Some(EVENTS_LIMIT));
    }

    /// Tool argument failures are kept apart from the capped event stream
    /// so its tail cannot hide the cause.
    pub fn tool_failure(&self, event: &Value) {
        self.append("tool-failures.jsonl", &event.to_string(), None);
    }

    /// The entries changed since the last call.
    fn take(&self) -> Vec<(String, String)> {
        let mut e = self.entries.lock().unwrap();
        let names = std::mem::take(&mut e.dirty);
        names
            .into_iter()
            .map(|n| {
                let text = e.text[&n].clone();
                (n, text)
            })
            .collect()
    }
}

/// The comparison identity of a request: same purpose, connection, driver,
/// model and effort.
pub fn identity(route: &Route) -> Value {
    json!({"kind": route.kind, "connection_id": route.connection.id, "driver": route.connection.driver,
           "model": route.model, "effort": route.effort})
}

/// Record the request before it is sent.
pub fn request(trace: &Trace, route: &Route, prepared: &Prepared) {
    trace.put("input.txt", prepared.prompt.as_str());
    trace.put("schema.json", prepared.schema.to_string());
    if let Some(tools) = &prepared.tools {
        trace.put("tools.json", tools.to_string());
    }
    trace.put("request.json", identity(route).to_string());
}

/// Save what changed since the last save.
pub async fn save(db: &Db, request_id: &str, trace: &Trace) -> Result<()> {
    let entries = trace.take();
    if entries.is_empty() {
        return Ok(());
    }
    db.save_ai_trace(request_id.into(), trace.at, entries).await
}

/// The `ai_trace` request: one call's saved input/output, or with
/// `previous` the latest earlier input of the same identity.
pub async fn read(db: &Db, retain: bool, p: &Value) -> Result<Value> {
    let id = p["id"].as_str().ok_or_else(|| Error::new("缺少调用编号"))?;
    uuid::Uuid::parse_str(id).map_err(|_| Error::new("无效的调用编号"))?;
    let entries: BTreeMap<String, String> = db.ai_trace(id.into()).await?.into_iter().collect();
    if entries.is_empty() {
        return Err(Error::new(if retain {
            "这次调用没有保存详细输入输出（旧版记录）"
        } else {
            "完整跟踪已关闭，本次未保存输入输出"
        }));
    }
    let read = |name: &str| entries.get(name).cloned().unwrap_or_default();
    if p["previous"] != true {
        return Ok(json!({"input": read("input.txt"), "output": read("output.txt"),
            "events": format!("{}\n{}", read("tool-failures.jsonl"), read("events.jsonl")),
            "schema": read("schema.json"), "error": read("error.txt")}));
    }
    if !entries.contains_key("request.json") {
        return Err(Error::new("当前记录缺少同类请求标识，请使用更新后的调用记录比较"));
    }
    let (previous_id, previous) =
        db.previous_ai_trace_input(id.into()).await?.ok_or_else(|| Error::new("没有更早的已保留请求可供比较"))?;
    Ok(json!({"input": read("input.txt"), "previous_input": previous, "previous_request_id": previous_id}))
}
