//! Plugin installation and source configuration methods.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use serde_json::{json, Value};

use super::plugin::{self, Plugin};
use super::{invoke, validate, Source, Work, STALE};
use crate::{bail, exec, Ctx, Error, Result};

const UPLOAD_LIMIT: usize = 32 * 1024 * 1024;
const BUSY: &str = "插件正在读取或配置，请稍后重试";

fn tmp(ctx: &Ctx, kind: &str) -> PathBuf {
    ctx.root.join("tmp").join(kind).join(uuid::Uuid::new_v4().to_string())
}

async fn find(ctx: &Ctx, id: &str) -> Result<Plugin> {
    let root = ctx.root.clone();
    let id = id.to_string();
    exec::blocking(move || plugin::find(&root, &id)).await
}

pub async fn plugins(ctx: &Arc<Ctx>) -> Result<Value> {
    let _access = ctx.sources.plugins.read().await;
    let root = ctx.root.clone();
    let (plugins, errors) = exec::blocking(move || Ok(plugin::catalog(&root))).await?;
    let sources = ctx.db.sources().await?;
    let manifests: Vec<_> = plugins.into_iter().map(|p| p.manifest).collect();
    Ok(json!({"plugins": manifests, "errors": errors, "sources": sources}))
}

pub async fn install(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let path =
        p["path"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| Error::new("请选择插件 ZIP 包或 plugin.json"))?;
    install_path(ctx, PathBuf::from(path)).await
}

async fn install_path(ctx: &Arc<Ctx>, path: PathBuf) -> Result<Value> {
    let _access = ctx.sources.plugins.try_write().map_err(|_| Error::new(BUSY))?;
    let (root, host) = (ctx.root.clone(), ctx.host.clone());
    let installed = exec::blocking(move || {
        host.plugin_reset()?;
        plugin::install(&root, &path)
    })
    .await?;
    let mut result = serde_json::to_value(installed.manifest)?;
    result["install_status"] = installed.status.into();
    Ok(result)
}

/// A ZIP dropped onto the page arrives as base64 content, never a path.
pub async fn upload(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let name = p["name"].as_str().ok_or_else(|| Error::new("缺少插件包名称"))?;
    let encoded = p["base64"].as_str().ok_or_else(|| Error::new("缺少插件包内容"))?;
    if !name.to_ascii_lowercase().ends_with(".zip") {
        bail!("请拖入插件 ZIP 包");
    }
    if encoded.len() > UPLOAD_LIMIT.div_ceil(3) * 4 {
        bail!("拖入的 ZIP 超过32 MiB，请点击安装区域选择文件");
    }
    if encoded.is_empty() || !encoded.is_ascii() {
        bail!("插件包传输不完整，请重试");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
        .filter(|b| b.len() <= UPLOAD_LIMIT)
        .ok_or_else(|| Error::new("插件包传输不完整，请重试"))?;
    if !bytes.starts_with(b"PK\x03\x04") {
        bail!("插件包不是有效的 ZIP 文件");
    }
    let dir = ctx.root.join("tmp/plugin-uploads");
    let path = dir.join(format!("{}.zip", uuid::Uuid::new_v4()));
    let result = async {
        fs::create_dir_all(&dir)?;
        fs::write(&path, bytes)?;
        install_path(ctx, path.clone()).await
    }
    .await;
    let _ = fs::remove_file(&path);
    result
}

pub async fn uninstall(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let _access = ctx.sources.plugins.try_write().map_err(|_| Error::new(BUSY))?;
    let id = p["id"].as_str().ok_or_else(|| Error::new("缺少插件编号"))?.to_string();
    let plugin = find(ctx, &id).await?;
    let parent = plugin::plugins_dir(&ctx.root).canonicalize()?;
    let inside = plugin.dir.canonicalize().ok().and_then(|d| d.parent().map(|p| p == parent)).unwrap_or(false);
    if !inside {
        bail!("插件目录不属于安装目录，未卸载");
    }
    let host = ctx.host.clone();
    exec::blocking(move || host.plugin_reset()).await?;
    // Disable the sources first: a failure here changes nothing, and a
    // failed move afterwards leaves an installed plugin to uninstall again.
    ctx.db.disable_plugin_sources(id).await?;
    let removed = ctx.root.join("tmp/removed-plugins");
    fs::create_dir_all(&removed)?;
    let retired = removed.join(uuid::Uuid::new_v4().to_string());
    fs::rename(&plugin.dir, &retired).map_err(|e| Error(format!("来源已停用，插件文件正在使用，请稍后再卸载：{e}")))?;
    plugin::clean_replaced(&retired, &plugin);
    Ok(json!(true))
}

pub fn guide() -> Value {
    let text = [
        include_str!("../../../docs/PLUGIN_ADAPTATION.md"),
        include_str!("../../../PLUGIN_API.txt"),
        include_str!("../../../examples/json-source/plugin.json"),
        include_str!("../../../examples/json-source/src/lib.rs"),
    ]
    .join("\n\n");
    json!({"text": text})
}

pub async fn discover(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let _access = ctx.sources.plugins.read().await;
    let id = p["plugin_id"].as_str().ok_or_else(|| Error::new("请选择插件"))?;
    let plugin = find(ctx, id).await?;
    if plugin.manifest.settings.is_some() {
        return Ok(json!({"settings": super::settings::begin(ctx, &plugin, &p["config"])?}));
    }
    let (host, config, work) = (ctx.host.clone(), p["config"].clone(), tmp(ctx, "discovery"));
    exec::blocking(move || {
        let work = Work::new(work)?;
        invoke(&*host, &plugin, "discover", &config, json!({}), &work.0)
    })
    .await
}

async fn validated(ctx: &Ctx, plugin: Plugin, source: Source) -> Result<String> {
    let (host, work) = (ctx.host.clone(), tmp(ctx, "validate"));
    exec::blocking(move || {
        let work = Work::new(work)?;
        validate(&*host, &plugin, &source, &work.0)
    })
    .await
}

pub async fn save(ctx: &Arc<Ctx>, value: Value) -> Result<Value> {
    let access = ctx.sources.plugins.read().await;
    let source: Source = serde_json::from_value(value)?;
    if source.name.trim().is_empty() || source.name.chars().count() > 100 {
        bail!("来源名称应为 1–100 字");
    }
    let plugin = find(ctx, &source.plugin_id).await?;
    plugin.validate_fields(&source.config)?;
    let key = if source.enabled {
        Some(validated(ctx, plugin, Source { account_key: String::new(), ..source.clone() }).await?)
    } else {
        None
    };
    drop(access);
    ctx.db.save_source(source, key).await
}

/// Disabling never loads the plugin, so it works after an uninstall.
pub async fn toggle(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let _access = ctx.sources.plugins.read().await;
    let id = p["id"].as_str().ok_or_else(|| Error::new("缺少来源编号"))?.to_string();
    let expected = p["updated"].as_i64().ok_or_else(|| Error::new("缺少来源版本"))?;
    let enabled = p["enabled"].as_bool().ok_or_else(|| Error::new("缺少开关状态"))?;
    let before = ctx.db.source(id.clone()).await?.ok_or_else(|| Error::new("消息来源不存在"))?;
    if before.updated != expected {
        bail!("{STALE}");
    }
    if before.enabled == enabled {
        return Ok(serde_json::to_value(before)?);
    }
    let key = if enabled {
        let plugin = find(ctx, &before.plugin_id).await?;
        Some(validated(ctx, plugin, before.clone()).await?)
    } else {
        None
    };
    ctx.db.toggle_source(id, expected, enabled, key).await
}

pub async fn remove(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let (id, updated) = (p["id"].as_str().unwrap_or("").to_string(), p["updated"].as_i64());
    let disable = ctx.db.remove_source(id, updated).await?;
    if disable && ctx.config.get().enabled {
        ctx.config
            .update(&ctx.db, |c| {
                c.enabled = false;
                Ok(())
            })
            .await?;
    }
    Ok(json!(true))
}

#[cfg(test)]
mod tests;
