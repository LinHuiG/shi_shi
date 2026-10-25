//! Application-facing AI contracts. Provider wire protocols live in adapters.
use crate::model::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

pub mod agent_approval;
pub mod api;
pub mod codex;
pub mod dsh;
pub mod model_api;

/// No credentials belong in this serializable descriptor. Future API adapters
/// must use a separate credential store and retain only credential references.
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
        Self {
            id: String::new(),
            name: "Codex".into(),
            driver: "codex".into(),
            executable: String::new(),
            base_url: String::new(),
            key_env: None,
            credential_id: None,
            models: vec![],
            stream: true,
        }
    }
}
impl Connection {
    pub fn cache_key(&self) -> String {
        // Names can change without disconnecting an active conversation.
        serde_json::to_string(&(
            &self.id,
            &self.driver,
            &self.executable,
            &self.base_url,
            &self.key_env,
            &self.credential_id,
        ))
        .unwrap()
    }
}

pub fn connection(config: &Config) -> Result<Connection, String> {
    if config.selected_connection == crate::model::UNCONFIGURED_CONNECTION {
        return Err("AI_UNCONFIGURED:未配置此用途的 AI 接入或模型".into());
    }
    if config.selected_connection.is_empty() {
        return Ok(default_codex_connection(config));
    }
    config
        .ai_connections
        .iter()
        .find(|c| c.id == config.selected_connection)
        .cloned()
        .ok_or_else(|| "所选 AI 连接不存在，请重新选择接入".into())
}

pub fn default_codex_connection(config: &Config) -> Connection {
    Connection {
        id: String::new(),
        name: config.codex_name.clone(),
        executable: config.codex.clone(),
        models: config.codex_models.clone(),
        ..Connection::default()
    }
}

pub fn handling_available(config: &Config) -> bool {
    !config.codex.trim().is_empty()
        || config.ai_connections.iter().any(|c| {
            matches!(c.driver.as_str(), "codex" | "dsh") && !c.executable.trim().is_empty()
        })
}

/// Public request identity only: never include an endpoint, path or credential.
pub fn source_info(config: &Config) -> Value {
    let Ok(c) = connection(config) else {
        return Value::Null;
    };
    connection_source(&c, &config.model)
}
pub fn connection_source(c: &Connection, model_id: &str) -> Value {
    let item = c.models.iter().find(|m| m["model"] == model_id);
    let route = serde_json::from_str::<Vec<String>>(model_id)
        .ok()
        .filter(|r| r.len() == 2);
    let provider = item
        .and_then(|m| m["providerName"].as_str().filter(|s| !s.trim().is_empty()))
        .or_else(|| item.and_then(|m| m["provider"].as_str().filter(|s| !s.trim().is_empty())))
        .or_else(|| route.as_ref().map(|r| r[0].as_str()))
        .unwrap_or("");
    let model = item
        .and_then(|m| m["displayName"].as_str().filter(|s| !s.trim().is_empty()))
        .or_else(|| route.as_ref().map(|r| r[1].as_str()))
        .unwrap_or(model_id);
    let source = if c.name.trim().is_empty() {
        match c.driver.as_str() {
            "dsh" => "DeepSeek Harness",
            "api" => "API",
            _ => "Codex",
        }
    } else {
        &c.name
    };
    let label = if provider.is_empty() {
        source.to_owned()
    } else {
        format!("{source} / {provider}")
    };
    serde_json::json!({"driver":c.driver,"connection_id":c.id,"connection_name":source,"provider":provider,"model_name":model,"label":label})
}

pub fn validate_connections(config: &Config) -> Result<(), String> {
    if !(30..=1800).contains(&config.ai_timeout_seconds) {
        return Err("AI 总时限须为 30—1800 秒".into());
    }
    if config.ai_connections.len() > 20 {
        return Err("最多配置 20 个附加 AI 连接".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for c in &config.ai_connections {
        if c.id.is_empty() || c.id.len() > 100 || !ids.insert(&c.id) {
            return Err("AI 连接编号为空或重复".into());
        }
        if c.name.trim().is_empty()
            || c.name.chars().count() > 80
            || (c.driver != "api" && c.executable.trim().is_empty())
        {
            return Err("请填写 AI 连接名称和程序路径（名称最多 80 字）".into());
        }
        backend(c)?;
        if c.driver == "api" {
            shishi_api::Endpoint::parse(&c.base_url).map_err(|e| e.to_string())?;
        }
    }
    for kind in crate::model::PURPOSES {
        if config.for_kind(kind).selected_connection == crate::model::UNCONFIGURED_CONNECTION {
            continue;
        }
        if kind == "handling" && !handling_available(config) {
            continue;
        }
        let c = connection(&config.for_kind(kind))?;
        backend(&c)?.validate(&c, &config.for_kind(kind))?;
        if kind == "handling" && c.driver == "api" {
            return Err("官方 API 仅用于分析，不能用于一键处置".into());
        }
    }
    Ok(())
}

pub struct AnalysisRequest<'a> {
    /// Synthetic regression must not navigate the user's desktop.
    pub show_session: bool,
    pub config: &'a Config,
    pub kind: &'a str,
    pub payload: &'a Value,
    pub root: &'a Path,
    pub trace: &'a Path,
    pub prompt: &'a str,
    pub schema: &'a Value,
    /// Exact MCP tool declarations measured for this request. Adapters must use
    /// this value rather than deriving a second, potentially different set.
    pub tools: Option<&'a Value>,
    pub(crate) registered_tools: Option<&'a crate::tool_calling::ToolSession<'a>>,
    pub cancel: &'a AtomicBool,
}
pub const NOT_SUBMITTED: &str = "处置未提交：";

#[derive(Debug, PartialEq)]
pub enum AnalysisEvent {
    SessionStarted(String),
    Usage(Value),
    /// Live cumulative output only; never used for persisted billing totals.
    OutputTokens(Option<u64>),
    StepFinished {
        output_tokens: Option<u64>,
        decode_ms: Option<u64>,
    },
    CacheDiagnostics(Value),
    Stream {
        kind: String,
        text: String,
        append: bool,
    },
    VisibilityIssue {
        thread_id: String,
        error: String,
    },
}

/// Result is (business JSON, usage with snake_case token counters). An absent
/// counter is unknown, never zero. Events contain no provider protocol fields.
pub trait Backend: Sync {
    fn analyze(
        &self,
        connection: &Connection,
        request: AnalysisRequest<'_>,
        event: &mut dyn FnMut(AnalysisEvent),
    ) -> Result<(Value, Value), String>;
    fn models(&self, connection: &Connection, root: &Path) -> Result<Value, String>;
    fn validate(&self, connection: &Connection, config: &Config) -> Result<(), String>;
    fn status(&self, connection: &Connection, root: &Path) -> Result<Value, String>;
    fn agent(&self, connection: &Connection) -> Result<Arc<dyn AgentSession>, String>;
    fn open_session(&self, thread: &str) -> Result<(), String>;
}

pub fn backend(connection: &Connection) -> Result<&'static dyn Backend, String> {
    match connection.driver.as_str() {
        "codex" => Ok(&codex::Codex),
        "api" => Ok(&api::Api),
        "dsh" => Ok(&dsh::Dsh),
        _ => Err(format!(
            "尚未支持的 AI 接入：{}，未发起调用",
            connection.driver
        )),
    }
}

/// Session operations, independent of RPC method names. Value results follow
/// ShiShi's existing history/approval DTOs for persistence and UI compatibility;
/// new adapters must translate their native events and history to those DTOs.
/// Methods that submit work must never automatically retry ambiguous failures.
pub trait AgentSession: Send + Sync {
    fn close(&self) -> Result<(), String>;
    fn is_alive(&self) -> bool;
    fn start(
        &self,
        cwd: &str,
        project: &str,
        context: &ContextTool,
        config: &Config,
    ) -> Result<Value, String>;
    fn rename(&self, thread: &str, name: &str) -> Result<(), String>;
    fn submit(
        &self,
        thread: &str,
        text: &str,
        config: &Config,
        message_id: &str,
    ) -> Result<Value, String>;
    fn steer(&self, thread: &str, turn: &str, text: &str, message_id: &str) -> Result<(), String>;
    fn read(&self, thread: &str) -> Result<Value, String>;
    fn history(&self, thread: &str) -> Result<Value, String>;
    fn resume(&self, thread: &str, context: &ContextTool) -> Result<Value, String>;
    fn unsubscribe(&self, thread: &str) -> Result<(), String>;
    fn interrupt(&self, thread: &str, turn: &str) -> Result<(), String>;
    fn events(&self) -> Vec<UsageEvent>;
    fn requests(&self, thread: &str) -> Vec<Value>;
    fn respond(
        &self,
        thread: &str,
        key: &str,
        request: &Value,
        answer: &Value,
    ) -> Result<(), String>;
}

pub struct ContextTool {
    pub cwd: std::path::PathBuf,
    pub executable: std::path::PathBuf,
    pub context_path: std::path::PathBuf,
}

pub struct UsageEvent {
    pub notification: Value,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub usage: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn source_identity_preserves_inner_provider_and_never_exposes_endpoint() {
        let mut config = Config::default();
        config.selected_connection = "example".into();
        config.model = r#"["provider-b","shared-model"]"#.into();
        config.ai_connections.push(Connection {
            id: "example".into(),
            name: "示例 Harness".into(),
            driver: "dsh".into(),
            base_url: "https://private.example.invalid".into(),
            models: vec![
                json!({"model":config.model,"providerName":"供应商乙","displayName":"共享模型"}),
            ],
            ..Connection::default()
        });
        let captured = source_info(&config);
        config.ai_connections[0].name = "改名后".into();
        assert_eq!(captured["label"], "示例 Harness / 供应商乙");
        assert_eq!(captured["model_name"], "共享模型");
        assert!(!captured.to_string().contains("private.example"));
        assert_ne!(captured, source_info(&config));
    }

    #[test]
    fn new_configs_use_factory_profiles_and_default_connection() {
        let config: Config = serde_json::from_value(
            json!({"codex":"original.exe","model":"old-model","reasoning_effort":"high"}),
        )
        .unwrap();
        for kind in ["classify", "format", "handling", "archive", "report"] {
            let resolved = config.for_kind(kind);
            assert_eq!(resolved.model, "gpt-6-astra");
            assert_eq!(connection(&resolved).unwrap().executable, "original.exe");
        }
        let serialized = serde_json::to_value(&config).unwrap();
        assert!(serialized.get("selected_connection").is_none());
        let restored: Config = serde_json::from_value(serialized).unwrap();
        assert_eq!(connection(&restored).unwrap(), connection(&config).unwrap());
    }

    #[test]
    fn api_only_configuration_routes_all_analysis_purposes_without_enabling_handling() {
        let mut config: Config = serde_json::from_value(json!({
            "ai_connections":[{"id":"api","name":"Synthetic API","driver":"api","base_url":"https://example.com/v1","models":[{"model":"synthetic"}]}],
            "model_profiles":{
                "filter":{"connection_id":"api","model":"synthetic"},
                "analysis":{"connection_id":"api","model":"synthetic"},
                "review":{"connection_id":"api","model":"synthetic"},
                "tool_call":{"connection_id":"api","model":"synthetic"},
                "handling":{"connection_id":"api","model":"synthetic"}
            }
        })).unwrap();
        assert!(!handling_available(&config));
        validate_connections(&config).unwrap();
        for purpose in crate::model::PURPOSES
            .into_iter()
            .filter(|p| *p != "handling")
        {
            assert_eq!(connection(&config.for_kind(purpose)).unwrap().driver, "api");
        }
        let root = std::env::temp_dir().join(format!("shishi-api-only-{}", crate::model::id()));
        let app = crate::engine::App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config = config.clone();
        assert!(app
            .handling_start(&json!({"task_id":"synthetic"}))
            .unwrap_err()
            .contains("不能执行一键处置"));
        assert_eq!(app.state.lock().unwrap().calls, 0);
        assert!(app.state.lock().unwrap().handling.runs.is_empty());
        // Inactive handling profiles are retained; re-enabling agents validates them again.
        config.codex = "synthetic.exe".into();
        assert!(handling_available(&config));
        assert!(validate_connections(&config)
            .unwrap_err()
            .contains("不能用于一键处置"));
        config.codex.clear();
        config.ai_connections.push(Connection {
            id: "dsh".into(),
            driver: "dsh".into(),
            executable: "synthetic.cmd".into(),
            ..Default::default()
        });
        assert!(handling_available(&config));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn routes_are_independent_and_invalid_routes_never_fall_back() {
        let mut config: Config = serde_json::from_value(json!({
            "codex":"original.exe",
            "ai_connections":[{"id":"second","name":"备用","driver":"codex","executable":"second.exe"}],
            "model_profiles":{
                "analysis":{"connection_id":"second","model":"analysis"},
                "handling":{"model":"handling"},
                "review":{"connection_id":"second","model":"review"}
            }
        })).unwrap();
        validate_connections(&config).unwrap();
        for kind in [
            "classify", "edit", "schedule", "archive", "learning", "report",
        ] {
            assert_eq!(
                connection(&config.for_kind(kind)).unwrap().executable,
                "second.exe"
            );
        }
        assert_eq!(
            connection(&config.for_kind("handling")).unwrap().executable,
            "original.exe"
        );
        let second = config.ai_connections[0].clone();
        config.ai_connections.push(second);
        assert!(validate_connections(&config).is_err());
        config.ai_connections.pop();
        config.ai_connections[0].driver = "deepseek-harness".into();
        assert!(validate_connections(&config)
            .unwrap_err()
            .contains("尚未支持"));
        config.ai_connections.clear();
        assert!(connection(&config.for_kind("classify")).is_err());
        assert!(validate_connections(&config).is_err());
    }
}
