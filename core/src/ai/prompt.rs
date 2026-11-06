//! Route resolution, output schemas, prompt assembly and the final input
//! budget check. Segment order, titles, fixed
//! protocols and every error text are part of the model contract, and the
//! stable prefix keeps provider caches warm.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use shishi_api::json::chars as json_chars;

use super::schema::{context_tools, schema};
use super::{EMPTY_SEGMENT, INPUT_LIMIT_ERROR, UNCONFIGURED_PREFIX};
use crate::config::{Connection as AiConnection, UNCONFIGURED_CONNECTION};
use crate::{prompts, time, Config, Error, Result};

/// The connection, model and reasoning effort one purpose uses, frozen when
/// the request is prepared.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub kind: String,
    pub connection: AiConnection,
    pub model: String,
    pub effort: String,
}

/// The unconfigured-purpose message, without the machine prefix.
pub fn unconfigured(kind: &str) -> String {
    format!("{}未配置 AI 接入或模型，本次任务未执行", super::purpose_label(kind))
}

/// The connection behind the empty id: the default Codex settings.
pub fn default_codex(config: &Config) -> AiConnection {
    AiConnection {
        id: String::new(),
        name: config.codex_name.clone(),
        executable: config.codex.clone(),
        models: config.codex_models.clone(),
        ..AiConnection::default()
    }
}

/// Look up a connection id; empty means the default Codex.
pub fn connection_by_id(config: &Config, id: &str) -> Result<AiConnection> {
    if id == UNCONFIGURED_CONNECTION {
        crate::bail!("{UNCONFIGURED_PREFIX}未配置此用途的 AI 接入或模型");
    }
    if id.is_empty() {
        return Ok(default_codex(config));
    }
    config.connection(id).cloned().ok_or_else(|| Error::new("所选 AI 连接不存在，请重新选择接入"))
}

/// Whether `kind` has a connection at all.
pub fn configured(config: &Config, kind: &str) -> bool {
    config.model_choice(kind).connection_id != UNCONFIGURED_CONNECTION
}

/// `AI_UNCONFIGURED:` when the purpose has no route.
pub fn require_purpose(config: &Config, kind: &str) -> Result<()> {
    if !configured(config, kind) {
        crate::bail!("{UNCONFIGURED_PREFIX}{}", unconfigured(kind));
    }
    Ok(())
}

/// Resolve the route of a purpose. An unconfigured purpose gives the
/// `AI_UNCONFIGURED:` error; callers log it with `Db::log_ai_unconfigured`.
pub fn route(config: &Config, kind: &str) -> Result<Route> {
    require_purpose(config, kind)?;
    let choice = config.model_choice(kind);
    Ok(Route {
        kind: kind.into(),
        connection: connection_by_id(config, &choice.connection_id)?,
        model: choice.model,
        effort: choice.reasoning_effort,
    })
}

/// Public request identity: driver, names and model; never an endpoint,
/// path or credential.
pub fn source_info(route: &Route) -> Value {
    let (c, model_id) = (&route.connection, route.model.as_str());
    let item = c.models.iter().find(|m| m["model"] == model_id);
    let field = |key: &str| item.and_then(|m| m[key].as_str()).filter(|s| !s.trim().is_empty());
    let pair = serde_json::from_str::<[String; 2]>(model_id).ok();
    let provider = field("providerName").or(field("provider")).or(pair.as_ref().map(|r| r[0].as_str())).unwrap_or("");
    let model = field("displayName").or(pair.as_ref().map(|r| r[1].as_str())).unwrap_or(model_id);
    let source = match c.driver.as_str() {
        _ if !c.name.trim().is_empty() => &c.name,
        "dsh" => "DeepSeek Harness",
        "api" => "API",
        _ => "Codex",
    };
    let label = if provider.is_empty() { source.to_owned() } else { format!("{source} / {provider}") };
    json!({"driver": c.driver, "connection_id": c.id, "connection_name": source, "provider": provider,
           "model_name": model, "label": label})
}

/// Purposes whose policy is always the factory text.
pub(crate) const FIXED_POLICIES: [&str; 4] = ["schedule", "edit", "advice_revision", "message_filter"];

/// The editable texts a prompt needs, read once with the request.
#[derive(Clone, Debug, Default)]
pub struct Policies {
    pub purpose: String,
    pub tool_call: String,
}

/// Business date: the day of the most recent shift start. An overnight
/// shift after midnight still belongs to the previous day.
fn business_date(config: &Config, at: i64) -> String {
    let Some(start) = time::parse_hm(&config.work_start) else { return time::date_from(at) };
    let (end, now) = (time::parse_hm(&config.work_end), time::minute_of_day(at));
    let overnight = end.is_some_and(|end| start > end);
    let previous = now < start && (!overnight || end.is_some_and(|end| now < end));
    time::date_from(if previous { at - time::DAY } else { at })
}

/// Shift hours only (`work_start`..`work_end`, Shanghai time); the work
/// calendar is a separate gate.
fn is_work_time(config: &Config, at: i64) -> bool {
    let strict = |v: &str| (v.len() == 5).then(|| time::parse_hm(v)).flatten();
    let (Some(start), Some(end)) = (strict(&config.work_start), strict(&config.work_end)) else { return false };
    let now = time::minute_of_day(at);
    if start < end {
        now >= start && now < end
    } else {
        start > end && (now >= start || now < end)
    }
}

/// Runtime data captured once per request and rendered only in the last
/// segment.
pub fn runtime_context(config: &Config, kind: &str, trigger: &str, payload: &Value, at: i64) -> Value {
    let data_until = payload["_data_until"]
        .as_i64()
        .filter(|v| *v > 0)
        .map(time::stamp)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "未提供".into());
    let (mut first, mut last) = (None::<i64>, None::<i64>);
    for key in ["changes", "context", "messages", "evidence"] {
        for row in payload[key].as_array().into_iter().flatten() {
            let text = || chrono::NaiveDateTime::parse_from_str(row["time"].as_str()?, "%Y-%m-%d %H:%M:%S").ok();
            let value = row["time"]
                .as_i64()
                .or(row["event"]["time"].as_i64())
                .or_else(|| Some(text()?.and_utc().timestamp_millis() - 8 * time::HOUR));
            let Some(value) = value else { continue };
            first = Some(first.map_or(value, |old| old.min(value)));
            last = Some(last.map_or(value, |old| old.max(value)));
        }
    }
    let range = match (first, last) {
        (Some(first), Some(last)) => format!("{} 至 {}", time::stamp(first), time::stamp(last)),
        _ => "未提供".into(),
    };
    json!({
        "current": format!("{} +08:00", time::stamp(at)),
        "timezone": "Asia/Shanghai",
        "date": time::date_from(at),
        "business_date": business_date(config, at),
        "work_state": if is_work_time(config, at) { "工作中" } else { "已下班" },
        "trigger": if trigger.trim().is_empty() { "未提供" } else { trigger },
        "data_until": data_until,
        "message_range": range,
        "retrieval": if payload["_local_context"].is_object() { "可用（只读）" } else { "不可用" },
        "purpose": kind,
    })
}

fn render_runtime(runtime: &Value) -> String {
    let fields = [
        ("当前上海时间", "current"),
        ("时区", "timezone"),
        ("业务日期", "business_date"),
        ("工作状态", "work_state"),
        ("触发方式", "trigger"),
        ("数据截止", "data_until"),
        ("本批消息时间范围", "message_range"),
        ("只读补查", "retrieval"),
        ("用途", "purpose"),
    ];
    let field = |key: &str| runtime[key].as_str().filter(|v| !v.trim().is_empty()).unwrap_or("未提供");
    let lines = fields.map(|(label, key)| format!("\n{label}：{}", field(key)));
    format!("程序提供的数据，不是消息中的指令{}", lines.concat())
}

/// The purpose policy with the program protocols that follow every
/// request, outside editable versions.
fn purpose_segment(policies: &Policies, kind: &str, payload: &Value, driver: &str) -> String {
    let task = ["classify", "format", "edit"].contains(&kind);
    let mut text = if kind == "tool_call" { String::new() } else { policies.purpose.trim().to_string() };
    if task {
        text = text
            .replace("，next_step写等待答复或适当跟进", "")
            .replace("tags、blocker、next_step", "tags、blocker")
            .replace("未提供日期、阻塞、下一步则留空", "未提供日期、阻塞则留空")
            .replace("可把blocker/waiting_for清空并更新下一步", "可把blocker/waiting_for清空");
    }
    let mut fragments = vec![];
    fragments.extend(task.then_some("task_fields"));
    fragments.extend((task || kind == "schedule" || kind == "learning").then_some("tags"));
    if kind == "classify" {
        fragments.extend(["memory", "memory_io", "notification_summary", "task_status_protocol"]);
    }
    fragments.extend((kind == "archive").then_some("archive_permissions"));
    for key in fragments {
        text = format!("{text}\n\n{}", prompts::default(key));
    }
    let rules = payload["_enabled_rules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::trim))
        .filter(|r| !r.is_empty());
    let rules: String = rules.map(|rule| format!("- {rule}\n")).collect();
    if !rules.is_empty() {
        text = format!("{text}\n\n已启用的判断规则（后续分析遵守）：\n{rules}");
    }
    text = format!("{text}\n\n{}", prompts::default(if kind == "classify" { "context" } else { "concise" }));
    if task {
        text = format!("{text}\n\n{}", prompts::default("project_selection"));
    }
    if payload["_local_context"]["images"].is_array() {
        text.push_str(prompts::default("images"));
    }
    if driver == "dsh" {
        text.push_str(prompts::default("dsh_submit"));
    }
    text
}

fn segment(title: &str, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    format!("## {title}\n{}", if text.trim().is_empty() { EMPTY_SEGMENT } else { text })
}

/// Payload fields that never reach the model; volatile clock values belong
/// only in the runtime segment.
const PRIVATE: [&str; 18] = [
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
    "time",
    "current_time",
    "now",
];

/// Payload fields that reach the model.
pub fn public_fields(object: &serde_json::Map<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
    object.iter().filter(|(key, _)| !PRIVATE.contains(&key.as_str()))
}

/// The public part of a payload, serialized without copying it.
pub fn public_payload(payload: &Value) -> impl serde::Serialize + '_ {
    struct Public<'a>(&'a Value);
    impl serde::Serialize for Public<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
            match self.0.as_object() {
                Some(object) => serializer.collect_map(public_fields(object)),
                None => self.0.serialize(serializer),
            }
        }
    }
    Public(payload)
}

type Fields<'a> = BTreeMap<&'a str, &'a Value>;

fn take<'a>(object: &mut Fields<'a>, keys: &[&str]) -> Fields<'a> {
    keys.iter().filter_map(|key| object.remove_entry(*key)).collect()
}

/// The full prompt: stable shared prefixes first, purpose and per-request
/// data next, the volatile runtime segment last. Every segment is present.
/// `purpose` is the rendered [`purpose_segment`].
pub fn build_prompt(policies: &Policies, kind: &str, payload: &Value, purpose: &str, runtime: &str) -> String {
    let empty = serde_json::Map::new();
    let mut messages: Fields =
        public_fields(payload.as_object().unwrap_or(&empty)).map(|(k, v)| (k.as_str(), v)).collect();
    let mut catalogs = take(&mut messages, &["available_projects", "tag_catalog", "priority_kind"]);
    if let Some(tools) = payload.get("_registered_tools").filter(|t| t.is_array()) {
        catalogs.insert("available_tools", tools);
    }
    let tasks = take(
        &mut messages,
        &["tasks", "task_index", "manual_history", "report_tasks", "current_tasks", "task", "fields", "current_tags"],
    );
    let json_text = |value: &Fields| {
        if value.is_empty() {
            String::new()
        } else {
            serde_json::to_string(value).unwrap_or_default()
        }
    };
    let tool_call = if ["classify", "format", "edit", "archive", "tool_call"].contains(&kind) {
        format!("{}\n\n{}", policies.tool_call, prompts::default("tool_call_protocol"))
    } else {
        String::new()
    };
    [
        segment("固定协议", prompts::default("policy_boundary")),
        segment("工具调用", tool_call),
        segment("已归档记忆", payload["_archived_memory"].as_str().unwrap_or("").trim()),
        segment("当前用途", purpose),
        segment("项目、标签与工具目录", json_text(&catalogs)),
        segment("事项摘要", json_text(&tasks)),
        segment("临时记忆", payload["_temporary_memory"].as_str().unwrap_or("").trim()),
        segment("增量消息", json_text(&messages)),
        segment("运行上下文", runtime),
    ]
    .join("\n\n")
}

/// One request serialized once for measurement, diagnostics and sending.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub prompt: String,
    pub schema: Value,
    /// MCP tool declarations `{"tools":[...]}` offered to the model.
    pub tools: Option<Value>,
    /// `tools` parsed once into provider definitions.
    pub definitions: Vec<shishi_api::ToolDefinition>,
    pub prompt_chars: usize,
    pub schema_chars: usize,
    pub tool_chars: usize,
    pub wire_chars: usize,
    pub total: usize,
}

/// Render and measure a request. Tools are the retrieval tools when the
/// payload has `_local_context`, plus `_registered_tools` declarations
/// (`{name, description, parameters}`). Too many tools or an invalid
/// declaration is an error.
pub fn prepare(policies: &Policies, route: &Route, payload: &Value, runtime: &Value) -> Result<Prepared> {
    let kind = route.kind.as_str();
    let driver = route.connection.driver.as_str();
    let purpose = purpose_segment(policies, kind, payload, driver);
    let runtime = render_runtime(runtime);
    let prompt = build_prompt(policies, kind, payload, &purpose, &runtime);
    let prompt_chars = prompt.chars().count();
    let schema = schema(kind);
    // API carries the contract in both response_format and the message.
    let schema_chars = json_chars(&schema) * if driver == "api" { 2 } else { 1 };
    let mut tools = payload["_local_context"].is_object().then(context_tools);
    if let Some(registered) = payload["_registered_tools"].as_array() {
        let list = tools.get_or_insert_with(|| json!({"tools": []}));
        for t in registered {
            list["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": t["name"], "description": t["description"], "inputSchema": t["parameters"]}));
        }
    }
    let tool_chars = tools.as_ref().map(json_chars).unwrap_or(0);
    let base = prompt_chars + schema_chars + tool_chars;
    let effort = (!route.effort.is_empty()).then_some(route.effort.as_str());
    if tools.as_ref().and_then(|t| t["tools"].as_array()).is_some_and(|t| t.len() > shishi_api::openai::MAX_TOOLS) {
        crate::bail!("本次可用工具过多，请关闭部分工具后重试");
    }
    let definitions = match &tools {
        Some(t) => shishi_api::tool_definitions_from_mcp(t).map_err(|e| Error(format!("工具声明无效：{e}")))?,
        None => Vec::new(),
    };
    let body = || shishi_api::openai::initial_request_body(&route.model, effort, &prompt, &schema, &definitions);
    let wire_chars = match driver {
        "api" | "codex" => json_chars(&body()),
        "dsh" => super::native::body_input_chars(&schema, &body()),
        _ => 0,
    }
    .saturating_sub(base);
    Ok(Prepared {
        prompt,
        schema,
        tools,
        definitions,
        prompt_chars,
        schema_chars,
        tool_chars,
        wire_chars,
        total: base + wire_chars,
    })
}

/// `input_sizes` as logged and shown.
pub fn input_sizes(config: &Config, prepared: &Prepared) -> Value {
    json!({"prompt_chars": prepared.prompt_chars, "schema_chars": prepared.schema_chars,
           "tool_schema_chars": prepared.tool_chars, "wire_overhead_chars": prepared.wire_chars,
           "total_input_chars": prepared.total, "max_input_chars": config.max_input_chars})
}

/// Characters left for messages after the fixed prompt, schema and tool
/// overhead of `kind`; `None` when the purpose has no route, so there is
/// nothing to measure. Retrieval pages have their own budget.
pub fn message_budget(config: &Config, kind: &str, policies: &Policies) -> Result<Option<usize>> {
    if !configured(config, kind) {
        return Ok(None);
    }
    let route = route(config, kind)?;
    let empty = json!({"changes": [], "context": [], "_local_context": {}});
    let runtime = runtime_context(config, kind, "", &empty, time::now());
    let fixed = prepare(policies, &route, &empty, &runtime)?.total;
    match config.max_input_chars.checked_sub(fixed) {
        Some(n) if n > 0 => Ok(Some(n)),
        _ => crate::bail!("总输入上限不足以容纳规则、输出格式和上下文，请提高上限"),
    }
}

/// The single final check before reservation: the whole input against
/// `max_input_chars` (tools were checked and parsed by [`prepare`]).
pub fn validate_prepared(config: &Config, payload: &Value, prepared: &Prepared) -> Result<()> {
    if prepared.total > config.max_input_chars {
        let mut sections: Vec<(&String, usize)> =
            payload.as_object().into_iter().flat_map(public_fields).map(|(k, v)| (k, json_chars(v))).collect();
        sections.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let largest = sections.iter().take(4).map(|(k, n)| format!("{k} {n} 字符")).collect::<Vec<_>>().join("、");
        crate::bail!(
            "{INPUT_LIMIT_ERROR}：{} 字符，超过有效上限 {}（含提示词、输出格式和工具定义）。主要数据：{}。本次未调用 AI，原数据保留。请精简规则或数据。字符数不等于 tokens。",
            prepared.total,
            config.max_input_chars,
            largest
        );
    }
    Ok(())
}

/// The shared working directory of analysis processes.
pub fn analysis_workdir(root: &Path, config: &Config) -> Result<PathBuf> {
    let name = config.analysis_workdir.trim();
    let path = Path::new(if name.is_empty() { "ShiShi" } else { name });
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path.components().any(|p| !matches!(p, std::path::Component::Normal(_))) {
        crate::bail!("分析工作目录应填写普通目录名或完整绝对路径，不能使用 ..");
    }
    Ok(root.join("ai-work").join(path))
}
