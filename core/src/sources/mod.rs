//! Message sources: installed plugins, configured sources, scanning and the
//! inbox methods.

mod manage;
mod normalize;
pub mod plugin;
mod scan;
mod settings;
mod zip;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{messages, Ctx, Host, Result};

pub(crate) use normalize::Fetched;
use plugin::Plugin;

pub const MAX_SOURCES: usize = 16;
pub(crate) const STALE: &str = "消息来源已变化，请刷新后重试";

/// A configured message source; stored in `data::sources`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Source {
    pub id: String,
    pub plugin_id: String,
    pub account_key: String,
    pub name: String,
    pub enabled: bool,
    pub config: Value,
    pub cursor: Value,
    pub last_scan: i64,
    pub last_error: String,
    pub updated: i64,
}

/// The next optimistic-concurrency version.
pub fn next_version(old: i64) -> i64 {
    crate::time::now().max(old + 1)
}

/// In-memory state of the sources domain.
#[derive(Default)]
pub struct Runtime {
    /// Scans, validation and settings calls read; install and uninstall
    /// take it exclusively so no call runs against files being replaced.
    plugins: tokio::sync::RwLock<()>,
    scanning: AtomicBool,
    /// Start of the last scan attempt, successful or not.
    last_attempt: AtomicI64,
    sessions: Mutex<settings::Sessions>,
}

pub const METHODS: &[&str] = &[
    "plugins",
    "plugin_install",
    "plugin_upload",
    "plugin_uninstall",
    "plugin_development_guide",
    "source_discover",
    "plugin_settings",
    "source_save",
    "source_toggle",
    "source_remove",
    "scan",
    "scan_only",
    "scan_control",
    "source_range",
    "messages_query",
    "messages_delete",
    "image_preview",
    "evidence_query",
];

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    match method {
        "plugins" => manage::plugins(ctx).await,
        "plugin_install" => manage::install(ctx, &p).await,
        "plugin_upload" => manage::upload(ctx, &p).await,
        "plugin_uninstall" => manage::uninstall(ctx, &p).await,
        "plugin_development_guide" => Ok(manage::guide()),
        "source_discover" => manage::discover(ctx, &p).await,
        "plugin_settings" => settings::dispatch(ctx, &p).await,
        "source_save" => manage::save(ctx, p.get("source").cloned().unwrap_or_default()).await,
        "source_toggle" => manage::toggle(ctx, &p).await,
        "source_remove" => manage::remove(ctx, &p).await,
        "scan" => scan::scan(ctx, true).await,
        "scan_only" => scan::scan(ctx, false).await,
        "scan_control" => scan::control(ctx, &p).await,
        "source_range" => scan::range(ctx, &p).await,
        "messages_query" => {
            let days = ctx.config.get().message_retention_days;
            ctx.db.query_messages(p, days).await
        }
        "messages_delete" => ctx.db.delete_messages(p).await,
        "image_preview" => messages::image_preview(ctx, &p).await,
        "evidence_query" => ctx.db.task_evidence(p).await,
        _ => Err(crate::Error::new("未知请求")),
    }
}

/// Retention runs at most this often after data changed, and at least
/// hourly so messages still expire on an idle day (ms).
const PRUNE_AFTER_CHANGE: i64 = 15_000;
const PRUNE_IDLE: i64 = 60 * crate::time::MINUTE;

/// Start the scan loop: it sleeps until the next scan or retention pass is
/// due, or a business change may have moved that time.
pub fn spawn(ctx: &Arc<Ctx>) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut changes = ctx.db.changes();
        let (mut pruned, mut changed) = (0, true);
        loop {
            let scan = scan::tick(&ctx).await;
            let now = crate::time::now();
            let prune_at = pruned + if changed { PRUNE_AFTER_CHANGE } else { PRUNE_IDLE };
            if now >= prune_at {
                (pruned, changed) = (now, false);
                let days = ctx.config.get().message_retention_days;
                let _ = ctx.db.prune_messages(days).await;
                // Our own commit is not a reason to prune again.
                changes.borrow_and_update();
            }
            let prune_at = pruned + if changed { PRUNE_AFTER_CHANGE } else { PRUNE_IDLE };
            let wake = scan.map_or(prune_at, |s| s.min(prune_at));
            let wait = (wake - crate::time::now()).max(0) as u64;
            if tokio::time::timeout(std::time::Duration::from_millis(wait), changes.changed()).await.is_ok() {
                changed = true;
            }
        }
    });
}

/// A private plugin work directory, emptied before use and removed after.
pub(crate) struct Work(pub PathBuf);

impl Work {
    pub fn new(path: PathBuf) -> Result<Self> {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Call a plugin method through the host's worker.
pub(crate) fn invoke(
    host: &dyn Host,
    plugin: &Plugin,
    method: &str,
    config: &Value,
    params: Value,
    work: &Path,
) -> Result<Value> {
    host.plugin_call(&plugin.manifest_path(), false, plugin.request(method, config, params, work), None)
}

/// Check the source's configuration with the plugin and return its account.
pub(crate) fn validate(host: &dyn Host, plugin: &Plugin, source: &Source, work: &Path) -> Result<String> {
    plugin.validate_fields(&source.config)?;
    let result = invoke(host, plugin, "validate", &source.config, serde_json::json!({}), work)?;
    normalize::identity(source, &result)
}

#[cfg(test)]
pub(crate) mod testing;
