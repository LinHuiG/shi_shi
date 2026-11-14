//! The one user-editable configuration file. The settings page and manual
//! edits write the same file through `ConfigStore::save` (temporary file,
//! flush, atomic replace). A file that fails to parse is reported with the
//! offending field and never reset.
//!
//! Prompts and tools are business data with versions and live in SQLite;
//! credentials live in the credential store and only their ids appear here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

#[cfg(test)]
pub(crate) use crate::data::ConfigStore;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub auto_update: bool,
    pub ai_connections: Vec<Connection>,
    /// `None` leaves every purpose unconfigured.
    pub model_profiles: Option<ModelProfiles>,
    pub message_retention_days: u32,
    pub codex: String,
    pub codex_models: Vec<Value>,
    pub codex_name: String,
    pub ai_timeout_seconds: u64,
    pub analysis_workdir: String,
    pub interval_minutes: u64,
    pub analysis_interval_minutes: u64,
    pub analysis_lookback_hours: u64,
    /// Optional relevance gate; off includes all unanalysed incremental messages.
    pub analysis_related_only: bool,
    pub leaders: Vec<String>,
    pub enabled: bool,
    pub auto_ai: bool,
    pub message_filter_enabled: bool,
    pub auto_adopt_advice: bool,
    pub retain_ai_traces: bool,
    pub work_start: String,
    pub work_end: String,
    pub daily_calls: u64,
    pub max_input_chars: usize,
    /// Shared continuation allowance for retrieval, command results and model
    /// messages, separate from the initial request's `max_input_chars`.
    pub retrieval_budget_chars: usize,
    pub model: String,
    pub reasoning_effort: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_update: true,
            ai_connections: Vec::new(),
            model_profiles: Some(ModelProfiles::default()),
            message_retention_days: 90,
            codex: String::new(),
            codex_models: Vec::new(),
            codex_name: "默认 Codex".into(),
            ai_timeout_seconds: 600,
            analysis_workdir: "ShiShi".into(),
            interval_minutes: 3,
            analysis_interval_minutes: 30,
            analysis_lookback_hours: 24,
            analysis_related_only: false,
            leaders: Vec::new(),
            enabled: true,
            auto_ai: true,
            message_filter_enabled: true,
            auto_adopt_advice: false,
            retain_ai_traces: false,
            work_start: "08:00".into(),
            work_end: "20:00".into(),
            daily_calls: 150,
            max_input_chars: 80_000,
            retrieval_budget_chars: 80_000,
            model: "gpt-6-astra".into(),
            reasoning_effort: "low".into(),
        }
    }
}

/// An AI backend. Secrets stay in the credential store; `credential_id` or
/// `key_env` only reference them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub executable: String,
    pub base_url: String,
    pub key_env: Option<String>,
    pub credential_id: Option<String>,
    pub models: Vec<Value>,
    pub stream: bool,
}

impl Default for Connection {
    fn default() -> Self {
        let none = String::new;
        Self {
            id: none(),
            name: "Codex".into(),
            driver: "codex".into(),
            executable: none(),
            base_url: none(),
            key_env: None,
            credential_id: None,
            models: Vec::new(),
            stream: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelChoice {
    pub connection_id: String,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelProfiles {
    pub filter: Option<ModelChoice>,
    pub analysis: Option<ModelChoice>,
    pub review: Option<ModelChoice>,
}

impl Default for ModelProfiles {
    fn default() -> Self {
        let choice = |effort: &str| {
            Some(ModelChoice { model: "gpt-6-astra".into(), reasoning_effort: effort.into(), ..Default::default() })
        };
        Self { filter: choice("low"), analysis: choice("low"), review: choice("high") }
    }
}

/// Every purpose that can reach a provider.
pub const PURPOSES: [&str; 10] = [
    "classify",
    "format",
    "edit",
    "schedule",
    "report",
    "learning",
    "prompt_review",
    "advice_revision",
    "archive",
    "message_filter",
];

pub const UNCONFIGURED_CONNECTION: &str = "__unconfigured_purpose__";

impl Config {
    pub fn automatic_analysis_enabled(&self) -> bool {
        self.enabled && self.auto_ai
    }

    pub fn early_analysis_enabled(&self) -> bool {
        self.automatic_analysis_enabled() && self.message_filter_enabled
    }

    /// Resolve the model route for a purpose. A profile field left empty
    /// keeps the base value; a missing profile means "not configured".
    pub fn model_choice(&self, purpose: &str) -> ModelChoice {
        let connection = self.ai_connections.first().map(|c| c.id.clone()).unwrap_or_default();
        let base = ModelChoice {
            connection_id: connection,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        };
        let Some(p) = &self.model_profiles else { return base };
        let route = match purpose {
            "message_filter" => &p.filter,
            "archive" | "learning" | "prompt_review" | "report" | "advice_revision" => &p.review,
            _ => &p.analysis,
        };
        let Some(choice) = route else {
            return ModelChoice { connection_id: UNCONFIGURED_CONNECTION.into(), ..Default::default() };
        };
        let pick = |own: &str, base: String| if own.is_empty() { base } else { own.to_string() };
        ModelChoice {
            connection_id: pick(&choice.connection_id, base.connection_id),
            model: pick(&choice.model, base.model),
            reasoning_effort: pick(&choice.reasoning_effort, base.reasoning_effort),
        }
    }

    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.ai_connections.iter().find(|c| c.id == id)
    }

    /// Field-level checks the type system cannot express.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [("work_start", &self.work_start), ("work_end", &self.work_end)] {
            if crate::time::parse_hm(value).is_none() {
                crate::bail!("配置字段 {name} 必须是 HH:MM 格式");
            }
        }
        let positive = [
            ("interval_minutes", self.interval_minutes),
            ("analysis_interval_minutes", self.analysis_interval_minutes),
            ("ai_timeout_seconds", self.ai_timeout_seconds),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v == 0) {
            crate::bail!("配置字段 {name} 必须大于 0");
        }
        let mut ids = std::collections::HashSet::new();
        if self.ai_connections.iter().any(|c| c.id.trim().is_empty() || !ids.insert(c.id.as_str())) {
            crate::bail!("配置字段 ai_connections 中的连接标识为空或重复");
        }
        Ok(())
    }
}

/// Parse a config file's text, naming the field that failed.
pub(crate) fn parse(text: &str) -> Result<Config> {
    let config: Config = serde_json::from_str(text).map_err(|e| Error(format!("配置文件无法读取：{e}")))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests;
