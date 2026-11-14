//! `analysis_backlog`: preview and skip old pending messages.
//!
//! The preview lists the exact versions older than `hours`; applying skips
//! only those versions, so a message that arrived or was edited after the
//! preview stays pending.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::progress::Row;
use crate::{time, Ctx, Result};

pub const HOURS_ERROR: &str = "请输入1–87600小时";
pub const PREVIEW_FIRST: &str = "请先预览要跳过的消息";

pub async fn handle(ctx: &Arc<Ctx>, params: Value) -> Result<Value> {
    let hours = params["hours"].as_u64().unwrap_or(24);
    if !(1..=87_600).contains(&hours) {
        crate::bail!("{HOURS_ERROR}");
    }
    let apply = params["apply"] == true;
    let expected: Option<Map<String, Value>> = params["records"].as_object().cloned();
    if apply && expected.is_none() {
        crate::bail!("{PREVIEW_FIRST}");
    }
    let (config, analyzing) = (ctx.config.get(), ctx.analysis.analyzing());
    if !apply {
        let rows = ctx.db.analysis_waiting(config, analyzing.clone()).await?;
        return Ok(preview(&rows, hours, analyzing.len()).0);
    }
    ctx.db.skip_analysis_backlog(config, analyzing, hours, expected.unwrap_or_default()).await
}

/// The preview of the waiting `rows` and the versions older than `hours`.
pub fn preview(rows: &[Row], hours: u64, analyzing: usize) -> (Value, BTreeMap<String, String>) {
    let cutoff = time::now() - hours as i64 * time::HOUR;
    let mut reasons = BTreeMap::<String, usize>::new();
    let mut records = BTreeMap::new();
    for r in rows {
        if !r.reason.is_empty() {
            *reasons.entry(r.reason.clone()).or_default() += 1;
        }
        if r.time > 0 && r.time < cutoff {
            records.insert(r.id.clone(), r.hash.clone());
        }
    }
    let more = reasons.len().saturating_sub(3);
    let retry_reasons: Vec<Value> =
        reasons.into_iter().take(3).map(|(reason, count)| json!({"reason": reason, "count": count})).collect();
    let value = json!({
        "hours": hours, "count": records.len(), "records": &records, "eligible": rows.len(),
        "analyzing": analyzing,
        "retry_reasons": retry_reasons, "retry_reasons_more": more,
    });
    (value, records)
}
