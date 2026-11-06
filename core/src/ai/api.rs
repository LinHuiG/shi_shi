//! OpenAI-compatible HTTP driver: the shared SS tool loop over Chat Completions. The caller resolves the key ([`key`])
//! from the connection's environment variable or the stored credentials and
//! hands it in with the request; the driver never reads storage.

use std::time::Duration;

use serde_json::{json, Value};
use shishi_api::{Client, ModelInfo, Wire};

use super::credentials::{self, CredentialSource, CredentialStatus, ResolvedCredential};
use super::provider::{self, Context, Driver, Event, Job, Reply};
use crate::config::Connection as AiConnection;
use crate::Db;

pub struct Api;

/// The store reference of a connection's key.
pub fn reference(c: &AiConnection) -> &str {
    c.credential_id.as_deref().unwrap_or(&c.id)
}

/// The key of `c` from exactly one source: its environment variable when
/// set (never falling back), otherwise the stored entry.
pub async fn key(db: &Db, c: &AiConnection) -> Result<ResolvedCredential, String> {
    let resolved = match c.key_env.as_deref() {
        Some(name) => credentials::environment(name).map_err(|e| e.to_string())?,
        None => db.credential(reference(c).into()).await.map_err(|e| e.0)?,
    };
    resolved.ok_or_else(|| "未配置此连接的 API Key".into())
}

/// `ai_connection_status` of an API connection; never reveals the key.
pub async fn status(db: &Db, c: &AiConnection) -> Result<Value, String> {
    let status = match c.key_env.as_deref() {
        Some(name) => {
            let found = credentials::environment(name).map_err(|e| e.to_string())?.is_some();
            CredentialStatus { configured: found, source: Some(CredentialSource::Environment), updated_unix: None }
        }
        None => db.credential_status(reference(c).into()).await.map_err(|e| e.0)?,
    };
    Ok(json!({"configured": status.configured,
        "status": if status.configured { "API Key 已配置" } else { "未配置 API Key" },
        "source": format!("{:?}", status.source)}))
}

impl Driver for Api {
    fn analyze(&self, job: &Job, context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply {
        if job.kind == "handling" {
            return Err("API 不支持处置会话".into());
        }
        let c = &job.route.connection;
        let client = Client::new(&c.base_url, Wire::Chat).map_err(|e| e.to_string())?;
        let secret = job.key.as_ref().ok_or("未配置此连接的 API Key")?.expose_secret();
        provider::tool_loop(job, &job.route.model, context, event, secret, false, |r, e| {
            client.analyze_with_events(r, c.stream, e)
        })
    }
}

/// An API route needs a valid address and a model.
pub fn validate(c: &AiConnection, model: &str) -> Result<(), String> {
    shishi_api::Endpoint::parse(&c.base_url)?;
    if model.trim().is_empty() {
        return Err("请选择或填写 API 模型".into());
    }
    Ok(())
}

/// The model catalog of `c` with `secret` (a draft key is used only for
/// discovery; never written or logged).
pub fn models_with_key(c: &AiConnection, secret: &str) -> Result<Value, String> {
    let client = Client::new(&c.base_url, Wire::Chat).map_err(|e| e.to_string())?;
    Ok(catalog_json(&client.discover_models(secret, Duration::from_secs(15)).map_err(|e| e.to_string())?))
}

/// Discovered models as the UI lists them.
pub fn catalog_json(rows: &[ModelInfo]) -> Value {
    let models = rows
        .iter()
        .map(|m| {
            let efforts = m.supported_reasoning_efforts.clone().unwrap_or_else(|| {
                ["none", "minimal", "low", "medium", "high", "xhigh"].into_iter().map(str::to_string).collect()
            });
            json!({"model": m.id, "displayName": m.display_name.as_deref().unwrap_or(&m.id),
                "reasoningEffortSupport": if m.supported_reasoning_efforts.is_some() { "declared" } else { "unverified" },
                "supportedReasoningEfforts": efforts.into_iter().map(|e| json!({"reasoningEffort": e})).collect::<Vec<_>>()})
        })
        .collect::<Vec<_>>();
    json!({"models": models})
}

#[cfg(test)]
mod tests;
