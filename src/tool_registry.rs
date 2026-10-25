//! Locally registered tools with a global enable switch. No model writes here.
use crate::{engine::App, model::*};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

pub const COMMAND_TIMEOUT_SECONDS: u64 = 60;
pub const CALLS_PER_MINUTE: usize = 60;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    pub description: String,
    pub default: Option<Value>,
    #[serde(rename = "enum")]
    pub enumeration: Vec<Value>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}
impl Default for Parameter {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: "string".into(),
            required: false,
            description: String::new(),
            default: None,
            enumeration: vec![],
            minimum: None,
            maximum: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ToolDefinition {
    #[serde(default)]
    pub definition_version: u32,
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub parameters: Vec<Parameter>,
    pub command_template: String,
    pub working_directory: String,
    pub timeout_seconds: u64,
    pub rate_limit_per_minute: usize,
    /// Old execution fields remain readable for compatibility and run history.
    pub output_encoding: String,
    pub enabled: bool,
    pub updated: i64,
}
impl Default for ToolDefinition {
    fn default() -> Self {
        Self {
            definition_version: 2,
            id: String::new(),
            name: String::new(),
            display_name: String::new(),
            description: String::new(),
            parameters: vec![],
            command_template: String::new(),
            working_directory: String::new(),
            timeout_seconds: 60,
            rate_limit_per_minute: 60,
            output_encoding: "utf-8".into(),
            enabled: false,
            updated: 0,
        }
    }
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
impl ToolDefinition {
    pub fn compatibility_issue(&self) -> Option<&'static str> {
        if self.definition_version > 2 {
            Some("工具定义版本较新，请更新软件")
        } else if self.parameters.iter().any(|p| {
            p.kind != "string"
                || p.required
                || p.default.is_some()
                || !p.enumeration.is_empty()
                || p.minimum.is_some()
                || p.maximum.is_some()
        }) {
            Some("旧参数包含类型或约束，请检查后重新保存为文本参数")
        } else if !self.working_directory.is_empty()
            || self.timeout_seconds != 60
            || self.rate_limit_per_minute != 60
            || !self.output_encoding.eq_ignore_ascii_case("utf-8")
        {
            Some("旧执行配置需要调整，请将目录要求写入包装命令并确认 UTF-8 输出后重新保存")
        } else {
            None
        }
    }
    pub fn simplify(&mut self) {
        self.definition_version = 2;
        self.display_name.clear();
        self.working_directory.clear();
        self.timeout_seconds = 60;
        self.rate_limit_per_minute = 60;
        self.output_encoding = "utf-8".into();
        for p in &mut self.parameters {
            p.kind = "string".into();
            p.required = false;
            p.default = None;
            p.enumeration.clear();
            p.minimum = None;
            p.maximum = None;
        }
    }
    pub fn schema(&self) -> Value {
        let mut properties = Map::new();
        for p in &self.parameters {
            properties.insert(
                p.name.clone(),
                json!({"type":"string","description":p.description}),
            );
        }
        json!({"type":"object","properties":properties,"required":[],"additionalProperties":false})
    }
    pub fn declaration(&self) -> Value {
        json!({"name":self.name,"description":self.description,"parameters":self.schema()})
    }
    pub fn validate(&self, complete: bool) -> Result<(), String> {
        if self.id.len() > 100
            || self.name.len() > 64
            || self.display_name.chars().count() > 100
            || self.description.chars().count() > 1024
            || self.command_template.len() > 16_000
            || self.working_directory.len() > 4096
            || self.parameters.len() > 40
        {
            return Err("工具配置超过长度限制".into());
        }
        if !self.name.is_empty()
            && (!valid_name(&self.name)
                || !self
                    .name
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_'))
        {
            return Err("工具名应以字母或下划线开头，使用1–64位字母、数字、下划线或连字符".into());
        }
        let mut names = BTreeSet::new();
        for p in &self.parameters {
            if (complete || !p.name.is_empty()) && !valid_name(&p.name)
                || !p.name.is_empty() && !names.insert(&p.name)
                || p.description.chars().count() > 1024
            {
                return Err("参数名无效、重复或说明过长".into());
            }
        }
        if complete {
            if ["read_context", "read_image", "submit_analysis"].contains(&self.name.as_str()) {
                return Err("工具名与内置工具重复，请更换名称".into());
            }
            if let Some(issue) = self.compatibility_issue() {
                return Err(issue.into());
            }
            if self.name.is_empty()
                || self.description.trim().is_empty()
                || self.command_template.trim().is_empty()
            {
                return Err("启用/测试前请填写工具名、说明和 CMD 命令模板".into());
            }
            crate::tool_runner::validate_template(self)?;
        }
        Ok(())
    }
    pub fn arguments(&self, arguments: &Value) -> Result<Map<String, Value>, String> {
        let args = arguments.as_object().ok_or("参数必须是 JSON 对象")?;
        if arguments.to_string().len() > 64 * 1024 {
            return Err("参数过长".into());
        }
        if args
            .keys()
            .any(|k| !self.parameters.iter().any(|p| &p.name == k))
        {
            return Err("包含未定义参数".into());
        }
        let mut result = Map::new();
        for p in &self.parameters {
            let value = match args.get(&p.name) {
                Some(Value::String(value)) => value.clone(),
                Some(_) => return Err(format!("参数 {} 必须是字符串", p.name)),
                None => String::new(),
            };
            result.insert(p.name.clone(), Value::String(value));
        }
        Ok(result)
    }
}
pub fn effective(state: &State) -> Vec<ToolDefinition> {
    state
        .config
        .tools
        .iter()
        .filter(|tool| tool.enabled && tool.compatibility_issue().is_none())
        .cloned()
        .collect()
}
impl App {
    pub fn tool_making_prompt(&self, p: &Value) -> Result<Value, String> {
        let tool: ToolDefinition =
            serde_json::from_value(p["tool"].clone()).map_err(|e| e.to_string())?;
        let pending = |s: &str| {
            if s.trim().is_empty() {
                "待填写".to_string()
            } else {
                s.to_string()
            }
        };
        let fields = std::collections::BTreeMap::from([
            ("tool_name", pending(&tool.name)),
            ("description", pending(&tool.description)),
            (
                "parameters_json",
                if tool.parameters.is_empty() {
                    "待填写（可明确为无参数）".into()
                } else {
                    serde_json::to_string_pretty(
                        &tool
                            .parameters
                            .iter()
                            .map(|p| json!({"name":p.name,"description":p.description}))
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|e| e.to_string())?
                },
            ),
            (
                "command_template_or_pending",
                pending(&tool.command_template),
            ),
        ]);
        let mut template = crate::prompt_config::get("tool_maker");
        let mut text = String::new();
        while let Some(start) = template.find("{{") {
            text.push_str(&template[..start]);
            let rest = &template[start + 2..];
            let Some(end) = rest.find("}}") else {
                text.push_str(&template[start..]);
                template = "";
                break;
            };
            if let Some(value) = fields.get(&rest[..end]) {
                text.push_str(value);
            } else {
                text.push_str(&template[start..start + 2 + end + 2]);
            }
            template = &rest[end + 2..];
        }
        text.push_str(template);
        Ok(json!({"text":text}))
    }
    pub fn tool_list(&self) -> Value {
        json!(self
            .state
            .lock()
            .unwrap()
            .config
            .tools
            .iter()
            .map(|tool| {
                let mut value = json!(tool);
                value["compatibility_issue"] = json!(tool.compatibility_issue());
                value
            })
            .collect::<Vec<_>>())
    }
    pub fn tool_save(&self, p: &Value) -> Result<Value, String> {
        let mut tool: ToolDefinition =
            serde_json::from_value(p["tool"].clone()).map_err(|e| e.to_string())?;
        // Saving from the v2 editor explicitly confirms its displayed conversion.
        if p["tool"]["definition_version"] == 2 {
            tool.simplify();
        }
        tool.validate(tool.enabled)?;
        self.commit_state(|state| {
            if state
                .config
                .tools
                .iter()
                .any(|t| t.id != tool.id && !tool.name.is_empty() && t.name == tool.name)
            {
                return Err("工具名已存在".into());
            }
            if tool.id.is_empty() {
                if state.config.tools.len() >= 200 {
                    return Err("工具库最多200项".into());
                }
                tool.id = id();
                tool.updated = now();
                state.config.tools.push(tool.clone());
            } else {
                let old = state
                    .config
                    .tools
                    .iter_mut()
                    .find(|t| t.id == tool.id)
                    .ok_or("工具已删除")?;
                if old.updated != tool.updated {
                    return Err("工具已修改，请重新打开编辑".into());
                }
                tool.updated = now().max(old.updated + 1);
                *old = tool.clone();
            }
            Ok(json!(tool))
        })
    }
    pub fn tool_toggle(&self, p: &Value) -> Result<Value, String> {
        self.commit_state(|state| {
            let tool = state
                .config
                .tools
                .iter_mut()
                .find(|t| Some(t.id.as_str()) == p["id"].as_str())
                .ok_or("工具已删除")?;
            if Some(tool.updated) != p["updated"].as_i64() {
                return Err("工具已修改，请刷新".into());
            }
            let enabled = p["enabled"].as_bool().ok_or("缺少开关状态")?;
            tool.validate(enabled)?;
            tool.enabled = enabled;
            tool.updated = now().max(tool.updated + 1);
            Ok(json!(tool))
        })
    }
    pub fn tool_delete(&self, p: &Value) -> Result<Value, String> {
        self.commit_state(|state| {
            let i = state
                .config
                .tools
                .iter()
                .position(|t| Some(t.id.as_str()) == p["id"].as_str())
                .ok_or("工具已删除")?;
            if Some(state.config.tools[i].updated) != p["updated"].as_i64() {
                return Err("工具已修改，请刷新".into());
            }
            state.config.tools.remove(i);
            // Bindings intentionally survive deletion, so unavailable entries remain visible.
            Ok(json!(true))
        })
    }
}
