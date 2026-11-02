//! Deterministic retrieval and projections. History size must never determine model input size.
use crate::model::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Account for exactly the compact wire representation, including shared room metadata.
#[derive(Default)]
struct MessagePack<'a> {
    rooms: Vec<Value>,
    keys: BTreeMap<(&'a str, &'a str, bool), String>,
    chars: usize,
}
impl<'a> MessagePack<'a> {
    fn push(&mut self, m: &'a Message, limit: usize) -> Value {
        let room = self
            .keys
            .entry((&m.source_id, &m.chat, m.private))
            .or_insert_with(|| {
                let id = format!("r{}", self.rooms.len() + 1);
                let name = if !m.group.is_empty() {
                    &m.group
                } else {
                    &m.chat
                };
                let mut row = json!({"id":id,"name":clip(name,80)});
                if m.private {
                    row["private"] = json!(true);
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
                self.chars += json_chars(&row) + usize::from(!self.rooms.is_empty());
                self.rooms.push(row);
                id
            });
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
        .map(|m| json_chars(&pack.push(m, 2000)))
        .sum::<usize>();
    r#"{"changes":[],"rooms":[]}"#.len() + rows + messages.len().saturating_sub(1) + pack.chars
}
/// Bound preparation by characters that cannot disappear from the final request:
/// every preview body plus its room, truncation flag and original length.
/// Include the first row beyond the bound so the exact fitter can report an
/// indivisible oversized message. Original ids and field names are not counted;
/// wire aliases and shared columns may make those much smaller.
pub fn analysis_candidates<'a>(
    messages: impl Iterator<Item = &'a Message>,
    limit: usize,
) -> Vec<Message> {
    let mut rows = Vec::new();
    let mut lower_bound = 0usize;
    for message in messages {
        lower_bound = lower_bound
            .saturating_add(message.text.chars().take(2000).count() + r#"["r1",true,0]"#.len());
        rows.push(message.clone());
        if lower_bound > limit {
            break;
        }
    }
    rows
}
#[cfg(test)]
pub fn pending(s: &State) -> Vec<Message> {
    pending_for(s, false)
}
#[cfg(test)]
pub fn pending_for(s: &State, manual: bool) -> Vec<Message> {
    pending_refs(s, manual).cloned().collect()
}
pub(crate) const COMMAND_RETRY_REASON: &str =
    "本批存在已执行或结果未知的命令，须核对命令记录后手动重试，未标为已分析";
pub(crate) fn automatic_retry(s: &State, id: &str, scope: Option<&String>) -> bool {
    if let Some(previous) = s.analysis_retry_scopes.get(id) {
        return Some(previous) != scope;
    }
    // Retry counts order ordinary failures behind new messages, not retire them.
    // The exact persisted command guard also covers installations predating this policy.
    !s.analysis_retries
        .get(id)
        .is_some_and(|reason| reason.ends_with(COMMAND_RETRY_REASON))
}
pub fn pending_refs(s: &State, manual: bool) -> impl Iterator<Item = &Message> {
    let cutoff = now() - s.config.analysis_lookback_hours as i64 * 3_600_000;
    let retry_scope = (!manual && !s.analysis_retry_scopes.is_empty())
        .then(|| crate::engine::analysis_block_fingerprint(s));
    let rooms = s
        .tasks
        .iter()
        .filter(|t| s.config.analysis_related_only && !t.archived && t.merged_into.is_empty())
        .flat_map(|t| t.evidence.iter())
        .filter_map(|id| s.messages.get(id))
        .filter(|m| !m.withdrawn)
        .map(|m| (&m.source_id, &m.chat))
        .collect::<BTreeSet<_>>();
    s.messages
        .values()
        .filter(move |m| {
            !m.withdrawn
                && (m.time <= 0 || m.time >= cutoff)
                && (!s.config.analysis_related_only
                    || m.relevant()
                    || rooms.contains(&(&m.source_id, &m.chat)))
                && !crate::message_progress::analyzed(s, m)
        })
        .filter(move |m| manual || automatic_retry(s, &m.id, retry_scope.as_ref()))
}

/// Compare the route and inputs this purpose actually uses. Catalog refreshes,
/// other purposes' prompts/models and trace preferences do not revoke results.
pub(crate) fn config_authority(config: &Config, kind: &str) -> Value {
    let selected = config.model_choice(kind);
    let route = crate::ai_backend::connection_by_id(config, selected.connection_id)
        .map(|connection| json!({"identity":connection.cache_key(),"stream":connection.stream}))
        .unwrap_or_else(|_| json!(selected.connection_id));
    let mut value = json!({
        "route":route,"model":selected.model,"effort":selected.reasoning_effort,
        "policy":crate::ai::effective_policy(config, kind),
        "workdir":config.analysis_workdir,"max_input_chars":config.max_input_chars,
        "retrieval_budget_chars":config.retrieval_budget_chars,
        "analysis_lookback_hours":config.analysis_lookback_hours,
        "analysis_related_only":config.analysis_related_only,
    });
    if kind == "classify" {
        let tools = config
            .tools
            .iter()
            .filter(|tool| tool.enabled && tool.compatibility_issue().is_none())
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            value["tool_policy"] = json!(crate::ai::effective_policy(config, "tool_call"));
        }
        value["tools"] = json!(tools);
        value["leaders"] = json!(config.leaders);
    }
    value
}
pub fn automatic_due<'a>(
    s: &State,
    pending: impl IntoIterator<Item = &'a Message>,
    at: i64,
    last_attempt: i64,
) -> bool {
    let mut pending = pending.into_iter().peekable();
    if !s.config.automatic_analysis_enabled() || pending.peek().is_none() {
        return false;
    }
    // A fresh one-shot filter decision hands off immediately. Regular scheduled
    // retries retain their cooldown; consumption happens durably at reservation.
    (s.config.message_filter_enabled && s.message_filter.early)
        || (at - last_attempt >= 60_000 && due(s, pending, at))
}
pub fn due<'a>(s: &State, pending: impl IntoIterator<Item = &'a Message>, at: i64) -> bool {
    due_reason(s, pending, at).is_some()
}
/// Keep the original cadence after cancellation. A boundary already reached
/// when the user stopped the round is consumed, including an exact tie.
pub(crate) fn next_interval_at(anchor: i64, interval: i64, interrupted_at: i64) -> i64 {
    let interval = interval.max(1);
    if anchor <= 0 {
        return if interrupted_at > 0 {
            interrupted_at.saturating_add(interval)
        } else {
            0
        };
    }
    let steps = (interrupted_at.saturating_sub(anchor).max(0) / interval).saturating_add(1);
    anchor.saturating_add(interval.saturating_mul(steps))
}
pub fn due_reason<'a>(
    s: &State,
    pending: impl IntoIterator<Item = &'a Message>,
    at: i64,
) -> Option<&'static str> {
    let mut pending = pending.into_iter().peekable();
    pending.peek()?;
    // New rounds share this timer. Batches within an active round continue
    // directly and do not re-enter these startup conditions.
    if s.last_analysis_at > 0 && at - s.last_analysis_at < 60_000 {
        return None;
    }
    let anchor = if s.last_analysis_at > 0 {
        s.last_analysis_at
    } else {
        s.analysis_pending_since
    };
    let scheduled_at = next_interval_at(
        anchor,
        s.config.analysis_interval_minutes as i64 * 60_000,
        s.analysis_interrupted_at,
    );
    if s.analysis_interrupted_at > 0 && at < scheduled_at {
        return None;
    }
    let budget = crate::ai::message_budget(&s.config, "classify");
    let mut pack = MessagePack::default();
    let mut volume = "{\"changes\":[],\"rooms\":[]}".chars().count();
    for m in pending {
        volume += json_chars(&pack.push(m, 2000)) + 1;
        if volume + pack.chars >= budget * 4 / 5 {
            return Some("message_volume");
        }
    }
    (anchor > 0 && at >= scheduled_at).then_some("analysis_interval")
}
pub fn mark_pending(s: &mut State, pending: bool, at: i64) {
    s.pending_analysis = pending;
    if !pending {
        s.analysis_pending_since = 0;
    } else if s.analysis_pending_since == 0 {
        s.analysis_pending_since = at;
    }
}
pub fn materialize(action: &Value, old: Option<&Task>) -> Result<Task, String> {
    let mut task = old.cloned().unwrap_or_default();
    let fields = action["fields"].as_array().ok_or("AI 动作缺少字段差异")?;
    let mut seen = BTreeSet::new();
    for change in fields {
        let key = change["field"].as_str().ok_or("AI 字段无效")?;
        let value = change["value"].as_str().ok_or("AI 字段重复或无效")?;
        if !seen.insert(key) {
            return Err("AI 字段重复或无效".into());
        }
        if key == "deferred_until" {
            task.deferred_until = if value == "0" {
                0
            } else {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map_err(|_| "延后时间无效")?
                    .timestamp_millis()
            };
        } else {
            *task.text_field_mut(key).ok_or("AI 字段重复或无效")? = value.into();
        }
    }
    task.reason = action["reason"].as_str().ok_or("AI 动作缺少理由")?.into();
    Ok(task)
}

pub fn clip(s: &str, n: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(n).collect();
    if chars.next().is_some() {
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
pub(crate) fn text_fields<'a>(
    fields: impl IntoIterator<Item = (&'a str, &'a str)>,
    n: usize,
) -> Value {
    let mut out = serde_json::Map::new();
    let mut truncated = vec![];
    for (key, text) in fields {
        if text.is_empty() {
            continue;
        }
        if text.chars().nth(n).is_some() {
            truncated.push(key);
        }
        out.insert(
            key.into(),
            json!(if key == "id" || key == "task_id" {
                text.to_owned()
            } else {
                clip(text, n)
            }),
        );
    }
    if !truncated.is_empty() {
        out.insert("truncated_fields".into(), json!(truncated));
    }
    Value::Object(out)
}
fn project(v: &Value, fields: &[&str], n: usize) -> Value {
    let mut out = text_fields(
        fields
            .iter()
            .filter_map(|&key| v[key].as_str().map(|s| (key, s))),
        n,
    );
    for key in fields {
        if let Some(value) = v.get(key).filter(|v| !v.is_null() && !v.is_string()) {
            out[*key] = value.clone();
        }
    }
    out
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
pub fn task_detail(t: &Task) -> Value {
    let mut value = json!(t);
    value.as_object_mut().unwrap().remove("steps");
    if t.summary.trim().is_empty() {
        value["summary"] = json!(crate::model::summary_content(t));
    }
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
    audit_changes(
        json!({"task_id":a.task_id,"time":a.time,"kind":a.kind,"detail":clip(&a.detail,160)}),
        &a.before,
        &a.after,
    )
}
pub fn audit_value(a: &Value) -> Value {
    audit_changes(
        json!({"task_id":a["task_id"],"time":a["time"],"kind":a["kind"],"detail":clip(a["detail"].as_str().unwrap_or(""),160)}),
        &a["before"],
        &a["after"],
    )
}
fn audit_changes(mut result: Value, before: &Value, after: &Value) -> Value {
    let [before, after] = [before, after].map(|value| {
        value.as_array().map_or(value, |rows| {
            rows.iter()
                .find(|row| row["id"] == result["task_id"])
                .unwrap_or(&Value::Null)
        })
    });
    let mut changes = serde_json::Map::new();
    for key in [
        "title",
        "description",
        "status",
        "blocker",
        "waiting_for",
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
    result["changes"] = Value::Object(changes);
    result["note"] = json!("字段为摘录；省略字段不代表清空");
    if before["decision_context"].is_object() {
        result["prior_decision"] = before["decision_context"].clone();
    }
    result
}
/// Find the largest nonempty prefix that fits a monotone request budget.
/// Preparation errors stop the search; None means only that this prefix is too large.
pub(crate) fn fit_prefix<T>(
    count: usize,
    mut prepare: impl FnMut(usize) -> Result<Option<T>, String>,
) -> Result<Option<(usize, T)>, String> {
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

/// Select whole rows under a section budget and disclose omissions; never silently truncate JSON.
pub fn rows(items: impl IntoIterator<Item = Value>, chars: usize) -> Value {
    let mut items = items.into_iter();
    let mut result = vec![];
    let mut used = 2;
    let mut omitted = 0;
    while used < chars {
        let Some(row) = items.next() else { break };
        let n = json_chars(&row) + usize::from(!result.is_empty());
        if n <= chars.saturating_sub(used) {
            used += n;
            result.push(row);
        } else {
            omitted += 1;
        }
    }
    json!({"items":result,"omitted":omitted + items.count()})
}
pub fn suggestions(s: &State, context: &str) -> Value {
    let terms = terms([context]);
    let mut entries = s.suggestions.iter().collect::<Vec<_>>();
    entries.sort_by_cached_key(|r| {
        (
            std::cmp::Reverse(
                terms
                    .intersection(&self::terms([r.title.as_str(), r.rule.as_str()]))
                    .count(),
            ),
            std::cmp::Reverse(r.updated),
        )
    });
    rows(entries.into_iter().map(|r|json!({"id":r.id,"title":clip(&r.title,80),"rule":clip(&r.rule,240),"decision":crate::evolution::decision(r),"evidence":r.evidence.iter().rev().take(3).collect::<Vec<_>>()})),2400)
}
fn terms<'a>(parts: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for part in parts {
        for word in part
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
        {
            if word.is_ascii() && word.len() >= 3 {
                out.insert(word.to_string());
            }
            if !word.is_ascii() {
                for ((start, _), (end, ch)) in word.char_indices().zip(word.char_indices().skip(3))
                {
                    out.insert(word[start..end + ch.len_utf8()].to_owned());
                }
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
const REPLY_CONTEXT_CHARS: usize = 8000;

/// Build one fresh-session batch. Only explicit reply ancestors may accompany
/// the delta, within the serialized character budget. Other history remains in
/// the bounded snapshot tool; proximity alone never includes a message body.
pub fn classify(s: &State, changes: &[Message]) -> Batch {
    // Keys include source/account identity. A room name alone is never an identity.
    let mut owners: BTreeMap<&str, Vec<&Task>> = BTreeMap::new();
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
    let changed_ids = changes
        .iter()
        .map(|m| m.id.as_str())
        .collect::<BTreeSet<_>>();
    let max_room_id = format!("r{}", changes.len() + s.messages.len());
    let mut context = Vec::new();
    let mut reply_messages = Vec::new();
    let mut walked = BTreeMap::new();
    let mut context_chars = 2; // JSON array brackets; count each row and separator below.
    let mut reply_context_incomplete = false;
    for (chain_id, m) in changes.iter().enumerate() {
        let mut cursor = m;
        // Explicit reply identity is structural context, not an arbitrary
        // neighbouring-message window.
        loop {
            if let Some(previous_chain) = walked.insert(cursor.id.as_str(), chain_id) {
                reply_context_incomplete |= previous_chain == chain_id;
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
            if cursor.reply.is_empty() {
                break;
            }
            let Some(parent) = s
                .messages
                .get(&cursor.reply)
                .filter(|p| !p.withdrawn && p.source_id == m.source_id && p.chat == m.chat)
            else {
                reply_context_incomplete = true;
                break;
            };
            if !changed_ids.contains(parent.id.as_str()) && !walked.contains_key(parent.id.as_str())
            {
                reply_messages.push(parent);
                // Measure the uncompressed row conservatively. Room compression
                // only reduces repeated metadata; the final request still goes
                // through the engine's exact global input-budget check.
                let mut row = message(parent, 2000);
                row["room"] = json!(max_room_id);
                let chars = json_chars(&row) + usize::from(!context.is_empty());
                if context_chars + chars <= REPLY_CONTEXT_CHARS {
                    context_chars += chars;
                    context.push(parent);
                    reply_context_incomplete |= row["truncated"] == true;
                } else {
                    reply_context_incomplete = true;
                }
            }
            cursor = parent;
        }
    }
    let query = terms(changes.iter().map(|m| m.text.as_str()));
    let rooms = changes
        .iter()
        .map(|m| (&m.source_id, &m.chat))
        .collect::<BTreeSet<_>>();
    for t in s.tasks.iter().filter(|t| {
        !t.archived
            && t.merged_into.is_empty()
            && t.generation.is_empty()
            && (["待处理", "处理中"].contains(&t.status.as_str()) || t.updated > now() - 86_400_000)
    }) {
        let words = terms([t.title.as_str(), t.description.as_str()]);
        let (shared, precise) =
            query
                .intersection(&words)
                .fold((0, false), |(count, precise), word| {
                    (
                        count + 1,
                        precise
                            || (word.is_ascii()
                                && word.len() >= 6
                                && word.chars().any(|c| c.is_ascii_digit())),
                    )
                });
        let same_room = t
            .evidence
            .iter()
            .filter_map(|id| s.messages.get(id))
            .any(|p| rooms.contains(&(&p.source_id, &p.chat)));
        if precise
            || (same_room
                && (shared >= 2
                    || changes
                        .iter()
                        .any(|m| (m.time - t.updated).abs() < 86_400_000)))
        {
            scores
                .entry(&t.id)
                .or_insert((if precise { 500 } else { 100 + shared.min(99) }, t));
        }
    }
    // Freeze the complete relevance order once. Budget fitting only takes a
    // prefix of these summaries; it never reranks tasks for each candidate.
    let mut ordered = scores
        .values()
        .copied()
        .chain(
            s.tasks
                .iter()
                .filter(|task| {
                    !task.archived
                        && task.merged_into.is_empty()
                        && task.generation.is_empty()
                        && !scores.contains_key(task.id.as_str())
                })
                .map(|task| (0, task)),
        )
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(score, t)| {
        (
            std::cmp::Reverse(*score),
            std::cmp::Reverse(t.updated),
            &t.id,
        )
    });
    let allowed = changes
        .iter()
        .chain(context.iter().copied())
        .map(|m| (m.id.clone(), m.clone()))
        .collect();
    let versions = ordered
        .iter()
        .map(|(_, t)| (t.id.clone(), t.updated))
        .collect::<BTreeMap<_, _>>();
    let initial_tasks = ordered
        .iter()
        .map(|(score, t)| {
            let mut v = task(t);
            if *score > 0 {
                v["match_rank"] = json!(score);
            }
            v
        })
        .collect::<Vec<_>>();
    let history = rows(
        s.audit
            .iter()
            .rev()
            .filter(|a| {
                !a.task_id.is_empty()
                    && versions.contains_key(&a.task_id)
                    && (a.kind.starts_with("manual") || a.kind == "merge" || a.kind == "undo")
            })
            .map(audit),
        2800,
    );
    let mut records = changes
        .iter()
        .chain(reply_messages.iter().copied())
        .map(|m| {
            // Full text and secondary metadata remain local until explicitly requested.
            let mut data = json!(m);
            let o = data.as_object_mut().unwrap();
            o.retain(|_, v| v != &json!(false));
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
    let mut pack = MessagePack::default();
    let changes_wire = changes
        .iter()
        .map(|m| pack.push(m, 2000))
        .collect::<Vec<_>>();
    let context_wire = context
        .iter()
        .map(|m| pack.push(m, 2000))
        .collect::<Vec<_>>();
    let room_aliases = pack
        .keys
        .iter()
        .map(|((source, chat, _), id)| (id.clone(), json!({"source":source,"chat":chat})))
        .collect::<BTreeMap<_, _>>();
    let mut payload = json!({"changes":changes_wire,"rooms":pack.rooms,"context":context_wire,"tasks":initial_tasks,"manual_history":history,"leaders":s.config.leaders,"rules":rules,"retrieval":{"open_tasks":ordered.len(),"local_candidates":scores.len(),"omitted_tasks":0},"_local_context":{"records":records,"room_aliases":room_aliases,"expires":now()+300_000}});
    if reply_context_incomplete {
        payload["reply_context_incomplete"] = json!(true);
    }
    if payload["manual_history"]["items"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        payload.as_object_mut().unwrap().remove("manual_history");
    }
    omit_empty(&mut payload);
    Batch {
        payload,
        fingerprints: changes
            .iter()
            .map(|m| (m.id.clone(), fingerprint(m)))
            .collect(),
        allowed,
        versions,
    }
}

/// Project a payload into the bounded per-purpose request shape. This is the
/// current request contract, not a migration of stored old snapshots.
pub fn prepare(kind: &str, p: &Value) -> Value {
    let mut p = p.clone();
    if kind == "advice_revision" {
        let selected = rows(
            p["evidence"].as_array().into_iter().flatten().map(|e| {
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
                        "rounds",
                        "omitted_rounds",
                    ],
                    400,
                );
                if let Some(task) = e.get("task") {
                    row["task"] = task_value(task);
                }
                if e.is_string() {
                    return json!(clip(e.as_str().unwrap(), 600));
                }
                row
            }),
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
        let selected = rows(audits, 7000);
        p["daily_evidence"] = json!({"audit":selected["items"],"omitted":selected["omitted"],"logs":[],"notice":"仅人工修正的字段差异；技术运行日志不用于推导业务规则，摘录之外不推断"});
    }
    if kind == "report" {
        let mut tasks = p["report_tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        tasks.sort_by_cached_key(|t| {
            (
                t["status"] != "待处理" && t["status"] != "处理中",
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
        let selected = rows(tasks.into_iter().map(task_value), 7000);
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
            5000,
        );
        p["delta"] = delta;
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
        let messages = vec![crate::test_support::message("new", "New message", at)];
        assert!(automatic_due(&s, &messages, at, at - 2_000));
        s.message_filter.consume(Some(s.message_filter.last_run));
        assert!(!automatic_due(&s, &messages, at, at - 2_000));
        assert!(!automatic_due(&s, &messages, at + 65_000, at - 2_000));
        assert!(automatic_due(&s, &messages, at + 31 * 60_000, at - 2_000));
        s.message_filter.early = true;
        assert!(!automatic_due(&s, &[], at, 0));
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
        let batch = classify(&s, &[a]);
        assert_eq!(batch.payload["changes"][0]["text"], "请验证");
        assert!(batch.payload.get("task_index").is_none());
        assert_eq!(
            batch.payload["_local_context"]["records"][0]["data"]["source_id"],
            "account1"
        );
    }

    #[test]
    fn timer_threshold_and_manual_reset_are_independent_of_scan_clock() {
        let mut s = State::default();
        s.config.analysis_interval_minutes = 30;
        let fixed = s.config.max_input_chars - crate::ai::message_budget(&s.config, "classify");
        s.config.max_input_chars = fixed + 9000;
        let at = 10_000_000;
        let small = vec![crate::test_support::message("m", "短消息", 0)];
        mark_pending(&mut s, true, at);
        assert!(!due(&s, &small, at + 29 * 60_000));
        assert!(due(&s, &small, at + 30 * 60_000));
        let big = (0..30)
            .map(|i| crate::test_support::message(i.to_string(), "大".repeat(550), 0))
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
            tags: (0..12)
                .map(|i| crate::tags::Tag {
                    kind: "合成类型".into(),
                    value: format!("核对{i}"),
                })
                .collect(),
            manual: true,
            ..Default::default()
        };
        let a = json!({"action":"update","task_id":"t","reason":"已经收到答复","fields":[{"field":"waiting_for","value":""}],"tag_changes":[]});
        let updated = materialize(&a, Some(&t)).unwrap();
        assert_eq!(updated.description, t.description);
        assert_eq!(updated.priority, "高");
        assert_eq!(updated.title, t.title);
        assert_eq!(updated.tags, t.tags);
        assert_eq!(task(&t)["tags"], json!(t.tags));
        assert!(updated.waiting_for.is_empty());
        let mut a = a;
        a["fields"] = json!([{"field":"sources","value":"fake"}]);
        assert!(materialize(&a, Some(&t)).is_err());
        a["fields"] = json!([{"field":"title","value":"a"},{"field":"title","value":"b"}]);
        assert!(materialize(&a, Some(&t)).is_err());
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
        let prepared = crate::ai::prepare(&c, "classify", &payload);
        assert!(crate::ai::validate_prepared(&c, &payload, &prepared).is_err());
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
        let a = classify(&s, &[m.clone()]);
        assert_eq!(a.payload["context"][0]["id"], "old");
        assert_eq!(a.payload["context"][0]["text"], "旧故障");
        assert_eq!(a.payload["context"].as_array().unwrap().len(), 1);
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
        let b = classify(&s, &[m]);
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
    fn message_bodies_truncate_by_unicode_chars_and_always_carry_metadata() {
        for (chars, truncated) in [(1999usize, false), (2000, false), (2001, true)] {
            let m = crate::test_support::message(format!("m{chars}"), "甲".repeat(chars), 0);
            let row = message(&m, 2000);
            assert_eq!(row["id"], format!("m{chars}"));
            assert_eq!(row["truncated"], truncated);
            assert_eq!(row["original_chars"], chars);
            assert_eq!(
                row["text"].as_str().unwrap().chars().count(),
                chars.min(2000)
            );
        }
        let emoji = crate::test_support::message("emoji", "🙂".repeat(2001), 0);
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
        for (budget, selected, omitted) in [
            (9, json!(["x", ""]), 2),
            (8, json!(["x", ""]), 2),
            (7, json!(["x", 0]), 2),
            (2, json!([]), 4),
        ] {
            let input = [json!("long".repeat(30)), json!("x"), json!(""), json!(0)];
            assert_eq!(
                rows(input, budget),
                json!({"items":selected,"omitted":omitted})
            );
        }
        let short = vec![json!("甲"); 30];
        let budget = json_chars(&json!(short));
        assert_eq!(
            rows(short.clone(), budget),
            json!({"items":short,"omitted":0})
        );
        let mut event = json!({"kind":"manual_edit","task_id":"x","time":1,"detail":"Synthetic correction","before":{"id":"x","title":"Before","description":"a".repeat(10000)},"after":{"id":"x","title":"After","description":"b".repeat(10000)}});
        for array in [false, true] {
            if array {
                event["before"] = json!([{"id":"other","title":"Unrelated"},event["before"]]);
                event["after"] = json!([event["after"]]);
            }
            let row = audit(&serde_json::from_value(event.clone()).unwrap());
            assert_eq!(row, audit_value(&event));
            assert_eq!(
                row["changes"]["title"],
                json!({"before":"Before","after":"After"})
            );
        }
        let p = json!({"daily_evidence":{"audit":(0..200).map(|i|json!({"evidence_id":format!("audit:{i}"),"event":event})).collect::<Vec<_>>(),"logs":[{"raw":"x".repeat(100000)}]}});
        let p = prepare("prompt_review", &p);
        assert!(p.to_string().len() < 12000);
        assert!(p["daily_evidence"]["audit"][0]["event"]["before"].is_null());
        let short = json!({"report_tasks":(0..30).map(|i|json!({"id":i.to_string(),"title":"task","status":"待处理"})).collect::<Vec<_>>()});
        let p = prepare("report", &short);
        assert_eq!(p["report_tasks"].as_array().unwrap().len(), 30);
        assert_eq!(p["coverage"]["omitted_tasks"], 0);
        let p = prepare(
            "report",
            &json!({"report_tasks":(0..10000).map(|i|json!({"id":i.to_string(),"title":"task","description":"x".repeat(500),"status":"待处理"})).collect::<Vec<_>>(),"delta":[event]}),
        );
        assert!(p.to_string().len() < 15000);
        assert!(p["coverage"]["omitted_tasks"].as_u64().unwrap() > 0);
    }
}
