//! Deterministic retrieval and projections. History size must never determine model input size.
use crate::model::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Account for exactly the compact wire representation, including shared room metadata.
#[derive(Default)]
struct MessagePack {
    rooms: Vec<Value>,
    keys: BTreeMap<(String, String, bool), String>,
    chars: usize,
}
impl MessagePack {
    fn push(&mut self, m: &Message, limit: usize) -> Value {
        let key = (m.source_id.clone(), m.chat.clone(), m.private);
        let room = if let Some(id) = self.keys.get(&key) {
            id.clone()
        } else {
            let id = format!("r{}", self.rooms.len() + 1);
            let name = if !m.group.is_empty() {
                &m.group
            } else {
                &m.chat
            };
            let mut row = json!({"id":id,"name":clip(name,80)});
            if m.private {
                row["private"] = json!(true);
            }
            if m.private {
                let peer = if !m.mine {
                    &m.sender
                } else if !m.recipient.is_empty() {
                    &m.recipient
                } else {
                    &m.to
                };
                if !peer.is_empty() {
                    row["recipient"] = json!(clip(peer, 50));
                }
            }
            omit_empty(&mut row);
            self.chars += row.to_string().chars().count() + usize::from(!self.rooms.is_empty());
            self.rooms.push(row);
            self.keys.insert(key, id.clone());
            id
        };
        let mut row = message(m, limit);
        let fields = row.as_object_mut().unwrap();
        for key in ["source_id", "chat", "private", "recipient"] {
            fields.remove(key);
        }
        row["room"] = json!(room);
        row
    }
}
pub fn message_batch_chars(messages: &[Message]) -> usize {
    let mut pack = MessagePack::default();
    let rows = messages
        .iter()
        .map(|m| pack.push(m, 2000))
        .collect::<Vec<_>>();
    json!({"changes":rows,"rooms":pack.rooms})
        .to_string()
        .chars()
        .count()
}
/// No fixed message-count cap: long and short messages share the configured
/// character budget. Each body is clipped to the first 2,000 Unicode chars
/// before volume accounting.
pub fn batch_messages(mut messages: Vec<Message>, limit: usize) -> Vec<Message> {
    messages.sort_by_key(|m| (m.updated_at, m.time, m.id.clone()));
    let mut pack = MessagePack::default();
    let mut chars = "{\"changes\":[],\"rooms\":[]}".chars().count();
    let mut count = 0;
    messages
        .into_iter()
        .take_while(|m| {
            let before = pack.chars;
            let row = pack.push(m, 2000);
            let size =
                row.to_string().chars().count() + pack.chars - before + usize::from(count > 0);
            if chars + size > limit {
                return false;
            }
            chars += size;
            count += 1;
            true
        })
        .collect()
}
pub fn pending(s: &State) -> Vec<Message> {
    pending_for(s, false)
}
pub fn pending_for(s: &State, manual: bool) -> Vec<Message> {
    let cutoff = now() - s.config.analysis_lookback_hours as i64 * 3_600_000;
    pending_all(s)
        .into_iter()
        .filter(|m| m.time <= 0 || m.time >= cutoff)
        .filter(|m| manual || s.analysis_retry_counts.get(&m.id).copied().unwrap_or(0) < 3)
        .collect()
}
pub fn pending_all(s: &State) -> Vec<Message> {
    let rooms = s
        .tasks
        .iter()
        .filter(|t| !t.archived && t.merged_into.is_empty())
        .flat_map(|t| t.evidence.all())
        .filter_map(|id| s.messages.get(&id))
        .filter(|m| !m.withdrawn)
        .map(|m| (&m.source_id, &m.chat))
        .collect::<BTreeSet<_>>();
    s.messages
        .values()
        .filter(|m| {
            !m.withdrawn
                && (!s.config.analysis_related_only
                    || m.relevant()
                    || rooms.contains(&(&m.source_id, &m.chat)))
                && !crate::message_progress::analyzed(s, m)
        })
        .cloned()
        .collect()
}
pub fn automatic_due(s: &State, pending: &[Message], at: i64, last_attempt: i64) -> bool {
    if !s.config.automatic_analysis_enabled() {
        return false;
    }
    if pending.is_empty() {
        return false;
    }
    // A fresh one-shot filter decision hands off immediately. Regular scheduled
    // retries retain their cooldown; consumption happens durably at reservation.
    (s.config.message_filter_enabled && s.message_filter.early)
        || (at - last_attempt >= 60_000 && due(s, pending, at))
}
pub fn due(s: &State, pending: &[Message], at: i64) -> bool {
    due_reason(s, pending, at).is_some()
}
pub fn due_reason(s: &State, pending: &[Message], at: i64) -> Option<&'static str> {
    if pending.is_empty() {
        return None;
    }
    // A threshold-triggered run resets the same timer; never immediately chain another run.
    if s.last_analysis_at > 0 && at - s.last_analysis_at < 60_000 {
        return None;
    }
    if s.analysis_interrupted_at > 0
        && at - s.analysis_interrupted_at < s.config.analysis_interval_minutes as i64 * 60_000
    {
        return None;
    }
    let budget = crate::ai::message_budget(&s.config, "classify");
    let mut pack = MessagePack::default();
    let mut volume = "{\"changes\":[],\"rooms\":[]}".chars().count();
    for m in pending {
        volume += pack.push(m, 2000).to_string().chars().count() + 1;
        if volume + pack.chars >= budget * 4 / 5 {
            return Some("message_volume");
        }
    }
    let anchor = if s.last_analysis_at > 0 {
        s.last_analysis_at
    } else {
        s.analysis_pending_since
    };
    (anchor > 0 && at - anchor >= s.config.analysis_interval_minutes as i64 * 60_000)
        .then_some("analysis_interval")
}
pub fn mark_pending(s: &mut State, pending: bool, at: i64) {
    s.pending_analysis = pending;
    if !pending {
        s.analysis_pending_since = 0;
    } else if s.analysis_pending_since == 0 {
        s.analysis_pending_since = at;
    }
}
pub fn materialize(action: &Value, tasks: &[Task]) -> Result<Task, String> {
    let old = if action["action"] == "update" {
        tasks
            .iter()
            .find(|t| Some(t.id.as_str()) == action["task_id"].as_str())
    } else {
        None
    };
    let mut v = old
        .map(|t| json!(t))
        .unwrap_or_else(|| json!(Task::default()));
    let fields = action["fields"].as_array().ok_or("AI 动作缺少字段差异")?;
    let mut seen = BTreeSet::new();
    for change in fields {
        let key = change["field"].as_str().ok_or("AI 字段无效")?;
        if ![
            "title",
            "description",
            "status",
            "blocker",
            "waiting_for",
            "next_step",
            "due",
            "summary",
        ]
        .contains(&key)
            || !seen.insert(key)
            || !change["value"].is_string()
        {
            return Err("AI 字段重复或无效".into());
        }
        v[key] = change["value"].clone();
    }
    v["reason"] = action["reason"].clone();
    let mut t: Task = serde_json::from_value(v).map_err(|e| e.to_string())?;
    if let Some(tags) = action.get("tags").filter(|v| v.is_array()) {
        t.tags = serde_json::from_value(tags.clone()).map_err(|_| "事项标签格式无效")?;
    }
    Ok(t)
}
pub fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}
/// Omit absent data, but preserve false/zero and explicit before/after clearing semantics.
fn omit_empty(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| {
            !x.is_null()
                && x.as_str().is_none_or(|s| !s.is_empty())
                && x.as_array().is_none_or(|a| !a.is_empty())
                && x.as_object().is_none_or(|o| !o.is_empty())
        });
    }
}
fn project(v: &Value, fields: &[&str], n: usize) -> Value {
    let mut out = serde_json::Map::new();
    let mut truncated = vec![];
    for key in fields {
        if let Some(x) = v.get(key) {
            if let Some(s) = x.as_str() {
                if s.is_empty() {
                    continue;
                }
                if s.chars().count() > n {
                    truncated.push(*key);
                }
                out.insert(
                    (*key).into(),
                    json!(if *key == "id" || *key == "task_id" {
                        s.to_string()
                    } else {
                        clip(s, n)
                    }),
                );
            } else if !x.is_null() {
                out.insert((*key).into(), x.clone());
            }
        }
    }
    if !truncated.is_empty() {
        out.insert("truncated_fields".into(), json!(truncated));
    }
    Value::Object(out)
}
pub fn task(t: &Task) -> Value {
    task_value(&json!(t))
}
/// Initial summaries intentionally omit full content and evidence. The model
/// requests those by task id through the bounded snapshot tool.
pub fn task_value(t: &Value) -> Value {
    let content = t["description"].as_str().unwrap_or("");
    let summary = t["summary"].as_str().unwrap_or("");
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
            "next_step",
            "manual",
        ],
        180,
    );
    if summary.trim().is_empty() {
        let fallback = crate::model::summary_fallback(content);
        v["summary"] = json!(if fallback.is_empty() {
            crate::model::summary_fallback(t["title"].as_str().unwrap_or(""))
                .chars()
                .take(240)
                .collect::<String>()
        } else {
            fallback
        });
        if v["summary"].as_str().is_none_or(str::is_empty) {
            v["summary"] = json!("（未命名事项）");
        }
    }
    v["tags"] = json!(t["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .map(|x| project(x, &["kind", "value"], 40))
        .collect::<Vec<_>>());
    if v["manual"] == false {
        v.as_object_mut().unwrap().remove("manual");
    }
    omit_empty(&mut v);
    v
}
pub fn task_detail(t: &Task) -> Value {
    let mut value = json!(t);
    value.as_object_mut().unwrap().remove("steps");
    value["summary"] = json!(if t.summary.trim().is_empty() {
        crate::model::summary_content(t)
    } else {
        t.summary.clone()
    });
    value["evidence"] = json!({
        "direct":t.evidence.direct,
        "background":t.evidence.background,
        "completion":t.evidence.completion
    });
    value
}
pub fn message(m: &Message, limit: usize) -> Value {
    // First N Unicode chars, never bytes. Metadata is present on every message
    // so a clipped body can be followed up by stable id through read_context.
    let original_chars = m.text.chars().count();
    let text = m.text.chars().take(limit).collect::<String>();
    let mut v = json!({
        "id":m.id,
        "chat":m.chat,
        "sender":clip(&m.sender,50),
        "time":stamp(m.time),
        "text":text,
        "truncated":original_chars > limit,
        "original_chars":original_chars,
        "reply":m.reply
    });
    for (key, flag) in [
        ("mine", m.mine),
        ("private", m.private),
        ("mention_me", m.mention_me),
        ("mention_all", m.mention_all),
    ] {
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
        v["images"] = crate::images::brief(&m.images);
    }
    omit_empty(&mut v);
    v
}
pub fn audit(a: &Audit) -> Value {
    audit_value(&json!(a))
}
pub fn audit_value(a: &Value) -> Value {
    let select = |v: &Value| {
        if let Some(rows) = v.as_array() {
            rows.iter()
                .find(|r| r["id"] == a["task_id"])
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            v.clone()
        }
    };
    let before = select(&a["before"]);
    let after = select(&a["after"]);
    let mut changes = serde_json::Map::new();
    for key in [
        "title",
        "description",
        "status",
        "blocker",
        "waiting_for",
        "next_step",
        "due",
        "archive_note",
        "archived",
    ] {
        if before[key] != after[key] {
            let short = |v: &Value| {
                v.as_str().map(|s| json!(clip(s, 140))).unwrap_or_else(|| {
                    if v.is_boolean() {
                        v.clone()
                    } else {
                        Value::Null
                    }
                })
            };
            changes.insert(
                key.into(),
                json!({"before":short(&before[key]),"after":short(&after[key])}),
            );
        }
    }
    let mut result = json!({"task_id":a["task_id"],"time":a["time"],"kind":a["kind"],"detail":clip(a["detail"].as_str().unwrap_or(""),160),"changes":changes,"note":"字段为摘录；省略字段不代表清空"});
    if before["decision_context"].is_object() {
        result["prior_decision"] = before["decision_context"].clone();
    }
    result
}
/// Select whole rows under a section budget and disclose omissions; never silently truncate JSON.
pub fn rows(items: impl IntoIterator<Item = Value>, max_rows: usize, chars: usize) -> Value {
    let mut result = vec![];
    let mut used = 2;
    let mut omitted = 0;
    for row in items {
        let n = row.to_string().chars().count() + 1;
        if result.len() < max_rows && n <= chars.saturating_sub(used) {
            used += n;
            result.push(row);
        } else {
            omitted += 1;
        }
    }
    json!({"items":result,"omitted":omitted})
}
pub fn suggestions(s: &State, context: &str) -> Value {
    let terms = terms(context);
    let mut entries = s.suggestions.iter().collect::<Vec<_>>();
    entries.sort_by_key(|r| {
        (
            std::cmp::Reverse(
                terms
                    .intersection(&self::terms(&format!("{} {}", r.title, r.rule)))
                    .count(),
            ),
            std::cmp::Reverse(r.updated),
        )
    });
    rows(entries.into_iter().map(|r|json!({"id":r.id,"title":clip(&r.title,80),"rule":clip(&r.rule,240),"decision":crate::evolution::decision(r),"evidence":r.evidence.iter().rev().take(3).collect::<Vec<_>>()})),8,2400)
}
fn terms(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for word in s
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
    {
        if word.is_ascii() && word.len() >= 3 {
            out.insert(word.to_string());
        }
        if !word.is_ascii() {
            let c: Vec<_> = word.chars().collect();
            for a in c.windows(4) {
                out.insert(a.iter().collect());
            }
        }
    }
    out
}
pub struct Batch {
    pub payload: Value,
    pub fingerprints: BTreeMap<String, String>,
    pub allowed: BTreeMap<String, Message>,
    pub versions: BTreeMap<String, i64>,
}
#[cfg(test)]
pub fn classify(s: &State, changes: &[Message]) -> Result<Batch, String> {
    classify_with_task_limit(s, changes, usize::MAX)
}
/// Build one fresh-session batch. Neighbouring messages are deliberately not
/// injected: the model must request additional history through the bounded
/// snapshot tool, so an old conversation cannot silently pollute a new batch.
pub fn classify_with_task_limit(
    s: &State,
    changes: &[Message],
    task_limit: usize,
) -> Result<Batch, String> {
    // Keys include source/account identity. A room name alone is never an identity.
    let mut owners: BTreeMap<String, Vec<&Task>> = BTreeMap::new();
    for t in s
        .tasks
        .iter()
        .filter(|t| !t.archived && t.merged_into.is_empty() && t.generation.is_empty())
    {
        for id in t.evidence.all() {
            owners.entry(id).or_default().push(t);
        }
    }
    let mut scores: BTreeMap<&str, (usize, &Task)> = BTreeMap::new();
    // A fresh batch contains only the newly selected messages. Explicit reply
    // links may associate a task, but never inject the older parent body.
    let context: Vec<&Message> = vec![];
    for m in changes {
        let mut cursor = m;
        let mut chain = BTreeSet::new();
        // Explicit reply identity is structural context, not an arbitrary
        // neighbouring-message window.
        for _ in 0..8 {
            if !chain.insert(cursor.id.as_str()) {
                break;
            }
            if let Some(ts) = owners.get(cursor.id.as_str()) {
                for t in ts {
                    scores.insert(&t.id, (1000, *t));
                }
            }
            // Normalized reply IDs retain identity even when the original text was pruned.
            if let Some(ts) = owners.get(cursor.reply.as_str()).filter(|_| {
                s.messages
                    .get(&cursor.reply)
                    .is_none_or(|p| p.source_id == m.source_id && p.chat == m.chat)
            }) {
                for t in ts {
                    scores.insert(&t.id, (1000, *t));
                }
            }
            let Some(parent) = s
                .messages
                .get(&cursor.reply)
                .filter(|p| !p.withdrawn && p.source_id == m.source_id && p.chat == m.chat)
            else {
                break;
            };
            // Reply identity is used only to associate the task. The parent
            // body is not injected into a fresh batch; the model can request it
            // by id through the bounded snapshot tool.
            cursor = parent;
        }
    }
    for m in &context {
        if let Some(ts) = owners.get(m.id.as_str()) {
            for t in ts {
                scores.entry(&t.id).or_insert((700, *t));
            }
        }
    }
    let query = terms(
        &changes
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    for t in s.tasks.iter().filter(|t| {
        !t.archived
            && t.merged_into.is_empty()
            && t.generation.is_empty()
            && (t.status == "待处理" || t.updated > now() - 86_400_000)
    }) {
        let shared = query
            .intersection(&terms(&format!("{} {}", t.title, t.description)))
            .cloned()
            .collect::<Vec<_>>();
        let same_room = t
            .evidence
            .all()
            .iter()
            .filter_map(|id| s.messages.get(id))
            .any(|p| {
                changes
                    .iter()
                    .any(|m| m.source_id == p.source_id && m.chat == p.chat)
            });
        let precise = shared
            .iter()
            .any(|w| w.is_ascii() && w.len() >= 6 && w.chars().any(|c| c.is_ascii_digit()));
        if precise
            || (same_room
                && (shared.len() >= 2
                    || changes
                        .iter()
                        .any(|m| (m.time - t.updated).abs() < 86_400_000)))
        {
            scores.entry(&t.id).or_insert((
                if precise {
                    500
                } else {
                    100 + shared.len().min(99)
                },
                t,
            ));
        }
    }
    let mut candidates = scores.values().copied().collect::<Vec<_>>();
    candidates.sort_by_key(|(score, t)| {
        (
            std::cmp::Reverse(*score),
            std::cmp::Reverse(t.updated),
            &t.id,
        )
    });
    // All unarchived summaries are supplied by default. The engine only lowers
    // this to <=10 after the exact final prompt exceeds the configured limit.
    let candidate_ids = candidates
        .iter()
        .map(|(_, task)| task.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut ordered = candidates.clone();
    let mut others = s
        .tasks
        .iter()
        .filter(|task| {
            !task.archived
                && task.merged_into.is_empty()
                && task.generation.is_empty()
                && !candidate_ids.contains(task.id.as_str())
        })
        .collect::<Vec<_>>();
    others.sort_by_key(|task| (std::cmp::Reverse(task.updated), &task.id));
    ordered.extend(others.into_iter().map(|task| (0, task)));
    let allowed = changes
        .iter()
        .chain(context.iter().copied())
        .map(|m| (m.id.clone(), m.clone()))
        .collect();
    let versions = ordered
        .iter()
        .map(|(_, t)| (t.id.clone(), t.updated))
        .collect::<BTreeMap<_, _>>();
    let task_limit = task_limit.min(ordered.len());
    let initial_tasks = rows(
        ordered.iter().take(task_limit).map(|(score, t)| {
            let mut v = task(t);
            if *score > 0 {
                v["match_rank"] = json!(score);
            }
            v
        }),
        task_limit.max(1),
        usize::MAX,
    );
    let initial_ids = initial_tasks["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect::<BTreeSet<_>>();
    let history = rows(
        s.audit
            .iter()
            .rev()
            .filter(|a| {
                initial_ids.contains(a.task_id.as_str())
                    && (a.kind.starts_with("manual") || a.kind == "merge" || a.kind == "undo")
            })
            .take(6)
            .map(audit),
        6,
        2800,
    );
    let mut records = changes
        .iter()
        .chain(context.iter().copied())
        .map(|m| {
            // Full text and secondary metadata remain local until explicitly requested.
            let mut data = json!(m);
            let o = data.as_object_mut().unwrap();
            o.retain(|_, v| v != &json!(false));
            data["time"] = json!(stamp(m.time));
            omit_empty(&mut data);
            json!({"id":m.id,"kind":"message","source":m.source_id,"chat":m.chat,"time":m.time,"data":data})
        })
        .collect::<Vec<_>>();
    for (_, t) in &ordered {
        records.push(json!({
            "id":t.id,
            "kind":"task",
            "source":"",
            "chat":"",
            "time":t.updated,
            "data":task_detail(t)
        }));
    }
    let current = crate::prompts::content(&s.config, "classify");
    let rules = s
        .suggestions
        .iter()
        .filter(|r| r.active && !current.contains(r.rule.trim()))
        .map(|r| r.rule.clone())
        .collect::<BTreeSet<_>>();
    let index = ordered
        .iter()
        .filter(|(_, t)| !initial_ids.contains(t.id.as_str()))
        .map(|(_, t)| json!({"id":t.id,"title":clip(&t.title,80)}))
        .collect::<Vec<_>>();
    let mut pack = MessagePack::default();
    let changes_wire = changes
        .iter()
        .map(|m| pack.push(m, 2000))
        .collect::<Vec<_>>();
    let context_wire = context
        .iter()
        .map(|m| pack.push(m, 2000))
        .collect::<Vec<_>>();
    let mut payload = json!({"changes":changes_wire,"rooms":pack.rooms,"context":context_wire,"tasks":initial_tasks["items"],"task_index":index,"manual_history":history,"leaders":s.config.leaders,"rules":rules,"retrieval":{"open_tasks":ordered.len(),"local_candidates":candidates.len(),"omitted_tasks":index.len()},"_local_context":{"records":records,"expires":now()+300_000}});
    if payload["manual_history"]["items"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        payload.as_object_mut().unwrap().remove("manual_history");
    }
    omit_empty(&mut payload);
    Ok(Batch {
        payload,
        fingerprints: changes
            .iter()
            .map(|m| (m.id.clone(), fingerprint(m)))
            .collect(),
        allowed,
        versions,
    })
}

/// Project a payload into the bounded per-purpose request shape. This is the
/// current request contract, not a migration of stored old snapshots.
pub fn prepare(kind: &str, p: &Value) -> Value {
    let mut p = p.clone();
    if kind == "advice_revision" {
        let values = p["evidence"].as_array().cloned().unwrap_or_default();
        let selected = rows(
            values.iter().map(|e| {
                if e.get("before").is_some() {
                    return audit_value(e);
                }
                let mut row = project(
                    e,
                    &[
                        "id",
                        "project_id",
                        "project",
                        "instructions",
                        "title",
                        "text",
                    ],
                    400,
                );
                if let Some(task) = e.get("task") {
                    row["task"] = task_value(task);
                }
                if let Some(rounds) = e["rounds"].as_array() {
                    row["rounds"] = json!(rounds
                        .iter()
                        .take(2)
                        .map(|r| project(r, &["question", "conclusion"], 600))
                        .collect::<Vec<_>>());
                }
                if e.is_string() {
                    return json!(clip(e.as_str().unwrap(), 600));
                }
                row
            }),
            6,
            6500,
        );
        if p["evidence"].is_array() {
            p["evidence"] = selected["items"].clone();
            p["omitted_evidence"] = selected["omitted"].clone();
        } else if p["evidence"].is_object() {
            let review = prepare("prompt_review", &json!({"daily_evidence":p["evidence"]}));
            p["evidence"] = review["daily_evidence"].clone();
        }
        // The editable suggestion itself may embed the complete old evidence and revision history.
        if let Some(suggestion) = p.get_mut("suggestion").and_then(Value::as_object_mut) {
            for key in ["evidence", "excerpts", "revisions"] {
                suggestion.remove(key);
            }
        }
    }
    if kind == "prompt_review" {
        let e = &p["daily_evidence"];
        let audits=e["audit"].as_array().into_iter().flatten().filter(|r| {
            let k=r["event"]["kind"].as_str().unwrap_or("");
            k.starts_with("manual")||k=="undo"||k=="merge"
        }).map(|r|json!({"evidence_id":r["evidence_id"],"event":if r["event"]["changes"].is_object(){r["event"].clone()}else{audit_value(&r["event"])}}));
        let selected = rows(audits, 6, 7000);
        p["daily_evidence"] = json!({"audit":selected["items"],"omitted":selected["omitted"],"logs":[],"notice":"仅人工修正的字段差异；技术运行日志不用于推导业务规则，摘录之外不推断"});
    }
    if kind == "report" {
        let mut tasks = p["report_tasks"].as_array().cloned().unwrap_or_default();
        tasks.sort_by_key(|t| {
            (
                t["status"] != "待处理",
                t["due"].as_str().is_none_or(str::is_empty),
                t["due"].as_str().unwrap_or("").is_empty(),
                t["id"].to_string(),
            )
        });
        let mut counts = BTreeMap::<String, usize>::new();
        for t in &tasks {
            *counts
                .entry(t["status"].as_str().unwrap_or("未知").into())
                .or_default() += 1;
        }
        let selected = rows(tasks.iter().map(task_value), 12, 7000);
        p["report_tasks"] = selected["items"].clone();
        p["coverage"] = json!({"status_counts":counts,"omitted_tasks":selected["omitted"],"notice":"代表事项摘录，不能声称逐项检查了所有事项"});
        let delta = rows(
            p["delta"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .filter(|a| !a["task_id"].as_str().unwrap_or("").is_empty())
                .map(audit_value),
            8,
            5000,
        );
        p["delta"] = delta;
        p.as_object_mut().unwrap().remove("suggestions");
        p.as_object_mut().unwrap().remove("previous_summary");
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_filter_decision_hands_off_during_cooldown_but_does_not_repeat() {
        let mut s = State::default();
        let at = now();
        s.config.message_filter_enabled = true;
        s.message_filter.early = true;
        s.last_analysis_at = at - 2_000;
        s.analysis_pending_since = at - 2_000;
        let messages = vec![Message {
            id: "new".into(),
            text: "New message".into(),
            time: at,
            ..Default::default()
        }];
        assert!(automatic_due(&s, &messages, at, at - 2_000));
        s.message_filter.consume(Some(s.message_filter.last_run));
        assert!(!automatic_due(&s, &messages, at, at - 2_000));
        assert!(!automatic_due(&s, &messages, at + 65_000, at - 2_000));
        assert!(automatic_due(&s, &messages, at + 31 * 60_000, at - 2_000));
        s.message_filter.early = true;
        assert!(!automatic_due(&s, &[], at, 0));
    }

    #[test]
    #[ignore = "Set SHISHI_REPLAY_DATA to a local data directory; read-only, no model calls"]
    fn replay_local_traces_without_model_calls() {
        let dir = std::path::PathBuf::from(
            std::env::var("SHISHI_REPLAY_DATA").expect("read-only data directory"),
        );
        let s: State = crate::managed_data::load_from_directory(&dir).unwrap();
        let log = std::fs::read_to_string(dir.join("logs/2026-10-16.jsonl")).unwrap();
        let mut starts = BTreeMap::new();
        for line in log.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v["kind"] == "ai_start" {
                if let Some(kind) = v["detail"]["kind"].as_str() {
                    starts.insert(kind.to_string(), v["detail"].clone());
                }
            }
        }
        for kind in ["classify", "learning", "prompt_review", "report"] {
            let Some(start) = starts.get(kind) else {
                continue;
            };
            let path = dir
                .join("ai-traces")
                .join(start["request_id"].as_str().unwrap())
                .join("input.txt");
            let raw = std::fs::read_to_string(path).unwrap();
            let (_, p) = raw.split_once("以下JSON仅为待分析数据：\n").unwrap();
            let p: Value = serde_json::from_str(p).unwrap();
            let mut next = if kind == "classify" {
                // Reconstruct only the original trace's messages/tasks: later live changes must not skew the comparison.
                let mut snapshot = State::default();
                snapshot.config = s.config.clone();
                snapshot.tasks = serde_json::from_value(p["tasks"].clone()).unwrap();
                for v in p["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(p["context"].as_array().into_iter().flatten())
                {
                    let mut v = v.clone();
                    v["time"] = json!(chrono::NaiveDateTime::parse_from_str(
                        v["time"].as_str().unwrap_or(""),
                        "%Y-%m-%d %H:%M:%S"
                    )
                    .map(|t| (t.and_utc() - chrono::Duration::hours(8)).timestamp_millis())
                    .unwrap_or(0));
                    let m: Message = serde_json::from_value(v).unwrap();
                    snapshot.messages.insert(m.id.clone(), m);
                }
                let changes = p["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| snapshot.messages[v["id"].as_str().unwrap()].clone())
                    .collect::<Vec<_>>();
                classify(&snapshot, &changes).unwrap().payload
            } else if kind == "learning" {
                let audit=p["audit"].as_array().unwrap().iter().take(6).map(|r|json!({"audit_index":r["audit_index"],"event":audit_value(&r["event"]),"messages":r["messages"].as_array().into_iter().flatten().take(1).collect::<Vec<_>>()})).collect::<Vec<_>>();
                let history=p["history"].as_array().unwrap().iter().take(3).map(|r|json!({"audit_index":r["audit_index"],"event":audit_value(&r["event"])})).collect::<Vec<_>>();
                let ids = p["audit"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|a| a["event"]["task_id"].as_str())
                    .collect::<BTreeSet<_>>();
                json!({"audit":audit,"history":history,"current_prompt":p["current_prompt"],"current_tasks":rows(p["current_tasks"].as_array().into_iter().flatten().filter(|t|ids.contains(t["id"].as_str().unwrap_or(""))).map(task_value),6,3000),"existing_suggestions":suggestions(&s,&json!(audit).to_string())})
            } else {
                prepare(kind, &p)
            };
            if ["classify", "learning"].contains(&kind) {
                next["tag_catalog"] = p["tag_catalog"].clone();
            }
            let new = crate::ai::build_prompt(&s.config.for_kind(kind), kind, &next)
                .chars()
                .count();
            println!(
                "OFFLINE_REPLAY {}",
                json!({"kind":kind,"old_prompt_chars":raw.chars().count(),"new_prompt_chars":new,"new_sizes":crate::ai::input_sizes(&s.config,kind,&next)})
            );
            assert!(new < raw.chars().count());
        }
    }
    #[test]
    fn optional_relevance_gate_keeps_all_incremental_messages_by_default() {
        let mut s = State::default();
        for key in [
            "plain",
            "mine",
            "at-me",
            "at-all",
            "private",
            "tracked",
            "other-account",
            "withdrawn",
            "done",
            "old",
            "anchor",
        ] {
            let m = Message {
                id: key.into(),
                source_id: if key == "other-account" {
                    "other"
                } else {
                    "source"
                }
                .into(),
                chat: if ["tracked", "anchor", "other-account"].contains(&key) {
                    "tracked-room"
                } else {
                    "ordinary-room"
                }
                .into(),
                time: if key == "old" {
                    now() - 25 * 3_600_000
                } else {
                    now()
                },
                text: "合成增量资料".into(),
                mine: key == "mine",
                mention_me: key == "at-me",
                mention_all: key == "at-all",
                private: key == "private",
                withdrawn: key == "withdrawn",
                ..Default::default()
            };
            if ["done", "anchor"].contains(&key) {
                s.analyzed.insert(m.id.clone(), fingerprint(&m));
            }
            s.messages.insert(m.id.clone(), m);
        }
        s.tasks.push(Task {
            id: "task".into(),
            evidence: TaskEvidence {
                direct: vec!["anchor".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let ids = |s: &State| {
            pending(s)
                .into_iter()
                .map(|m| m.id)
                .collect::<BTreeSet<_>>()
        };
        let all = ids(&s);
        assert_eq!(all.len(), 7);
        assert!(all.contains("plain") && all.contains("other-account"));
        let progress = s.analyzed.clone();
        s.config.analysis_related_only = true;
        assert_eq!(
            ids(&s),
            ["mine", "at-me", "at-all", "private", "tracked"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        s.config.analysis_related_only = false;
        assert_eq!(ids(&s), all);
        assert_eq!(s.analyzed, progress);
        let selected = batch_messages(pending(&s), 60_000);
        assert_eq!(selected.len(), 7, "all eligible messages fit in one batch");
    }
    #[test]
    fn batching_is_volume_based_and_never_consumes_the_backlog() {
        let messages = (0..200)
            .map(|i| Message {
                id: format!("m{i:03}"),
                text: "一句短消息".into(),
                time: now() - 1000 + i,
                private: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let small = batch_messages(messages.clone(), 2000);
        let large = batch_messages(messages.clone(), 12000);
        assert!(large.len() > 64);
        assert!(large.len() > small.len());
        assert!(large.len() < messages.len());
        assert!(message_batch_chars(&large) <= 12000);
        let mut next = large.clone();
        next.push(messages[large.len()].clone());
        assert!(message_batch_chars(&next) > 12000);
        assert_eq!(large[0].id, "m000");
        let mut s = State::default();
        s.messages = messages.into_iter().map(|m| (m.id.clone(), m)).collect();
        for m in &large {
            s.analyzed.insert(m.id.clone(), fingerprint(m));
        }
        assert_eq!(pending(&s).len(), 200 - large.len());
    }
    #[test]
    fn compact_messages_preserve_reply_identity_and_omit_empty_fields() {
        let a = Message {
            id: "a".into(),
            source_id: "account1".into(),
            chat: "room".into(),
            sender: "甲".into(),
            text: "请验证".into(),
            mine: true,
            ..Default::default()
        };
        let b = Message {
            id: "b".into(),
            source_id: "account2".into(),
            reply: "a".into(),
            mention_me: true,
            ..a.clone()
        };
        let mut pack = MessagePack::default();
        let first = pack.push(&a, 2000);
        let second = pack.push(&b, 2000);
        assert_ne!(first["room"], second["room"]);
        assert_eq!(second["reply"], "a");
        assert_eq!(second["mention_me"], true);
        assert_eq!(first["truncated"], false);
        assert_eq!(first["original_chars"], 3);
        for key in [
            "source_id",
            "chat",
            "private",
            "recipient",
            "to",
            "relevant",
        ] {
            assert!(first.get(key).is_none(), "{key}");
        }
        let mut s = State::default();
        s.messages.insert(a.id.clone(), a.clone());
        let batch = classify(&s, &[a]).unwrap();
        assert_eq!(batch.payload["changes"][0]["text"], "请验证");
        assert!(batch.payload.get("task_index").is_none());
        assert_eq!(
            batch.payload["_local_context"]["records"][0]["data"]["source_id"],
            "account1"
        );
    }

    #[test]
    #[ignore = "Read-only real-message replay; no model call. Set SHISHI_REPLAY_DATA"]
    fn replay_compact_batches_without_model_calls() {
        let dir = std::path::PathBuf::from(std::env::var("SHISHI_REPLAY_DATA").unwrap());
        let s: State = crate::managed_data::load_from_directory(&dir).unwrap();
        let old_message = |m: &Message| json!({"id":m.id,"source_id":m.source_id,"chat":m.chat,"sender":clip(&m.sender,50),"recipient":clip(&m.recipient,50),"to":clip(&m.to,80),"private":m.private,"mine":m.mine,"relevant":m.relevant(),"reply":m.reply,"time":stamp(m.time),"text":clip(&m.text,550),"truncated":m.text.chars().count()>550});
        let mut pending = pending(&s);
        pending.sort_by_key(|m| (m.time, m.id.clone()));
        let budget = crate::ai::message_budget(&s.config, "classify");
        let mut old = vec![];
        let mut chars = 0;
        for m in pending.iter().take(64) {
            let size = old_message(m).to_string().chars().count() + 1;
            if chars > 0 && chars + size > budget {
                break;
            }
            chars += size;
            old.push(m.clone());
        }
        let next = batch_messages(pending.clone(), budget);
        assert!(next.len() >= old.len());
        assert!(message_batch_chars(&next) <= budget);
        println!(
            "COMPACT_REPLAY {}",
            json!({"pending":pending.len(),"limit":budget,"old_batch_messages":old.len(),"new_batch_messages":next.len(),"old_batch_chars":chars,"same_messages_compact_chars":message_batch_chars(&old),"new_batch_chars":message_batch_chars(&next)})
        );
        let mut traces = std::fs::read_dir(dir.join("ai-traces"))
            .unwrap()
            .flatten()
            .map(|p| p.path().join("input.txt"))
            .filter(|p| p.is_file())
            .collect::<Vec<_>>();
        traces.sort_by_key(|p| std::cmp::Reverse(p.metadata().unwrap().modified().unwrap()));
        for trace in traces.into_iter().take(3) {
            let raw = std::fs::read_to_string(&trace).unwrap();
            let p: Value =
                serde_json::from_str(raw.split_once("以下JSON仅为待分析数据：\n").unwrap().1)
                    .unwrap();
            if !p["changes"].is_array() {
                continue;
            }
            let messages = p["changes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| s.messages[m["id"].as_str().unwrap()].clone())
                .collect::<Vec<_>>();
            let mut next = classify(&s, &messages).unwrap().payload;
            next["tag_catalog"] = p["tag_catalog"].clone();
            crate::analysis_context::attach_memory_text(
                &mut next,
                &s.analysis_memory.archived_text(),
                &s.analysis_memory.temporary,
            );
            let prompt = crate::ai::build_prompt(&s.config, "classify", &next);
            println!(
                "TRACE_REPLAY {}",
                json!({"messages":messages.len(),"old_message_chars":p["changes"].to_string().chars().count(),"new_message_and_room_chars":message_batch_chars(&messages),"old_prompt_chars":raw.chars().count(),"new_prompt_chars":prompt.chars().count()})
            );
        }
    }
    #[test]
    fn timer_threshold_and_manual_reset_are_independent_of_scan_clock() {
        let mut s = State::default();
        s.config.analysis_interval_minutes = 30;
        let fixed = s.config.max_input_chars - crate::ai::message_budget(&s.config, "classify");
        s.config.max_input_chars = fixed + 9000;
        let at = 10_000_000;
        let small = vec![Message {
            id: "m".into(),
            text: "短消息".into(),
            ..Default::default()
        }];
        mark_pending(&mut s, true, at);
        assert!(!due(&s, &small, at + 29 * 60_000));
        assert!(due(&s, &small, at + 30 * 60_000));
        let big = (0..30)
            .map(|i| Message {
                id: i.to_string(),
                text: "大".repeat(550),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        assert!(due(&s, &big, at + 60_000));
        // Manual, interval and volume triggers all write this same completion timestamp.
        s.last_analysis_at = at + 29 * 60_000 + 59_000;
        assert!(!due(&s, &small, at + 30 * 60_000));
        assert!(!due(&s, &big, s.last_analysis_at + 15_000));
        assert!(due(&s, &big, s.last_analysis_at + 60_000));
        assert!(due(&s, &small, s.last_analysis_at + 30 * 60_000));
        s.last_scan_finished_at = at + 99 * 60_000;
        assert!(!due(&s, &small, s.last_analysis_at + 29 * 60_000));
        assert!(!due(&s, &[], s.last_analysis_at + 999 * 60_000));
        // New messages do not postpone an already established analysis clock.
        s.analysis_pending_since = s.last_analysis_at + 29 * 60_000;
        assert!(due(&s, &small, s.last_analysis_at + 30 * 60_000));
        let saved: State = serde_json::from_value(json!(s)).unwrap();
        assert_eq!(saved.last_analysis_at, s.last_analysis_at);
    }
    #[test]
    fn sparse_updates_preserve_unmentioned_manual_fields_and_validate_keys() {
        let t = Task {
            id: "t".into(),
            title: "人工标题".into(),
            description: "完整原始描述".repeat(200),
            priority: "高".into(),
            waiting_for: "同事".into(),
            manual: true,
            ..Default::default()
        };
        let a = json!({"action":"update","task_id":"t","reason":"已经收到答复","fields":[{"field":"waiting_for","value":""}],"tags":[]});
        let updated = materialize(&a, &[t.clone()]).unwrap();
        assert_eq!(updated.description, t.description);
        assert_eq!(updated.priority, "高");
        assert_eq!(updated.title, t.title);
        assert!(updated.waiting_for.is_empty());
        let mut a = a;
        a["fields"] = json!([{"field":"sources","value":"fake"}]);
        assert!(materialize(&a, &[t.clone()]).is_err());
        a["fields"] = json!([{"field":"title","value":"a"},{"field":"title","value":"b"}]);
        assert!(materialize(&a, &[t]).is_err());
    }
    #[test]
    fn local_file_is_excluded_from_prompt_but_query_budget_is_reserved() {
        let payload = json!({"changes":[],"_local_context":{"records":[{"id":"private","text":"本地数据".repeat(100000)}]}});
        let p = crate::ai::build_prompt(&Config::default(), "classify", &payload);
        assert!(!p.contains("本地数据"));
        let sizes = crate::ai::input_sizes(&Config::default(), "classify", &payload);
        assert!(sizes.get("reserved_context_chars").is_none());
        assert_eq!(
            sizes["total_input_chars"].as_u64().unwrap(),
            sizes["prompt_chars"].as_u64().unwrap()
                + sizes["schema_chars"].as_u64().unwrap()
                + sizes["tool_schema_chars"].as_u64().unwrap()
                + sizes["wire_overhead_chars"].as_u64().unwrap()
        );
        assert!(sizes["tool_schema_chars"].as_u64().unwrap() > 0);
        let c = Config {
            max_input_chars: sizes["total_input_chars"].as_u64().unwrap() as usize - 1,
            ..Default::default()
        };
        assert!(crate::ai::validate_input(&c, "classify", &payload).is_err());
    }
    #[test]
    fn old_reply_links_win_and_unrelated_history_does_not_grow_input() {
        let mut s = State::default();
        let old = Message {
            id: "old".into(),
            source_id: "a".into(),
            chat: "room".into(),
            text: "旧故障".into(),
            time: 1,
            ..Default::default()
        };
        s.messages.insert(old.id.clone(), old);
        s.tasks.push(Task {
            id: "owner".into(),
            title: "等待修复".into(),
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            status: "待处理".into(),
            ..Default::default()
        });
        let m = Message {
            id: "new".into(),
            source_id: "a".into(),
            chat: "room".into(),
            reply: "old".into(),
            text: "修复了，请验证".into(),
            time: now(),
            private: true,
            ..Default::default()
        };
        s.messages.insert(m.id.clone(), m.clone());
        let a = classify(&s, &[m.clone()]).unwrap();
        assert!(a
            .payload
            .get("context")
            .is_none_or(|rows| rows.as_array().is_some_and(|rows| rows.is_empty())));
        for i in 0..10000 {
            let id = format!("unrelated{i}");
            s.messages.insert(
                id.clone(),
                Message {
                    id: id.clone(),
                    source_id: "b".into(),
                    chat: "room".into(),
                    text: "irrelevant".repeat(100),
                    time: now(),
                    ..Default::default()
                },
            );
            s.tasks.push(Task {
                id: format!("t{i}"),
                evidence: TaskEvidence {
                    direct: vec![id],
                    ..Default::default()
                },
                title: "别的来源事项".into(),
                status: "待处理".into(),
                updated: now(),
                ..Default::default()
            });
        }
        let b = classify(&s, &[m]).unwrap();
        // All unarchived summaries are supplied by default; adding unrelated
        // messages must not inject their bodies into the delta or context.
        assert_eq!(b.payload["tasks"][0]["id"], "owner");
        assert_eq!(b.payload["tasks"].as_array().unwrap().len(), 10_001);
        assert_eq!(a.payload["context"], b.payload["context"]);
        assert_eq!(a.payload["changes"], b.payload["changes"]);
        assert!(b.versions.contains_key("owner"));
        assert_eq!(b.versions.len(), 10_001);
    }
    #[test]
    fn all_unarchived_summaries_are_sent_and_the_omitted_index_covers_every_omitted_task() {
        let mut s = State::default();
        let m = Message {
            id: "m".into(),
            text: "新材料".into(),
            time: now(),
            private: true,
            ..Default::default()
        };
        s.messages.insert(m.id.clone(), m.clone());
        for i in 0..15 {
            s.tasks.push(Task {
                id: format!("t{i}"),
                title: format!("事项{i}"),
                description: "摘要内容".into(),
                status: "待处理".into(),
                updated: 100 + i,
                ..Default::default()
            });
        }
        let all = classify(&s, &[m.clone()]).unwrap();
        assert_eq!(all.payload["tasks"].as_array().unwrap().len(), 15);
        assert!(all.payload.get("task_index").is_none());
        assert_eq!(all.versions.len(), 15);

        let reduced = classify_with_task_limit(&s, &[m], 10).unwrap();
        assert_eq!(reduced.payload["tasks"].as_array().unwrap().len(), 10);
        let index = reduced.payload["task_index"].as_array().unwrap();
        assert_eq!(index.len(), 5);
        let listed = reduced.payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .chain(index.iter())
            .filter_map(|task| task["id"].as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(listed.len(), 15);
        assert_eq!(reduced.versions.len(), 15);
    }

    #[test]
    fn message_bodies_truncate_by_unicode_chars_and_always_carry_metadata() {
        for (chars, truncated) in [(1999usize, false), (2000, false), (2001, true)] {
            let m = Message {
                id: format!("m{chars}"),
                text: "甲".repeat(chars),
                ..Default::default()
            };
            let row = message(&m, 2000);
            assert_eq!(row["id"], format!("m{chars}"));
            assert_eq!(row["truncated"], truncated);
            assert_eq!(row["original_chars"], chars);
            assert_eq!(
                row["text"].as_str().unwrap().chars().count(),
                chars.min(2000)
            );
        }
        let emoji = Message {
            id: "emoji".into(),
            text: "🙂".repeat(2001),
            ..Default::default()
        };
        let row = message(&emoji, 2000);
        assert_eq!(row["truncated"], true);
        assert_eq!(row["original_chars"], 2001);
        assert_eq!(row["text"].as_str().unwrap().chars().count(), 2000);
        let mut pack = MessagePack::default();
        let wire = pack.push(&emoji, 2000);
        assert_eq!(wire["truncated"], true);
        assert_eq!(wire["original_chars"], 2001);
    }

    #[test]
    fn report_and_legacy_review_are_bounded_and_do_not_contain_snapshots() {
        let event = json!({"kind":"manual_edit","task_id":"x","before":{"description":"a".repeat(10000)},"after":{"description":"b".repeat(10000)}});
        let p = json!({"daily_evidence":{"audit":(0..200).map(|i|json!({"evidence_id":format!("audit:{i}"),"event":event})).collect::<Vec<_>>(),"logs":[{"raw":"x".repeat(100000)}]}});
        let p = prepare("prompt_review", &p);
        assert!(p.to_string().len() < 12000);
        assert!(p["daily_evidence"]["audit"][0]["event"]["before"].is_null());
        let p = prepare(
            "report",
            &json!({"report_tasks":(0..10000).map(|i|json!({"id":i.to_string(),"title":"task","description":"x".repeat(500),"status":"待处理"})).collect::<Vec<_>>(),"delta":[event]}),
        );
        assert!(p.to_string().len() < 15000);
        assert!(p["coverage"]["omitted_tasks"].as_u64().unwrap() > 0);
    }
}
