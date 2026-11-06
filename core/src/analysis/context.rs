//! The read-only `read_context` tool: bounded pages over the records a
//! request may read.
//!
//! [`query_from`] is the pager. It validates the arguments, walks a
//! [`Records`] source, cuts long bodies into Unicode-character windows that
//! fit the page and the remaining budget, and meters what was delivered.
//! [`Reader`] is the live source of classify requests: every query reads the
//! current data, stages what it delivered, and only [`Reader::accept`] — once
//! the whole tool message fitted the continuation allowance — turns the
//! staged reads into evidence candidates. A rejected page is dropped whole.
//!
//! Ids are raw here; the AI layer maps them to and from its wire short ids.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::batch;
use crate::messages::{self, ImageRef, Message};
use crate::prompts::memory::{ARCHIVED_LIMIT, TEMPORARY_LIMIT};
use crate::tasks::model::{self as tasks, Task};
use crate::Db;
use crate::{Config, Error, Result};

/// Characters of one serialized response, wrapper and notices included.
pub const PAGE: usize = 4000;
/// Retrieval budget of a request when the snapshot names none.
pub const DEFAULT_BUDGET: usize = 60_000;
pub const INVALID_ARGS: &str =
    "提供id，或关键词/来源/会话/类型/时间范围之一；offset续读需单个id；更新流水不含来源/会话字段";
pub const NOTE: &str = "只读上下文摘录；未命中或未列出不等于不存在。可继续按id/范围/offset查询。";
pub const UNAVAILABLE_NOTE: &str = "指定记录不在当前可读取范围内；不返回空文本冒充全文";
pub const INVALID_LOCAL: &str = "本地分析记录格式无效";
pub const IMAGE_OUT_OF_SCOPE: &str = "图片不在本批允许范围内";
const BODY_FIELDS: [&str; 3] = ["text", "description", "detail"];

/// Per-request retrieval accounting. Only delivered rows advance `seen`,
/// `pages`, `offsets` and `complete`; every result counts toward `chars`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meter {
    pub image_calls: usize,
    pub image_bytes: usize,
    pub image_hashes: BTreeSet<String>,
    pub images: BTreeMap<String, String>,
    pub calls: usize,
    pub chars: usize,
    pub seen: BTreeSet<String>,
    pub pages: BTreeSet<String>,
    pub complete: BTreeSet<String>,
    pub offsets: BTreeMap<String, usize>,
    /// Set once the remaining budget cannot carry another response; sticky.
    pub exhausted: bool,
}

/// Validated, resolved filters; a source may use them to skip rows early,
/// the pager checks every row again.
#[derive(Debug, Default)]
pub struct QueryFilter {
    pub ids: BTreeSet<String>,
    pub source: String,
    pub chat: String,
    pub kind: String,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// A readable record source.
pub trait Records {
    fn contains(&self, id: &str) -> bool;
    /// An empty `source` matches any account; `except` excludes one room.
    fn has_room(&self, source: &str, chat: &str, except: Option<(&str, &str)>) -> bool;
    fn rows<'a>(&'a self, filter: &'a QueryFilter) -> Box<dyn Iterator<Item = Value> + 'a>;
    /// Called for rows of the delivered page only, never for errors.
    fn delivered(&self, _row: &Value) {}
    fn error(&self) -> Option<String> {
        None
    }
}

/// Frozen records (`snapshot.records`), served by the same pager.
pub struct Snapshot<'a>(pub &'a Value);

impl Records for Snapshot<'_> {
    fn contains(&self, id: &str) -> bool {
        self.0["records"].as_array().into_iter().flatten().any(|row| row["id"] == id)
    }
    fn has_room(&self, source: &str, chat: &str, except: Option<(&str, &str)>) -> bool {
        self.0["records"].as_array().into_iter().flatten().any(|row| {
            row["chat"] == chat
                && (source.is_empty() || row["source"] == source)
                && except.is_none_or(|(account, room)| row["source"] != account || row["chat"] != room)
        })
    }
    fn rows<'a>(&'a self, _filter: &'a QueryFilter) -> Box<dyn Iterator<Item = Value> + 'a> {
        Box::new(self.0["records"].as_array().into_iter().flatten().cloned())
    }
    fn error(&self) -> Option<String> {
        (!self.0["records"].is_null() && !self.0["records"].is_array()).then(|| INVALID_LOCAL.to_string())
    }
}

/// Query frozen records.
pub fn query(snapshot: &Value, meter: &mut Meter, args: &Value, encoded_limit: Option<usize>) -> String {
    query_from(snapshot, meter, args, encoded_limit, &Snapshot(snapshot))
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Characters of `text` once escaped as a JSON string, quotes excluded.
fn encoded(text: &str) -> usize {
    serde_json::to_string(text).map_or(0, |s| chars(&s).saturating_sub(2))
}

fn page_fits(text: &str, limit: usize, encoded_limit: Option<usize>) -> bool {
    chars(text) <= limit && encoded_limit.is_none_or(|remaining| encoded(text) <= remaining)
}

fn budget_notice(budget: usize) -> String {
    json!({
        "error": format!("本次补查预算已用尽（上限 {budget} 字符），覆盖不完整：请只依据已读内容作答，并在结论中说明未覆盖的部分"),
        "budget_exhausted": true,
        "budget_chars": budget,
    })
    .to_string()
}

/// One page in the making; rendering borrows the rows for every probe.
#[derive(Default)]
struct Page<'a> {
    rows: Vec<Value>,
    errors: Vec<Value>,
    omitted: usize,
    unavailable: Vec<&'a str>,
}

impl Page<'_> {
    fn fits(&self, row: &Value, limit: usize, encoded_limit: Option<usize>) -> bool {
        page_fits(&self.render(Some(row)), limit, encoded_limit)
    }

    fn fits_alone(&self, row: &Value, include_scope: bool) -> bool {
        Page { unavailable: if include_scope { self.unavailable.clone() } else { vec![] }, ..Default::default() }
            .fits(row, PAGE, None)
    }

    fn render(&self, extra: Option<&Value>) -> String {
        #[derive(Serialize)]
        struct Response<'a> {
            #[serde(skip_serializing_if = "<[Value]>::is_empty")]
            errors: &'a [Value],
            note: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            omitted_oversized: Option<usize>,
            page_limit_chars: usize,
            records: Vec<&'a Value>,
            returned: usize,
            #[serde(skip_serializing_if = "<[&str]>::is_empty")]
            unavailable_ids: &'a [&'a str],
            #[serde(skip_serializing_if = "Option::is_none")]
            unavailable_note: Option<&'static str>,
        }
        serde_json::to_string(&Response {
            errors: &self.errors,
            note: NOTE,
            omitted_oversized: (self.omitted > 0).then_some(self.omitted),
            page_limit_chars: PAGE,
            records: self.rows.iter().chain(extra).collect(),
            returned: self.rows.len() + usize::from(extra.is_some()),
            unavailable_ids: &self.unavailable,
            unavailable_note: (!self.unavailable.is_empty()).then_some(UNAVAILABLE_NOTE),
        })
        .unwrap_or_default()
    }
}

/// Room aliases bind an account and a chat for this request only; names
/// and caller-made aliases never widen the records.
fn resolve_room<'a>(
    snapshot: &'a Value,
    source: &'a str,
    chat: &'a str,
    records: &dyn Records,
) -> std::result::Result<(&'a str, &'a str), &'static str> {
    let Some(aliases) = snapshot.get("room_aliases") else {
        return Ok((source, chat));
    };
    let alias = chat.strip_prefix('r').is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()));
    if !alias {
        return Ok((source, chat));
    }
    let Some(room) = aliases.get(chat) else {
        if !source.is_empty() && records.has_room(source, chat, None) {
            return Ok((source, chat));
        }
        return Err("未知会话短编号，请使用本次输入中的 rooms.id 或指定来源和真实会话编号");
    };
    let (Some(account), Some(original)) =
        (room["source"].as_str().filter(|s| !s.is_empty()), room["chat"].as_str().filter(|s| !s.is_empty()))
    else {
        return Err("会话短编号映射无效");
    };
    if !source.is_empty() && source != account {
        return Err("会话短编号与来源不匹配");
    }
    if records.has_room(source, chat, Some((account, original))) {
        return Err("会话短编号与真实会话编号重名，请改按消息 id 查询");
    }
    Ok((account, original))
}

fn invalid_args(args: &Value) -> bool {
    let Some(o) = args.as_object() else { return true };
    let ids = args["ids"].as_array();
    let term = args["query"].as_str().unwrap_or("").trim();
    let text = |key: &str| args[key].as_str().unwrap_or("").trim();
    let (source, chat, kind) = (text("source"), text("chat"), text("kind"));
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let (since, until) = (args["since"].as_i64(), args["until"].as_i64());
    let not_int = |key: &str| args.get(key).is_some_and(|v| v.as_i64().is_none());
    o.keys().any(|k| !["ids", "query", "offset", "source", "chat", "kind", "since", "until"].contains(&k.as_str()))
        || (o.contains_key("ids") && ids.is_none())
        || ids.is_some_and(|a| {
            a.len() > 256 || a.iter().any(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 256))
        })
        || ["query", "source", "chat", "kind"].iter().any(|k| o.get(*k).is_some_and(|v| !v.is_string()))
        || chars(term) > 80
        || args.get("offset").is_some_and(|v| v.as_u64().is_none())
        || (offset > 0
            && (!term.is_empty() || ids.is_none_or(|a| a.len() != 1) || !source.is_empty() || !chat.is_empty()))
        || (kind == "update" && (!source.is_empty() || !chat.is_empty()))
        || chars(source) > 128
        || chars(chat) > 128
        || !["", "message", "task", "update"].contains(&kind)
        || not_int("since")
        || not_int("until")
        || since.zip(until).is_some_and(|(a, b)| a > b)
        || (term.is_empty()
            && ids.is_none_or(Vec::is_empty)
            && source.is_empty()
            && chat.is_empty()
            && kind.is_empty()
            && since.is_none()
            && until.is_none())
}

/// Answer one `read_context` call from `records`. `encoded_limit` is the
/// continuation allowance left on the wire, JSON escaping included.
pub fn query_from(
    snapshot: &Value,
    meter: &mut Meter,
    args: &Value,
    encoded_limit: Option<usize>,
    records: &dyn Records,
) -> String {
    meter.calls += 1;
    let budget = snapshot["budget_chars"].as_u64().map_or(DEFAULT_BUDGET, |b| b as usize);
    if meter.exhausted {
        return budget_notice(budget);
    }
    let page_limit = PAGE.min(budget.saturating_sub(meter.chars));
    // Even an empty page has the wrapper: stop instead of serving empties.
    if !page_fits(&Page::default().render(None), page_limit, encoded_limit) {
        meter.exhausted = true;
        return budget_notice(budget);
    }
    let invalid = invalid_args(args);
    let ids: Vec<&str> = args["ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let term = args["query"].as_str().unwrap_or("").trim().to_lowercase();
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let text = |key: &str| args[key].as_str().unwrap_or("").trim();
    let (source, chat, kind) = (text("source"), text("chat"), text("kind"));
    let (since, until) = (args["since"].as_i64(), args["until"].as_i64());
    let scope = resolve_room(snapshot, source, chat, records);
    let (source, chat) = scope.as_ref().copied().unwrap_or((source, chat));
    let id_filter: BTreeSet<&str> = ids.iter().copied().collect();
    let available: BTreeSet<&str> =
        if invalid { BTreeSet::new() } else { id_filter.iter().copied().filter(|id| records.contains(id)).collect() };
    let mut page =
        Page { unavailable: ids.iter().copied().filter(|id| !available.contains(id)).collect(), ..Default::default() };
    let mut budget_limited = false;
    let mut scope_limited = false;
    if !invalid && scope.is_ok() {
        let filter = QueryFilter {
            ids: id_filter.iter().map(|id| id.to_string()).collect(),
            source: source.into(),
            chat: chat.into(),
            kind: kind.into(),
            since,
            until,
        };
        for r in records.rows(&filter) {
            let id = r["id"].as_str().unwrap_or("");
            let time = r["time"].as_i64().unwrap_or(0);
            if (offset == 0 && meter.seen.contains(id))
                || (!id_filter.is_empty() && !id_filter.contains(id))
                || (!source.is_empty() && r["source"].as_str().unwrap_or("") != source)
                || (!chat.is_empty() && r["chat"].as_str().unwrap_or("") != chat)
                || (!kind.is_empty() && r["kind"].as_str().unwrap_or("") != kind)
                || since.is_some_and(|since| time < since)
                || until.is_some_and(|until| time > until)
                || meter.pages.contains(&format!("{id}@{offset}"))
                || (!term.is_empty() && !r.to_string().to_lowercase().contains(&term))
            {
                continue;
            }
            let mut row = r.clone();
            let body = BODY_FIELDS.into_iter().find(|field| r["data"][field].as_str().is_some());
            let mut error = None;
            let mut no_text_space = false;
            if let Some(field) = body {
                let text = r["data"][field].as_str().unwrap_or("");
                let length = chars(text);
                if offset > 0 && offset >= length {
                    error = Some("offset 超出该记录正文长度");
                } else {
                    // Only this page's window is needed; offsets count chars.
                    let window: Vec<char> = text.chars().skip(offset).take(page_limit).collect();
                    let cut = |row: &mut Value, end: usize| {
                        row["data"][field] = json!(window[..end - offset].iter().collect::<String>());
                        if offset > 0 {
                            row["text_offset"] = json!(offset);
                        }
                        if end < length {
                            row["next_offset"] = json!(end);
                        } else if let Some(o) = row.as_object_mut() {
                            o.remove("next_offset");
                        }
                    };
                    let mut end = offset + window.len();
                    cut(&mut row, end);
                    if end > offset && !page.fits(&row, page_limit, encoded_limit) {
                        let (mut low, mut high) = (offset, end - 1);
                        while low < high {
                            let mid = low + (high - low).div_ceil(2);
                            cut(&mut row, mid);
                            if page.fits(&row, page_limit, encoded_limit) {
                                low = mid;
                            } else {
                                high = mid - 1;
                            }
                        }
                        end = low;
                        cut(&mut row, end);
                    }
                    no_text_space = end == offset && length > 0;
                    if no_text_space {
                        cut(&mut row, offset + 1);
                    }
                }
            } else if offset > 0 {
                error = Some("该记录类型不支持 offset 续读");
            }
            let mut oversized = false;
            if error.is_none() && (no_text_space || !page.fits(&row, page_limit, encoded_limit)) {
                if !page.rows.is_empty() {
                    // The row that did not fit stays unread for the next page.
                    break;
                }
                if page.fits_alone(&row, false) {
                    if !page.errors.is_empty() {
                        break;
                    }
                    // A wide query or a small allowance must not make a
                    // readable record permanently unseen.
                    scope_limited |= !page.fits_alone(&row, true);
                    budget_limited = true;
                    continue;
                }
                oversized = !no_text_space;
                error = Some(if no_text_space {
                    "该记录正文与元数据无法装入单页上限"
                } else {
                    "该记录元数据超过单页上限，无法安全返回"
                });
            }
            if let Some(error) = error {
                page.errors.push(json!({"id": id, "error": error}));
                page.omitted += usize::from(oversized);
                let error_page = page.render(None);
                if !page_fits(&error_page, page_limit, encoded_limit) {
                    page.errors.pop();
                    page.omitted -= usize::from(oversized);
                    scope_limited |= chars(&error_page) > PAGE;
                    budget_limited = true;
                    break;
                }
                if !oversized && id_filter.len() == 1 {
                    break;
                }
                continue;
            }
            page.rows.push(row);
        }
    }
    let limited = || {
        json!({"error": "剩余补查预算不足以返回所选记录的最小内容；可查询更小记录，覆盖仍不完整",
               "budget_limited": true, "remaining_chars": page_limit})
        .to_string()
    };
    let too_wide =
        || json!({"error": "本次查询结果超过单页上限，请缩小范围或按id分页", "page_limit_chars": PAGE}).to_string();
    let (result, delivered) = if let Some(error) = records.error() {
        (json!({"error": error}).to_string(), false)
    } else if invalid {
        (json!({"error": INVALID_ARGS}).to_string(), false)
    } else if let Err(error) = scope {
        (json!({"error": error}).to_string(), false)
    } else if scope_limited && page.rows.is_empty() && page.errors.is_empty() {
        (too_wide(), false)
    } else if budget_limited && page.rows.is_empty() && page.errors.is_empty() {
        (limited(), false)
    } else {
        let value = page.render(None);
        if chars(&value) > PAGE {
            (too_wide(), false)
        } else if !page_fits(&value, page_limit, encoded_limit) {
            (limited(), false)
        } else {
            (value, true)
        }
    };
    if !page_fits(&result, page_limit, encoded_limit) {
        meter.exhausted = true;
        return budget_notice(budget);
    }
    meter.chars += chars(&result);
    if delivered {
        // A delivered error uses a page, never coverage.
        meter.pages.extend(
            page.rows.iter().chain(&page.errors).map(|row| format!("{}@{offset}", row["id"].as_str().unwrap_or(""))),
        );
        for row in &page.rows {
            records.delivered(row);
            let id = row["id"].as_str().unwrap_or("");
            meter.seen.insert(id.into());
            if offset <= meter.offsets.get(id).copied().unwrap_or(0) {
                if let Some(text) = BODY_FIELDS.into_iter().find_map(|field| row["data"][field].as_str()) {
                    let frontier = meter.offsets.entry(id.into()).or_default();
                    *frontier = (*frontier).max(offset + chars(text));
                }
                if row.get("next_offset").is_none() {
                    meter.complete.insert(id.into());
                }
            }
        }
    }
    result
}

// ---- memory replacement -----------------------------------------------------

/// The temporary-memory replacement a classify result asks for, checked
/// against what the model fully read: `{before, text, expected}`.
pub fn memory_update(snapshot: &Value, meter: &Meter, result: &Value) -> Result<Option<Value>> {
    replacement(snapshot, meter, result, "memory_update", "memory", "memory", TEMPORARY_LIMIT)
}

/// The archived-memory replacement of an archive result.
pub fn archived_memory_update(snapshot: &Value, meter: &Meter, result: &Value) -> Result<Option<Value>> {
    replacement(snapshot, meter, result, "archived_memory", "archived_memory", "archived-memory", ARCHIVED_LIMIT)
}

fn replacement(
    snapshot: &Value,
    meter: &Meter,
    result: &Value,
    field: &str,
    slot: &str,
    record: &str,
    max: usize,
) -> Result<Option<Value>> {
    let Some(value) = result.get(field) else { return Ok(None) };
    let rows = value.as_array().ok_or("笔记更新必须是数组")?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 || snapshot[slot]["writable"] != true {
        crate::bail!("共享笔记不可写或更新超过一份");
    }
    let before = snapshot[slot]["before"].as_str().ok_or("笔记原版本无效")?;
    if !before.trim().is_empty()
        && !meter.complete.contains(record)
        && snapshot[slot]["inline_text"].as_str() != Some(before)
    {
        crate::bail!("未完整读取原笔记，本次不覆盖");
    }
    let text = rows[0].as_str().ok_or("笔记内容无效")?;
    if chars(text) > max {
        crate::bail!("笔记超过{max}字符，未保存");
    }
    Ok(Some(json!({"before": before, "text": text, "expected": snapshot[slot]["revision"]})))
}

// ---- live reads -----------------------------------------------------------------

/// What a request read beyond its batch: messages that may become evidence,
/// task versions the result depends on, and the delivered rows.
#[derive(Clone, Debug, Default)]
pub struct ReadSet {
    pub messages: BTreeMap<String, Message>,
    pub task_versions: BTreeMap<String, i64>,
    /// `seq` of the update rows read.
    pub updates: BTreeSet<i64>,
    pub records: Vec<Value>,
}

impl ReadSet {
    pub(crate) fn merge(&mut self, other: ReadSet) {
        self.messages.extend(other.messages);
        self.task_versions.extend(other.task_versions);
        self.updates.extend(other.updates);
        self.records.extend(other.records);
    }
}

/// The live source of one classify request.
pub struct Reader {
    pub(crate) db: Db,
    retention_days: u32,
    budget: usize,
    /// The request's own records (`_local_context.records`).
    records: Vec<Value>,
    accepted: Mutex<ReadSet>,
    pending: Mutex<Option<ReadSet>>,
}

impl Reader {
    pub fn new(db: Db, config: &Config, payload: &Value) -> Self {
        Self {
            db,
            retention_days: config.message_retention_days,
            budget: config.retrieval_budget_chars,
            records: payload["_local_context"]["records"].as_array().cloned().unwrap_or_default(),
            accepted: Mutex::default(),
            pending: Mutex::default(),
        }
    }

    /// Answer one call from current data (off the executor).
    pub fn query_blocking(
        &self,
        snapshot: &Value,
        meter: &mut Meter,
        args: &Value,
        encoded_limit: Option<usize>,
    ) -> String {
        *self.pending.lock().unwrap() = None;
        let mut snapshot = snapshot.clone();
        if snapshot.is_object() && snapshot.get("budget_chars").is_none() {
            snapshot["budget_chars"] = json!(self.budget);
        }
        let live = Live {
            accepted: self.accepted.lock().unwrap().clone(),
            records: self.records.clone(),
            cutoff: messages::cutoff(self.retention_days),
        };
        match self.db.analysis_context_blocking(live, snapshot, meter.clone(), args.clone(), encoded_limit) {
            Ok((result, after, pending)) => {
                *meter = after;
                *self.pending.lock().unwrap() = Some(pending);
                result
            }
            Err(e) => json!({"error": e.0}).to_string(),
        }
    }

    /// The last tool message fitted the allowance: keep what it read.
    pub fn accept(&self) {
        if let Some(pending) = self.pending.lock().unwrap().take() {
            self.accepted.lock().unwrap().merge(pending);
        }
    }

    /// Everything accepted so far: extra evidence candidates and versions.
    pub fn accepted(&self) -> ReadSet {
        self.accepted.lock().unwrap().clone()
    }

    /// An image the request may look at: from its own messages or a message
    /// whose read was accepted. Paths stay with the layer.
    pub fn image(&self, image_id: &str) -> Result<ImageRef> {
        let own = self.records.iter().filter(|r| r["kind"] == "message").find_map(|r| {
            r["data"]["images"]
                .as_array()?
                .iter()
                .find(|i| i["id"] == image_id)
                .and_then(|i| Some(ImageRef { id: image_id.into(), path: i["path"].as_str()?.into() }))
        });
        own.or_else(|| {
            self.accepted.lock().unwrap().messages.values().flat_map(|m| &m.images).find(|i| i.id == image_id).cloned()
        })
        .ok_or_else(|| Error::new(IMAGE_OUT_OF_SCOPE))
    }
}

/// Inputs of one live query, moved onto the data thread.
pub(crate) struct Live {
    pub accepted: ReadSet,
    pub records: Vec<Value>,
    pub cutoff: i64,
}

/// Whitelisted task fields an update row may show.
const UPDATE_FIELDS: [&str; 36] = [
    "decision_context",
    "suggested_project_id",
    "suggested_project_reason",
    "tags",
    "id",
    "title",
    "description",
    "summary",
    "content_updated_at",
    "summary_updated_at",
    "summary_fallback",
    "evidence",
    "priority",
    "environment",
    "category",
    "generation",
    "generation_error",
    "status",
    "blocker",
    "waiting_for",
    "due",
    "created",
    "updated",
    "archived",
    "archive_note",
    "archived_at",
    "manual",
    "locked",
    "reason",
    "merged_into",
    "follow_deadline",
    "deadline_changed",
    "deferred_until",
    "resume_processing",
    "schedule_id",
    "scheduled_for",
];

fn whitelisted<'v>(value: &'v Value, task_id: &str) -> BTreeMap<&'static str, &'v Value> {
    let selected = match value {
        Value::Array(rows) => rows.iter().find(|row| row["id"].as_str() == Some(task_id)),
        Value::Object(_) => Some(value),
        _ => None,
    };
    UPDATE_FIELDS.into_iter().filter_map(|field| selected?.get(field).map(|v| (field, v))).collect()
}

fn parse(text: Option<String>) -> Value {
    text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// One `task_audit` row as an update record: the whitelisted change document.
pub(crate) fn update_row(
    seq: i64,
    at: i64,
    kind: &str,
    task_id: &str,
    before: Option<String>,
    after: Option<String>,
) -> Value {
    let (before, after) = (parse(before), parse(after));
    let (before, after) = (whitelisted(&before, task_id), whitelisted(&after, task_id));
    let fields: Vec<&str> = UPDATE_FIELDS.into_iter().filter(|f| before.get(f) != after.get(f)).collect();
    let document = json!({"task_id": task_id, "audit_kind": kind, "time": at, "before": before, "after": after});
    json!({
        "id": format!("audit:{seq}"), "kind": "update", "source": "", "chat": "", "time": at,
        "data": {
            "task_id": task_id, "audit_kind": kind, "audit_index": seq, "evidence_id": format!("audit:{seq}"),
            "fields": fields, "detail": document.to_string(),
            "note": "detail 为完整白名单变更文档，可用 next_offset 续读；省略字段不代表清空",
        }
    })
}

/// A message as `read_context` shows it: the whole body.
pub(crate) fn message_row(m: &Message) -> Value {
    let mut data = batch::message(m, usize::MAX);
    data["truncated"] = json!(false);
    json!({"id": m.id, "kind": "message", "source": m.source_id, "chat": m.chat, "time": m.time, "data": data})
}

/// A task as `read_context` shows it: every field, with a summary.
pub fn task_detail(t: &Task) -> Value {
    let mut v = t.payload();
    if t.summary.trim().is_empty() {
        let summary = [&t.description, &t.title]
            .into_iter()
            .map(|s| tasks::summary_fallback(s))
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| "（未命名事项）".into());
        v["summary"] = json!(summary);
    }
    v
}
