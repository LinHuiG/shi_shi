//! Connection editing, independent of the
//! settings form. Draft secrets never enter the config; a replacement
//! secret gets a fresh credential reference before the config is published
//! and the old one is removed only after the publish succeeded.
//!
//! Also the full connection check of a settings save
//! ([`validate_connections`]) and DSH launcher discovery.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use super::prompt::{connection_by_id, default_codex};
use super::provider::{backend, unsupported};
use crate::config::{Connection as AiConnection, PURPOSES, UNCONFIGURED_CONNECTION};
use crate::exec::blocking;
use crate::{bail, Config, Ctx, Error, Result};

/// Every connection and every configured purpose route, as the settings
/// save checks them (the total time limit first).
pub fn validate_connections(config: &Config) -> Result<()> {
    if !(30..=1800).contains(&config.ai_timeout_seconds) {
        bail!("AI 总时限须为 30—1800 秒");
    }
    if config.ai_connections.len() > 20 {
        bail!("最多配置 20 个附加 AI 连接");
    }
    let mut ids = BTreeSet::new();
    for c in &config.ai_connections {
        if c.id.is_empty() || c.id.len() > 100 || !ids.insert(c.id.as_str()) {
            bail!("AI 连接编号为空或重复");
        }
        if c.name.trim().is_empty()
            || c.name.chars().count() > 80
            || (c.driver != "api" && c.executable.trim().is_empty())
        {
            bail!("请填写 AI 连接名称和程序路径（名称最多 80 字）");
        }
        backend(c).map_err(Error)?;
        if c.driver == "api" {
            shishi_api::Endpoint::parse(&c.base_url).map_err(Error)?;
        }
    }
    for kind in PURPOSES {
        let choice = config.model_choice(kind);
        if choice.connection_id == UNCONFIGURED_CONNECTION {
            continue;
        }
        let c = connection_by_id(config, &choice.connection_id)?;
        match c.driver.as_str() {
            "codex" => super::codex::validate(&choice.reasoning_effort),
            "api" => super::api::validate(&c, &choice.model),
            "dsh" => super::dsh::validate(&c, &choice.model),
            _ => Err(unsupported(&c)),
        }
        .map_err(Error)?;
    }
    Ok(())
}

fn validate_catalog(models: &[Value]) -> Result<()> {
    if models.is_empty() || models.len() > 1000 {
        bail!("请先检测模型，或填写至少一个模型编号（最多 1000 个）");
    }
    if !shishi_api::json::fits_bytes(models, 512_000) {
        bail!("模型目录过大");
    }
    let mut ids = BTreeSet::new();
    for m in models {
        let id = m["model"].as_str().unwrap_or("");
        if id.trim().is_empty() || id.chars().count() > 300 || !ids.insert(id) {
            bail!("模型编号为空、过长或重复");
        }
        if !shishi_api::json::fits_bytes(m, 8192) {
            bail!("模型信息过长");
        }
    }
    Ok(())
}

fn referenced(c: &Config, id: &str) -> bool {
    PURPOSES.iter().any(|kind| c.model_choice(kind).connection_id == id)
}

/// Clear every purpose route that uses `id`; the purpose becomes
/// unconfigured instead of silently falling back to another connection.
fn clear_routes(config: &mut Config, id: &str) {
    let matching = ["message_filter", "classify", "archive"].map(|kind| config.model_choice(kind).connection_id == id);
    let base = config.model_choice("classify");
    let p = config.model_profiles.get_or_insert_with(|| crate::config::ModelProfiles {
        filter: Some(base.clone()),
        analysis: Some(base.clone()),
        review: Some(base),
    });
    for (choice, matching) in [&mut p.filter, &mut p.analysis, &mut p.review].into_iter().zip(matching) {
        if matching {
            *choice = None;
        }
    }
}

fn parse_connection(value: &Value) -> Result<AiConnection> {
    serde_json::from_value(value.clone()).map_err(|e| Error(e.to_string()))
}

/// Remove a credential reference; a failure leaves only an unused entry.
async fn forget(ctx: &Ctx, reference: String) {
    let _ = ctx.db.delete_credential(reference).await;
}

/// `models`: the catalog of a draft connection. An API draft whose address
/// changed must bring its key again.
pub async fn connection_models(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let mut c = parse_connection(&p["connection"])?;
    let root = ctx.root.clone();
    match c.driver.as_str() {
        "codex" => return blocking(|| super::codex::list_models().map_err(Error)).await,
        "dsh" => return blocking(move || super::dsh::models(&c, &root).map_err(Error)).await,
        "api" => {}
        _ => bail!("{}", unsupported(&c)),
    }
    let previous = ctx.config.get().connection(&c.id).cloned();
    let key = p["key"].as_str().filter(|v| !v.is_empty()).map(str::to_string);
    if key.is_none() && c.key_env.is_none() && previous.as_ref().is_some_and(|old| old.base_url != c.base_url) {
        bail!("地址已修改，请重新填写密钥后检测");
    }
    c.credential_id = previous.and_then(|v| v.credential_id);
    let key = match key {
        Some(draft) => draft,
        None => super::api::key(&ctx.db, &c).await.map_err(Error)?.expose_secret().to_string(),
    };
    blocking(move || super::api::models_with_key(&c, &key).map_err(Error)).await
}

/// The checked change of a save, computed on one config snapshot.
enum Plan {
    Reused(Value),
    Publish { next: Config, c: AiConnection, fresh: Option<String>, secret: Option<String>, retired: Option<String> },
}

fn plan(snapshot: &Config, p: &Value) -> Result<Plan> {
    let mut c = parse_connection(&p["connection"])?;
    validate_catalog(&c.models)?;
    if c.name.trim().is_empty() || c.name.chars().count() > 80 || c.id.len() > 100 {
        bail!("请填写连接名称（最多 80 字）");
    }
    backend(&c).map_err(Error)?;
    if c.driver == "api" {
        shishi_api::Endpoint::parse(&c.base_url).map_err(Error)?;
    } else if c.executable.trim().is_empty() {
        bail!("请选择程序路径");
    }
    if c.id.is_empty() && c.driver != "codex" {
        bail!("默认连接只支持 Codex");
    }
    let mut next = snapshot.clone();
    let previous = if c.id.is_empty() { Some(default_codex(&next)) } else { next.connection(&c.id).cloned() };
    if let Some(expected) = p.get("expected") {
        let expected: Option<AiConnection> =
            serde_json::from_value(expected.clone()).map_err(|_| Error::new("连接编辑快照无效，请重新打开编辑"))?;
        if previous != expected {
            bail!("连接已被修改，请重新打开编辑");
        }
    }
    if previous.is_none() && c.driver == "dsh" {
        let identity = launcher_identity(Path::new(&c.executable));
        if let Some(existing) = next
            .ai_connections
            .iter()
            .find(|old| old.driver == "dsh" && launcher_identity(Path::new(&old.executable)) == identity)
        {
            return Ok(Plan::Reused(json!({"connection":existing,"config":next,"reused":true})));
        }
    }
    if previous.is_none() && next.ai_connections.len() >= 20 {
        bail!("最多配置 20 个附加连接");
    }
    if previous.as_ref().is_some_and(|old| old.driver != c.driver) && referenced(&next, &c.id) {
        bail!("连接仍被用途引用，请先切换用途再修改接入类型");
    }
    // A caller cannot replace a reference with another connection's secret.
    c.credential_id = previous.as_ref().and_then(|v| v.credential_id.clone());
    let (key, clear) = (p["key"].as_str().filter(|s| !s.is_empty()), p["clear_key"] == true);
    let replacing = key.is_some() || clear;
    let moved = previous.as_ref().is_some_and(|old| old.base_url != c.base_url && old.driver == "api");
    if c.driver == "api" && !replacing && c.key_env.is_none() && moved {
        bail!("地址已修改，请重新填写密钥后保存");
    }
    if key.is_some() && clear {
        bail!("请在替换密钥和清除密钥中选择一种操作");
    }
    if c.driver != "api" && replacing {
        bail!("仅 API 连接支持密钥");
    }
    let fresh = replacing.then(|| uuid::Uuid::new_v4().to_string());
    if replacing {
        c.credential_id = fresh.clone();
    }
    if c.id.is_empty() {
        next.codex = c.executable.clone();
        next.codex_name = c.name.clone();
        next.codex_models = c.models.clone();
    } else if let Some(row) = next.ai_connections.iter_mut().find(|v| v.id == c.id) {
        *row = c.clone();
    } else {
        next.ai_connections.push(c.clone());
    }
    let retired = previous.filter(|_| replacing).map(|old| old.credential_id.unwrap_or(old.id));
    Ok(Plan::Publish { next, c, fresh, secret: key.map(str::to_string), retired })
}

/// `connection_save`: publishes only while the config is still the planned
/// snapshot; the unused one of the fresh and retired references is removed.
pub async fn save_connection(ctx: &Arc<Ctx>, p: Value) -> Result<Value> {
    let snapshot = ctx.config.get();
    let (planned, input) = (snapshot.clone(), p);
    let planned = blocking(move || plan(&planned, &input)).await?;
    let (next, c, fresh, secret, retired) = match planned {
        Plan::Reused(value) => return Ok(value),
        Plan::Publish { next, c, fresh, secret, retired } => (next, c, fresh, secret, retired),
    };
    if let (Some(secret), Some(reference)) = (secret, fresh.clone()) {
        ctx.db.set_credential(reference, secret).await?;
    }
    let published = ctx
        .config
        .update(&ctx.db, |current| {
            if *current != *snapshot {
                bail!("连接已被修改，请重新打开编辑");
            }
            *current = next;
            Ok(())
        })
        .await;
    if let Some(reference) = if published.is_ok() { retired } else { fresh } {
        forget(ctx, reference).await;
    }
    Ok(json!({"connection":c,"config":*published?}))
}

/// `connection_delete`: routes using the connection become unconfigured.
pub async fn delete_connection(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let id = p["id"].as_str().ok_or_else(|| Error::new("缺少连接编号"))?.to_string();
    let mut removed = None;
    let config = ctx
        .config
        .update(&ctx.db, |next| {
            clear_routes(next, &id);
            removed = next.connection(&id).cloned();
            if id.is_empty() {
                next.codex.clear();
                next.codex_models.clear();
            } else {
                next.ai_connections.retain(|v| v.id != id);
            }
            Ok(())
        })
        .await?;
    if let Some(old) = removed {
        forget(ctx, old.credential_id.unwrap_or(old.id)).await;
    }
    Ok(json!({"config":*config}))
}

/// The comparable identity of a launcher path (case- and separator-blind).
fn launcher_identity(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').trim_start_matches(r"\\?\").to_lowercase()
}

fn desktop_candidate(path: &Path) -> Option<Value> {
    const LAUNCHER: &str = "resources/runtime/cli/bin/dsh.cmd";
    let launcher = match () {
        _ if path.file_name().is_some_and(|v| v.eq_ignore_ascii_case("DeepSeek Harness.exe")) => {
            path.parent()?.join(LAUNCHER)
        }
        _ if path.is_dir() => path.join(LAUNCHER),
        _ => path.to_path_buf(),
    };
    // The launcher sits five levels below the folder of the desktop exe.
    let root = launcher.ancestors().nth(5)?;
    let valid =
        launcher.is_file() && root.join("DeepSeek Harness.exe").is_file() && root.join("resources/app.asar").is_file();
    if !valid || launcher_identity(&launcher) != launcher_identity(&root.join(LAUNCHER)) {
        return None;
    }
    Some(json!({"path":launcher.components().collect::<PathBuf>().to_string_lossy(),"label":"DeepSeek Harness 桌面版"}))
}

fn candidates(paths: Vec<PathBuf>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    paths
        .iter()
        .filter_map(|p| desktop_candidate(p))
        .filter(|v| seen.insert(launcher_identity(Path::new(v["path"].as_str().unwrap()))))
        .collect()
}

/// `detect` for DSH: desktop launchers from the saved path, the usual
/// install locations, PATH and the uninstall registry, with versions.
pub fn detect_dsh(saved: &str) -> Value {
    let mut paths = vec![PathBuf::from(saved)];
    for key in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(key) {
            paths.push(PathBuf::from(&base).join("DeepSeek Harness"));
            paths.push(PathBuf::from(base).join("Programs/DeepSeek Harness"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path).map(|p| p.join("dsh.cmd")));
    }
    paths.extend(windows::install_locations());
    let mut result = candidates(paths);
    for row in &mut result {
        let launcher = PathBuf::from(row["path"].as_str().unwrap());
        let version =
            launcher.ancestors().nth(5).and_then(|root| windows::product_version(&root.join("DeepSeek Harness.exe")));
        row["version"] = version.map_or(Value::Null, Value::String);
    }
    json!({"candidates":result})
}

#[cfg(not(windows))]
mod windows {
    use std::path::{Path, PathBuf};

    pub fn install_locations() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn product_version(_: &Path) -> Option<String> {
        None
    }
}

/// Uninstall registry and file version resources for DSH discovery.
#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
        RRF_RT_REG_SZ,
    };

    fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
        text.as_ref().encode_wide().chain(Some(0)).collect()
    }

    fn text(units: &[u16]) -> String {
        String::from_utf16_lossy(&units[..units.iter().position(|u| *u == 0).unwrap_or(units.len())])
    }

    /// A string value (`REG_EXPAND_SZ` comes back expanded).
    fn value(key: HKEY, subkey: *const u16, name: &str) -> Option<String> {
        let name = wide(name);
        let mut size = 0u32;
        let get = |data: *mut u16, size: &mut u32| unsafe {
            RegGetValueW(key, subkey, name.as_ptr(), RRF_RT_REG_SZ, null_mut(), data.cast(), size)
        };
        if get(null_mut(), &mut size) != ERROR_SUCCESS {
            return None;
        }
        let mut units = vec![0u16; size as usize / 2 + 1];
        let mut size = units.len() as u32 * 2;
        (get(units.as_mut_ptr(), &mut size) == ERROR_SUCCESS).then(|| text(&units))
    }

    /// `InstallLocation` of every uninstall entry named like `*DeepSeek*Harness*`.
    pub fn install_locations() -> Vec<PathBuf> {
        const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
        const WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
        let mut found = Vec::new();
        for (root, path) in [(HKEY_CURRENT_USER, KEY), (HKEY_LOCAL_MACHINE, KEY), (HKEY_LOCAL_MACHINE, WOW)] {
            let mut key: HKEY = null_mut();
            if unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
                continue;
            }
            for index in 0.. {
                // Registry key names are at most 255 characters.
                let mut name = [0u16; 256];
                let mut length = name.len() as u32;
                let status = unsafe {
                    RegEnumKeyExW(
                        key,
                        index,
                        name.as_mut_ptr(),
                        &mut length,
                        null(),
                        null_mut(),
                        null_mut(),
                        null_mut(),
                    )
                };
                if status == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if status != ERROR_SUCCESS {
                    continue;
                }
                let display = value(key, name.as_ptr(), "DisplayName").unwrap_or_default().to_lowercase();
                let matches = display.find("deepseek").is_some_and(|at| display[at + 8..].contains("harness"));
                if let Some(location) = value(key, name.as_ptr(), "InstallLocation").filter(|_| matches) {
                    if !location.trim().is_empty() {
                        found.push(PathBuf::from(location));
                    }
                }
            }
            unsafe { RegCloseKey(key) };
        }
        found
    }

    /// The `ProductVersion` string of an executable, as Explorer shows it.
    pub fn product_version(exe: &Path) -> Option<String> {
        let path = wide(exe);
        let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), null_mut()) };
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
            return None;
        }
        // The result points into `data`; `length` counts bytes for binary
        // values and characters for strings.
        let query = |block: &str| -> Option<(*const u16, usize)> {
            let (mut pointer, mut length) = (null_mut(), 0u32);
            let found =
                unsafe { VerQueryValueW(data.as_ptr().cast(), wide(block).as_ptr(), &mut pointer, &mut length) };
            (found != 0 && length > 0).then_some((pointer as *const u16, length as usize))
        };
        // The declared language first, then US English Unicode as .NET does.
        let mut blocks = Vec::new();
        if let Some((pair, _)) = query(r"\VarFileInfo\Translation").filter(|(_, bytes)| *bytes >= 4) {
            blocks.push(unsafe { format!("{:04x}{:04x}", *pair, *pair.add(1)) });
        }
        blocks.push("040904b0".into());
        blocks.iter().find_map(|block| {
            let (pointer, length) = query(&format!(r"\StringFileInfo\{block}\ProductVersion"))?;
            Some(text(unsafe { std::slice::from_raw_parts(pointer, length) }))
        })
    }
}

/// `codex_status` / `ai_connection_status`: the given draft connection, or
/// the connection of the analysis route.
pub async fn status(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let connection = match p.get("connection") {
        Some(value) => parse_connection(value)?,
        None => connection_by_id(&ctx.config.get(), &ctx.config.get().model_choice("classify").connection_id)?,
    };
    let root = ctx.root.clone();
    match connection.driver.as_str() {
        "api" => super::api::status(&ctx.db, &connection).await.map_err(Error),
        "codex" => Ok(super::codex::status()),
        "dsh" => blocking(move || super::dsh::status(&connection, &root).map_err(Error)).await,
        _ => bail!("{}", unsupported(&connection)),
    }
}

/// `analysis_session_open`: only Codex sessions open automatically.
pub fn open_session(p: &Value) -> Result<Value> {
    let driver = p["driver"].as_str().unwrap_or("");
    let thread = p["thread_id"].as_str().ok_or_else(|| Error::new("缺少会话编号"))?;
    uuid::Uuid::parse_str(thread).map_err(|_| Error::new("无效会话编号"))?;
    if driver != "codex" {
        bail!("此接入不支持自动打开会话，请在对应客户端按会话编号查看");
    }
    super::codex::open_session(thread).map_err(Error)?;
    Ok(json!(true))
}

/// `dsh_initialize_acp` and `dsh_workspace_plugin`.
pub async fn dsh(ctx: &Arc<Ctx>, method: &str, p: &Value) -> Result<Value> {
    let connection = parse_connection(&p["connection"])?;
    let (root, confirmed, install) = (ctx.root.clone(), p["confirmed"] == true, p["install"] == true);
    let initialize = method == "dsh_initialize_acp";
    blocking(move || {
        if initialize {
            super::dsh::initialize_acp(&connection, &root, confirmed)
        } else {
            super::dsh::workspace_plugin(&connection, install, confirmed)
        }
        .map_err(Error)
    })
    .await
}

#[cfg(test)]
mod tests;
