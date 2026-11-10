//! System prompts and AI memory. Prompts are versioned in the database;
//! `config/prompts.json` supplies the fixed protocol fragments and the
//! built-in versions, which replace only a built-in version on upgrade. The AI never adopts a prompt by itself: a stored
//! suggestion only changes the active prompt through `prompt_adopt`.

pub mod learning;
pub mod memory;
mod review;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{Ctx, Error, Result};

pub use review::{has_result, retry, Review};

/// Prompts the user edits; every other key is a fixed fragment.
pub const TARGETS: [&str; 8] =
    ["tool_call", "classify", "format", "learning", "report", "handling", "prompt_review", "archive"];

pub const RULE_VERSION: &str = "rule_version";

/// In-memory state of the prompts domain.
#[derive(Default)]
pub struct Runtime {
    /// A prompt suggestion is being regenerated.
    pub(crate) busy: AtomicBool,
}

/// Factory text of one key in `config/prompts.json`; empty when unknown.
pub fn default(key: &str) -> &'static str {
    static PROMPTS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    PROMPTS
        .get_or_init(|| serde_json::from_str(include_str!("../../../config/prompts.json")).unwrap_or_default())
        .get(key)
        .map(String::as_str)
        .unwrap_or("")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub target: String,
    pub content: String,
    pub previous_id: String,
    pub action: String,
    pub reason: String,
    pub created: i64,
}

pub const METHODS: &[&str] = &[
    "prompts",
    "prompt_save",
    "prompt_history",
    "prompt_adopt",
    "prompt_rollback",
    "prompt_retry",
    "learn",
    "ai_accuracy",
    "memory_read",
    "memory_save",
];

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    let id = || p["id"].as_str().unwrap_or("").to_string();
    match method {
        "prompts" => Ok(TARGETS.iter().map(|t| (t.to_string(), Value::from(default(t)))).collect::<Map<_, _>>().into()),
        "prompt_save" => save(ctx, &p).await,
        "prompt_history" => history(ctx, &p).await,
        "prompt_adopt" => Ok(json!({"version": ctx.db.adopt_prompt_review(id()).await?})),
        "prompt_rollback" => {
            let expected = p["expected"].as_str().unwrap_or("").to_string();
            Ok(json!({"version": ctx.db.rollback_prompt(id(), expected).await?}))
        }
        "prompt_retry" => {
            retry(ctx, &id(), |payload| crate::ai::ask::ask(ctx, "prompt_review", payload, crate::ai::Mode::Manual))
                .await
        }
        "learn" => learning::learn(ctx).await,
        "ai_accuracy" => {
            let days = p["days"].as_i64().unwrap_or(30).clamp(1, 365);
            ctx.db.ai_accuracy(crate::time::now() - days * 86_400_000).await
        }
        "memory_read" => ctx.db.memories().await,
        "memory_save" => memory::save(ctx, &p).await,
        _ => Err(Error::new("未知请求")),
    }
}

async fn save(ctx: &Ctx, p: &Value) -> Result<Value> {
    let target = p["target"].as_str().ok_or_else(|| Error::new("缺少提示词类型"))?;
    let text = p["text"].as_str().ok_or_else(|| Error::new("缺少提示词"))?;
    if text.chars().count() > 20000 {
        crate::bail!("每套提示词最多 20000 字");
    }
    // An empty policy means the factory text, as before.
    let text = if text.trim().is_empty() { default(target) } else { text };
    let expected = p["expected"].as_str().map(str::to_owned);
    ctx.db.save_prompt(target.into(), text.into(), expected).await?;
    Ok(json!(true))
}

async fn history(ctx: &Ctx, p: &Value) -> Result<Value> {
    let target = p["target"].as_str().unwrap_or("classify");
    if !TARGETS.contains(&target) {
        crate::bail!("未知提示词用途");
    }
    if let Some(id) = p["version"].as_str() {
        let found = ctx.db.prompt_version(target.into(), id.into()).await?;
        return Ok(json!(found.ok_or_else(|| Error::new("提示词版本不存在"))?));
    }
    ctx.db.prompt_history(target.into()).await
}
