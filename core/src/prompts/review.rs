//! Stored prompt rewrite suggestions. Nothing creates new ones; existing
//! rows can be regenerated (`prompt_retry`) and adopted by the user.

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Ctx, Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub day: String,
    pub created: i64,
    pub status: String,
    pub base_version: String,
    pub base_prompt: String,
    pub evidence: Value,
    pub proposed_prompt: String,
    pub reason: String,
    pub evidence_ids: Vec<String>,
    pub error: String,
    pub last_attempt: i64,
    pub archived_at: i64,
}

pub fn has_result(r: &Review) -> bool {
    ["ready", "adopted", "rejected", "unchanged", "superseded"].contains(&r.status.as_str())
        && !r.reason.trim().is_empty()
        && (r.status == "unchanged" || !r.proposed_prompt.trim().is_empty())
}

/// A pending review with the AI answer validated into it.
fn answer(mut r: Review, result: &Value) -> Result<Review> {
    let improved = result["improved"].as_bool().ok_or_else(|| Error::new("缺少是否改进的判断"))?;
    let reason = result["reason"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 4000)
        .ok_or_else(|| Error::new("修改原因无效"))?;
    let proposed = result["proposed_prompt"].as_str().ok_or_else(|| Error::new("缺少完整提示词"))?;
    if improved && (proposed.trim().is_empty() || proposed.chars().count() > 20000) {
        crate::bail!("新提示词应为 1–20000 字");
    }
    let evidence = result["evidence_ids"]
        .as_array()
        .ok_or_else(|| Error::new("缺少日志依据"))?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| Error::new("日志依据格式无效")))
        .collect::<Result<Vec<_>>>()?;
    let allowed: Vec<&str> = ["audit", "logs"]
        .iter()
        .filter_map(|k| r.evidence[*k].as_array())
        .flatten()
        .filter_map(|v| v["evidence_id"].as_str())
        .collect();
    if improved && (evidence.is_empty() || evidence.iter().any(|id| !allowed.contains(&id.as_str()))) {
        crate::bail!("新提示词没有有效的当日日志依据");
    }
    r.status = if improved && proposed != r.base_prompt { "ready" } else { "unchanged" }.into();
    r.reason = reason.into();
    r.proposed_prompt = proposed.into();
    r.evidence_ids = evidence;
    Ok(r)
}

/// Regenerate a suggestion against the current classify prompt. `invoke`
/// is the AI call; it gets `{day, current_prompt, daily_evidence}`. A
/// failure stores nothing; a stored result replaces the old row.
pub async fn retry<F, Fut>(ctx: &Arc<Ctx>, id: &str, invoke: F) -> Result<Value>
where
    F: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let _busy = crate::exec::Flag::claim(&ctx.prompts.busy).ok_or("提示词建议正在生成")?;
    let (old, next) = ctx.db.prompt_retry(id.to_string()).await?;
    let result =
        invoke(json!({"day": next.day, "current_prompt": next.base_prompt, "daily_evidence": next.evidence})).await?;
    let next = answer(next, &result)?;
    Ok(json!(ctx.db.save_prompt_retry(old, next).await?))
}
