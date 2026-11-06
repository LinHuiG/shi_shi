//! Plugin settings pages: a sandboxed HTML page in the source editor talks to
//! its plugin through short-lived sessions.

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::plugin::Plugin;
use crate::{bail, exec, Ctx, Error, Result};

const LIFETIME: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Calling,
    Saving,
    Closed,
}

pub struct Session {
    plugin: Plugin,
    config: Value,
    created: Instant,
    cancelled: Arc<AtomicBool>,
    phase: Mutex<Phase>,
}

pub type Sessions = BTreeMap<String, Arc<Session>>;

impl Session {
    fn close(&self) {
        *self.phase.lock().unwrap() = Phase::Closed;
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Open a settings session for a plugin that declares a settings page.
pub fn begin(ctx: &Ctx, plugin: &Plugin, config: &Value) -> Result<Value> {
    let settings = plugin.manifest.settings.as_ref().ok_or_else(|| Error::new("插件未提供配置页面"))?;
    let html = fs::read_to_string(plugin.dir.join(&settings.entrypoint))?;
    let mut sessions = ctx.sources.sessions.lock().unwrap();
    sessions.retain(|_, s| {
        let keep = s.created.elapsed() < LIFETIME || *s.phase.lock().unwrap() == Phase::Saving;
        if !keep {
            s.close();
        }
        keep
    });
    let id = uuid::Uuid::new_v4().to_string();
    sessions.insert(
        id.clone(),
        Arc::new(Session {
            plugin: plugin.clone(),
            config: config.clone(),
            created: Instant::now(),
            cancelled: Default::default(),
            phase: Mutex::new(Phase::Idle),
        }),
    );
    Ok(json!({"session": id, "html": html}))
}

/// Returns the phase to idle unless the session was closed meanwhile.
struct Busy(Arc<Session>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut phase = self.0.phase.lock().unwrap();
        if *phase != Phase::Closed {
            *phase = Phase::Idle;
        }
    }
}

pub async fn dispatch(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let id = p["session"].as_str().ok_or_else(|| Error::new("缺少插件配置会话"))?.to_string();
    let action = p["action"].as_str().unwrap_or("");
    if action == "cancel" {
        let mut sessions = ctx.sources.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&id) {
            if *session.phase.lock().unwrap() == Phase::Saving {
                bail!("正在保存来源，请等待保存结果");
            }
            session.close();
        }
        sessions.remove(&id);
        return Ok(json!(true));
    }
    let session = ctx
        .sources
        .sessions
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| Error::new("插件配置已关闭，请重新打开"))?;
    {
        let mut phase = session.phase.lock().unwrap();
        match *phase {
            Phase::Closed => bail!("插件配置已取消"),
            Phase::Calling | Phase::Saving => bail!("请等待当前配置操作完成"),
            Phase::Idle => *phase = if action == "complete" { Phase::Saving } else { Phase::Calling },
        }
    }
    let _busy = Busy(session.clone());
    let access = ctx.sources.plugins.read().await;
    let root = ctx.root.clone();
    let plugin_id = session.plugin.manifest.id.clone();
    let current = exec::blocking(move || super::plugin::find(&root, &plugin_id)).await?;
    if current.manifest != session.plugin.manifest {
        bail!("插件已更新，请重新打开配置页面");
    }
    if action == "complete" {
        let config = p.get("config").unwrap_or(&session.config).clone();
        current.validate_fields(&config)?;
        let mut source = p["source"].clone();
        if !source.is_object() || source["plugin_id"] != current.manifest.id.as_str() {
            bail!("来源与当前插件配置不匹配");
        }
        source["config"] = config.clone();
        // source_save takes the plugin lock itself; a waiting writer would
        // otherwise block the second read.
        drop(access);
        super::manage::save(ctx, source).await?;
        session.close();
        ctx.sources.sessions.lock().unwrap().remove(&id);
        return Ok(config);
    }
    if action != "call" {
        bail!("不支持的插件配置操作");
    }
    let method = p["method"].as_str().ok_or_else(|| Error::new("缺少配置方法"))?.to_string();
    if !current.manifest.settings.as_ref().is_some_and(|s| s.methods.contains(&method)) {
        bail!("插件未声明此配置方法");
    }
    let input = json!({"protocol": 1, "method": method, "config": session.config, "params": p["params"]});
    let host = ctx.host.clone();
    let manifest = current.manifest_path();
    let cancel = session.cancelled.clone();
    let result = exec::blocking(move || host.plugin_call(&manifest, true, input, Some(cancel))).await;
    drop(access);
    if let Err(e) = &result {
        let detail = json!({"plugin": current.manifest.id, "method": method, "error": e.0});
        ctx.db.record_activity("plugin_failed", detail).await;
    }
    result
}

#[cfg(test)]
mod tests;
