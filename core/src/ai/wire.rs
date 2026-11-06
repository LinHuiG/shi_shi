//! Short evidence ids on the wire. Message and task ids become `m1`/`t1`
//! aliases before a request is rendered and are mapped back, strictly, on
//! the reply; message rows are packed into columns. The alias scheme is part
//! of the prompt protocol.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct IdMap {
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
}

const MESSAGES: [&str; 3] = ["changes", "context", "messages"];

/// Call `f` with every string in `value` and the nearest object key above it.
fn strings(value: &mut Value, key: &str, f: &mut dyn FnMut(&str, &mut String)) {
    match value {
        Value::String(s) => f(key, s),
        Value::Array(rows) => rows.iter_mut().for_each(|v| strings(v, key, f)),
        Value::Object(obj) => obj.iter_mut().for_each(|(k, v)| strings(v, k, f)),
        _ => {}
    }
}

/// Fields whose strings are evidence references.
fn reference(key: &str) -> bool {
    matches!(
        key,
        "id" | "task_id"
            | "message_id"
            | "reply"
            | "sources"
            | "direct"
            | "background"
            | "completion"
            | "evidence"
            | "background_evidence"
            | "completion_evidence"
            | "deadline_evidence"
            | "ids"
            | "evidence_ids"
            | "_context_seen"
    )
}

/// User-facing prose fields.
fn prose(key: &str) -> bool {
    matches!(key, "title" | "description" | "summary" | "blocker" | "waiting_for" | "reason")
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Message ids a task row cites, in its evidence roles or directly on an
/// action-like row, so aliases always have a reverse mapping.
fn evidence(task: &Value, messages: &mut BTreeSet<String>) {
    let roles = ["direct", "background", "completion"].map(|r| &task["evidence"][r]);
    for list in roles
        .into_iter()
        .chain(["background_evidence", "completion_evidence", "deadline_evidence", "sources"].map(|k| &task[k]))
    {
        messages.extend(list.as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from));
    }
}

impl IdMap {
    fn insert(&mut self, id: String, alias: String) {
        self.forward.insert(id.clone(), alias.clone());
        self.reverse.insert(alias, id);
    }

    /// Only known transport aliases in user-facing prose are resolved. Genuine
    /// source tokens (for example a product code) are never rewritten.
    pub fn humanize_prose<'a>(&self, value: &mut Value, records: impl IntoIterator<Item = &'a Value>) {
        let mut needed = BTreeSet::new();
        strings(value, "", &mut |key, text| {
            if prose(key) {
                needed
                    .extend(text.split(|c| !token_char(c)).filter(|t| self.reverse.contains_key(*t)).map(String::from));
            }
        });
        if needed.is_empty() {
            return;
        }
        // Scan source text once, regardless of the number of transport IDs.
        // Keep the original token boundary: a source code such as m1-x also
        // protects m1, while the prose replacement never rewrites m1-x.
        let mut by_id = BTreeMap::new();
        for record in records {
            for token in
                record["data"]["text"].as_str().unwrap_or("").split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            {
                needed.remove(token);
            }
            for id in [record["id"].as_str(), record["data"]["id"].as_str()].into_iter().flatten() {
                by_id.entry(id).or_insert(record);
            }
        }
        let names: BTreeMap<String, String> = needed
            .into_iter()
            .map(|alias| {
                let message = alias.starts_with('m');
                let record = by_id.get(self.reverse[&alias].as_str());
                let name = record
                    .and_then(|r| r["data"][if message { "sender" } else { "title" }].as_str())
                    .filter(|s| !s.trim().is_empty());
                let name = name.unwrap_or(if message { "相关消息" } else { "相关事项" }).to_owned();
                (alias, name)
            })
            .collect();
        strings(value, "", &mut |key, text| {
            if prose(key) {
                let (mut out, mut token) = (String::new(), String::new());
                for c in text.chars().chain(['\0']) {
                    if token_char(c) {
                        token.push(c);
                        continue;
                    }
                    out.push_str(names.get(&token).unwrap_or(&token));
                    token.clear();
                    if c != '\0' {
                        out.push(c);
                    }
                }
                *text = out;
            }
        });
    }

    /// Add `newer`'s ids not yet mapped, at the first free alias of their prefix.
    pub fn extend_from(&mut self, newer: Self) {
        let mut next = [1, 1];
        for (original, candidate) in newer.forward {
            if self.forward.contains_key(&original) {
                continue;
            }
            let (prefix, n) = if candidate.starts_with('t') { ("t", &mut next[1]) } else { ("m", &mut next[0]) };
            while self.reverse.contains_key(&format!("{prefix}{n}")) {
                *n += 1;
            }
            self.insert(original, format!("{prefix}{n}"));
            *n += 1;
        }
    }

    pub fn from_payload(payload: &Value) -> Self {
        let (mut messages, mut tasks) = (BTreeSet::new(), BTreeSet::new());
        let rows = |key: &str| payload[key].as_array().into_iter().flatten();
        let id = |row: &Value| row["id"].as_str().map(String::from);
        messages.extend(MESSAGES.into_iter().flat_map(rows).filter_map(id));
        for row in ["tasks", "task_index"].into_iter().flat_map(rows).chain([&payload["task"]]) {
            tasks.extend(id(row));
            evidence(row, &mut messages);
        }
        for record in payload["_local_context"]["records"].as_array().into_iter().flatten() {
            match record["kind"].as_str() {
                Some("message") => messages.extend(id(record)),
                Some("task") => {
                    tasks.extend(id(record));
                    evidence(&record["data"], &mut messages);
                }
                _ => {}
            }
        }
        Self::from_ids(messages, tasks)
    }

    /// Aliases that never collide with an id that already looks like one.
    pub fn from_ids(messages: BTreeSet<String>, tasks: BTreeSet<String>) -> Self {
        let all = messages.union(&tasks).cloned().collect::<BTreeSet<_>>();
        let mut result = Self::default();
        for (prefix, ids) in [("m", messages), ("t", tasks)] {
            let mut n = 1;
            for id in ids.into_iter().filter(|id| !id.is_empty()) {
                if result.forward.contains_key(&id) {
                    continue;
                }
                while all.contains(&format!("{prefix}{n}")) {
                    n += 1;
                }
                result.insert(id, format!("{prefix}{n}"));
                n += 1;
            }
        }
        result
    }

    pub fn encode(&self, value: &mut Value) {
        rewrite(value, &self.forward);
    }

    /// Map known aliases back without rejecting unknown ones: tool
    /// arguments, where an unknown id is simply not found.
    pub fn decode_lenient(&self, value: &mut Value) {
        rewrite(value, &self.reverse);
    }

    pub fn decode(&self, value: &mut Value) -> crate::Result<()> {
        // Creates have no existing target. The application allocates their ID;
        // a model-supplied placeholder is not an evidence reference.
        for action in value["actions"].as_array_mut().into_iter().flatten() {
            if action["action"] == "create" && action["task_id"].is_string() {
                action["task_id"] = json!("");
            }
        }
        let mut unknown = false;
        strings(value, "", &mut |key, s| {
            let alias = s.len() > 1
                && matches!(s.as_bytes()[0], b'm' | b't')
                && s.as_bytes()[1..].iter().all(u8::is_ascii_digit);
            unknown |= alias && reference(key) && !self.reverse.is_empty() && !self.reverse.contains_key(s.as_str());
        });
        if unknown {
            return Err(crate::Error::new(super::UNKNOWN_EVIDENCE));
        }
        self.decode_lenient(value);
        Ok(())
    }
}

fn rewrite(value: &mut Value, map: &BTreeMap<String, String>) {
    strings(value, "", &mut |key, s| {
        if let Some(to) = map.get(s.as_str()).filter(|_| reference(key)) {
            *s = to.clone();
        }
    });
}

/// The wire form of a payload: empty fields dropped, chats as `rooms`
/// aliases and message rows packed under `message_cols`.
pub fn compact(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("retrieval");
    }
    clean(payload);
    if payload.get("rooms").is_none() {
        let (mut rooms, mut ids) = (Vec::new(), BTreeMap::new());
        for key in MESSAGES {
            for row in payload[key].as_array_mut().into_iter().flatten() {
                if row.get("room").is_some() || row.get("chat").is_none() {
                    continue;
                }
                let obj = row.as_object_mut().unwrap();
                let (source, chat) = (obj.remove("source_id").unwrap_or(Value::Null), obj.remove("chat").unwrap());
                let id = ids.entry(json!([source, chat]).to_string()).or_insert_with(|| {
                    let id = format!("r{}", rooms.len() + 1);
                    rooms.push(json!({"id":id,"source_id":source,"chat":chat}));
                    id
                });
                obj.insert("room".into(), json!(id));
            }
        }
        if !rooms.is_empty() {
            payload["rooms"] = json!(rooms);
        }
    }
    if payload["_local_context"].is_object() {
        let mut aliases =
            payload["_local_context"]["room_aliases"].as_object_mut().map(std::mem::take).unwrap_or_default();
        let rooms = payload["rooms"].as_array().into_iter().flatten();
        // A reduced candidate exposes only its delivered room aliases. Format
        // requests build rooms here; classify supplies the same private map.
        let delivered = rooms.clone().filter_map(|room| room["id"].as_str()).collect::<BTreeSet<_>>();
        aliases.retain(|id, _| delivered.contains(id.as_str()));
        for room in rooms {
            if let (Some(id), Some(source), Some(chat)) =
                (room["id"].as_str(), room["source_id"].as_str(), room["chat"].as_str())
            {
                aliases.insert(id.into(), json!({"source":source,"chat":chat}));
            }
        }
        payload["_local_context"]["room_aliases"] = json!(aliases);
    }
    let mut fields = BTreeSet::new();
    let mut any = false;
    for key in MESSAGES {
        for obj in payload[key].as_array().into_iter().flatten().filter_map(Value::as_object) {
            any = true;
            fields.extend(obj.keys().cloned());
        }
    }
    if !any {
        return;
    }
    let mut columns: Vec<String> = ["id", "room", "sender", "time", "text"]
        .into_iter()
        .filter(|key| fields.remove(*key))
        .map(String::from)
        .collect();
    columns.extend(fields);
    for key in MESSAGES {
        for row in payload[key].as_array_mut().into_iter().flatten() {
            if let Value::Object(obj) = row {
                *row = columns.iter().map(|key| obj.remove(key).unwrap_or(Value::Null)).collect();
            }
        }
    }
    payload["message_cols"] = json!(columns);
}

/// Drop rank noise and empty values; private snapshots are validation
/// state, not wire data, and stay unchanged.
fn clean(value: &mut Value) {
    match value {
        Value::Array(rows) => rows.iter_mut().for_each(clean),
        Value::Object(obj) => {
            obj.remove("match_rank");
            obj.iter_mut().filter(|(key, _)| !key.starts_with('_')).for_each(|(_, row)| clean(row));
            let empty = |v: &Value| {
                v.is_null()
                    || v == ""
                    || v.as_array().is_some_and(Vec::is_empty)
                    || v.as_object().is_some_and(|o| o.is_empty())
            };
            obj.retain(|key, v| key.starts_with('_') || !empty(v));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
