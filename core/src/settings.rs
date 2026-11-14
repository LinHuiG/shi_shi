//! The settings page: whole-config saves (`config`) and the field patches
//! the page sends while the user edits (`settings_patch`). Both run the same
//! checks; the file write itself is `ConfigStore::save`.

use std::path::{Component, Path};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::ai::work::WorkSchedule;
use crate::{bail, Config, Ctx, Error, Result};

pub const METHODS: &[&str] = &["config", "settings_patch"];

/// Fields the page may patch; everything else has its own method.
const PATCHABLE: [&str; 19] = [
    "auto_update",
    "ai_timeout_seconds",
    "retain_ai_traces",
    "model_profiles",
    "message_retention_days",
    "analysis_workdir",
    "interval_minutes",
    "analysis_interval_minutes",
    "analysis_lookback_hours",
    "analysis_related_only",
    "daily_calls",
    "max_input_chars",
    "retrieval_budget_chars",
    "enabled",
    "auto_ai",
    "work_start",
    "work_end",
    "message_filter_enabled",
    "leaders",
];
const PROFILES: [&str; 3] = ["analysis", "review", "filter"];

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    match method {
        "config" => save(ctx, p).await.map(|_| json!(true)),
        "settings_patch" => patch(ctx, &p).await,
        _ => Err(Error::new("未知请求")),
    }
}

async fn patch(ctx: &Arc<Ctx>, patch: &Value) -> Result<Value> {
    let fields = patch.as_object().ok_or_else(|| Error::new("设置补丁必须是对象"))?;
    if fields.keys().any(|k| !PATCHABLE.contains(&k.as_str())) {
        bail!("设置补丁包含不可修改字段");
    }
    let before = serde_json::to_value(&*ctx.config.get())?;
    let mut candidate = before.clone();
    for (key, value) in fields {
        if key != "model_profiles" {
            candidate[key] = value.clone();
            continue;
        }
        for (kind, route) in value.as_object().ok_or_else(|| Error::new("模型用途配置无效"))? {
            if !PROFILES.contains(&kind.as_str()) {
                bail!("模型用途无效");
            }
            if !candidate["model_profiles"].is_object() {
                candidate["model_profiles"] = json!({});
            }
            candidate["model_profiles"][kind] = route.clone();
        }
    }
    if candidate == before {
        return Ok(json!({ "config": before }));
    }
    save(ctx, candidate).await.map(|config| json!({ "config": config }))
}

/// Check and store a full configuration; the last save wins.
async fn save(ctx: &Arc<Ctx>, candidate: Value) -> Result<Value> {
    let next: Config = serde_json::from_value(candidate).map_err(|e| Error(e.to_string()))?;
    let next = check_candidate(ctx, next).await?;
    let mut old = None;
    let saved = ctx
        .config
        .update(&ctx.db, |current| {
            old = Some((serde_json::to_value(&*current)?, current.message_filter_enabled));
            *current = next;
            Ok(())
        })
        .await?;
    let (before, filter_was_on) = old.expect("update ran the change");
    let after = serde_json::to_value(&*saved)?;
    // The filter's "run early" hint and progress mean nothing once early
    // analysis is off or the filter itself was switched off.
    let reset = !saved.early_analysis_enabled() || (filter_was_on && !saved.message_filter_enabled);
    ctx.db.record_settings(before, after.clone(), reset).await?;
    Ok(after)
}

/// Value checks of the settings page, in a fixed order so the first
/// failing field reports the same message. `budget` is
/// [`crate::ai::prompt::message_budget`] of `classify` for `c`; an
/// unconfigured classify has nothing to check.
pub fn check(c: &Config, budget: Result<Option<usize>>) -> Result<()> {
    let workdir = Path::new(c.analysis_workdir.trim());
    if !workdir.is_absolute() && workdir.components().any(|p| !matches!(p, Component::Normal(_))) {
        bail!("分析工作目录应填写普通目录名或完整绝对路径，不能使用 ..");
    }
    crate::ai::connections::validate_connections(c)?;
    let within = |value: u64, range: std::ops::RangeInclusive<u64>, error: &str| -> Result<()> {
        if range.contains(&value) {
            Ok(())
        } else {
            Err(Error::new(error))
        }
    };
    // The smallest retrieval budget is one `read_context` page.
    within(c.max_input_chars as u64, 5_000..=200_000, "AI 输入上限应为5000–200000字符")?;
    within(c.retrieval_budget_chars as u64, 4_000..=1_000_000, "补查预算应为 4000–1000000 字符")?;
    within(c.analysis_interval_minutes, 1..=1440, "AI 分析间隔应为1–1440分钟")?;
    within(c.analysis_lookback_hours, 1..=87600, "待分析消息时间范围应为1–87600小时")?;
    budget?;
    within(c.message_retention_days.into(), 1..=3650, "消息保留期限应为1–3650天")?;
    WorkSchedule::parse(&c.work_start, &c.work_end).map_err(|e| Error(format!("上下班时间无效：{e}")))?;
    within(c.interval_minutes, 1..=120, "扫描间隔应在 1–120 分钟之间")?;
    within(c.daily_calls, 0..=500, "每日调用上限不能超过500")
}

/// [`check`] with the message budget measured against the stored prompts.
/// Connection checks touch the file system and run off the async threads.
pub async fn check_candidate(ctx: &Arc<Ctx>, c: Config) -> Result<Config> {
    let policies = ctx.db.ai_policy("classify".into()).await;
    let budget = policies.and_then(|p| crate::ai::prompt::message_budget(&c, "classify", &p));
    crate::exec::blocking(move || check(&c, budget).map(|_| c)).await
}

#[cfg(test)]
mod tests;
