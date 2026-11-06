//! The tool library: definitions, validation and the making prompt.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{Error, Result};

pub const CALLS_PER_MINUTE: usize = 60;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Parameter {
    pub name: String,
    pub description: String,
}

/// One command tool. Fields the 1.x editor still sends are ignored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Vec<Parameter>,
    pub command_template: String,
    pub enabled: bool,
    pub updated: i64,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl Tool {
    pub fn schema(&self) -> Value {
        let mut properties = Map::new();
        for p in &self.parameters {
            properties.insert(p.name.clone(), json!({"type": "string", "description": p.description}));
        }
        json!({"type": "object", "properties": properties, "required": [], "additionalProperties": false})
    }

    /// The function declaration offered to a model.
    pub fn declaration(&self) -> Value {
        json!({"name": self.name, "description": self.description, "parameters": self.schema()})
    }

    /// `complete` is required before a tool can be enabled or tested.
    pub fn validate(&self, complete: bool) -> Result<()> {
        if self.id.len() > 100
            || self.name.len() > 64
            || self.description.chars().count() > 1024
            || self.command_template.len() > 16_000
            || self.parameters.len() > 40
        {
            crate::bail!("工具配置超过长度限制");
        }
        if !self.name.is_empty()
            && (!valid_name(&self.name)
                || !self.name.as_bytes().first().is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_'))
        {
            crate::bail!("工具名应以字母或下划线开头，使用1–64位字母、数字、下划线或连字符");
        }
        let mut names = BTreeSet::new();
        for p in &self.parameters {
            if (complete || !p.name.is_empty()) && !valid_name(&p.name)
                || !p.name.is_empty() && !names.insert(&p.name)
                || p.description.chars().count() > 1024
            {
                crate::bail!("参数名无效、重复或说明过长");
            }
        }
        if complete {
            if ["read_context", "read_image", "submit_analysis"].contains(&self.name.as_str()) {
                crate::bail!("工具名与内置工具重复，请更换名称");
            }
            if self.name.is_empty() || self.description.trim().is_empty() || self.command_template.trim().is_empty() {
                crate::bail!("启用/测试前请填写工具名、说明和 CMD 命令模板");
            }
            super::runner::parts(self)?;
        }
        Ok(())
    }

    /// Canonical arguments: every declared parameter as a string, missing
    /// ones empty.
    pub fn arguments(&self, arguments: &Value) -> Result<Map<String, Value>> {
        let args = arguments.as_object().ok_or_else(|| Error::new("参数必须是 JSON 对象"))?;
        if !shishi_api::json::fits_bytes(arguments, 64 * 1024) {
            crate::bail!("参数过长");
        }
        if args.keys().any(|k| !self.parameters.iter().any(|p| &p.name == k)) {
            crate::bail!("包含未定义参数");
        }
        let mut result = Map::new();
        for p in &self.parameters {
            let value = match args.get(&p.name) {
                Some(Value::String(value)) => value.clone(),
                Some(_) => crate::bail!("参数 {} 必须是字符串", p.name),
                None => String::new(),
            };
            result.insert(p.name.clone(), Value::String(value));
        }
        Ok(result)
    }

    /// Whether `other` would run a different command.
    pub fn same_command(&self, other: &Tool) -> bool {
        self.name == other.name
            && self.command_template == other.command_template
            && self.parameters.iter().map(|p| &p.name).eq(other.parameters.iter().map(|p| &p.name))
    }
}

pub fn making_prompt(p: &Value) -> Result<Value> {
    let tool: Tool = serde_json::from_value(p.get("tool").cloned().unwrap_or_default())?;
    let pending = |s: &str| if s.trim().is_empty() { "待填写".to_string() } else { s.to_string() };
    let parameters = if tool.parameters.is_empty() {
        "待填写（可明确为无参数）".into()
    } else {
        serde_json::to_string_pretty(&tool.parameters)?
    };
    let fields = BTreeMap::from([
        ("tool_name", pending(&tool.name)),
        ("description", pending(&tool.description)),
        ("parameters_json", parameters),
        ("command_template_or_pending", pending(&tool.command_template)),
    ]);
    let mut template = crate::prompts::default("tool_maker");
    let mut text = String::new();
    while let Some(start) = template.find("{{") {
        text.push_str(&template[..start]);
        let rest = &template[start + 2..];
        let Some(end) = rest.find("}}") else {
            text.push_str(&template[start..]);
            template = "";
            break;
        };
        match fields.get(&rest[..end]) {
            Some(value) => text.push_str(value),
            None => text.push_str(&template[start..start + end + 4]),
        }
        template = &rest[end + 2..];
    }
    text.push_str(template);
    Ok(json!({"text": text}))
}
