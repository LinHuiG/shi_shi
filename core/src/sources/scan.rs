//! Incremental scans, time-range reads and the automatic scan schedule.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use super::normalize::{self, Fetched};
use super::plugin;
use super::{invoke, validate, Source, Work};
use crate::messages;
use crate::time::{now, DAY, HOUR, MINUTE};
use crate::{analysis, bail, exec, Ctx, Error, Host, Result};

const RUNNING: &str = "扫描正在运行";
const LOOKUP_CHUNK: usize = 500;

/// `scan` and `scan_only`: read every enabled source, then run (or queue)
/// the analysis of what arrived.
pub async fn scan(ctx: &Arc<Ctx>, analyze: bool) -> Result<Value> {
    let result = scan_data(ctx).await;
    if matches!(&result, Err(e) if e.0 == RUNNING) {
        return result;
    }
    let finished = now();
    ctx.sources.last_attempt.store(finished, Ordering::SeqCst);
    ctx.db.finish_scan(finished).await?;
    let mut result = result?;
    result["analysis"] = if analyze { analysis::after_scan(ctx, true).await? } else { json!({"queued": true}) };
    Ok(result)
}

async fn scan_data(ctx: &Arc<Ctx>) -> Result<Value> {
    let _access = ctx.sources.plugins.read().await;
    let _scanning = exec::Flag::claim(&ctx.sources.scanning).ok_or(RUNNING)?;
    let _working = ctx.flags.working(ctx);
    let start = now();
    ctx.sources.last_attempt.store(start, Ordering::SeqCst);
    let lookback = ctx.config.get().analysis_lookback_hours as i64 * HOUR;
    if !ctx.db.any_source_enabled().await? {
        bail!("请先配置并启用一个消息来源");
    }
    let plan = ctx.db.scan_plan(start, start - lookback).await?;
    // The plugin worker serves one call at a time; read the sources in turn
    // on one blocking thread instead of queueing a thread per source.
    let (host, root) = (ctx.host.clone(), ctx.root.clone());
    let results = exec::blocking(move || {
        Ok(plan
            .into_iter()
            .map(|(source, tracked)| {
                let fetched = fetch(&*host, &root, &source, tracked, start);
                (source, fetched)
            })
            .collect::<Vec<_>>())
    })
    .await?;
    let days = ctx.config.get().message_retention_days;
    let outcome = ctx.db.commit_scan(results, start, days).await?;
    let mut result = json!({"rows": outcome.counts["received"], "added": outcome.counts["added"],
        "updated": outcome.counts["updated"], "sources": outcome.sources});
    let mut detail = result.clone();
    detail["scan_counts"] = outcome.counts;
    detail["errors"] = json!(outcome.errors);
    detail["elapsed_ms"] = json!(now() - start);
    ctx.db.record_activity("scan", detail).await;
    if outcome.successful == 0 && !outcome.errors.is_empty() {
        bail!("{}", outcome.errors.join("；"));
    }
    let mut warnings = outcome.warnings;
    warnings.extend(outcome.errors);
    result["warnings"] = json!(warnings);
    Ok(result)
}

/// Read one source's increment. Runs on a blocking thread.
fn fetch(host: &dyn Host, root: &Path, source: &Source, tracked: Vec<(String, String)>, until: i64) -> Result<Fetched> {
    let plugin = plugin::find(root, &source.plugin_id)?;
    let since = if source.last_scan == 0 { until - DAY } else { (source.last_scan - 10 * MINUTE).max(until - DAY) };
    let dir = root.join("tmp/plugins").join(&source.id);
    if plugin.has("fetch_scan") {
        plugin.validate_fields(&source.config)?;
        let work = Work::new(dir.join(format!("scan-{}", uuid::Uuid::new_v4())))?;
        let externals: BTreeSet<String> = tracked.into_iter().map(|(_, e)| e).collect();
        return normalize::scan_pages(source, since, until, externals.into_iter().collect(), |params| {
            invoke(host, &plugin, "fetch_scan", &source.config, params, &work.0)
        });
    }
    let work = Work::new(dir)?;
    validate(host, &plugin, source, &work.0)?;
    let result = invoke(
        host,
        &plugin,
        "fetch_incremental",
        &source.config,
        json!({"cursor": source.cursor, "since": since, "until": until}),
        &work.0,
    )?;
    let checkpoint = normalize::checkpoint(source, &result, since, until)?;
    let cursor = result.get("cursor").cloned().ok_or_else(|| Error::new("插件未返回增量游标"))?;
    let mut rows = normalize::normalize(source, &result)?;
    let missing = normalize::missing_tracked(tracked, &rows);
    for ids in missing.chunks(LOOKUP_CHUNK) {
        let requested: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        let found =
            invoke(host, &plugin, "lookup_messages", &source.config, json!({"ids": ids, "until": until}), &work.0)?;
        let found = normalize::normalize(source, &found)?;
        rows.extend(found.into_iter().filter(|m| requested.contains(m.external_id.as_str())));
    }
    Ok((rows, cursor, checkpoint))
}

/// `source_range`: read a past window of one source.
pub async fn range(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let _access = ctx.sources.plugins.read().await;
    let _scanning = exec::Flag::claim(&ctx.sources.scanning).ok_or(RUNNING)?;
    let since = p.get("since").and_then(Value::as_i64).ok_or_else(|| Error::new("缺少开始时间"))?;
    let until = p.get("until").and_then(Value::as_i64).ok_or_else(|| Error::new("缺少结束时间"))?;
    if since <= 0 || until < since || until > now() || until - since > 31 * DAY {
        bail!("请选择过去31天以内的有效时间段（单次最多31天）");
    }
    let id = p["id"].as_str().unwrap_or_default().to_string();
    let source = ctx.db.source(id).await?.ok_or_else(|| Error::new("来源不存在"))?;
    let (host, root) = (ctx.host.clone(), ctx.root.clone());
    let read = source.clone();
    let rows = exec::blocking(move || -> Result<Vec<messages::Message>> {
        let plugin = plugin::find(&root, &read.plugin_id)?;
        let work = Work::new(root.join("tmp").join("range").join(uuid::Uuid::new_v4().to_string()))?;
        validate(&*host, &plugin, &read, &work.0)?;
        let result =
            invoke(&*host, &plugin, "fetch_range", &read.config, json!({"since": since, "until": until}), &work.0)?;
        normalize::normalize(&read, &result)
    })
    .await?;
    let days = ctx.config.get().message_retention_days;
    let count = ctx.db.commit_range(source, rows, days).await?;
    drop(_scanning);
    Ok(json!({"rows": count, "analysis": analysis::after_scan(ctx, true).await?}))
}

/// `scan_control`: switch the automatic scan.
pub async fn control(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let config = ctx.config.get();
    let enabled = p.get("enabled").and_then(Value::as_bool).unwrap_or(config.enabled);
    if enabled && !ctx.db.any_source_enabled().await? {
        bail!("请先在消息来源中配置并启用一个插件");
    }
    let before = json!({"enabled": config.enabled});
    let after = json!({"enabled": enabled});
    ctx.config
        .update(&ctx.db, |c| {
            c.enabled = enabled;
            Ok(())
        })
        .await?;
    ctx.db.record_scan_control(before, after.clone(), enabled).await?;
    Ok(after)
}

/// Run the automatic scan when it is due. Returns when the next one is due
/// (epoch ms), or `None` when only a change can make one due (scanning, busy,
/// disabled or no enabled source).
pub async fn tick(ctx: &Arc<Ctx>) -> Option<i64> {
    if ctx.sources.scanning.load(Ordering::SeqCst) || ctx.flags.busy() {
        return None;
    }
    let config = ctx.config.get();
    if !config.enabled {
        return None;
    }
    let last = ctx.db.last_scan().await;
    let Ok(Some(last)) = last else { return None };
    // A last scan in the future (the clock went back) counts as now.
    let last = last.max(ctx.sources.last_attempt.load(Ordering::SeqCst)).min(now());
    let due = last + config.interval_minutes as i64 * MINUTE;
    if now() < due {
        return Some(due);
    }
    if let Err(e) = scan(ctx, false).await {
        ctx.db.record_activity("error", json!({"message": e.0})).await;
    }
    Some(now() + config.interval_minutes as i64 * MINUTE)
}

#[cfg(test)]
mod tests;
