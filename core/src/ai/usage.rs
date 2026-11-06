//! Daily call accounting and the one-row analysis state. Reservation runs
//! in a short transaction before the provider call; token totals are added
//! in another one after it returns. Neither spans the call itself; both are
//! stored by `data::ai`.

use serde_json::Value;

/// Counters of one `usage_daily` row.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub day: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cached_reports: u64,
    pub unreported_calls: u64,
}

pub(crate) fn row(kind: &str) -> &'static str {
    if kind == "message_filter" {
        "message_filter"
    } else {
        "main"
    }
}

/// Whether both token counters are known.
pub fn known(usage: &Value) -> bool {
    usage["input_tokens"].as_u64().is_some() && usage["output_tokens"].as_u64().is_some()
}

/// Missing cache counter, explicit zero, or a real hit.
pub fn cache_state(usage: &Value) -> (&'static str, Option<u64>) {
    let cached =
        usage["cached_input_tokens"].as_u64().or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64());
    match cached {
        None => ("missing", None),
        Some(0) => ("zero", Some(0)),
        Some(value) => ("hit", Some(value)),
    }
}

/// How completely the provider reported this call (`usage_status`).
pub fn status(usage: &Value, failed: bool) -> &'static str {
    match (known(usage), usage["partial"] == true) {
        (true, true) => "reported_partial",
        (true, false) => "reported",
        (false, _) if failed => "request_failed_without_complete_usage",
        _ => "provider_did_not_report_complete_usage",
    }
}

/// The `analysis_state` row.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub pending: bool,
    pub pending_since: i64,
    /// When the last analysis round really started (manual or automatic);
    /// the one trigger time for cooldown and interval, never moved back.
    pub last_analysis_at: i64,
    pub last_success_at: i64,
    pub ai_interrupted_at: i64,
    pub analysis_interrupted_at: i64,
    pub last_error: String,
}
