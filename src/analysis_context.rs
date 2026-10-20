//! A bounded, per-request file. The model can read only this snapshot, never the live archive.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, Read, Write},
    path::Path,
};

/// Maximum characters in one serialized read-only response, including wrapper
/// metadata and the error/notice text.
pub const PAGE: usize = 4000;
pub const MEMORY_LIMIT: usize = 2000;
pub const ARCHIVED_MEMORY_LIMIT: usize = 4000;
pub const PAGE_SIZE_MAX: usize = 8;
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
    let mut value = json!({"tools":[{"name":"read_context","description":"只读查询分析快照（消息/事项/更新流水），可跨来源和会话按id、关键词、来源、会话、类型和时间范围检索；事项按updated时间过滤，来源/会话仅在其全部依据一致时可用，更新流水不支持来源/会话过滤；长文本用next_offset分页续读，每次返回有上限，可重复按需查询；重复页和已读区域不会重复返回，进程受取消和超时控制。","inputSchema":{"type":"object","additionalProperties":false,"properties":{"ids":{"type":"array","maxItems":PAGE_SIZE_MAX,"items":{"type":"string"}},"query":{"type":"string","maxLength":80},"offset":{"type":"integer","minimum":0},"source":{"type":"string","maxLength":128},"chat":{"type":"string","maxLength":128},"kind":{"type":"string","enum":["message","task","update"]},"since":{"type":"integer"},"until":{"type":"integer"}},"required":[]},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}]});
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
    let Some(value) = result.get("memory_update") else {
        return Ok(None);
    };
    let rows = value.as_array().ok_or("笔记更新必须是数组")?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 || snapshot["memory"]["writable"] != true {
        return Err("共享笔记不可写或更新超过一份".into());
    }
    let before = snapshot["memory"]["before"]
        .as_str()
        .ok_or("笔记原版本无效")?;
    if !before.is_empty()
        && !meter.complete.contains("memory")
        && snapshot["memory"]["inline_text"].as_str() != Some(before)
    {
        return Err("未完整读取原笔记，本次不覆盖".into());
    }
    let text = rows[0].as_str().ok_or("笔记内容无效")?;
    if text.chars().count() > MEMORY_LIMIT {
        return Err("笔记超过2000字符，未保存".into());
    }
    Ok(Some(
        json!({"before":before,"text":text,"expected":snapshot["memory"]["revision"]}),
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
        )
        .unwrap();
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

#[derive(Default, Serialize, Deserialize)]
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
    /// Set once the request's retrieval budget refused a response.
    #[serde(default)]
    pub exhausted: bool,
}

/// Includes errors and wrapper text in the per-response bound. Delivered text
/// is also capped per request by `budget_chars` (default `DEFAULT_BUDGET`);
/// once a response would exceed it, every further call, including errors and
/// retries, gets the same explicit incomplete-coverage notice.
pub fn query(snapshot: &Value, meter: &mut Meter, args: &Value) -> Result<String, String> {
    meter.calls += 1;
    let budget = snapshot["budget_chars"]
        .as_u64()
        .map_or(DEFAULT_BUDGET, |b| b as usize);
    if meter.exhausted {
        return Ok(budget_notice(meter, budget));
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
        a.len() > PAGE_SIZE_MAX
            || a.iter()
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
    let id_filter = ids.cloned().unwrap_or_default();
    let available_ids = snapshot["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let mut rows = vec![];
    let mut seen = vec![];
    let mut pages = vec![];
    let mut complete = vec![];
    let mut errors = vec![];
    let mut omitted = 0usize;
    let mut pending_offsets = Vec::new();
    if !invalid {
        for r in snapshot["records"].as_array().into_iter().flatten() {
            let id = r["id"].as_str().unwrap_or("");
            let page = format!("{id}@{offset}");
            if meter.pages.contains(&page) || (offset == 0 && meter.seen.contains(id)) {
                continue;
            }
            if !id_filter.is_empty() && !id_filter.contains(&json!(id)) {
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
            if !term.is_empty() && !r.to_string().to_lowercase().contains(&term) {
                continue;
            }
            let mut row = r.clone();
            let body_field = ["text", "description", "detail"]
                .into_iter()
                .find(|field| r["data"][field].as_str().is_some());
            let mut offset_error = None;
            if let Some(field) = body_field {
                let text = r["data"][field].as_str().unwrap_or("");
                let chars = text.chars().collect::<Vec<_>>();
                if offset > 0 && offset >= chars.len() {
                    offset_error = Some(if offset >= chars.len() {
                        "offset 超出该记录正文长度"
                    } else {
                        "记录正文不可用"
                    });
                } else {
                    // Fit the whole final wrapper, not a smaller intermediate
                    // wrapper, into PAGE. Never cut JSON bytes. The rendered
                    // length grows with the window, so binary search the
                    // largest window that fits; `offset` means nothing fits.
                    let window = |row: &mut Value, end: usize| {
                        row["data"][field] = json!(chars[offset..end].iter().collect::<String>());
                        if offset > 0 {
                            row["text_offset"] = json!(offset);
                        }
                        if end < chars.len() {
                            row["next_offset"] = json!(end);
                        } else {
                            row.as_object_mut().unwrap().remove("next_offset");
                        }
                    };
                    let mut end = offset.saturating_add(PAGE).min(chars.len());
                    window(&mut row, end);
                    if end > offset
                        && !fits(
                            &mut rows,
                            &row,
                            omitted,
                            &id_filter,
                            &available_ids,
                            &errors,
                        )
                    {
                        let (mut low, mut high) = (offset, end - 1);
                        while low < high {
                            let mid = low + (high - low).div_ceil(2);
                            window(&mut row, mid);
                            if fits(
                                &mut rows,
                                &row,
                                omitted,
                                &id_filter,
                                &available_ids,
                                &errors,
                            ) {
                                low = mid;
                            } else {
                                high = mid - 1;
                            }
                        }
                        end = low;
                        window(&mut row, end);
                    }
                    if end == offset && !chars.is_empty() {
                        offset_error = Some("该记录正文与元数据无法装入单页上限");
                    }
                }
            } else if offset > 0 {
                offset_error = Some("该记录类型不支持 offset 续读");
            }
            if let Some(error) = offset_error {
                // Explicit, bounded failure instead of silently disappearing.
                if !errors.iter().any(|row| row["id"] == json!(id)) {
                    errors.push(json!({"id":id,"error":error}));
                }
                if id_filter.len() == 1 {
                    break;
                }
                continue;
            }
            let continuous = offset <= meter.offsets.get(id).copied().unwrap_or(0);
            let text_end = body_field
                .and_then(|field| row["data"][field].as_str())
                .map(|s| offset + s.chars().count());
            let finished = row.get("next_offset").is_none() && continuous;
            if !fits(
                &mut rows,
                &row,
                omitted,
                &id_filter,
                &available_ids,
                &errors,
            ) {
                // Metadata itself is too large for one page.
                errors.push(json!({"id":id,"error":"该记录元数据超过单页上限，无法安全返回"}));
                omitted += 1;
                continue;
            }
            rows.push(row);
            seen.push(id.to_string());
            pages.push(page);
            if continuous {
                if let Some(end) = text_end {
                    pending_offsets.push((id.to_string(), end));
                }
            }
            if finished {
                complete.push(id.to_string());
            }
            if rows.len() == PAGE_SIZE_MAX {
                break;
            }
        }
    }
    let (result, delivered) = if invalid {
        (
            json!({"error":"提供最多8个id，或关键词/来源/会话/类型/时间范围之一；offset续读需单个id；更新流水不含来源/会话字段"})
                .to_string(),
            false,
        )
    } else {
        let value = render_page(&rows, omitted, &id_filter, &available_ids, &errors);
        if value.chars().count() > PAGE {
            (
                json!({"error":"本次查询结果超过单页上限，请缩小范围或按id分页","page_limit_chars":PAGE})
                    .to_string(),
                false,
            )
        } else {
            (value, true)
        }
    };
    if meter.chars + result.chars().count() > budget {
        meter.exhausted = true;
        return Ok(budget_notice(meter, budget));
    }
    meter.chars += result.chars().count();
    if delivered {
        // Only records actually delivered in the accepted page may advance the
        // seen/page/offset/complete meter. A generic oversized fallback must not
        // make valid rows permanently unreachable.
        meter.seen.extend(seen);
        meter.pages.extend(pages);
        meter.complete.extend(complete);
        for (id, end) in pending_offsets {
            meter.offsets.insert(id, end);
        }
    }
    Ok(result)
}

fn budget_notice(meter: &mut Meter, budget: usize) -> String {
    let notice = json!({
        "error":format!("本次补查预算已用尽（上限 {budget} 字符），覆盖不完整：请只依据已读内容作答，并在结论中说明未覆盖的部分"),
        "budget_exhausted":true,
        "budget_chars":budget,
    })
    .to_string();
    meter.chars += notice.chars().count();
    notice
}

/// Whether the page still fits PAGE with `row` appended; `rows` is restored.
fn fits(
    rows: &mut Vec<Value>,
    row: &Value,
    omitted: usize,
    requested: &[Value],
    available: &BTreeSet<String>,
    errors: &[Value],
) -> bool {
    rows.push(row.clone());
    let ok = render_page(rows, omitted, requested, available, errors)
        .chars()
        .count()
        <= PAGE;
    rows.pop();
    ok
}

fn render_page(
    rows: &[Value],
    omitted: usize,
    requested: &[Value],
    available: &BTreeSet<String>,
    errors: &[Value],
) -> String {
    let unavailable = requested
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| !available.contains(*id))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut value = json!({
        "records":rows,
        "returned":rows.len(),
        "page_limit_chars":PAGE,
        "note":"只读快照摘录；未命中或未列出不等于不存在。可继续按id/范围/offset查询。"
    });
    if omitted > 0 {
        value["omitted_oversized"] = json!(omitted);
    }
    if !unavailable.is_empty() {
        value["unavailable_ids"] = json!(unavailable);
        value["unavailable_note"] = json!("指定记录不在当前可重建快照中；不返回空文本冒充全文");
    }
    if !errors.is_empty() {
        value["errors"] = json!(errors);
    }
    value.to_string()
}

pub fn serve(path: &Path) -> Result<(), String> {
    let snapshot: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if snapshot["expires"].as_i64().unwrap_or(0) < crate::model::now() {
        return Err("分析资料已过期".into());
    }
    // Lock the meter for this server lifetime. Restarting the server cannot reset its allowance.
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|e| e.to_string())?;
    let meter_path = path.with_extension("meter");
    let mut meter: Meter = match fs::read(&meter_path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Meter::default(),
        Err(e) => return Err(e.to_string()),
    };
    let mut reader = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        let size = reader
            .by_ref()
            .take(8193)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        if size > 8192 {
            return Err("查询过长".into());
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        let Some(id) = v.get("id") else { continue };
        let result = match v["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"shishi_analysis","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "tools/list" => tools(),
            "tools/call" => {
                let result = if v["params"]["name"] == "read_image" {
                    match crate::images::read(&snapshot, &mut meter, &v["params"]["arguments"]) {
                        Ok(content) => content,
                        Err(error) => {
                            json!({"isError":true,"content":[{"type":"text","text":error}]})
                        }
                    }
                } else {
                    let args = if v["params"]["name"] == "read_context" {
                        v["params"]["arguments"].clone()
                    } else {
                        Value::Null
                    };
                    let text = query(&snapshot, &mut meter, &args)?;
                    json!({"content":[{"type":"text","text":text}]})
                };
                fs::write(&meter_path, serde_json::to_vec(&meter).unwrap())
                    .map_err(|e| e.to_string())?;
                result
            }
            _ => return Err("不支持的查询方法".into()),
        };
        writeln!(out, "{}", json!({"jsonrpc":"2.0","id":id,"result":result}))
            .map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        )
        .unwrap();
        assert!(memory_update(&s, &meter, &json!({"memory_update":["new"]})).is_err());
        let mut meter = Meter::default();
        let mut offset = 0u64;
        loop {
            let page: Value = serde_json::from_str(
                &query(
                    &s,
                    &mut meter,
                    &json!({"ids":["memory"],"query":"","offset":offset}),
                )
                .unwrap(),
            )
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
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["long"],"query":""})).unwrap())
                .unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        assert_eq!(first["records"][0]["data"]["reply"], "parent");
        let second: Value = serde_json::from_str(
            &query(
                &s,
                &mut m,
                &json!({"ids":["long"],"query":"","offset":offset}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(second["records"][0]["text_offset"], offset);
        let repeat: Value = serde_json::from_str(
            &query(
                &s,
                &mut m,
                &json!({"ids":["long"],"query":"","offset":offset}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(repeat["records"].as_array().unwrap().is_empty());
        assert!(first.to_string().chars().count() <= PAGE);
        assert!(second.to_string().chars().count() <= PAGE);
        assert!(m.seen.contains("long"));
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
            serde_json::from_str(&query(&s, &mut m, &json!({"query":"needle"})).unwrap()).unwrap();
        assert_eq!(keyword["records"][0]["id"], "m");
        assert!(keyword.to_string().chars().count() <= PAGE);
        let mut m = Meter::default();
        let task_time: Value = serde_json::from_str(
            &query(&s, &mut m, &json!({"kind":"task","since":150,"until":250})).unwrap(),
        )
        .unwrap();
        assert_eq!(task_time["records"][0]["id"], "t");
        let mut m = Meter::default();
        let update_filter = query(&s, &mut m, &json!({"kind":"update","source":"s1"})).unwrap();
        assert!(serde_json::from_str::<Value>(&update_filter).unwrap()["error"].is_string());
        let mut m = Meter::default();
        let missing: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["absent"]})).unwrap()).unwrap();
        assert_eq!(missing["unavailable_ids"], json!(["absent"]));
        assert!(missing["unavailable_note"].is_string());
    }

    #[test]
    fn update_records_page_across_offsets() {
        let s = json!({"records":[{"id":"u","kind":"update","source":"","chat":"","time":10,"data":{"detail":"乙".repeat(9000)}}]});
        let mut m = Meter::default();
        let first: Value =
            serde_json::from_str(&query(&s, &mut m, &json!({"ids":["u"]})).unwrap()).unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        let second: Value = serde_json::from_str(
            &query(&s, &mut m, &json!({"ids":["u"],"offset":offset})).unwrap(),
        )
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
            let page: Value = serde_json::from_str(
                &query(&s, &mut m, &json!({"ids":["big"],"offset":offset})).unwrap(),
            )
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
            let raw = query(&s, &mut m, &json!({"ids":["long"],"offset":offset})).unwrap();
            let page: Value = serde_json::from_str(&raw).unwrap();
            if page["budget_exhausted"] == true {
                break page;
            }
            delivered += raw.chars().count();
            offset = page["records"][0]["next_offset"].as_u64().unwrap();
        };
        assert!(delivered <= 10_000, "delivered {delivered}");
        assert!(m.exhausted);
        assert!(notice["error"].as_str().unwrap().contains("覆盖不完整"));
        // Errors, new ranges and repeats all get the same notice afterwards.
        for args in [
            json!({"ids":["long"],"offset":offset}),
            json!({"query":"丙"}),
            json!({"bogus":1}),
        ] {
            let page: Value = serde_json::from_str(&query(&s, &mut m, &args).unwrap()).unwrap();
            assert_eq!(page["budget_exhausted"], true);
            assert!(page.get("records").is_none());
        }
        assert!(validate_budget(DEFAULT_BUDGET).is_ok());
        assert!(validate_budget(PAGE - 1).is_err());
    }

    #[test]
    fn oversized_metadata_and_large_task_bodies_are_bounded_or_explicit() {
        let huge_meta = json!({"records":[
            {"id":"huge","kind":"task","source":"","chat":"","time":1,"data":{"description":"","blob":"x".repeat(12000)}},
            {"id":"valid","kind":"message","source":"s","chat":"c","time":2,"data":{"text":"仍然可读"}}
        ]});
        let mut m = Meter::default();
        let result = query(&huge_meta, &mut m, &json!({"ids":["huge"]})).unwrap();
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
            serde_json::from_str(&query(&huge_meta, &mut m, &json!({"ids":["valid"]})).unwrap())
                .unwrap();
        assert_eq!(valid["records"][0]["id"], "valid");

        let large_task = json!({"records":[{"id":"t","kind":"task","source":"s","chat":"c","time":1,"data":{"description":"甲".repeat(9000)}}]});
        let mut m = Meter::default();
        let first: Value =
            serde_json::from_str(&query(&large_task, &mut m, &json!({"ids":["t"]})).unwrap())
                .unwrap();
        let offset = first["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0);
        assert!(first.to_string().chars().count() <= PAGE);
        let second: Value = serde_json::from_str(
            &query(&large_task, &mut m, &json!({"ids":["t"],"offset":offset})).unwrap(),
        )
        .unwrap();
        assert_eq!(second["records"][0]["text_offset"], offset);
        assert!(second.to_string().chars().count() <= PAGE);
    }

    #[test]
    fn read_budget_counts_errors_deduplicates_and_bounds_large_rows() {
        let s = json!({"records":[{"id":"a","text":"x".repeat(1000)},{"id":"large","text":"x".repeat(50000)}]});
        let mut m = Meter::default();
        let a = query(&s, &mut m, &json!({"ids":["a","large"],"query":""})).unwrap();
        assert!(a.len() < PAGE);
        assert_eq!(m.seen.len(), 1);
        let b = query(&s, &mut m, &json!({"ids":["a"],"query":""})).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&b).unwrap()["records"],
            json!([])
        );
        let invalid = query(&s, &mut m, &json!({"file":"state.json"})).unwrap();
        assert!(serde_json::from_str::<Value>(&invalid).unwrap()["error"].is_string());
        // Repeated calls are allowed: bounded per-response, not cumulative.
        let again = query(&s, &mut m, &json!({"ids":["large"],"query":""})).unwrap();
        assert!(serde_json::from_str::<Value>(&again).unwrap()["records"]
            .as_array()
            .is_some());
        assert!(a.chars().count() <= PAGE);
        assert!(m.calls >= 2);
    }
}
