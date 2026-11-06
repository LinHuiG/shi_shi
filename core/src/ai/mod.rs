//! AI call layer: route resolution, request preparation and budget check,
//! the serial call slot, daily accounting, traces, short evidence ids and
//! the provider drivers. Callers (analysis, archive, handling, task edits,
//! prompt review) build the payload and apply the result; this module only
//! turns one prepared request into one validated JSON reply.

pub mod api;
pub mod ask;
pub mod calendar;
pub mod codex;
pub mod connections;
pub mod credentials;
pub mod dsh;
pub mod invoke;
pub mod native;
pub mod prompt;
pub mod provider;
pub mod schema;
pub mod trace;
pub mod usage;
pub mod wire;
pub mod work;

use serde_json::Value;

use crate::Result;

pub use crate::exec::{ARCHIVE_BUSY, BUSY, CANCELLED};

pub const AUTOMATION_DISABLED: &str = "AI_AUTOMATION_DISABLED";
pub const DEFERRED: &str = "当前不是工作或加班状态，AI 已暂停；请确认日历或开启加班后重试";
pub const DAILY_LIMIT: &str = "已达到每日AI调用上限；消息仍保留，可手动处理或调整上限";
pub const UNCONFIGURED_PREFIX: &str = "AI_UNCONFIGURED:";
pub const INPUT_LIMIT_ERROR: &str = "AI 输入超限";
pub const EMPTY_SEGMENT: &str = "（本段无内容）";
pub const UNKNOWN_EVIDENCE: &str = "AI 返回未知证据短编号，本次结果未应用";

/// Who started the work. Manual work ignores the work calendar and the
/// automation switches; nightly archive passes the gate with its claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Manual,
    Automatic,
    Nightly,
}

/// Purpose name used in the unconfigured error.
pub fn purpose_label(kind: &str) -> &'static str {
    match kind {
        "message_filter" => "消息过滤",
        "handling" => "AI一键处置",
        "tool_call" => "工具调用",
        "archive" | "learning" | "prompt_review" | "report" | "advice_revision" => "归档分析",
        _ => "事项更新",
    }
}

/// Marker the drivers put in the text of a provider refusal or content
/// filter (API `Refusal`, DSH refusal stop).
pub const REFUSED: &str = "供应商拒答";

/// Whether a failed call was refused by the provider: only the drivers'
/// own marker, never a raw word such as "connection refused".
pub fn refused(error: &str) -> bool {
    error.contains(REFUSED)
}

/// Stable diagnostic category for a failed call; the first match wins.
pub fn failure_class(error: &str) -> &'static str {
    let text = error.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if error == CANCELLED || any(&["中断", "cancel"]) {
        "cancelled"
    } else if error.starts_with(INPUT_LIMIT_ERROR)
        || any(&["上限", "超限", "quota", "rate limit", "429", "capacity", "overloaded"])
    {
        "resource_limit"
    } else if any(&["供应商拒答", "stop reason refusal"]) {
        "provider_refusal"
    } else if any(&["http 504"]) {
        "upstream_timeout"
    } else if any(&["tool arguments"]) {
        "tool_arguments"
    } else if any(&["timeout", "timed out", "超时", "deadline", "temporar", "unavailable", "connection reset", "eof"])
    {
        "temporary_failure"
    } else if any(&["来源", "插件", "source", "plugin", "scan"]) {
        "source_failure"
    } else {
        "unknown"
    }
}

/// IPC methods of the AI layer.
pub const METHODS: &[&str] = &[
    "work_state",
    "ai_trace",
    "analysis_session_open",
    "models",
    "detect",
    "connection_save",
    "connection_delete",
    "codex_status",
    "ai_connection_status",
    "dsh_initialize_acp",
    "dsh_workspace_plugin",
];

pub async fn handle(ctx: &std::sync::Arc<crate::Ctx>, method: &str, p: Value) -> Result<Value> {
    match method {
        "work_state" => ctx.db.set_work_state(ctx.config.get(), p, crate::time::now()).await,
        "ai_trace" => trace::read(&ctx.db, ctx.config.get().retain_ai_traces, &p).await,
        "analysis_session_open" => connections::open_session(&p),
        "models" if p.get("connection").is_some() => connections::connection_models(ctx, &p).await,
        "models" => crate::exec::blocking(|| codex::list_models().map_err(crate::Error)).await,
        "detect" if p["driver"] == "dsh" => {
            let saved = p["saved"].as_str().unwrap_or("").to_string();
            crate::exec::blocking(move || Ok(connections::detect_dsh(&saved))).await
        }
        "detect" => crate::exec::blocking(|| Ok(serde_json::json!({"codex": codex::detect_cli()}))).await,
        "connection_save" => connections::save_connection(ctx, p).await,
        "connection_delete" => connections::delete_connection(ctx, &p).await,
        "codex_status" | "ai_connection_status" => connections::status(ctx, &p).await,
        "dsh_initialize_acp" | "dsh_workspace_plugin" => connections::dsh(ctx, method, &p).await,
        _ => Err(crate::Error::new("未知请求")),
    }
}

#[cfg(test)]
mod tests;
