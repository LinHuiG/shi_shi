use crate::model::Config;
use serde_json::{json, Value};
use shishi_api::json::chars as json_chars;
use std::{borrow::Cow, collections::BTreeMap, fs, path::Path};
pub const NO_ACTION_REASON_LIMIT: usize = 240;
pub(crate) const DEGRADED_ANALYSIS_REASON: &str =
    "模型在一次纠正后仍未给出结论，本批按无事项变更降级接收";

pub(crate) fn empty_analysis_without_reason(value: &Value) -> bool {
    value["actions"].as_array().is_some_and(Vec::is_empty)
        && value["no_action_reason"]
            .as_str()
            .is_some_and(|s| s.trim().is_empty())
}
/// Field declarations and their required order are updated together.
macro_rules! extend_schema {
    ($schema:expr, {$($name:expr => $field:expr),* $(,)?}) => {{
        let schema = &mut *($schema);
        for (name, field) in [$(($name, $field)),*] {
            schema["properties"][name] = field;
            schema["required"].as_array_mut().unwrap().push(json!(name));
        }
    }};
}
fn tag_schema(changes: bool) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false,"properties":{"kind":{"type":"string"},"value":{"type":"string"}},"required":["kind","value"]});
    if changes {
        schema["properties"]["action"] = json!({"type":"string","enum":["add","remove"]});
        schema["required"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!("action"));
    }
    schema
}
pub fn schema(kind: &str) -> Value {
    if kind == "message_filter" {
        return json!({"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["analyze","wait","uncertain"]},"reason":{"type":"string","maxLength":200}},"required":["decision","reason"]});
    }
    let mut schema = base_schema(kind);
    if ["classify", "format", "edit", "learning"].contains(&kind) {
        let mut proposal = tag_schema(false);
        extend_schema!(&mut proposal, {
            "reason" => json!({"type":"string"}),
            "evidence_ids" => json!({"type":"array","items":{"type":"string"}}),
        });
        extend_schema!(&mut schema, {
            "tag_suggestions" => json!({"type":"array","maxItems":8,"items":proposal}),
        });
    }
    if kind == "classify" || kind == "format" {
        let key = if kind == "classify" {
            "actions"
        } else {
            "tasks"
        };
        let fields = &mut schema["properties"][key]["items"];
        let tag_field = if kind == "classify" {
            "tag_changes"
        } else {
            "tags"
        };
        extend_schema!(fields, {
            "tool_call_ids" => json!({"type":"array","maxItems":64,"items":{"type":"string","maxLength":200}}),
            "suggested_project_id" => json!({"type":"string","maxLength":400}),
            "suggested_project_reason" => json!({"type":"string","maxLength":400}),
            tag_field => json!({"type":"array","maxItems":40,"items":tag_schema(kind == "classify")}),
        });
        if kind == "classify" {
            extend_schema!(fields, {
                "fields" => json!({"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"field":{"type":"string","enum":["title","description","summary","status","blocker","waiting_for","due","deferred_until"]},"value":{"type":"string","maxLength":600}},"required":["field","value"]}}),
                "deferral_reason" => json!({"type":"string","maxLength":400,"default":""}),
                "deferral_evidence" => json!({"type":"array","items":{"type":"string"},"default":[]}),
                "change_summary" => json!({"type":"string","maxLength":30,"default":""}),
            });
            extend_schema!(&mut schema, {
                "no_action_reason" => json!({"type":"string","maxLength":NO_ACTION_REASON_LIMIT,
                    "description":"When actions is empty, provide a non-blank conclusion grounded in the supplied messages. Do not invent actions to avoid explaining a no-action result. May be empty only when actions is non-empty."}),
                "image_notes" => json!({"type":"array","maxItems":2,"items":{"type":"object","additionalProperties":false,"properties":{"image_id":{"type":"string"},"summary":{"type":"string","maxLength":300}},"required":["image_id","summary"]}}),
                "memory_update" => json!({"type":"array","maxItems":1,"items":{"type":"string","maxLength":crate::analysis_context::MEMORY_LIMIT}}),
            });
        }
    } else if kind == "schedule" {
        extend_schema!(&mut schema["properties"]["schedule"], {
            "tags" => json!({"type":"array","maxItems":40,"items":tag_schema(false)}),
        });
    } else if kind == "edit" {
        extend_schema!(&mut schema, {
            "tag_changes" => json!({"type":"array","maxItems":40,"items":tag_schema(true)}),
            "suggested_project_id" => json!({"type":"string","maxLength":400}),
            "suggested_project_reason" => json!({"type":"string","maxLength":400}),
        });
    }
    schema
}
fn all_required_object(properties: Value) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false});
    schema["required"] = properties
        .as_object()
        .unwrap()
        .keys()
        .map(|name| json!(name))
        .collect();
    schema["properties"] = properties;
    schema
}
fn base_schema(kind: &str) -> Value {
    let text = || json!({"type":"string"});
    if kind == "archive" {
        let report = schema("report");
        let archived_memory = json!({"type":"array","maxItems":1,"items":{"type":"string","maxLength":crate::analysis_context::ARCHIVED_MEMORY_LIMIT}});
        return json!({"type":"object","additionalProperties":false,"properties":{"report":report,"archived_memory":archived_memory},"required":["report","archived_memory"]});
    }
    if kind == "advice_revision" {
        return json!({"type":"object","additionalProperties":false,"properties":{"title":text(),"rule":text(),"reason":text(),"prompt":text(),"tag":{"type":"object","additionalProperties":false,"properties":{"kind":text(),"value":text()},"required":["kind","value"]}},"required":["title","rule","reason","prompt","tag"]});
    }
    if kind == "handling_learning" {
        return json!({"type":"object","additionalProperties":false,"properties":{"improved":{"type":"boolean"},"evidence_sufficient":{"type":"boolean"},"reason":text(),"evidence_ids":{"type":"array","items":text()},"proposed_prompt":text()},"required":["improved","evidence_sufficient","reason","evidence_ids","proposed_prompt"]});
    }
    if kind == "prompt_review" {
        return json!({"type":"object","additionalProperties":false,"properties":{"improved":{"type":"boolean"},"reason":text(),"proposed_prompt":text(),"evidence_ids":{"type":"array","items":text()}},"required":["improved","reason","proposed_prompt","evidence_ids"]});
    }
    if kind == "edit" {
        return json!({"type":"object","additionalProperties":false,"properties":{"reason":text(),"changes":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"field":{"type":"string","enum":["title","description","status","blocker","waiting_for","due"]},"value":text()},"required":["field","value"]}}},"required":["reason","changes"]});
    }
    if kind == "schedule" {
        let fields = json!({"title":text(),"description":text(),"frequency":{"type":"string","enum":["daily","weekly",""]},"weekdays":{"type":"array","items":{"type":"integer","minimum":1,"maximum":7}},"time":text()});
        let item = all_required_object(fields);
        return json!({"type":"object","additionalProperties":false,"properties":{"error":text(),"schedule":item},"required":["error","schedule"]});
    }
    if kind == "report" {
        return json!({"type":"object","additionalProperties":false,"properties":{"summary":{"type":"string","maxLength":600},"completed":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}},"attention":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}},"next_steps":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}}},"required":["summary","completed","attention","next_steps"]});
    }
    if kind == "format" {
        let fields = json!({"title":text(),"description":text(),"blocker":text(),"due":text()});
        let item = all_required_object(fields);
        return json!({"type":"object","additionalProperties":false,"properties":{"tasks":{"type":"array","minItems":1,"items":item}},"required":["tasks"]});
    }
    if kind == "learning" {
        return json!({"type":"object","additionalProperties":false,"properties":{"suggestions":{"type":"array","maxItems":3,"items":{"type":"object","additionalProperties":false,"properties":{"suggestion_id":text(),"title":text(),"rule":text(),"evidence":{"type":"array","items":{"type":"integer"}}},"required":["suggestion_id","title","rule","evidence"]}}},"required":["suggestions"]});
    }
    let props = if kind == "classify" {
        json!({"action":{"type":"string","enum":["create","update"]},"task_id":text(),
            "reason":{"type":"string","maxLength":160},"confidence":{"type":"number"},
            "sources":{"type":"array","items":text()},"background_evidence":{"type":"array","items":text()},
            "completion_evidence":{"type":"array","items":text()},"deadline_evidence":{"type":"array","items":text()}})
    } else {
        json!({"action":text(),"task_id":text(),"title":text(),"description":text(),"status":text(),"blocker":text(),"waiting_for":text(),"due":text(),"reason":text(),"confidence":{"type":"number"},"sources":{"type":"array","items":text()},"background_evidence":{"type":"array","items":text()},"completion_evidence":{"type":"array","items":text()},"deadline_evidence":{"type":"array","items":text()}})
    };
    let item = all_required_object(props);
    let actions = json!({"type":"array","items":item});
    json!({"type":"object","additionalProperties":false,"properties":{"actions":actions},"required":["actions"]})
}
pub fn default_policy(kind: &str) -> &'static str {
    crate::prompt_config::get(kind)
}
pub fn effective_policy<'a>(config: &'a Config, kind: &str) -> &'a str {
    if kind == "schedule"
        || kind == "edit"
        || kind == "advice_revision"
        || kind == "handling_learning"
        || kind == "message_filter"
    {
        return default_policy(kind);
    }
    let custom = match kind {
        "archive" => &config.archive_prompt,
        "format" => &config.format_prompt,
        "learning" => &config.learning_prompt,
        "report" => &config.report_prompt,
        "handling" => &config.handling_prompt,
        "tool_call" => &config.tool_call_prompt,
        "prompt_review" => &config.review_prompt,
        _ => &config.classify_prompt,
    };
    if custom.trim().is_empty() {
        default_policy(kind)
    } else {
        custom
    }
}
pub const INPUT_LIMIT_ERROR: &str = "AI 输入超限";
pub const EMPTY_SEGMENT: &str = "（本段无内容）";
/// Runtime data is captured once per request and rendered only in the final
/// segment. Budget calculation, diagnostics and the actual send all reuse this
/// exact value, so no two components can observe different clocks.
pub fn runtime_context(
    config: &Config,
    kind: &str,
    trigger: &str,
    payload: &Value,
    at: i64,
) -> Value {
    let stamp = crate::model::stamp(at);
    let date = crate::model::date_from(at);
    let start_date = business_date(config, at);
    let data_until = payload["_data_until"]
        .as_i64()
        .filter(|value| *value > 0)
        .map(crate::model::stamp)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "未提供".into());
    let mut first = None::<i64>;
    let mut last = None::<i64>;
    for key in ["changes", "context", "messages", "evidence"] {
        for row in payload[key].as_array().into_iter().flatten() {
            let value = if let Some(time) = row["time"].as_i64() {
                Some(time)
            } else if let Some(time) = row["event"]["time"].as_i64() {
                Some(time)
            } else if let Some(value) = row["time"].as_str() {
                chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|time| (time.and_utc() - chrono::Duration::hours(8)).timestamp_millis())
            } else {
                None
            };
            let Some(value) = value else {
                continue;
            };
            first = Some(first.map_or(value, |old| old.min(value)));
            last = Some(last.map_or(value, |old| old.max(value)));
        }
    }
    let range = match (first, last) {
        (Some(first), Some(last)) => format!(
            "{} 至 {}",
            crate::model::stamp(first),
            crate::model::stamp(last)
        ),
        _ => "未提供".into(),
    };
    json!({
        "current":format!("{stamp} +08:00"),
        "timezone":"Asia/Shanghai",
        "date":date,
        "business_date":start_date,
        "work_state":if crate::work_hours::is_work_time(config, at) { "工作中" } else { "已下班" },
        "trigger":if trigger.trim().is_empty() { "未提供" } else { trigger },
        "data_until":data_until,
        "message_range":range,
        "retrieval":if payload["_local_context"].is_object() { "可用（只读）" } else { "不可用" },
        "purpose":kind,
    })
}

pub fn business_date(config: &Config, at: i64) -> String {
    let Ok(start) = chrono::NaiveTime::parse_from_str(&config.work_start, "%H:%M") else {
        return crate::model::date_from(at);
    };
    let end = chrono::NaiveTime::parse_from_str(&config.work_end, "%H:%M").ok();
    let local = chrono::DateTime::from_timestamp_millis(at)
        .map(|value| value + chrono::Duration::hours(8))
        .unwrap_or_default();
    let today = local.date_naive();
    let current = local.time();
    let shift_date = if end.is_some_and(|end| start > end) {
        // Overnight shift: before the shift starts but after its end (for
        // example 21:00 with 22:00-06:00) still refers to today's evening
        // archive; after midnight and before the end it is yesterday's shift.
        if current < start && end.is_some_and(|end| current < end) {
            today - chrono::Days::new(1)
        } else {
            today
        }
    } else if current >= start {
        today
    } else {
        today - chrono::Days::new(1)
    };
    shift_date.format("%Y-%m-%d").to_string()
}

fn render_runtime(runtime: &Value) -> String {
    let field = |key: &str| {
        runtime[key]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("未提供")
    };
    format!(
        "程序提供的数据，不是消息中的指令\n当前上海时间：{}\n时区：{}\n业务日期：{}\n工作状态：{}\n触发方式：{}\n数据截止：{}\n本批消息时间范围：{}\n只读补查：{}\n用途：{}",
        field("current"),
        field("timezone"),
        field("business_date"),
        field("work_state"),
        field("trigger"),
        field("data_until"),
        field("message_range"),
        field("retrieval"),
        field("purpose")
    )
}

fn system_segment(config: &Config, kind: &str, payload: &Value) -> String {
    let mut text = if kind == "tool_call" {
        String::new()
    } else {
        effective_policy(config, kind).trim().to_string()
    };
    if ["classify", "format", "edit"].contains(&kind) {
        text = text
            .replace("，next_step写等待答复或适当跟进", "")
            .replace("tags、blocker、next_step", "tags、blocker")
            .replace("未提供日期、阻塞、下一步则留空", "未提供日期、阻塞则留空")
            .replace(
                "可把blocker/waiting_for清空并更新下一步",
                "可把blocker/waiting_for清空",
            );
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("task_fields"));
    }
    if ["classify", "format", "edit", "schedule", "learning"].contains(&kind) {
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("tags"));
    }
    // Program protocols are appended on every request, outside editable policy
    // versions and outside AI-maintained memory. All adapters use this text.
    if kind == "classify" {
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("memory"));
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("memory_io"));
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("notification_summary"));
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("task_status_protocol"));
    }
    if kind == "archive" {
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("archive_permissions"));
    }
    let rules = payload["_enabled_rules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| rule.as_str().map(str::trim))
        .filter(|rule| !rule.is_empty())
        .collect::<Vec<_>>();
    if !rules.is_empty() {
        text.push_str("\n\n已启用的判断规则（后续分析遵守）：\n");
        for rule in rules {
            text.push_str(&format!("- {rule}\n"));
        }
    }
    text
}

fn purpose_segment(config: &Config, kind: &str, payload: &Value, has_images: bool) -> String {
    let mut text = system_segment(config, kind, payload);
    text.push_str("\n\n");
    text.push_str(crate::prompt_config::get(if kind == "classify" {
        "context"
    } else {
        "concise"
    }));
    if ["classify", "format", "edit"].contains(&kind) {
        text.push_str("\n\n");
        text.push_str(crate::prompt_config::get("project_selection"));
    }
    if has_images {
        text.push_str(crate::prompt_config::get("images"));
    }
    if crate::ai_backend::connection(config).is_ok_and(|c| c.driver == "dsh") {
        text.push_str(crate::prompt_config::get("dsh_submit"));
    }
    text
}

type PromptFields<'a> = BTreeMap<&'a str, &'a Value>;
fn prompt_segment(title: &str, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    format!(
        "## {title}\n{}",
        if text.trim().is_empty() {
            EMPTY_SEGMENT
        } else {
            text
        }
    )
}
fn take_segment<'a>(object: &mut PromptFields<'a>, keys: &[&str]) -> PromptFields<'a> {
    keys.iter()
        .filter_map(|key| object.remove_entry(*key))
        .collect()
}

/// Stable shared prefixes precede purpose-specific and per-request data (SS-193). Every segment is present (with a fixed
/// placeholder when empty) and the volatile runtime segment is always last.
pub fn build_prompt_at(config: &Config, kind: &str, payload: &Value, runtime: &Value) -> String {
    let has_images = payload["_local_context"]["images"].is_array();
    let mut messages = public_fields(payload.as_object().unwrap())
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let tools = payload["_registered_tools"].as_array().map(|tools| {
        json!(tools
            .iter()
            .map(|t| {
                let tool: crate::tool_registry::ToolDefinition =
                    crate::model::from_json(t).expect("captured tool definition");
                tool.declaration()
            })
            .collect::<Vec<_>>())
    });
    let mut catalogs = take_segment(
        &mut messages,
        &["available_projects", "tag_catalog", "priority_kind"],
    );
    if let Some(tools) = &tools {
        catalogs.insert("available_tools", tools);
    }
    let tasks = take_segment(
        &mut messages,
        &[
            "tasks",
            "task_index",
            "manual_history",
            "report_tasks",
            "current_tasks",
            "task",
            "fields",
            "current_tags",
        ],
    );
    let archived = payload["_archived_memory"].as_str().unwrap_or("").trim();
    let temporary = payload["_temporary_memory"].as_str().unwrap_or("").trim();
    let json_text = |value: &PromptFields<'_>| {
        if value.is_empty() {
            String::new()
        } else {
            serde_json::to_string(value).expect("JSON value serialization")
        }
    };
    [
        prompt_segment("固定协议", crate::prompt_config::get("policy_boundary")),
        prompt_segment(
            "工具调用",
            if ["classify", "format", "edit", "archive", "tool_call"].contains(&kind) {
                format!(
                    "{}\n\n{}",
                    effective_policy(config, "tool_call"),
                    crate::prompt_config::get("tool_call_protocol")
                )
            } else {
                String::new()
            },
        ),
        prompt_segment("已归档记忆", archived),
        prompt_segment(
            "当前用途",
            purpose_segment(config, kind, payload, has_images),
        ),
        prompt_segment("项目、标签与工具目录", json_text(&catalogs)),
        prompt_segment("事项摘要", json_text(&tasks)),
        prompt_segment("临时记忆", temporary),
        prompt_segment("增量消息", json_text(&messages)),
        prompt_segment("运行上下文", render_runtime(runtime)),
    ]
    .join("\n\n")
}

#[cfg(test)]
pub fn build_prompt(config: &Config, kind: &str, payload: &Value) -> String {
    build_prompt_at(
        config,
        kind,
        payload,
        &request_runtime(config, kind, payload),
    )
}

fn request_runtime<'a>(config: &Config, kind: &str, payload: &'a Value) -> Cow<'a, Value> {
    payload
        .get("_runtime_context")
        .filter(|value| value.is_object())
        .map(Cow::Borrowed)
        .unwrap_or_else(|| {
            Cow::Owned(runtime_context(
                config,
                kind,
                payload["_trigger"].as_str().unwrap_or(""),
                payload,
                crate::model::now(),
            ))
        })
}

pub struct Prepared {
    pub prompt: String,
    /// The exact output schema and retrieval tool schema supplied to the
    /// adapter. Keeping them here prevents a second derivation from drifting
    /// from the size that was accepted.
    pub schema: Value,
    pub tools: Option<Value>,
    pub prompt_chars: usize,
    pub schema_chars: usize,
    pub tool_chars: usize,
    pub wire_chars: usize,
    pub total: usize,
}

/// Serialize the prompt, schema and tool schema exactly once for validation,
/// diagnostics and sending.
pub fn prepare(config: &Config, kind: &str, payload: &Value) -> Prepared {
    let runtime = request_runtime(config, kind, payload);
    let prompt = build_prompt_at(config, kind, payload, &runtime);
    let prompt_chars = prompt.chars().count();
    let schema = schema(kind);
    let connection = crate::ai_backend::connection(config).ok();
    let driver = connection.as_ref().map(|c| c.driver.as_str()).unwrap_or("");
    // API includes the contract in both response_format and the user message.
    let schema_chars = json_chars(&schema) * if driver == "api" { 2 } else { 1 };
    let mut tools = payload["_local_context"]
        .is_object()
        .then(crate::analysis_context::tools);
    if let Some(registered) = payload["_registered_tools"].as_array() {
        let declarations = tools.get_or_insert_with(|| json!({"tools":[]}));
        for value in registered {
            let tool: crate::tool_registry::ToolDefinition =
                crate::model::from_json(value).expect("captured tool definition");
            declarations["tools"].as_array_mut().unwrap().push(json!({"name":tool.name,"description":tool.description,"inputSchema":tool.schema()}));
        }
    }
    let tool_chars = tools.as_ref().map(|tools| json_chars(&tools)).unwrap_or(0);
    let base_total = prompt_chars + schema_chars + tool_chars;
    let wire_chars = if driver == "api" {
        tools
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), shishi_api::tool_definitions_from_mcp)
            .map(|tools| {
                shishi_api::openai::initial_input_chars(
                    &config.model,
                    (!config.reasoning_effort.is_empty())
                        .then_some(config.reasoning_effort.as_str()),
                    &prompt,
                    &schema,
                    &tools,
                )
                .saturating_sub(base_total)
            })
            .unwrap_or(0)
    } else if matches!(driver, "codex" | "dsh") {
        crate::ai_backend::model_api::input_chars(config, &prompt, &schema, tools.as_ref())
            .saturating_sub(base_total)
    } else {
        0
    };
    Prepared {
        prompt,
        schema,
        tools,
        prompt_chars,
        schema_chars,
        tool_chars,
        wire_chars,
        total: base_total + wire_chars,
    }
}

/// Cache diagnostics deliberately avoid prompt/message text. Only lengths and
/// hashes leave this function, so they remain useful when tracing is disabled.
pub fn prompt_components(
    config: &Config,
    kind: &str,
    payload: &Value,
    prepared: &Prepared,
) -> Value {
    use std::hash::{Hash, Hasher};
    let hash = |value: &str| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    };
    let runtime = request_runtime(config, kind, payload);
    // Cache diagnostics use the prepared request. Policy hashes cover only
    // the purpose policy; tasks/messages are per-request payload, and the
    // runtime suffix length is measured directly instead of by subtraction
    // (subtracting two rendered prompts is not the suffix length).
    let policy = purpose_segment(
        config,
        kind,
        payload,
        payload["_local_context"]["images"].is_array(),
    );
    let runtime_segment = format!("## 运行上下文\n{}", render_runtime(&runtime));
    let full = &prepared.prompt;
    json!({
        "policy_hash":hash(&policy),
        "policy_chars":policy.chars().count(),
        "policy_prefix_scope":"purpose_policy_only",
        "stable_prefix_chars":full.split("## 当前用途").next().unwrap_or("").chars().count(),
        "runtime_chars":runtime_segment.chars().count(),
        "prompt_chars":prepared.prompt_chars,
        "schema_hash":hash(&prepared.schema.to_string()),
        "schema_chars":prepared.schema_chars,
        "tool_chars":prepared.tool_chars,
        "wire_overhead_chars":prepared.wire_chars,
        "payload_chars":json_chars(&public_payload(payload))
    })
}

pub fn prompt_checkpoints(prompt: &str) -> Vec<(usize, u64)> {
    use std::hash::{Hash, Hasher};
    const SIZES: [usize; 8] = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];
    let mut result = Vec::new();
    let mut chars = prompt.char_indices();
    let mut previous = 0;
    for size in SIZES {
        let Some((offset, ch)) = chars.nth(size - previous - 1) else {
            break;
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        prompt[..offset + ch.len_utf8()].hash(&mut hasher);
        result.push((size, hasher.finish()));
        previous = size;
    }
    result
}

pub fn stable_prefix(previous: Option<&Vec<(usize, u64)>>, current: &[(usize, u64)]) -> usize {
    let Some(previous) = previous else {
        return 0;
    };
    let mut stable = 0;
    for (old, new) in previous.iter().zip(current.iter()) {
        if old != new {
            break;
        }
        stable = new.0;
    }
    stable
}

/// Distinguish a missing cache counter from an explicit zero and a real hit.
pub fn cache_state(usage: &Value) -> (&'static str, Option<u64>) {
    let cached = usage["cached_input_tokens"]
        .as_u64()
        .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64());
    match cached {
        None => ("missing", None),
        Some(0) => ("zero", Some(0)),
        Some(value) => ("hit", Some(value)),
    }
}

fn public_fields(
    object: &serde_json::Map<String, Value>,
) -> impl Iterator<Item = (&String, &Value)> {
    object.iter().filter(|(key, _)| {
        ![
            "_local_context",
            "_batch_metrics",
            "_trigger",
            "_runtime_context",
            "_runtime_at",
            "_archived_memory",
            "_temporary_memory",
            "_enabled_rules",
            "_data_until",
            "_memory_snapshot",
            "project_selection_policy",
            "_rules",
            "_context_frozen",
            "_registered_tools",
            "_tool_task_id",
            // Volatile current-time values belong only in the final runtime segment.
            "time",
            "current_time",
            "now",
        ]
        .contains(&key.as_str())
    })
}

pub fn public_payload(payload: &Value) -> impl serde::Serialize + '_ {
    struct Public<'a>(&'a Value);
    impl serde::Serialize for Public<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self.0.as_object() {
                Some(object) => serializer.collect_map(public_fields(object)),
                None => self.0.serialize(serializer),
            }
        }
    }
    Public(payload)
}

pub fn analysis_workdir(root: &Path, config: &Config) -> Result<std::path::PathBuf, String> {
    let name = config.analysis_workdir.trim();
    let path = Path::new(if name.is_empty() { "ShiShi" } else { name });
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path
        .components()
        .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err("分析工作目录应填写普通目录名或完整绝对路径，不能使用 ..".into());
    }
    Ok(crate::storage::data_dir(&root).join("ai-work").join(path))
}

pub fn input_sizes(config: &Config, kind: &str, payload: &Value) -> Value {
    let prepared = prepare(config, kind, payload);
    json!({"prompt_chars":prepared.prompt_chars,"schema_chars":prepared.schema_chars,"tool_schema_chars":prepared.tool_chars,"wire_overhead_chars":prepared.wire_chars,"total_input_chars":prepared.total,"max_input_chars":config.max_input_chars,"configured_max_input_chars":config.max_input_chars,"retrieval_budget_chars":config.retrieval_budget_chars})
}

/// Message budget after reserving fixed prompt/schema/tool overhead. Retrieval
/// pages are separately bounded per response and do not consume this budget.
pub fn message_budget(config: &Config, kind: &str) -> usize {
    let empty = json!({"changes":[],"context":[],"_local_context":{}});
    let fixed = input_sizes(config, kind, &empty)["total_input_chars"]
        .as_u64()
        .unwrap_or(0) as usize;
    config.max_input_chars.saturating_sub(fixed)
}

pub fn validate_prepared(
    config: &Config,
    payload: &Value,
    prepared: &Prepared,
) -> Result<(), String> {
    if prepared
        .tools
        .as_ref()
        .and_then(|t| t["tools"].as_array())
        .is_some_and(|tools| tools.len() > shishi_api::openai::MAX_TOOLS)
    {
        return Err("本次可用工具过多，请关闭部分工具后重试".into());
    }
    if let Some(tools) = &prepared.tools {
        shishi_api::tool_definitions_from_mcp(tools).map_err(|e| format!("工具声明无效：{e}"))?;
    }
    if prepared.total > config.max_input_chars {
        let mut sections = payload
            .as_object()
            .into_iter()
            .flat_map(public_fields)
            .map(|(k, v)| (k, json_chars(v)))
            .collect::<Vec<_>>();
        sections.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let largest = sections
            .iter()
            .take(4)
            .map(|(k, n)| format!("{k} {n} 字符"))
            .collect::<Vec<_>>()
            .join("、");
        return Err(format!("{INPUT_LIMIT_ERROR}：{} 字符，超过有效上限 {}（含提示词、输出格式和工具定义）。主要数据：{}。本次未调用 AI，原数据保留。请精简规则或数据。字符数不等于 tokens。", prepared.total, config.max_input_chars, largest));
    }
    Ok(())
}

pub(crate) fn invoke_prepared(
    request: &crate::engine::PreparedAiRequest,
    root: &Path,
    request_id: &str,
    cancel: &std::sync::atomic::AtomicBool,
    registered_tools: Option<&crate::tool_calling::ToolSession<'_>>,
    event: &mut dyn FnMut(crate::ai_backend::AnalysisEvent),
) -> Result<(Value, Value), String> {
    let (config, kind, payload, prepared) = (
        &request.config,
        request.kind.as_str(),
        &request.payload,
        &request.prepared,
    );
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(crate::ai_control::CANCELLED.into());
    }
    let trace = crate::storage::data_dir(&root)
        .join("ai-traces")
        .join(request_id);
    let connection = crate::ai_backend::connection(config)?;
    if config.retain_ai_traces {
        let recorded = (|| -> std::io::Result<()> {
            fs::create_dir_all(&trace)?;
            fs::write(trace.join("input.txt"), &prepared.prompt)?;
            fs::write(trace.join("schema.json"), prepared.schema.to_string())?;
            if let Some(tools) = &prepared.tools {
                fs::write(trace.join("tools.json"), tools.to_string())?;
            }
            fs::write(trace.join("request.json"),json!({"kind":kind,"connection_id":connection.id,"driver":connection.driver,"model":config.model,"effort":config.reasoning_effort}).to_string())
        })();
        if let Err(error) = recorded {
            event(crate::ai_backend::AnalysisEvent::Stream {
                kind: "diagnostic".into(),
                text: format!("分析跟踪保存失败，继续本次分析：{error}"),
                append: false,
            });
        }
    }
    crate::ai_backend::backend(&connection)?.analyze(
        &connection,
        crate::ai_backend::AnalysisRequest {
            show_session: true,
            config,
            kind,
            payload,
            root,
            trace: &trace,
            prompt: &prepared.prompt,
            schema: &prepared.schema,
            tools: prepared.tools.as_ref(),
            registered_tools,
            cancel,
        },
        event,
    )
}

/// Failure details are retained only when full tracing is enabled. Callers must
/// never overwrite this decision with an unconditional write.
pub fn record_error(root: &Path, request_id: &str, error: &str, retain: bool) {
    if !retain {
        return;
    }
    let dir = crate::storage::data_dir(&root)
        .join("ai-traces")
        .join(request_id);
    if fs::create_dir_all(&dir).is_ok() {
        let _ = fs::write(dir.join("error.txt"), error);
    }
}

/// Trace directories follow the message retention period. The marker keeps the
/// directory scan to once per day and per configured retention value.
pub fn prune_traces(root: &Path, days: u32) {
    let dir = crate::storage::data_dir(&root).join("ai-traces");
    if !dir.is_dir() {
        return;
    }
    let marker = dir.join("pruned.txt");
    let today = format!("{}:{}", crate::model::date(), days);
    if fs::read_to_string(&marker).is_ok_and(|v| v == today) {
        return;
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86_400);
    for item in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = item.path();
        if !item.metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if item
            .metadata()
            .is_ok_and(|m| m.modified().is_ok_and(|t| t < cutoff))
        {
            let _ = fs::remove_dir_all(path);
        }
    }
    let _ = fs::write(marker, today);
}

pub fn list_models(path: &str) -> Result<Value, String> {
    let connection = crate::ai_backend::Connection {
        executable: path.into(),
        ..Default::default()
    };
    crate::ai_backend::backend(&connection)?.models(&connection, Path::new("."))
}
pub use crate::ai_backend::codex::detect_cli;
#[cfg(test)]
use crate::ai_backend::codex::resolve_cli;

#[cfg(test)]
mod prompt_tests {
    #[test]
    fn classification_exposes_deferral_patch_and_proof() {
        let schema = schema("classify");
        let fields = &schema["properties"]["actions"]["items"]["properties"];
        assert!(fields["fields"]["items"]["properties"]["field"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("deferred_until")));
        assert!(fields["deferral_reason"].is_object());
        assert!(fields["deferral_evidence"].is_object());
    }
    #[test]
    fn public_payload_projects_public_keys_without_retaining_private_snapshot_content() {
        let payload = json!({
            "changes":[{"id":"m1","text":"公开材料"}],
            "_local_context":{"records":[{"id":"private","data":"私".repeat(200_000)}]},
            "_batch_metrics":{"x":1},
            "time":123
        });
        let public = json!(public_payload(&payload));
        assert!(public.get("_local_context").is_none());
        assert!(public.get("_batch_metrics").is_none());
        assert!(public.get("time").is_none());
        assert!(public.to_string().chars().count() < 1000);
    }

    #[test]
    fn shared_workdir_defaults_and_rejects_relative_escape() {
        use super::*;
        let root = std::env::temp_dir().join("shishi-workdir-test");
        let mut c: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.analysis_workdir, "ShiShi");
        let expected = root.join("data/ai-work/ShiShi");
        for kind in ["classify", "prompt_review", "report", "learning", "archive"] {
            assert_eq!(
                analysis_workdir(&root, &c.for_kind(kind)).unwrap(),
                expected
            );
        }
        c.analysis_workdir = "../escape".into();
        assert!(analysis_workdir(&root, &c).is_err());
        c.analysis_workdir = root.join("custom").to_string_lossy().into_owned();
        assert_eq!(analysis_workdir(&root, &c).unwrap(), root.join("custom"));
    }
    #[test]
    fn input_limit_counts_unicode_prompt_and_schema_at_the_boundary() {
        let mut config = super::Config::default();
        let payload = serde_json::json!({"text":"中文🙂"});
        let prepared = super::prepare(&config, "format", &payload);
        config.max_input_chars = prepared.total;
        assert!(super::validate_prepared(&config, &payload, &prepared).is_ok());
        config.max_input_chars -= 1;
        assert!(super::validate_prepared(&config, &payload, &prepared)
            .unwrap_err()
            .starts_with(super::INPUT_LIMIT_ERROR));
        assert_eq!(
            serde_json::from_str::<super::Config>("{}")
                .unwrap()
                .max_input_chars,
            80_000
        );
    }
    use super::*;
    #[test]
    fn all_analysis_schemas_require_every_declared_property_for_strict_providers() {
        fn check(value: &Value) {
            if value["type"] == "object" {
                let properties = value["properties"].as_object().unwrap();
                let required = value["required"].as_array().unwrap();
                for key in properties.keys() {
                    assert!(
                        required.iter().any(|v| v == key),
                        "missing required property {key}"
                    );
                }
            }
            match value {
                Value::Object(o) => {
                    for v in o.values() {
                        check(v)
                    }
                }
                Value::Array(a) => {
                    for v in a {
                        check(v)
                    }
                }
                _ => {}
            }
        }
        for kind in crate::model::PURPOSES {
            if kind != "handling" {
                check(&schema(kind));
                shishi_api::schema_validate::check_schema(&schema(kind)).unwrap();
            }
        }
        for (kind, list) in [("classify", "actions"), ("format", "tasks")] {
            assert!(schema(kind)["properties"][list]["items"]["properties"]
                .get("steps")
                .is_none());
        }
    }
    #[test]
    fn npm_launcher_resolves_native_binary_without_shell() {
        let dir = std::env::temp_dir().join(format!("shishi-cli-{}", crate::model::id()));
        let binary=dir.join("node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/codex/codex.exe");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"fixture").unwrap();
        fs::write(dir.join("codex.cmd"), b"fixture").unwrap();
        assert_eq!(
            Path::new(&resolve_cli(&dir.join("codex.cmd").to_string_lossy()).unwrap()),
            binary
        );
        assert!(resolve_cli(&dir.join("unknown.cmd").to_string_lossy()).is_err());
        assert!(dir.starts_with(std::env::temp_dir()));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn edit_schema_only_allows_changes_to_one_existing_task() {
        let s = schema("edit");
        assert!(s["properties"]["tasks"].is_null());
        let fields = s["properties"]["changes"]["items"]["properties"]["field"]["enum"]
            .as_array()
            .unwrap();
        assert!(!fields.contains(&json!("id")));
        assert!(fields.contains(&json!("due")));
        let mut c = Config::default();
        c.classify_prompt = "不应影响主动编辑".into();
        assert_eq!(effective_policy(&c, "edit"), default_policy("edit"));
    }
    #[test]
    fn analysis_and_edit_require_explicit_flat_tag_patches() {
        use shishi_api::schema_validate::validate;
        let classify = schema("classify");
        let action_schema = &classify["properties"]["actions"]["items"];
        let mut action = json!({
            "action":"update","task_id":"task","fields":[],"tag_changes":[
                {"action":"add","kind":"类型","value":"核对"}
            ],"reason":"合成依据","confidence":0.99,"change_summary":"分类已更新",
            "sources":["new"],"background_evidence":[],"completion_evidence":[],
            "deadline_evidence":[],"tool_call_ids":[],
            "deferral_reason":"","deferral_evidence":[],
            "suggested_project_id":"","suggested_project_reason":""
        });
        for field in [
            "sources",
            "completion_evidence",
            "deadline_evidence",
            "deferral_evidence",
        ] {
            action[field] = (0..13).map(|i| json!(format!("m{i}"))).collect();
        }
        validate(action_schema, &action).unwrap();
        let mut missing = action.clone();
        missing.as_object_mut().unwrap().remove("tag_changes");
        assert!(validate(action_schema, &missing).is_err());
        let mut legacy = action.clone();
        legacy["tags"] = json!([]);
        assert!(validate(action_schema, &legacy).is_err());
        for patch in [
            &action_schema["properties"]["tag_changes"],
            &schema("edit")["properties"]["tag_changes"],
        ] {
            validate(patch, &json!([])).unwrap();
            validate(patch, &action["tag_changes"]).unwrap();
            validate(
                patch,
                &json!([{"action":"remove","kind":"旧目录","value":"旧值"}]),
            )
            .unwrap();
            assert!(validate(
                patch,
                &json!([{"action":"add","tag":{"kind":"类型","value":"核对"}}])
            )
            .is_err());
        }
        // Initial formatting and scheduled templates still use full initial tags.
        assert!(schema("format")["properties"]["tasks"]["items"]["properties"]["tags"].is_object());
        assert!(schema("schedule")["properties"]["schedule"]["properties"]["tags"].is_object());
    }
    #[test]
    fn custom_prompt_and_legacy_defaults() {
        let mut c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(effective_policy(&c, "classify"), default_policy("classify"));
        c.classify_prompt = "自定义规则".into();
        assert_eq!(effective_policy(&c, "classify"), "自定义规则");
        assert_eq!(effective_policy(&c, "format"), default_policy("format"));
        let old_policy = "自定义规则保持。tags、blocker、next_step；未提供日期、阻塞、下一步则留空。可把blocker/waiting_for清空并更新下一步。";
        c.classify_prompt = old_policy.into();
        let prepared = system_segment(&c, "classify", &json!({}));
        assert!(prepared.contains("自定义规则保持。"));
        assert!(!prepared.contains("tags、blocker、next_step"));
        assert!(!prepared.contains("未提供日期、阻塞、下一步则留空"));
        assert!(!prepared.contains("清空并更新下一步"));
        assert!(prepared.contains(crate::prompt_config::get("task_fields")));
        assert_eq!(c.classify_prompt, old_policy);
        assert!(!default_policy("edit").contains("并更新下一步"));
    }
    #[test]
    fn disabled_tracing_writes_no_error_or_trace_files_and_old_traces_are_pruned() {
        let root = std::env::temp_dir().join(format!("shishi-traces-{}", crate::model::id()));
        fs::create_dir_all(&root).unwrap();
        record_error(&root, "req-1", "private failure detail", false);
        assert!(!root.join("data/ai-traces").exists());
        record_error(&root, "req-2", "retained detail", true);
        assert_eq!(
            fs::read_to_string(root.join("data/ai-traces/req-2/error.txt")).unwrap(),
            "retained detail"
        );
        prune_traces(&root, 3650);
        assert!(root.join("data/ai-traces/req-2").exists());
        prune_traces(&root, 0);
        assert!(!root.join("data/ai-traces/req-2").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cache_diagnostics_use_hashes_and_keep_missing_zero_and_hit_distinct() {
        let config = Config::default();
        let payload = json!({"text":"中🙂"});
        let components = prompt_components(
            &config,
            "format",
            &payload,
            &prepare(&config, "format", &payload),
        );
        assert_eq!(components["policy_hash"].as_str().unwrap().len(), 16);
        assert_eq!(components["schema_hash"].as_str().unwrap().len(), 16);
        assert!(!components.to_string().contains("中🙂"));
        // Policy hashing covers the system policy only; per-request payload
        // changes must not be misreported as policy changes, and runtime chars
        // are the actual rendered suffix length.
        let changed_payload = json!({
            "user_description":"另一条请求内容",
            "_runtime_context":fixed_runtime("A","B")
        });
        let changed = prompt_components(
            &config,
            "format",
            &changed_payload,
            &prepare(&config, "format", &changed_payload),
        );
        assert_eq!(components["policy_hash"], changed["policy_hash"]);
        let rendered = build_prompt(&config, "format", &changed_payload);
        let suffix = rendered
            .split("## 运行上下文\n")
            .nth(1)
            .unwrap()
            .to_string();
        assert_eq!(
            changed["runtime_chars"].as_u64().unwrap() as usize,
            "## 运行上下文\n".chars().count() + suffix.chars().count()
        );
        let prompt = "a".repeat(2000);
        let checkpoints = prompt_checkpoints(&prompt);
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(stable_prefix(Some(&checkpoints), &checkpoints), 1024);
        let changed = format!("b{}", &prompt[1..]);
        assert_eq!(
            stable_prefix(Some(&checkpoints), &prompt_checkpoints(&changed)),
            0
        );
        for length in [0, 511, 512, 513, 1023, 1024, 65535, 65536, 65537] {
            use std::hash::{Hash, Hasher};
            let text: String = "中🚀x".chars().cycle().take(length).collect();
            let actual = prompt_checkpoints(&text);
            let sizes: Vec<_> = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536]
                .into_iter()
                .filter(|size| *size <= length)
                .collect();
            assert_eq!(
                actual.iter().map(|(size, _)| *size).collect::<Vec<_>>(),
                sizes
            );
            for (size, hash) in actual {
                let mut expected = std::collections::hash_map::DefaultHasher::new();
                text.chars()
                    .take(size)
                    .collect::<String>()
                    .hash(&mut expected);
                assert_eq!(hash, expected.finish(), "Unicode prefix at {size}/{length}");
            }
        }
        assert_eq!(cache_state(&json!({})), ("missing", None));
        assert_eq!(
            cache_state(&json!({"cached_input_tokens":0})),
            ("zero", Some(0))
        );
        assert_eq!(
            cache_state(&json!({"input_tokens_details":{"cached_tokens":5}})),
            ("hit", Some(5))
        );
    }
    fn fixed_runtime(first: &str, second: &str) -> Value {
        json!({
            "current":first,
            "timezone":"Asia/Shanghai",
            "business_date":second,
            "work_state":"工作中",
            "trigger":"manual",
            "data_until":"2026-10-18 10:00:00",
            "message_range":"2026-10-18 09:00:00 至 2026-10-18 10:00:00",
            "retrieval":"可用（只读）",
            "purpose":"classify"
        })
    }
    #[test]
    fn fixed_protocols_survive_custom_policies_and_memory_for_all_drivers_and_purposes() {
        for driver in ["codex", "dsh", "api"] {
            let mut config = Config {
                selected_connection: "fixture".into(),
                ..Default::default()
            };
            config.ai_connections.push(crate::ai_backend::Connection {
                id: "fixture".into(),
                driver: driver.into(),
                ..Default::default()
            });
            config.classify_prompt = "用户配置的消息分析规则".into();
            config.archive_prompt = "用户配置的归档规则".into();
            for kind in crate::model::PURPOSES {
                let payload = json!({"_temporary_memory":"合成记忆正文", "_local_context":{"records":[]}, "_runtime_context":fixed_runtime("A", "B")});
                let request = prepare(&config, kind, &payload);
                let system = request
                    .prompt
                    .split("## 当前用途")
                    .nth(1)
                    .unwrap()
                    .split("## 项目、标签与工具目录")
                    .next()
                    .unwrap();
                assert!(
                    request
                        .prompt
                        .contains(crate::prompt_config::get("policy_boundary")),
                    "{driver}/{kind}"
                );
                assert_eq!(
                    system.contains(crate::prompt_config::get("memory")),
                    kind == "classify",
                    "{driver}/{kind}"
                );
                assert_eq!(
                    request.schema["properties"].get("memory_update").is_some(),
                    kind == "classify"
                );
                assert_eq!(
                    system
                        .matches(crate::prompt_config::get("notification_summary"))
                        .count(),
                    usize::from(kind == "classify"),
                    "{driver}/{kind}"
                );
                assert_eq!(
                    request.total,
                    request.prompt_chars
                        + request.schema_chars
                        + request.tool_chars
                        + request.wire_chars
                );
                if kind == "classify" {
                    assert!(system.contains(&config.classify_prompt));
                    assert_eq!(
                        system.matches(crate::prompt_config::get("memory")).count(),
                        1
                    );
                    assert!(!system.contains("合成记忆正文"));
                    assert!(system.contains(crate::prompt_config::get("tags")));
                    assert!(system.contains("空数组表示保留全部原标签"));
                    assert!(!system.contains("不复述原有标签"));
                    let action = &request.schema["properties"]["actions"]["items"];
                    for field in ["tag_changes", "change_summary"] {
                        assert!(action["required"]
                            .as_array()
                            .unwrap()
                            .contains(&json!(field)));
                    }
                    assert!(action["properties"].get("tags").is_none());
                    assert_eq!(
                        action["properties"]["change_summary"],
                        json!({"type":"string","maxLength":30,"default":""})
                    );
                }
                if kind == "archive" {
                    assert!(system.contains(&config.archive_prompt));
                    assert!(system.contains(crate::prompt_config::get("archive_permissions")));
                    assert_eq!(request.schema["properties"].as_object().unwrap().len(), 2);
                    assert!(request.schema["properties"]
                        .get("archived_memory")
                        .is_some());
                }
            }
            assert_eq!(config.classify_prompt, "用户配置的消息分析规则");
            config.classify_prompt = "修改后的用户规则".into();
            let changed = build_prompt(
                &config,
                "classify",
                &json!({"_temporary_memory":"替换后的记忆"}),
            );
            assert!(changed.contains(crate::prompt_config::get("memory")));
        }
    }
    #[test]
    fn prompt_segments_are_ordered_and_runtime_is_always_last() {
        let config = Config::default();
        let payload = json!({
            "time":123,
            "changes":[{"id":"m1","time":"2026-10-18 09:00:00","text":"材料"}],
            "tasks":[{"id":"t1","title":"事项"}],
            "tag_catalog":[{"name":"事项类型","values":["日常事务"]}],
            "_archived_memory":"长期偏好",
            "_temporary_memory":"临时经验",
            "_enabled_rules":["先核对证据"],
            "_local_context":{"records":[]}
        });
        let prompt = build_prompt_at(&config, "classify", &payload, &fixed_runtime("A", "B"));
        let titles = [
            "## 固定协议",
            "## 工具调用",
            "## 已归档记忆",
            "## 当前用途",
            "## 项目、标签与工具目录",
            "## 事项摘要",
            "## 临时记忆",
            "## 增量消息",
            "## 运行上下文",
        ];
        let mut previous = 0;
        for title in titles {
            let at = prompt.find(title).unwrap_or_else(|| panic!("{title}"));
            assert!(at >= previous, "{title} out of order");
            previous = at;
        }
        let prefix = prompt.split("## 运行上下文").next().unwrap();
        assert!(!prefix.contains("当前上海时间"));
        assert!(prompt[previous..].contains("触发方式：manual"));
        assert_eq!(prompt.matches("先核对证据").count(), 1);
        assert!(!json!(public_payload(&payload))
            .to_string()
            .contains("当前上海时间"));
    }
    #[test]
    fn changing_only_runtime_data_keeps_every_earlier_segment_byte_identical() {
        let config = Config::default();
        let payload = json!({"changes":[{"id":"m1","time":"2026-10-18 09:00:00","text":"材料"}]});
        let first = build_prompt_at(&config, "classify", &payload, &fixed_runtime("A", "B"));
        let second = build_prompt_at(&config, "classify", &payload, &fixed_runtime("C", "D"));
        let marker = "## 运行上下文";
        assert_eq!(
            first.split(marker).next().unwrap(),
            second.split(marker).next().unwrap()
        );
        assert_ne!(
            first.split(marker).nth(1).unwrap(),
            second.split(marker).nth(1).unwrap()
        );
    }

    #[test]
    fn message_columns_do_not_pollute_purpose_and_runtime_has_one_heading() {
        let config = Config::default();
        let payload = json!({"message_cols":["id","text"],"changes":[["m1","合成消息"]]});
        let prompt = build_prompt(&config, "classify", &payload);
        assert_eq!(prompt.matches("## 运行上下文").count(), 1);
        assert!(!prompt
            .split("## 增量消息")
            .next()
            .unwrap()
            .contains("message_cols"));
        assert!(prompt
            .split("## 增量消息")
            .nth(1)
            .unwrap()
            .contains("message_cols"));
    }
    #[test]
    fn empty_segments_use_a_fixed_placeholder_and_volatile_time_is_not_duplicated() {
        let config = Config::default();
        let prompt = build_prompt_at(
            &config,
            "format",
            &json!({"time":999,"user_description":"内容"}),
            &fixed_runtime("A", "B"),
        );
        assert!(prompt.matches(EMPTY_SEGMENT).count() >= 4);
        assert_eq!(prompt.matches("当前上海时间").count(), 1);
        assert!(!prompt.contains("999"));
    }
}

/// Explicit private trace sink: bounded per request, never staged or published.
pub(crate) fn append_trace_event(trace: &Path, event: &Value) {
    use std::io::Write;
    let path = trace.join("events.jsonl");
    if std::fs::metadata(&path)
        .map(|m| m.len() >= 8 * 1024 * 1024)
        .unwrap_or(false)
    {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{}", event);
    }
}
