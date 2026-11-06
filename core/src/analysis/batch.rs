//! Batch assembly and the input budget.
//!
//! [`classify`] builds one fresh projection of the newest data: the new
//! message versions, explicit reply ancestors within [`REPLY_CONTEXT_CHARS`],
//! ranked task summaries, recent manual corrections, rules and memory. [`fit`]
//! then takes the largest prefix the model accepts, reducing in the fixed
//! order reply context → task summaries → task directory → message count, and
//! refills what the remaining room allows. Room ids and task order are frozen
//! by `classify`, so each reduction is monotone and binary search holds.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::{clip, context, progress, Measured, Model, Request};
use crate::ai::INPUT_LIMIT_ERROR;
use crate::messages::{self, ImageRef, Message};
use crate::prompts::memory::{Memory, ARCHIVED_LIMIT, TEMPORARY_LIMIT};
use crate::tasks::model::{self as tasks, Task};
use crate::{time, Config, Error, Result};

/// Serialized characters reply ancestors may take.
pub const REPLY_CONTEXT_CHARS: usize = 8000;
/// Characters of recent manual corrections.
pub const MANUAL_HISTORY_CHARS: usize = 2800;
/// How long the local context file of a request stays valid.
pub const LOCAL_CONTEXT_MS: i64 = 300_000;

/// One prepared batch: the payload and what its result may touch.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub payload: Value,
    /// Message id → version sent as a change.
    pub fingerprints: BTreeMap<String, String>,
    /// Messages the result may cite: changes and reply context.
    pub allowed: BTreeMap<String, Message>,
    /// Task id → `updated` of the tasks listed in the payload.
    pub versions: BTreeMap<String, i64>,
}

pub fn chars(v: &Value) -> usize {
    v.to_string().chars().count()
}

/// Drop absent data but keep false, zero and explicit clearing.
pub fn omit_empty(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| {
            !x.is_null()
                && x.as_str().is_none_or(|s| !s.is_empty())
                && x.as_array().is_none_or(|a| !a.is_empty())
                && x.as_object().is_none_or(|o| !o.is_empty())
        });
    }
}

/// Clip text fields to `n` characters, listing the clipped keys.
fn project(v: &Value, fields: &[&str], n: usize) -> Value {
    let mut out = Map::new();
    let mut truncated = vec![];
    for &key in fields {
        match &v[key] {
            Value::String(text) if text.is_empty() => {}
            Value::String(text) => {
                if text.chars().nth(n).is_some() {
                    truncated.push(key);
                }
                let text = if key == "id" || key == "task_id" { text.clone() } else { clip(text, n) };
                out.insert(key.into(), json!(text));
            }
            Value::Null => {}
            other => {
                out.insert(key.into(), other.clone());
            }
        }
    }
    if !truncated.is_empty() {
        out.insert("truncated_fields".into(), json!(truncated));
    }
    Value::Object(out)
}

/// A task summary without full content or evidence; the model asks for
/// those by id through `read_context`.
pub fn task_value(t: &Value) -> Value {
    let mut v = project(
        t,
        &[
            "id",
            "created",
            "updated",
            "content_updated_at",
            "summary_updated_at",
            "title",
            "summary",
            "status",
            "due",
            "blocker",
            "waiting_for",
            "manual",
        ],
        180,
    );
    if t["summary"].as_str().unwrap_or("").trim().is_empty() {
        let fallback = tasks::summary_fallback(t["description"].as_str().unwrap_or(""));
        let fallback = if fallback.is_empty() {
            tasks::summary_fallback(t["title"].as_str().unwrap_or("")).chars().take(240).collect()
        } else {
            fallback
        };
        v["summary"] = json!(if fallback.is_empty() { "（未命名事项）".to_string() } else { fallback });
    }
    v["deferred_until"] = t["deferred_until"].clone();
    v["tags"] = json!(t["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| project(x, &["kind", "value"], 40))
        .collect::<Vec<_>>());
    if v["manual"] == false {
        v.as_object_mut().unwrap().remove("manual");
    }
    omit_empty(&mut v);
    v
}

pub fn task(t: &Task) -> Value {
    task_value(&t.payload())
}

fn brief(images: &[ImageRef]) -> Value {
    json!(images.iter().take(8).map(|i| json!({"id": i.id, "available": !i.path.is_empty()})).collect::<Vec<_>>())
}

/// A message row: the first `limit` characters, with metadata enough to
/// follow a clipped body up by id.
pub fn message(m: &Message, limit: usize) -> Value {
    let original_chars = m.text.chars().count();
    let mut v = json!({
        "id": m.id, "chat": m.chat, "sender": clip(&m.sender, 50), "time": time::stamp(m.time),
        "text": m.text.chars().take(limit).collect::<String>(),
        "truncated": original_chars > limit, "original_chars": original_chars, "reply": m.reply,
    });
    for (key, flag) in
        [("mine", m.mine), ("private", m.private), ("mention_me", m.mention_me), ("mention_all", m.mention_all)]
    {
        if flag {
            v[key] = json!(true);
        }
    }
    if m.mine && !m.private && !m.to.is_empty() {
        v["to"] = json!(clip(&m.to, 80));
    }
    if m.private && !m.recipient.is_empty() {
        v["recipient"] = json!(clip(&m.recipient, 50));
    }
    if !m.images.is_empty() {
        v["images"] = brief(&m.images);
    }
    omit_empty(&mut v);
    v
}

/// The changed fields of an audit row, clipped.
pub fn audit_changes(mut result: Value, before: &Value, after: &Value) -> Value {
    let pick = |value: &'_ Value| -> Value {
        match value.as_array() {
            Some(rows) => rows.iter().find(|row| row["id"] == result["task_id"]).cloned().unwrap_or(Value::Null),
            None => value.clone(),
        }
    };
    let (before, after) = (pick(before), pick(after));
    let short = |v: &Value| match v {
        Value::String(s) => json!(clip(s, 140)),
        Value::Bool(_) => v.clone(),
        _ => Value::Null,
    };
    let mut changes = Map::new();
    for key in ["title", "description", "status", "blocker", "waiting_for", "due", "archive_note", "archived"] {
        if before[key] != after[key] {
            changes.insert(key.into(), json!({"before": short(&before[key]), "after": short(&after[key])}));
        }
    }
    result["changes"] = Value::Object(changes);
    result["note"] = json!("字段为摘录；省略字段不代表清空");
    if before["decision_context"].is_object() {
        result["prior_decision"] = before["decision_context"].clone();
    }
    result
}

/// Whole rows within a character budget; omissions are counted, never cut.
pub fn rows(items: impl IntoIterator<Item = Value>, budget: usize) -> Value {
    let mut items = items.into_iter();
    let mut result = vec![];
    let mut used = 2;
    let mut omitted = 0;
    while used < budget {
        let Some(row) = items.next() else { break };
        let n = chars(&row) + usize::from(!result.is_empty());
        if n <= budget - used {
            used += n;
            result.push(row);
        } else {
            omitted += 1;
        }
    }
    json!({"items": result, "omitted": omitted + items.count()})
}

/// Match terms: ASCII words of three or more characters, and four-character
/// windows of other words.
pub fn terms<'a>(parts: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for part in parts {
        for word in part.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.is_ascii() {
                if word.len() >= 3 {
                    out.insert(word.to_string());
                }
                continue;
            }
            for ((start, _), (end, ch)) in word.char_indices().zip(word.char_indices().skip(3)) {
                out.insert(word[start..end + ch.len_utf8()].to_owned());
            }
        }
    }
    out
}

/// The largest prefix in `1..=count` that `prepare` accepts; `prepare` is
/// monotone. Errors stop the search.
pub fn fit_prefix<T>(count: usize, mut prepare: impl FnMut(usize) -> Result<Option<T>>) -> Result<Option<(usize, T)>> {
    let (mut low, mut high) = (1, count);
    let mut accepted = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        if let Some(candidate) = prepare(middle)? {
            accepted = Some((middle, candidate));
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    Ok(accepted)
}

/// Shared room rows: messages refer to `r{n}` instead of repeating source,
/// chat and peer.
#[derive(Default)]
struct Pack {
    rooms: Vec<Value>,
    keys: BTreeMap<(String, String, bool), String>,
}

impl Pack {
    fn push(&mut self, m: &Message, mut row: Value) -> Value {
        let rooms = &mut self.rooms;
        let room = self.keys.entry((m.source_id.clone(), m.chat.clone(), m.private)).or_insert_with(|| {
            let id = format!("r{}", rooms.len() + 1);
            let name = if m.group.is_empty() { &m.chat } else { &m.group };
            let mut room = json!({"id": id, "name": clip(name, 80)});
            if m.private {
                room["private"] = json!(true);
                let peer = if !m.mine {
                    &m.sender
                } else if !m.recipient.is_empty() {
                    &m.recipient
                } else {
                    &m.to
                };
                if !peer.is_empty() {
                    room["recipient"] = json!(clip(peer, 50));
                }
            }
            omit_empty(&mut room);
            rooms.push(room);
            id
        });
        let fields = row.as_object_mut().unwrap();
        for key in ["source_id", "chat", "private", "recipient"] {
            fields.remove(key);
        }
        fields.insert("room".into(), json!(room));
        row
    }
}

/// What [`classify`] reads besides the changes: live tasks, the rooms of
/// their evidence, their manual corrections (already [`audit_changes`] rows,
/// newest first), the classify prompt, active rules and both memories.
pub struct Material {
    pub live: Vec<Task>,
    pub evidence: BTreeMap<String, BTreeSet<(String, String)>>,
    pub history: Vec<Value>,
    pub prompt: String,
    pub rules: Vec<Value>,
    pub temporary: Memory,
    pub archived: Memory,
}

/// Build one fresh batch for `changes`. Only explicit reply ancestors go
/// with the delta; other history stays behind `read_context`.
/// `parent` reads a reply target by id.
pub fn classify(
    config: &Config,
    changes: &[Message],
    material: &Material,
    parent: &mut dyn FnMut(&str) -> Result<Option<Message>>,
) -> Result<Batch> {
    let live = &material.live;
    let mut scores: BTreeMap<String, usize> = BTreeMap::new();
    let changed: BTreeSet<&str> = changes.iter().map(|m| m.id.as_str()).collect();
    let max_room = format!("r{}", changes.len() + REPLY_CONTEXT_CHARS);
    let mut context: Vec<Message> = Vec::new();
    let mut context_rows = Vec::new();
    let mut walked: BTreeMap<String, usize> = BTreeMap::new();
    let mut context_chars = 2;
    let mut incomplete = false;
    for (chain, m) in changes.iter().enumerate() {
        let mut cursor = m.clone();
        loop {
            if let Some(previous) = walked.insert(cursor.id.clone(), chain) {
                incomplete |= previous == chain;
                break;
            }
            let parent = if cursor.reply.is_empty() { None } else { parent(&cursor.reply)? };
            let same_room = parent.as_ref().is_none_or(|p| p.source_id == m.source_id && p.chat == m.chat);
            for t in live {
                if t.evidence.all().into_iter().any(|id| id == cursor.id || (same_room && id == cursor.reply)) {
                    scores.insert(t.id.clone(), 1000);
                }
            }
            if cursor.reply.is_empty() {
                break;
            }
            let Some(parent) = parent.filter(|p| !p.withdrawn && p.source_id == m.source_id && p.chat == m.chat) else {
                incomplete = true;
                break;
            };
            if !changed.contains(parent.id.as_str()) && !walked.contains_key(&parent.id) {
                // Measured with a room id at least as long as any real one.
                let mut row = message(&parent, progress::BODY_CHARS);
                row["room"] = json!(max_room);
                let n = chars(&row) + usize::from(!context_rows.is_empty());
                if context_chars + n > REPLY_CONTEXT_CHARS {
                    incomplete = true;
                    break;
                }
                context_chars += n;
                incomplete |= row["truncated"] == true;
                row.as_object_mut().unwrap().remove("room");
                context_rows.push(row);
                context.push(parent.clone());
            }
            cursor = parent;
        }
    }

    let query = terms(changes.iter().map(|m| m.text.as_str()));
    let rooms: BTreeSet<(String, String)> = changes.iter().map(|m| (m.source_id.clone(), m.chat.clone())).collect();
    let evidence = &material.evidence;
    let now = time::now();
    for t in live.iter().filter(|t| t.active() || t.updated > now - time::DAY) {
        let words = terms([t.title.as_str(), t.description.as_str()]);
        let (shared, precise) = query.intersection(&words).fold((0usize, false), |(n, precise), w| {
            (n + 1, precise || (w.is_ascii() && w.len() >= 6 && w.chars().any(|c| c.is_ascii_digit())))
        });
        let same_room = evidence.get(&t.id).is_some_and(|r| !r.is_disjoint(&rooms));
        if precise || (same_room && (shared >= 2 || changes.iter().any(|m| (m.time - t.updated).abs() < time::DAY))) {
            scores.entry(t.id.clone()).or_insert(if precise { 500 } else { 100 + shared.min(99) });
        }
    }
    // The complete relevance order, frozen once: fitting only takes prefixes.
    let mut ordered: Vec<(usize, &Task)> = live.iter().map(|t| (scores.get(&t.id).copied().unwrap_or(0), t)).collect();
    ordered.sort_by(|(a, x), (b, y)| b.cmp(a).then(y.updated.cmp(&x.updated)).then(x.id.cmp(&y.id)));
    let versions: BTreeMap<String, i64> = ordered.iter().map(|(_, t)| (t.id.clone(), t.updated)).collect();
    let task_rows: Vec<Value> = ordered
        .iter()
        .map(|(score, t)| {
            let mut v = task(t);
            if *score > 0 {
                v["match_rank"] = json!(score);
            }
            v
        })
        .collect();
    let history = rows(material.history.clone(), MANUAL_HISTORY_CHARS);

    let change_rows: Vec<Value> = changes.iter().map(|m| message(m, progress::BODY_CHARS)).collect();
    let records: Vec<Value> = changes
        .iter()
        .zip(&change_rows)
        .chain(context.iter().zip(&context_rows))
        .map(|(m, row)| {
            let mut data = row.clone();
            // Paths stay private; the layer turns them into an image catalog.
            if !m.images.is_empty() {
                data["images"] = json!(m.images);
            }
            json!({"id": m.id, "kind": "message", "source": m.source_id, "chat": m.chat, "time": m.time, "data": data})
        })
        .collect();
    let rules: BTreeSet<String> = material
        .rules
        .iter()
        .filter_map(|r| r["rule"].as_str().map(str::to_string))
        .filter(|rule| !material.prompt.contains(rule.trim()))
        .collect();
    let mut pack = Pack::default();
    let changes_wire: Vec<Value> = changes.iter().zip(change_rows).map(|(m, row)| pack.push(m, row)).collect();
    let context_wire: Vec<Value> = context.iter().zip(context_rows).map(|(m, row)| pack.push(m, row)).collect();
    let aliases: Map<String, Value> =
        pack.keys.iter().map(|((source, chat, _), id)| (id.clone(), json!({"source": source, "chat": chat}))).collect();
    let mut payload = json!({
        "changes": changes_wire, "rooms": pack.rooms, "context": context_wire, "tasks": task_rows,
        "manual_history": history, "leaders": config.leaders, "rules": rules,
        "retrieval": {"open_tasks": ordered.len(), "local_candidates": scores.len(), "omitted_tasks": 0},
        "_local_context": {"records": records, "room_aliases": aliases, "expires": now + LOCAL_CONTEXT_MS},
    });
    if incomplete {
        payload["reply_context_incomplete"] = json!(true);
    }
    if payload["manual_history"]["items"].as_array().is_some_and(Vec::is_empty) {
        payload.as_object_mut().unwrap().remove("manual_history");
    }
    omit_empty(&mut payload);
    attach_memory(&mut payload, &material.temporary, &material.archived);
    let allowed = changes.iter().chain(&context).map(|m| (m.id.clone(), m.clone())).collect();
    Ok(Batch {
        payload,
        fingerprints: changes.iter().map(|m| (m.id.clone(), messages::fingerprint(&m.text, m.withdrawn))).collect(),
        allowed,
        versions,
    })
}

/// Serve both memories as local records and announce them in the payload.
fn attach_memory(payload: &mut Value, temporary: &Memory, archived: &Memory) {
    let writable = temporary.text.chars().count() <= TEMPORARY_LIMIT;
    let local = &mut payload["_local_context"];
    let records = local["records"].as_array_mut().unwrap();
    if !temporary.text.trim().is_empty() {
        records.push(json!({"id": "memory", "kind": "memory", "data": {"text": temporary.text}}));
    }
    if !archived.text.trim().is_empty() {
        records.push(json!({"id": "archived-memory", "kind": "memory", "data": {"text": archived.text}}));
    }
    local["memory"] = json!({"before": temporary.text, "writable": writable, "revision": temporary.revision});
    local["archived_memory"] = json!({"before": archived.text, "writable": false, "revision": archived.revision});
    payload["memory"] = json!({
        "available": !temporary.text.trim().is_empty(), "archived_available": !archived.text.trim().is_empty(),
        "writable": writable, "max_chars": TEMPORARY_LIMIT, "archived_max_chars": ARCHIVED_LIMIT,
    });
}

// ---- fitting ---------------------------------------------------------------

/// Project the frozen payload to a prefix: `messages` changes, `tasks`
/// summaries, the reply context or not, and a directory of `directory`
/// rows (summaries count toward it; the rest are `{id, title}`).
pub fn prefix_payload(base: &Value, messages: usize, tasks: usize, context: bool, directory: usize) -> Value {
    let all = base["changes"].as_array().map(Vec::as_slice).unwrap_or_default();
    let changes = &all[..messages.min(all.len())];
    let mut payload = Value::Object(
        base.as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                !matches!(key.as_str(), "changes" | "tasks" | "task_index")
                    && (context || !matches!(key.as_str(), "context" | "manual_history"))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    payload["changes"] = Value::Array(changes.to_vec());
    if (!context || changes.len() < all.len())
        && changes.iter().any(|r| r["reply"].as_str().is_some_and(|s| !s.is_empty()))
    {
        payload["reply_context_incomplete"] = json!(true);
    }
    let quoted = || base["context"].as_array().filter(|_| context).into_iter().flatten();
    let used: BTreeSet<&str> = changes.iter().chain(quoted()).filter_map(|r| r["room"].as_str()).collect();
    if let Some(rooms) = payload["rooms"].as_array_mut() {
        rooms.retain(|r| r["id"].as_str().is_some_and(|id| used.contains(id)));
    }
    let visible: BTreeSet<&str> = changes.iter().chain(quoted()).filter_map(|r| r["id"].as_str()).collect();
    let records = payload.get_mut("_local_context").and_then(|c| c.get_mut("records")).and_then(Value::as_array_mut);
    if let Some(records) = records {
        records.retain(|r| r["kind"] == "memory" || r["id"].as_str().is_some_and(|id| visible.contains(id)));
    }
    let rows = base["tasks"].as_array().map(Vec::as_slice).unwrap_or_default();
    let existing = base["task_index"].as_array().map(Vec::as_slice).unwrap_or_default();
    if !rows.is_empty() || !existing.is_empty() {
        payload["tasks"] = Value::Array(rows[..tasks.min(rows.len())].to_vec());
        let omitted = rows.len().saturating_sub(tasks) + existing.len();
        let index: Vec<Value> = rows
            .iter()
            .skip(tasks)
            .map(|t| json!({"id": t["id"], "title": clip(t["title"].as_str().unwrap_or(""), 80)}))
            .chain(existing.iter().cloned())
            .take(directory.saturating_sub(tasks))
            .collect();
        let unlisted = omitted.saturating_sub(index.len());
        payload["retrieval"]["omitted_tasks"] = json!(omitted);
        payload["retrieval"]["unlisted_tasks"] = json!(unlisted);
        payload["retrieval"]["task_directory_incomplete"] = json!(unlisted > 0);
        payload["task_index"] = Value::Array(index);
    }
    payload
}

/// Why a batch could not be fitted.
#[derive(Debug)]
pub enum Fit {
    /// Cancellation, the deadline or a measuring error: end the batch.
    Stop(Error),
    /// Nothing fits. `alone` when an empty request would: only the first
    /// message is too large and the rest may go on. `log` is the
    /// `analysis_input_limit` row.
    TooLarge { error: String, alone: bool, log: Value },
}

impl From<Error> for Fit {
    fn from(e: Error) -> Self {
        Fit::Stop(e)
    }
}

/// The input-limit text of a request that cannot shrink below `smallest`.
pub fn minimum_error(smallest: usize, limit: usize) -> String {
    format!(
        "{INPUT_LIMIT_ERROR}：单条消息及必需上下文至少为 {smallest} 字符，超过上限 {limit}；本次未调用 AI，消息仍待处理。请精简配置或提高上限。"
    )
}

/// Fit `batch` to `limit` as measured by the model.
/// `check` runs before every probe (cancellation, deadline). Returns the
/// trimmed batch and the chosen request as measured, to be sent as
/// rendered; `_batch_metrics` is set on both.
pub fn fit(
    m: &dyn Model,
    mut batch: Batch,
    limit: usize,
    reader: Option<Arc<context::Reader>>,
    check: &dyn Fn() -> Result<()>,
) -> std::result::Result<(Batch, Measured<Request>), Fit> {
    check()?;
    let request = |payload: Value| Request { purpose: "classify", payload, reader: reader.clone(), reserve: None };
    let count = batch.payload["changes"].as_array().map_or(0, Vec::len);
    let task_count = batch.payload["tasks"].as_array().map_or(0, Vec::len);
    let directory_count = task_count + batch.payload["task_index"].as_array().map_or(0, Vec::len);
    let has_context = batch.payload["context"].as_array().is_some_and(|r| !r.is_empty());
    let mut probes = 0usize;
    // The last probe within the limit is the chosen request: every stage
    // only grows the accepted one.
    let mut chosen = None;
    let base = &batch.payload;
    let mut measure = |messages: usize, tasks: usize, quoted: bool, directory: usize| -> Result<usize> {
        check()?;
        probes += 1;
        let measured = m.measure(request(prefix_payload(base, messages, tasks, quoted, directory)))?;
        let total = measured.total;
        if total <= limit {
            chosen = Some(measured);
        }
        Ok(total)
    };
    let full = measure(count, task_count, true, directory_count)?;
    let (selected, summaries, directory, keep_context, total) = if full <= limit {
        (count, task_count, directory_count, true, full)
    } else {
        let without_quotes = if has_context { measure(count, task_count, false, directory_count)? } else { full };
        if without_quotes <= limit {
            (count, task_count, directory_count, false, without_quotes)
        } else {
            let mut minimal = if task_count > 0 { measure(count, 0, false, directory_count)? } else { without_quotes };
            let mut directory = directory_count;
            if minimal > limit && directory_count > 0 {
                minimal = measure(count, 0, false, 0)?;
                directory = 0;
            }
            let mut smallest = minimal;
            let mut accepted = (minimal <= limit).then_some((count, minimal));
            if accepted.is_none() {
                accepted = fit_prefix(count.saturating_sub(1), |middle| {
                    let n = measure(middle, 0, false, directory)?;
                    smallest = smallest.min(n);
                    Ok((n <= limit).then_some(n))
                })?;
            }
            let Some((selected, mut total)) = accepted else {
                check()?;
                let error = minimum_error(smallest, limit);
                let log = json!({"selected": 1, "initial_selected": count, "total_input_chars": smallest,
                                 "max_input_chars": limit, "reason": "minimum_request_exceeds_budget"});
                let alone = measure(0, 0, false, 0)? <= limit;
                return Err(Fit::TooLarge { error, alone, log });
            };
            if directory < directory_count {
                if let Some((n, t)) = fit_prefix(directory_count, |middle| {
                    let n = measure(selected, 0, false, middle)?;
                    Ok((n <= limit).then_some(n))
                })? {
                    directory = n;
                    total = t;
                }
            }
            let (summaries, t) = fit_prefix(task_count.min(directory), |middle| {
                let n = measure(selected, middle, false, directory)?;
                Ok((n <= limit).then_some(n))
            })?
            .unwrap_or((0, total));
            total = t;
            // All messages fit: restore their quotes if there is room. A
            // shortened prefix retrieves its own parents instead.
            let mut quoted = false;
            if has_context && selected == count {
                let n = measure(selected, summaries, true, directory)?;
                if n <= limit {
                    total = n;
                    quoted = true;
                }
            }
            (selected, summaries, directory, quoted, total)
        }
    };
    check()?;
    let mut chosen = chosen.expect("the selection was measured within the limit");
    let payload = &mut chosen.request.payload;
    let sent: BTreeSet<String> = payload["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    batch.fingerprints.retain(|id, _| sent.contains(id));
    let mut visible = sent;
    visible.extend(
        payload["context"].as_array().into_iter().flatten().filter_map(|r| r["id"].as_str().map(str::to_string)),
    );
    batch.allowed.retain(|id, _| visible.contains(id));
    let listed: BTreeSet<&str> = payload["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(payload["task_index"].as_array().into_iter().flatten())
        .filter_map(|t| t["id"].as_str())
        .collect();
    batch.versions.retain(|id, _| listed.contains(id.as_str()));
    let mut reduced_by = Vec::new();
    if has_context && !keep_context {
        reduced_by.push("reply_context");
    }
    if summaries < task_count {
        reduced_by.push("task_summaries");
    }
    if directory < directory_count {
        reduced_by.push("task_directory");
    }
    if selected < count {
        reduced_by.push("input_budget");
    }
    payload["_batch_metrics"] = json!({
        "selected": selected, "initial_selected": count, "max_input_chars": limit, "total_input_chars": total,
        "task_limit": summaries, "omitted_tasks": directory_count - summaries,
        "unlisted_tasks": directory_count - directory, "reduced_by": reduced_by, "packing_probes": probes,
    });
    batch.payload = payload.clone();
    Ok((batch, chosen))
}

/// Activity rows describing what a fitted batch left out: clipped bodies,
/// messages left for later (`remaining` ids) and tasks shown only as index.
pub fn trim_logs(payload: &Value, remaining: &[String]) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for row in payload["changes"].as_array().into_iter().flatten().filter(|r| r["truncated"] == true) {
        out.push((
            "message_truncated",
            json!({"id": row["id"], "original_chars": row["original_chars"],
                                              "kept_chars": progress::BODY_CHARS, "reason": "body_limit"}),
        ));
    }
    if !remaining.is_empty() {
        out.push((
            "messages_omitted",
            json!({"count": remaining.len(), "ids": &remaining[..remaining.len().min(50)],
                                             "reason": "message_budget"}),
        ));
    }
    let index: Vec<&Value> = payload["task_index"].as_array().into_iter().flatten().collect();
    if !index.is_empty() {
        let ids: Vec<&Value> = index.iter().take(50).map(|t| &t["id"]).collect();
        let chars: usize = index.iter().map(|t| chars(t)).sum();
        out.push((
            "tasks_omitted",
            json!({"count": index.len(), "chars": chars, "ids": ids, "reason": "summary_limit"}),
        ));
    }
    out
}
