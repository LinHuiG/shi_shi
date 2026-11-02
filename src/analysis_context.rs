//! Bounded retrieval from the request's frozen context, never the live archive.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Maximum characters in one serialized read-only response, including wrapper
/// metadata and the error/notice text.
pub const PAGE: usize = 4000;
pub const MEMORY_LIMIT: usize = 2000;
pub const ARCHIVED_MEMORY_LIMIT: usize = 4000;
/// Default per-request retrieval budget in characters (not tokens).
pub const DEFAULT_BUDGET: usize = 60_000;

pub fn validate_budget(chars: usize) -> Result<(), String> {
    if (PAGE..=1_000_000).contains(&chars) {
        Ok(())
    } else {
        Err(format!("补查预算应为 {PAGE}–1000000 字符"))
    }
}

pub fn tools() -> Value {
    let mut value = json!({"tools":[{"name":"read_context","description":"只读查询分析快照（消息/事项/更新流水），可跨来源和会话按id、关键词、来源、会话、类型和时间范围检索；事项按updated时间过滤，来源/会话仅在其全部依据一致时可用，更新流水不支持来源/会话过滤；长文本用next_offset分页续读，每次返回有上限，可重复按需查询；重复页和已读区域不会重复返回，进程受取消和超时控制。","inputSchema":{"type":"object","additionalProperties":false,"properties":{"ids":{"type":"array","items":{"type":"string"}},"query":{"type":"string","maxLength":80},"offset":{"type":"integer","minimum":0},"source":{"type":"string","maxLength":128},"chat":{"type":"string","maxLength":128},"kind":{"type":"string","enum":["message","task","update"]},"since":{"type":"integer"},"until":{"type":"integer"}},"required":[]},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}]});
    value["tools"][0]["inputSchema"]["properties"]["chat"]["description"] =
        json!("本次输入中的 rooms.id（自动绑定来源），或真实会话编号；不按展示名称匹配");
    value["tools"]
        .as_array_mut()
        .unwrap()
        .push(crate::images::tool());
    value
}

/// Attach archived and temporary memory without copying it into the public
/// prompt prefix. Both records are readable through the same bounded tool.
pub fn attach_memory_text(payload: &mut Value, archived: &str, temporary: &str) -> bool {
    if !payload["_local_context"].is_object() {
        payload["_local_context"] = json!({"records":[],"expires":crate::model::now()+300_000});
    }
    let snapshot = &mut payload["_local_context"];
    let records = snapshot["records"].as_array_mut().unwrap();
    records.retain(|r| !["memory", "archived-memory"].contains(&r["kind"].as_str().unwrap_or("")));
    if !temporary.trim().is_empty() {
        records.push(json!({"id":"memory","kind":"memory","data":{"text":temporary}}));
    }
    if !archived.trim().is_empty() {
        records.push(json!({"id":"archived-memory","kind":"memory","data":{"text":archived}}));
    }
    snapshot["memory"] =
        json!({"before":temporary,"writable":temporary.chars().count() <= MEMORY_LIMIT});
    snapshot["archived_memory"] = json!({"before":archived,"writable":false});
    payload["_archived_memory"] = json!(archived);
    payload["_temporary_memory"] = json!(temporary);
    payload["memory"] = json!({"available":!temporary.trim().is_empty(),"archived_available":!archived.trim().is_empty(),"writable":temporary.chars().count() <= MEMORY_LIMIT,"max_chars":MEMORY_LIMIT,"archived_max_chars":ARCHIVED_MEMORY_LIMIT});
    !archived.trim().is_empty()
}

/// Append read-only snapshot records to the per-request context file. The
/// records are only served on demand; they are never embedded in the prompt.
/// Attach a frozen set of read-only records. Used by archive so the model sees
/// exactly the candidate snapshot even if scans or manual edits continue.
pub fn attach_records(
    payload: &mut Value,
    records: Vec<Value>,
    budget_chars: usize,
) -> Result<usize, String> {
    if !payload["_local_context"].is_object() {
        payload["_local_context"] = json!({"records":[],"expires":crate::model::now()+300_000});
    }
    payload["_local_context"]["budget_chars"] = json!(budget_chars);
    let existing = payload["_local_context"]["records"]
        .as_array_mut()
        .ok_or("本地分析记录格式无效")?;
    // Keep memory records first and replace any previous snapshot rows.
    existing.retain(|row| row["kind"] == "memory");
    let count = records.len();
    existing.extend(records);
    Ok(count)
}

/// Record delivery only from the exact prepared prompt, never from a summary
/// or a caller-provided flag. This private receipt travels with this snapshot.
pub fn acknowledge_inline_memory(payload: &mut Value, kind: &str, prompt: &str) {
    // Archived memory is also a complete document. Only archive may replace it;
    // legacy documents above the new limit remain readable for consolidation.
    if let Some(memory) = payload
        .get_mut("_local_context")
        .and_then(|snapshot| snapshot.get_mut("archived_memory"))
        .and_then(Value::as_object_mut)
    {
        memory.remove("inline_text");
        memory.insert("writable".into(), json!(kind == "archive"));
        let before = memory["before"].as_str().unwrap_or("").to_owned();
        let delivered = prompt
            .split_once("\n\n## 已归档记忆\n")
            .and_then(|(_, tail)| tail.split_once("\n\n## 当前用途\n"))
            .map(|(text, _)| text);
        if kind == "archive" && !before.is_empty() && delivered == Some(before.trim()) {
            memory.insert("inline_text".into(), json!(before));
        }
    }
    let before = payload["_local_context"]["memory"]["before"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if let Some(memory) = payload
        .get_mut("_local_context")
        .and_then(|snapshot| snapshot.get_mut("memory"))
        .and_then(Value::as_object_mut)
    {
        memory.remove("inline_text");
    }
    let delivered = prompt
        .split_once("\n\n## 临时记忆\n")
        .and_then(|(_, tail)| tail.split_once("\n\n## 增量消息\n"))
        .map(|(text, _)| text);
    if kind == "classify"
        && !before.is_empty()
        && payload["_local_context"]["memory"]["writable"] == true
        && payload["_temporary_memory"].as_str() == Some(before.as_str())
        && delivered == Some(before.as_str())
    {
        payload["_local_context"]["memory"]["inline_text"] = json!(before);
    }
}

pub fn memory_update(
    snapshot: &Value,
    meter: &Meter,
    result: &Value,
) -> Result<Option<Value>, String> {
    memory_replacement(
        snapshot,
        meter,
        result,
        "memory_update",
        "memory",
        "memory",
        MEMORY_LIMIT,
    )
}

pub fn archived_memory_update(
    snapshot: &Value,
    meter: &Meter,
    result: &Value,
) -> Result<Option<Value>, String> {
    memory_replacement(
        snapshot,
        meter,
        result,
        "archived_memory",
        "archived_memory",
        "archived-memory",
        ARCHIVED_MEMORY_LIMIT,
    )
}

fn memory_replacement(
    snapshot: &Value,
    meter: &Meter,
    result: &Value,
    field: &str,
    slot: &str,
    record_id: &str,
    max_chars: usize,
) -> Result<Option<Value>, String> {
    let Some(value) = result.get(field) else {
        return Ok(None);
    };
    let rows = value.as_array().ok_or("笔记更新必须是数组")?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 || snapshot[slot]["writable"] != true {
        return Err("共享笔记不可写或更新超过一份".into());
    }
    let before = snapshot[slot]["before"].as_str().ok_or("笔记原版本无效")?;
    if !before.trim().is_empty()
        && !meter.complete.contains(record_id)
        && snapshot[slot]["inline_text"].as_str() != Some(before)
    {
        return Err("未完整读取原笔记，本次不覆盖".into());
    }
    let text = rows[0].as_str().ok_or("笔记内容无效")?;
    if text.chars().count() > max_chars {
        return Err(format!("笔记超过{max_chars}字符，未保存"));
    }
    Ok(Some(
        json!({"before":before,"text":text,"expected":snapshot[slot]["revision"]}),
    ))
}

#[cfg(test)]
mod inline_memory_tests {
    use super::*;
    #[test]
    fn receipt_requires_the_complete_sent_version_and_does_not_replace_paging_checks() {
        let mut payload = json!({});
        attach_memory_text(&mut payload, "", "原始完整笔记");
        let config = crate::model::Config::default();
        let prompt = crate::ai::prepare(&config, "classify", &payload).prompt;
        acknowledge_inline_memory(&mut payload, "classify", &prompt);
        let update = json!({"memory_update":["更新后的笔记"]});
        assert!(
            memory_update(&payload["_local_context"], &Meter::default(), &update)
                .unwrap()
                .is_some()
        );
        for sent in [
            prompt.replace("原始完整笔记", "原始…"),
            prompt.replace("原始完整笔记", ""),
        ] {
            acknowledge_inline_memory(&mut payload, "classify", &sent);
            assert!(memory_update(&payload["_local_context"], &Meter::default(), &update).is_err());
        }
        let mut meter = Meter::default();
        query(
            &payload["_local_context"],
            &mut meter,
            &json!({"ids":["memory"]}),
        );
        assert!(memory_update(&payload["_local_context"], &meter, &update).is_ok());
        acknowledge_inline_memory(&mut payload, "classify", &prompt);
        payload["_local_context"]["memory"]["before"] = json!("另一版本");
        assert!(memory_update(&payload["_local_context"], &Meter::default(), &update).is_err());
        acknowledge_inline_memory(&mut payload, "archive", &prompt);
        assert!(payload["_local_context"]["memory"]
            .get("inline_text")
            .is_none());
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Meter {
    #[serde(default)]
    pub image_calls: usize,
    #[serde(default)]
    pub image_bytes: usize,
    #[serde(default)]
    pub image_hashes: BTreeSet<String>,
    #[serde(default)]
    pub images: std::collections::BTreeMap<String, String>,
    pub calls: usize,
    pub chars: usize,
    pub seen: BTreeSet<String>,
    #[serde(default)]
    pub pages: BTreeSet<String>,
    #[serde(default)]
    pub complete: BTreeSet<String>,
    #[serde(default)]
    pub offsets: std::collections::BTreeMap<String, usize>,
    /// Set once the remaining budget cannot carry another bounded response.
    #[serde(default)]
    pub exhausted: bool,
}

/// Includes errors and wrapper text in the per-response bound. Delivered text
/// is also capped per request by `budget_chars` (default `DEFAULT_BUDGET`);
/// pages use the remaining allowance before declaring exhaustion. Errors and
/// retries also consume that allowance; control notices do not. Exhaustion is sticky.
#[cfg(test)]
pub fn query(snapshot: &Value, meter: &mut Meter, args: &Value) -> String {
    query_with_limit(snapshot, meter, args, None)
}

/// The shared continuation allowance also includes JSON escaping on the wire.
/// Keep the legacy page bound for local MCP readers without another wrapper.
pub(crate) fn query_with_limit(
    snapshot: &Value,
    meter: &mut Meter,
    args: &Value,
    encoded_limit: Option<usize>,
) -> String {
    meter.calls += 1;
    let budget = snapshot["budget_chars"]
        .as_u64()
        .map_or(DEFAULT_BUDGET, |b| b as usize);
    if meter.exhausted {
        return budget_notice(budget);
    }
    let page_limit = PAGE.min(budget.saturating_sub(meter.chars));
    // Even an empty legal page has this fixed wrapper. Do not enter a loop
    // producing empty pages when no record can possibly be returned anymore.
    if !page_fits(&Page::default().render(None), page_limit, encoded_limit) {
        meter.exhausted = true;
        return budget_notice(budget);
    }
    let ids = args["ids"].as_array();
    let term = args["query"].as_str().unwrap_or("").trim().to_lowercase();
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let source = args["source"].as_str().unwrap_or("").trim();
    let chat = args["chat"].as_str().unwrap_or("").trim();
    let kind = args["kind"].as_str().unwrap_or("").trim();
    let since = args["since"].as_i64();
    let until = args["until"].as_i64();
    let invalid = args.as_object().is_none_or(|o| {
        o.keys().any(|k| {
            ![
                "ids", "query", "offset", "source", "chat", "kind", "since", "until",
            ]
            .contains(&k.as_str())
        })
    }) || ids.is_some_and(|a| {
        a.iter()
            .any(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 256))
    }) || term.chars().count() > 80
        || args.get("offset").is_some_and(|v| v.as_u64().is_none())
        || (offset > 0
            && (!term.is_empty()
                || ids.is_none_or(|a| a.len() != 1)
                || !source.is_empty()
                || !chat.is_empty()))
        || (kind == "update" && (!source.is_empty() || !chat.is_empty()))
        || source.chars().count() > 128
        || chat.chars().count() > 128
        || !["", "message", "task", "update"].contains(&kind)
        || args.get("since").is_some_and(|v| v.as_i64().is_none())
        || args.get("until").is_some_and(|v| v.as_i64().is_none())
        || since.zip(until).is_some_and(|(a, b)| a > b)
        || (term.is_empty()
            && ids.is_none_or(|a| a.is_empty())
            && source.is_empty()
            && chat.is_empty()
            && kind.is_empty()
            && since.is_none()
            && until.is_none());
    let scope = resolve_room(snapshot, source, chat);
    let (source, chat) = scope.as_ref().copied().unwrap_or((source, chat));
    let id_filter = ids
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let available_ids = if invalid || id_filter.is_empty() {
        BTreeSet::new()
    } else {
        snapshot["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["id"].as_str())
            .filter(|id| id_filter.contains(id))
            .collect()
    };
    let mut page = Page {
        unavailable: ids
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|id| !available_ids.contains(*id))
            .collect(),
        ..Default::default()
    };
    let mut budget_limited = false;
    let mut scope_limited = false;
    if !invalid && scope.is_ok() {
        for r in snapshot["records"].as_array().into_iter().flatten() {
            let id = r["id"].as_str().unwrap_or("");
            if offset == 0 && meter.seen.contains(id) {
                continue;
            }
            if !id_filter.is_empty() && !id_filter.contains(id) {
                continue;
            }
            if !source.is_empty() && r["source"].as_str().unwrap_or("") != source {
                continue;
            }
            if !chat.is_empty() && r["chat"].as_str().unwrap_or("") != chat {
                continue;
            }
            if !kind.is_empty() && r["kind"].as_str().unwrap_or("") != kind {
                continue;
            }
            let time = r["time"].as_i64().unwrap_or(0);
            if since.is_some_and(|since| time < since) || until.is_some_and(|until| time > until) {
                continue;
            }
            if meter.pages.contains(&format!("{id}@{offset}")) {
                continue;
            }
            if !term.is_empty() && !r.to_string().to_lowercase().contains(&term) {
                continue;
            }
            let mut row = r.clone();
            let body_field = ["text", "description", "detail"]
                .into_iter()
                .find(|field| r["data"][field].as_str().is_some());
            let mut error = None;
            let mut no_text_space = false;
            if let Some(field) = body_field {
                let text = r["data"][field].as_str().unwrap();
                let length = text.chars().count();
                if offset > 0 && offset >= length {
                    error = Some("offset 超出该记录正文长度");
                } else {
                    // Only this page's character window is needed. Offsets are
                    // Unicode characters, while the budget includes the final
                    // JSON wrapper and (when supplied) its encoded wire form.
                    let chars = text
                        .chars()
                        .skip(offset)
                        .take(page_limit)
                        .collect::<Vec<_>>();
                    let window = |row: &mut Value, end: usize| {
                        row["data"][field] =
                            json!(chars[..end - offset].iter().collect::<String>());
                        if offset > 0 {
                            row["text_offset"] = json!(offset);
                        }
                        if end < length {
                            row["next_offset"] = json!(end);
                        } else {
                            row.as_object_mut().unwrap().remove("next_offset");
                        }
                    };
                    let mut end = offset + chars.len();
                    window(&mut row, end);
                    if end > offset && !page.fits(&row, page_limit, encoded_limit) {
                        let (mut low, mut high) = (offset, end - 1);
                        while low < high {
                            let mid = low + (high - low).div_ceil(2);
                            window(&mut row, mid);
                            if page.fits(&row, page_limit, encoded_limit) {
                                low = mid;
                            } else {
                                high = mid - 1;
                            }
                        }
                        end = low;
                        window(&mut row, end);
                    }
                    no_text_space = end == offset && length > 0;
                    if no_text_space {
                        window(&mut row, offset + 1);
                    }
                }
            } else if offset > 0 {
                error = Some("该记录类型不支持 offset 续读");
            }
            let mut oversized = false;
            if error.is_none() && (no_text_space || !page.fits(&row, page_limit, encoded_limit)) {
                if !page.rows.is_empty() {
                    // Leave a row that did not fit unread for the next page.
                    break;
                }
                if page.fits_alone(&row, false) {
                    if !page.errors.is_empty() {
                        break;
                    }
                    // A wide query or a small remaining allowance must not
                    // make this otherwise readable record permanently unseen.
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
                page.errors.push(json!({"id":id,"error":error}));
                page.omitted += usize::from(oversized);
                let error_page = page.render(None);
                if !page_fits(&error_page, page_limit, encoded_limit) {
                    page.errors.pop();
                    page.omitted -= usize::from(oversized);
                    scope_limited |= error_page.chars().count() > PAGE;
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
    let limited_notice = || {
        json!({
            "error":"剩余补查预算不足以返回所选记录的最小内容；可查询更小记录，覆盖仍不完整",
            "budget_limited":true,"remaining_chars":page_limit
        })
        .to_string()
    };
    let scope_notice = || {
        json!({"error":"本次查询结果超过单页上限，请缩小范围或按id分页","page_limit_chars":PAGE})
            .to_string()
    };
    let (result, delivered) = if invalid {
        (
            json!({"error":"提供id，或关键词/来源/会话/类型/时间范围之一；offset续读需单个id；更新流水不含来源/会话字段"})
                .to_string(),
            false,
        )
    } else if let Err(error) = scope {
        (json!({"error":error}).to_string(), false)
    } else if scope_limited && page.rows.is_empty() && page.errors.is_empty() {
        (scope_notice(), false)
    } else if budget_limited && page.rows.is_empty() && page.errors.is_empty() {
        (limited_notice(), false)
    } else {
        let value = page.render(None);
        if value.chars().count() > PAGE {
            (scope_notice(), false)
        } else if !page_fits(&value, page_limit, encoded_limit) {
            (limited_notice(), false)
        } else {
            (value, true)
        }
    };
    if !page_fits(&result, page_limit, encoded_limit) {
        meter.exhausted = true;
        return budget_notice(budget);
    }
    meter.chars += result.chars().count();
    if delivered {
        // Only records actually delivered in the accepted page may advance the
        // seen/page/offset/complete meter. A generic oversized fallback must not
        // make valid rows permanently unreachable.
        // A delivered error consumes a page, never evidence coverage.
        meter.pages.extend(
            page.rows
                .iter()
                .chain(&page.errors)
                .map(|row| format!("{}@{offset}", row["id"].as_str().unwrap_or(""))),
        );
        for row in &page.rows {
            let id = row["id"].as_str().unwrap_or("");
            meter.seen.insert(id.into());
            if offset <= meter.offsets.get(id).copied().unwrap_or(0) {
                if let Some(text) = ["text", "description", "detail"]
                    .into_iter()
                    .find_map(|field| row["data"][field].as_str())
                {
                    let frontier = meter.offsets.entry(id.into()).or_default();
                    *frontier = (*frontier).max(offset + text.chars().count());
                }
                if row.get("next_offset").is_none() {
                    meter.complete.insert(id.into());
                }
            }
        }
    }
    result
}

/// Room aliases are scoped to this frozen request and always bind an account
/// as well as a chat. Names and caller-supplied aliases never broaden records.
fn resolve_room<'a>(
    snapshot: &'a Value,
    source: &'a str,
    chat: &'a str,
) -> Result<(&'a str, &'a str), &'static str> {
    let Some(aliases) = snapshot.get("room_aliases") else {
        return Ok((source, chat));
    };
    let is_alias = chat
        .strip_prefix('r')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()));
    if !is_alias {
        return Ok((source, chat));
    }
    let mut records = snapshot["records"].as_array().into_iter().flatten();
    let Some(room) = aliases.get(chat) else {
        if !source.is_empty() && records.any(|r| r["source"] == source && r["chat"] == chat) {
            return Ok((source, chat));
        }
        return Err("未知会话短编号，请使用本次输入中的 rooms.id 或指定来源和真实会话编号");
    };
    let (Some(account), Some(original)) = (
        room["source"].as_str().filter(|s| !s.is_empty()),
        room["chat"].as_str().filter(|s| !s.is_empty()),
    ) else {
        return Err("会话短编号映射无效");
    };
    if !source.is_empty() && source != account {
        return Err("会话短编号与来源不匹配");
    }
    if records.any(|r| {
        r["chat"] == chat
            && (source.is_empty() || r["source"] == source)
            && (r["source"] != account || r["chat"] != original)
    }) {
        return Err("会话短编号与真实会话编号重名，请改按消息 id 查询");
    }
    Ok((account, original))
}

fn budget_notice(budget: usize) -> String {
    let notice = json!({
        "error":format!("本次补查预算已用尽（上限 {budget} 字符），覆盖不完整：请只依据已读内容作答，并在结论中说明未覆盖的部分"),
        "budget_exhausted":true,
        "budget_chars":budget,
    })
    .to_string();
    // Control notices disclose the limit; they are not retrieved material.
    notice
}

fn page_fits(text: &str, limit: usize, encoded_limit: Option<usize>) -> bool {
    text.chars().count() <= limit
        && encoded_limit
            .is_none_or(|remaining| shishi_api::json::chars(text).saturating_sub(2) <= remaining)
}

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
        Self {
            unavailable: if include_scope {
                self.unavailable.clone()
            } else {
                vec![]
            },
            ..Default::default()
        }
        .fits(row, PAGE, None)
    }
    fn render(&self, extra: Option<&Value>) -> String {
        // Borrow records for every binary-search probe instead of cloning the
        // page into a temporary JSON tree. Keep the existing field order.
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
            note: "只读快照摘录；未命中或未列出不等于不存在。可继续按id/范围/offset查询。",
            omitted_oversized: (self.omitted > 0).then_some(self.omitted),
            page_limit_chars: PAGE,
            records: self.rows.iter().chain(extra).collect(),
            returned: self.rows.len() + usize::from(extra.is_some()),
            unavailable_ids: &self.unavailable,
            unavailable_note: (!self.unavailable.is_empty())
                .then_some("指定记录不在当前可重建快照中；不返回空文本冒充全文"),
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_pages_stop_without_errors_and_every_record_can_be_read() {
        let ids = (0..30).map(|i| format!("m{i}")).collect::<Vec<_>>();
        let short = json!({"records":ids.iter().map(|id| json!({"id":id,"kind":"message","data":{"text":"短"}})).collect::<Vec<_>>()});
        for args in [json!({"kind":"message"}), json!({"ids":ids})] {
            shishi_api::schema_validate::validate(&tools()["tools"][0]["inputSchema"], &args)
                .unwrap();
            let mut meter = Meter::default();
            let raw = query(&short, &mut meter, &args);
            assert!(raw.chars().count() <= PAGE);
            let page: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(page["returned"], 30);
            assert_eq!(meter.complete.len(), 30);
        }
        let snapshot = json!({"budget_chars":1_000_000,"records":(0..500).map(|i| json!({"id":format!("m{i}"),"kind":"message","data":{"text":"甲".repeat(1000)}})).collect::<Vec<_>>()});
        let mut meter = Meter::default();
        while meter.seen.len() < 500 {
            let before = meter.seen.len();
            let raw = query(&snapshot, &mut meter, &json!({"kind":"message"}));
            assert!(raw.chars().count() <= PAGE);
            let page: Value = serde_json::from_str(&raw).unwrap();
            assert!(page.get("error").is_none() && page.get("errors").is_none());
            assert!(meter.seen.len() > before);
            for row in page["records"].as_array().unwrap() {
                if let Some(offset) = row["next_offset"].as_u64() {
                    assert!(!meter.complete.contains(row["id"].as_str().unwrap()));
                    let rest = query(
                        &snapshot,
                        &mut meter,
                        &json!({"ids":[row["id"]],"offset":offset}),
                    );
                    assert!(rest.chars().count() <= PAGE);
                    assert!(meter.complete.contains(row["id"].as_str().unwrap()));
                }
            }
        }
        assert_eq!(meter.complete.len(), 500);
    }

    #[test]
    fn oversized_errors_are_bounded_and_do_not_block_later_valid_rows() {
        let mut rows = (0..30).map(|i| json!({"id":format!("large-{i}"),"kind":"task","data":{"description":"text","metadata":"x".repeat(9000)}})).collect::<Vec<_>>();
        rows.push(json!({"id":"valid","kind":"task","data":{"description":"readable"}}));
        let snapshot = json!({"records":rows});
        let mut meter = Meter::default();
        let mut errors = BTreeSet::new();
        for _ in 0..5 {
            let raw = query(&snapshot, &mut meter, &json!({"kind":"task"}));
            assert!(raw.chars().count() <= PAGE);
            let page: Value = serde_json::from_str(&raw).unwrap();
            assert!(page.get("error").is_none());
            errors.extend(
                page["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| row["id"].as_str().unwrap().to_owned()),
            );
        }
        assert_eq!(errors.len(), 30);
        assert_eq!(meter.seen, ["valid".into()].into_iter().collect());
        assert_eq!(meter.complete, meter.seen);
    }

    #[test]
    fn free_work_memory_preserves_unicode_and_rejects_oversize_without_truncation() {
        let snapshot = json!({"memory":{"before":"","writable":true}});
        let note = "交接".repeat(MEMORY_LIMIT / 2);
        let update = memory_update(
            &snapshot,
            &Meter::default(),
            &json!({"memory_update":[note]}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            update["text"].as_str().unwrap().chars().count(),
            MEMORY_LIMIT
        );
        assert_eq!(update["text"], note);
        let too_long = format!("{note}甲");
        assert!(memory_update(
            &snapshot,
            &Meter::default(),
            &json!({"memory_update":[too_long]})
        )
        .is_err());
        assert_eq!(snapshot["memory"]["before"], "");
        assert!(
            memory_update(&snapshot, &Meter::default(), &json!({"memory_update":[]}))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn memory_cannot_be_replaced_after_skipping_unread_pages() {
        let body = "\"".repeat(9_000);
        let s = json!({"memory":{"before":body,"writable":true},"records":[{"id":"memory","kind":"memory","data":{"text":body}}]});
        let mut meter = Meter::default();
        query(
            &s,
            &mut meter,
            &json!({"ids":["memory"],"query":"","offset":7000}),
        );
        assert!(memory_update(&s, &meter, &json!({"memory_update":["new"]})).is_err());
        let mut meter = Meter::default();
        let mut offset = 0u64;
        loop {
            let page: Value = serde_json::from_str(&query(
                &s,
                &mut meter,
                &json!({"ids":["memory"],"query":"","offset":offset}),
            ))
            .unwrap();
            if let Some(next) = page["records"][0]["next_offset"].as_u64() {
                assert!(memory_update(&s, &meter, &json!({"memory_update":["new"]})).is_err());
                offset = next;
            } else {
                break;
            }
        }
        assert!(memory_update(&s, &meter, &json!({"memory_update":["new"]}))
            .unwrap()
            .is_some());
    }
    #[test]
    fn long_message_pages_keep_reply_metadata_and_do_not_repeat() {
        let s = json!({"records":[{"id":"long","kind":"message","data":{"text":"甲".repeat(7000),"reply":"parent","sender":"某人"}}]});
        let mut m = Meter::default();
        let first: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["long"],"query":""}))).unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        assert_eq!(first["records"][0]["data"]["reply"], "parent");
        let second: Value = serde_json::from_str(&query(
            &s,
            &mut m,
            &json!({"ids":["long"],"query":"","offset":offset}),
        ))
        .unwrap();
        assert_eq!(second["records"][0]["text_offset"], offset);
        let repeat: Value = serde_json::from_str(&query(
            &s,
            &mut m,
            &json!({"ids":["long"],"query":"","offset":offset}),
        ))
        .unwrap();
        assert!(repeat["records"].as_array().unwrap().is_empty());
        assert!(first.to_string().chars().count() <= PAGE);
        assert!(second.to_string().chars().count() <= PAGE);
        assert!(m.seen.contains("long"));
    }

    #[test]
    fn overlapping_read_preserves_contiguous_frontier_and_tail_completes_coverage() {
        let snapshot =
            json!({"records":[{"id":"long","kind":"message","data":{"text":"甲".repeat(9_000)}}]});
        let mut meter = Meter::default();
        let mut offset = 0u64;
        for _ in 0..2 {
            let page: Value = serde_json::from_str(&query(
                &snapshot,
                &mut meter,
                &json!({"ids":["long"],"offset":offset}),
            ))
            .unwrap();
            offset = page["records"][0]["next_offset"].as_u64().unwrap();
        }
        let frontier = meter.offsets["long"];
        assert_eq!(frontier, offset as usize);
        assert!(!meter.complete.contains("long"));

        let overlap: Value = serde_json::from_str(&query(
            &snapshot,
            &mut meter,
            &json!({"ids":["long"],"offset":1}),
        ))
        .unwrap();
        assert!(overlap["records"][0]["next_offset"].as_u64().unwrap() < offset);
        assert_eq!(meter.offsets["long"], frontier);
        assert!(!meter.complete.contains("long"));

        let tail: Value = serde_json::from_str(&query(
            &snapshot,
            &mut meter,
            &json!({"ids":["long"],"offset":offset}),
        ))
        .unwrap();
        assert_eq!(tail["records"][0]["text_offset"], offset);
        assert!(tail["records"][0].get("next_offset").is_none());
        assert_eq!(meter.offsets["long"], 9_000);
        assert!(meter.complete.contains("long"));
    }
    #[test]
    fn keyword_only_queries_do_not_require_ids_and_filters_have_defined_semantics() {
        let s = json!({"records":[
            {"id":"m","kind":"message","source":"s1","chat":"c1","time":100,"data":{"text":"needle in text"}},
            {"id":"t","kind":"task","source":"s1","chat":"c1","time":200,"data":{"description":"task body","updated":200}},
            {"id":"u","kind":"update","source":"","chat":"","time":150,"data":{"detail":"update detail"}}
        ]});
        let mut m = Meter::default();
        let keyword: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"query":"needle"}))).unwrap();
        assert_eq!(keyword["records"][0]["id"], "m");
        assert!(keyword.to_string().chars().count() <= PAGE);
        let mut m = Meter::default();
        let task_time: Value = serde_json::from_str(&query(
            &s,
            &mut m,
            &json!({"kind":"task","since":150,"until":250}),
        ))
        .unwrap();
        assert_eq!(task_time["records"][0]["id"], "t");
        let mut m = Meter::default();
        let update_filter = query(&s, &mut m, &json!({"kind":"update","source":"s1"}));
        assert!(serde_json::from_str::<Value>(&update_filter).unwrap()["error"].is_string());
        let mut m = Meter::default();
        let missing: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["absent"]}))).unwrap();
        assert_eq!(missing["unavailable_ids"], json!(["absent"]));
        assert!(missing["unavailable_note"].is_string());
    }

    #[test]
    fn room_aliases_preserve_scope_errors_and_paged_read_coverage() {
        fn read(snapshot: &Value, meter: &mut Meter, args: Value) -> Value {
            let before = meter.chars;
            let text = query(snapshot, meter, &args);
            assert!(text.chars().count() <= PAGE);
            assert_eq!(meter.chars - before, text.chars().count());
            serde_json::from_str(&text).unwrap()
        }
        let mut snapshot = json!({"budget_chars":PAGE * 4,"room_aliases":{
            "r1":{"source":"account-a","chat":"shared"},
            "r2":{"source":"account-b","chat":"shared"},
            "r3":{"source":"account-a","chat":"outside-snapshot"},
            "r4":{"source":"account-a","chat":""}
        },"records":[
            {"id":"a","kind":"message","source":"account-a","chat":"shared","data":{"text":"甲".repeat(7000)}},
            {"id":"b","kind":"message","source":"account-b","chat":"shared","data":{"text":"Other account"}},
            {"id":"collision","kind":"message","source":"account-b","chat":"r1","data":{"text":"Canonical r1"}},
            {"id":"canonical","kind":"message","source":"account-b","chat":"r999","data":{"text":"Canonical r999"}}
        ]});
        let mut meter = Meter::default();
        for args in [
            json!({"chat":"r404"}),
            json!({"chat":"r999"}),
            json!({"chat":"r1"}),
            json!({"source":"account-b","chat":"r1"}),
            json!({"chat":"r4"}),
        ] {
            assert!(read(&snapshot, &mut meter, args)["error"].is_string());
        }
        assert!(meter.seen.is_empty() && meter.pages.is_empty() && meter.offsets.is_empty());
        assert!(meter.complete.is_empty());

        let mut page = read(
            &snapshot,
            &mut meter,
            json!({"source":"account-a","chat":"r1"}),
        );
        assert_eq!(page["records"].as_array().unwrap().len(), 1);
        assert_eq!(page["records"][0]["id"], "a");
        assert!(page["records"][0]["next_offset"].is_number());
        while let Some(offset) = page["records"][0]["next_offset"].as_u64() {
            page = read(&snapshot, &mut meter, json!({"ids":["a"],"offset":offset}));
            assert_eq!(page["records"][0]["id"], "a");
        }
        assert!(meter.complete.contains("a"));
        assert_eq!(meter.offsets["a"], 7000);
        let repeat = read(
            &snapshot,
            &mut meter,
            json!({"source":"account-a","chat":"shared"}),
        );
        assert_eq!(repeat["records"], json!([]));
        let other = read(&snapshot, &mut meter, json!({"chat":"r2"}));
        assert_eq!(other["records"][0]["id"], "b");
        assert_eq!(other["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            read(&snapshot, &mut meter, json!({"chat":"r3"}))["records"],
            json!([])
        );
        assert_eq!(
            read(
                &snapshot,
                &mut meter,
                json!({"source":"account-b","chat":"r999"})
            )["records"][0]["id"],
            "canonical"
        );
        assert!(meter.chars <= PAGE * 4);

        // Snapshots without a room table retain exact canonical-chat semantics.
        snapshot.as_object_mut().unwrap().remove("room_aliases");
        assert_eq!(
            read(
                &snapshot,
                &mut meter,
                json!({"source":"account-b","chat":"r1"})
            )["records"][0]["id"],
            "collision"
        );
    }

    #[test]
    fn update_records_page_across_offsets() {
        let s = json!({"records":[{"id":"u","kind":"update","source":"","chat":"","time":10,"data":{"detail":"乙".repeat(9000)}}]});
        let mut m = Meter::default();
        let first: Value = serde_json::from_str(&query(&s, &mut m, &json!({"ids":["u"]}))).unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        let second: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["u"],"offset":offset})))
                .unwrap();
        assert_eq!(second["records"][0]["text_offset"], offset);
        assert!(second.to_string().chars().count() <= PAGE);
    }

    #[test]
    fn a_valid_long_ascii_source_can_be_retrieved_across_more_than_128_pages() {
        // Explicitly configured maximum budget; the default caps far earlier.
        let s = json!({"budget_chars":1_000_000,"records":[{"id":"big","kind":"message","source":"s","chat":"c","time":1,"data":{"text":"a".repeat(600_000)}}]});
        let mut m = Meter::default();
        let mut offset = 0u64;
        let mut pages = 0;
        loop {
            let page: Value =
                serde_json::from_str(&query(&s, &mut m, &json!({"ids":["big"],"offset":offset})))
                    .unwrap();
            if page["records"]
                .as_array()
                .is_none_or(|rows| rows.is_empty())
            {
                break;
            }
            pages += 1;
            assert!(page.to_string().chars().count() <= PAGE);
            match page["records"][0]["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert!(pages > 128, "expected more than 128 pages, got {pages}");
    }

    #[test]
    fn retrieval_budget_caps_delivered_text_and_cannot_be_bypassed() {
        let s = json!({"budget_chars":10_000,"records":[{"id":"long","kind":"message","source":"s","chat":"c","time":1,"data":{"text":"丙".repeat(50_000)}}]});
        let mut m = Meter::default();
        let mut offset = 0u64;
        let mut delivered = 0;
        let notice = loop {
            let raw = query(&s, &mut m, &json!({"ids":["long"],"offset":offset}));
            let page: Value = serde_json::from_str(&raw).unwrap();
            if page["budget_exhausted"] == true {
                break page;
            }
            delivered += raw.chars().count();
            offset = page["records"][0]["next_offset"].as_u64().unwrap();
        };
        assert!(delivered <= 10_000, "delivered {delivered}");
        assert!(
            delivered >= 9_998,
            "the usable final partial page was lost: {delivered}"
        );
        assert!(m.exhausted);
        assert!(notice["error"].as_str().unwrap().contains("覆盖不完整"));
        // Errors, new ranges and repeats all get the same notice afterwards.
        for args in [
            json!({"ids":["long"],"offset":offset}),
            json!({"query":"丙"}),
            json!({"bogus":1}),
        ] {
            let page: Value = serde_json::from_str(&query(&s, &mut m, &args)).unwrap();
            assert_eq!(page["budget_exhausted"], true);
            assert!(page.get("records").is_none());
        }
        assert!(validate_budget(DEFAULT_BUDGET).is_ok());
        assert!(validate_budget(PAGE - 1).is_err());
    }

    #[test]
    fn shared_continuation_allowance_counts_escaping_before_crediting_coverage() {
        let snapshot = json!({"budget_chars":80_000,"records":[{
            "id":"message","kind":"message","data":{"text":"\"\\\n🙂".repeat(1_000)}
        }]});
        let mut meter = Meter::default();
        let text = query_with_limit(
            &snapshot,
            &mut meter,
            &json!({"ids":["message"]}),
            Some(600),
        );
        assert!(serde_json::to_string(&text).unwrap().chars().count() - 2 <= 600);
        let page: Value = serde_json::from_str(&text).unwrap();
        let offset = page["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        assert!(meter.seen.contains("message"));
        assert!(!meter.complete.contains("message"));
        let before = (
            meter.seen.clone(),
            meter.pages.clone(),
            meter.offsets.clone(),
            meter.chars,
        );
        let rejected = query_with_limit(
            &snapshot,
            &mut meter,
            &json!({"ids":["message"],"offset":offset}),
            Some(0),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&rejected).unwrap()["budget_exhausted"],
            true
        );
        assert_eq!(
            (
                meter.seen.clone(),
                meter.pages.clone(),
                meter.offsets.clone(),
                meter.chars
            ),
            before
        );
    }

    #[test]
    fn final_partial_page_uses_remaining_budget_and_preserves_unicode_offsets() {
        let text = "甲\"\\\n".repeat(3_000);
        let snapshot = json!({"budget_chars":6_400,"records":[{"id":"long","kind":"message","data":{"text":text,"reply":"parent"}}]});
        let mut meter = Meter::default();
        let first_raw = query(&snapshot, &mut meter, &json!({"ids":["long"]}));
        let first: Value = serde_json::from_str(&first_raw).unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        let remaining = 6_400 - meter.chars;
        assert!((2_400..2_402).contains(&remaining));
        let second_raw = query(
            &snapshot,
            &mut meter,
            &json!({"ids":["long"],"offset":offset}),
        );
        let second: Value = serde_json::from_str(&second_raw).unwrap();
        let row = &second["records"][0];
        let end = row["next_offset"].as_u64().unwrap();
        assert!(end > offset);
        assert_eq!(row["text_offset"], offset);
        assert_eq!(row["data"]["reply"], "parent");
        assert_eq!(
            row["data"]["text"],
            text.chars()
                .skip(offset as usize)
                .take((end - offset) as usize)
                .collect::<String>()
        );
        assert!(second_raw.chars().count() <= remaining);
        assert!(second_raw.chars().count() + 1 >= remaining);
        assert_eq!(meter.offsets["long"], end as usize);
        assert!(!meter.exhausted && !meter.complete.contains("long"));
        let before = (
            meter.seen.clone(),
            meter.pages.clone(),
            meter.offsets.clone(),
        );
        let notice: Value = serde_json::from_str(&query(
            &snapshot,
            &mut meter,
            &json!({"ids":["long"],"offset":end}),
        ))
        .unwrap();
        assert_eq!(notice["budget_exhausted"], true);
        assert_eq!((meter.seen, meter.pages, meter.offsets), before);
    }

    #[test]
    fn large_metadata_does_not_exhaust_budget_that_can_still_deliver_smaller_records() {
        let snapshot = json!({"budget_chars":4_000,"records":[
            {"id":"large","kind":"message","data":{"text":"body","metadata":"x".repeat(2_000)}},
            {"id":"small","kind":"message","data":{"text":"available"}}
        ]});
        let mut meter = Meter {
            chars: 3_100,
            ..Default::default()
        };
        let raw = query(&snapshot, &mut meter, &json!({"ids":["large"]}));
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["budget_limited"], true);
        assert!(!meter.exhausted);
        assert!(meter.pages.is_empty() && meter.seen.is_empty() && meter.complete.is_empty());
        assert!(meter.offsets.is_empty());
        let small: Value =
            serde_json::from_str(&query(&snapshot, &mut meter, &json!({"ids":["small"]}))).unwrap();
        assert_eq!(small["records"][0]["id"], "small");
        assert_eq!(meter.complete, BTreeSet::from(["small".into()]));
        assert!(meter.chars <= 4_000);

        // A broad query should find the smaller row directly without marking
        // the skipped larger row as read or as an oversized-page error.
        let mut broad = Meter {
            chars: 3_100,
            ..Default::default()
        };
        let page: Value =
            serde_json::from_str(&query(&snapshot, &mut broad, &json!({"kind":"message"})))
                .unwrap();
        assert_eq!(page["records"][0]["id"], "small");
        assert!(page.get("errors").is_none());
        assert_eq!(broad.seen, BTreeSet::from(["small".into()]));
        assert!(!broad.pages.contains("large@0"));
    }

    #[test]
    fn unavailable_id_wrapper_cannot_poison_a_record_that_fits_by_itself() {
        for has_body in [false, true] {
            let mut row =
                json!({"id":"real","kind":"message","data":{"metadata":"x".repeat(2_500)}});
            if has_body {
                row["data"]["text"] = json!("a");
            }
            let snapshot = json!({"records":[row]});
            let mut requested = vec![json!("real")];
            requested.extend((0..7).map(|i| json!(format!("{}{}", i, "x".repeat(255)))));
            let mut meter = Meter::default();
            let raw = query(&snapshot, &mut meter, &json!({"ids":requested}));
            let rejected: Value = serde_json::from_str(&raw).unwrap();
            assert!(raw.chars().count() <= PAGE);
            assert!(rejected["error"].as_str().unwrap().contains("缩小范围"));
            assert_eq!(rejected["page_limit_chars"], PAGE);
            assert!(rejected.get("budget_limited").is_none());
            assert!(!meter.exhausted);
            assert!(meter.pages.is_empty() && meter.seen.is_empty());
            assert!(meter.complete.is_empty() && meter.offsets.is_empty());

            let raw = query(&snapshot, &mut meter, &json!({"ids":["real"]}));
            let accepted: Value = serde_json::from_str(&raw).unwrap();
            assert!(raw.chars().count() <= PAGE);
            assert_eq!(accepted["records"][0], row);
            assert!(accepted.get("error").is_none() && accepted.get("errors").is_none());
            assert_eq!(meter.complete, BTreeSet::from(["real".into()]));
        }
    }

    #[test]
    fn leftover_budget_applies_to_metadata_rows_and_rejected_errors_are_not_read() {
        let rows = vec![
            json!({"id":"first","kind":"task","data":{"flag":true}}),
            json!({"id":"second","kind":"task","data":{"flag":true}}),
        ];
        let one = Page::default().render(Some(&rows[0])).chars().count();
        let snapshot = json!({"budget_chars":4_000,"records":rows});
        let mut meter = Meter {
            chars: 4_000 - one,
            ..Default::default()
        };
        let raw = query(&snapshot, &mut meter, &json!({"kind":"task"}));
        assert_eq!(raw.chars().count(), one);
        let page: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(page["records"].as_array().unwrap().len(), 1);
        assert_eq!(meter.complete, BTreeSet::from(["first".into()]));
        let remaining: Value =
            serde_json::from_str(&query(&snapshot, &mut meter, &json!({"kind":"task"}))).unwrap();
        assert_eq!(remaining["budget_exhausted"], true);
        assert!(!meter.seen.contains("second") && !meter.pages.contains("second@0"));

        let huge = json!({"budget_chars":4_000,"records":[{"id":"huge","kind":"task","data":{"description":"text","metadata":"x".repeat(9_000)}}]});
        let mut meter = Meter {
            chars: 3_850,
            ..Default::default()
        };
        let result: Value =
            serde_json::from_str(&query(&huge, &mut meter, &json!({"ids":["huge"]}))).unwrap();
        assert!(result["budget_limited"] == true || result["budget_exhausted"] == true);
        assert!(meter.pages.is_empty() && meter.complete.is_empty() && meter.seen.is_empty());
    }

    #[test]
    fn oversized_metadata_and_large_task_bodies_are_bounded_or_explicit() {
        let huge_meta = json!({"records":[
            {"id":"huge","kind":"task","source":"","chat":"","time":1,"data":{"description":"","blob":"x".repeat(12000)}},
            {"id":"valid","kind":"message","source":"s","chat":"c","time":2,"data":{"text":"仍然可读"}}
        ]});
        let mut m = Meter::default();
        let result = query(&huge_meta, &mut m, &json!({"ids":["huge"]}));
        let value: Value = serde_json::from_str(&result).unwrap();
        assert!(value["errors"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(result.chars().count() <= PAGE);
        assert!(!value["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "huge"));
        // The failed oversized row must not mark unrelated records as seen.
        let valid: Value =
            serde_json::from_str(&query(&huge_meta, &mut m, &json!({"ids":["valid"]}))).unwrap();
        assert_eq!(valid["records"][0]["id"], "valid");

        let large_task = json!({"records":[{"id":"t","kind":"task","source":"s","chat":"c","time":1,"data":{"description":"甲".repeat(9000)}}]});
        let mut m = Meter::default();
        let first: Value =
            serde_json::from_str(&query(&large_task, &mut m, &json!({"ids":["t"]}))).unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        assert!(first.to_string().chars().count() <= PAGE);
        let second: Value = serde_json::from_str(&query(
            &large_task,
            &mut m,
            &json!({"ids":["t"],"offset":offset}),
        ))
        .unwrap();
        assert_eq!(second["records"][0]["text_offset"], offset);
        assert!(second.to_string().chars().count() <= PAGE);
    }

    #[test]
    fn read_budget_counts_errors_deduplicates_and_bounds_large_rows() {
        let s = json!({"records":[{"id":"a","text":"x".repeat(1000)},{"id":"large","text":"x".repeat(50000)}]});
        let mut m = Meter::default();
        let a = query(&s, &mut m, &json!({"ids":["a","large"],"query":""}));
        assert!(a.len() < PAGE);
        assert_eq!(m.seen.len(), 1);
        let b = query(&s, &mut m, &json!({"ids":["a"],"query":""}));
        assert_eq!(
            serde_json::from_str::<Value>(&b).unwrap()["records"],
            json!([])
        );
        let invalid = query(&s, &mut m, &json!({"file":"state.json"}));
        assert!(serde_json::from_str::<Value>(&invalid).unwrap()["error"].is_string());
        // Repeated calls are allowed: bounded per-response, not cumulative.
        let again = query(&s, &mut m, &json!({"ids":["large"],"query":""}));
        assert!(serde_json::from_str::<Value>(&again).unwrap()["records"]
            .as_array()
            .is_some());
        assert!(a.chars().count() <= PAGE);
        assert!(m.calls >= 2);
    }
}
