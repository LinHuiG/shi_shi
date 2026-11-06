//! The desktop workspace bridge plugin: installed only on explicit request;
//! analysis and handling only publish the ids of persisted sessions.
use serde_json::{json, Value};
use std::{fs, path::Path};
use yaml_serde::Value as Yaml;

const PLUGIN: &str = include_str!("../../../examples/dsh-workspace-bridge/index.js");
const PACKAGE: &str = include_str!("../../../examples/dsh-workspace-bridge/package.json");
const ID: &str = "shishi-workspace-bridge";

/// The bridge row of the desktop patch, later `insert` rows overriding fields.
fn plugin_row(rows: &[Yaml]) -> Option<Yaml> {
    let mut result: Option<Yaml> = None;
    let entries =
        rows.iter().flat_map(|row| row["insert"].as_sequence().map_or(std::slice::from_ref(row), |a| a.as_slice()));
    for entry in entries.filter(|e| e["id"].as_str() == Some(ID)) {
        match (result.as_mut().and_then(Yaml::as_mapping_mut), entry.as_mapping()) {
            (Some(current), Some(next)) => current.extend(next.clone()),
            _ => result = Some(entry.clone()),
        }
    }
    result
}

fn rows(home: &Path) -> Result<(std::path::PathBuf, String, Vec<Yaml>), String> {
    let path = home.join("profiles/desktop/cordis.patch.yml");
    let text = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        text => text.map_err(|_| "DSH desktop 配置不可读")?,
    };
    let rows = match yaml_serde::from_str(&text).map_err(|_| "DSH desktop YAML 损坏，未修改")? {
        Yaml::Null => vec![],
        value => value.as_sequence().ok_or("DSH desktop 配置不是列表")?.clone(),
    };
    Ok((path, text, rows))
}

pub fn status(home: &Path) -> Result<Value, String> {
    let plugin = home.join("plugins/shishi-workspace-bridge/index.js");
    let registered = plugin_row(&rows(home)?.2).is_some_and(|r| {
        r["disabled"].as_bool() != Some(true)
            && r["name"].as_str().and_then(|s| Path::new(s).canonicalize().ok()) == plugin.canonicalize().ok()
    });
    let current = fs::read_to_string(&plugin).ok().as_deref() == Some(PLUGIN)
        && fs::read_to_string(plugin.with_file_name("package.json")).ok().as_deref() == Some(PACKAGE);
    Ok(json!({"installed": registered && current, "outdated": registered && !current,
        "index_dir": home.join("ss-session-links").to_string_lossy(), "restart_required": true}))
}

/// Write the plugin and append its row, keeping the user's document verbatim
/// and a backup of it.
pub fn install(home: &Path) -> Result<Value, String> {
    let current = status(home)?;
    if current["installed"] == true {
        return Ok(current);
    }
    let (path, text, rows) = rows(home)?;
    if plugin_row(&rows).is_some() && current["outdated"] != true {
        return Err("已有 SS 插件配置不兼容或被禁用，请检查 DSH 插件设置；未覆盖".into());
    }
    let dir = home.join("plugins/shishi-workspace-bridge");
    let index = home.join("ss-session-links");
    let written = fs::create_dir_all(&dir)
        .and_then(|_| fs::create_dir_all(&index))
        .and_then(|_| fs::create_dir_all(path.parent().unwrap()))
        .and_then(|_| fs::write(dir.join("index.js"), PLUGIN))
        .and_then(|_| fs::write(dir.join("package.json"), PACKAGE));
    written.map_err(|e| e.to_string())?;
    // An outdated registered bridge only needs its files; its row stays.
    if current["outdated"] == true {
        return status(home);
    }
    let row = json!({"id": ID, "name": dir.join("index.js").to_string_lossy(), "config": {"indexDir": index.to_string_lossy()}});
    let addition = yaml_serde::to_string(&vec![json!({"insert": [row]})]).map_err(|e| e.to_string())?;
    if !text.is_empty() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        fs::write(path.with_file_name(format!("cordis.patch.ss-backup-{stamp}.yml")), &text)
            .map_err(|e| e.to_string())?;
    }
    let next = match text.trim() {
        "" | "[]" | "null" => addition,
        _ => format!("{}\n{addition}", text.trim_end_matches(['\n', '\r'])),
    };
    // Parse the composed document before publishing it (rejects YAML end markers).
    let check: Vec<Yaml> =
        yaml_serde::from_str(&next).map_err(|_| "无法安全追加插件，请在 DSH 中手动安装；原配置未修改")?;
    if check.len() != rows.len() + 1 {
        return Err("追加插件校验失败，原配置未修改".into());
    }
    let temp = path.with_extension("ss-install.tmp");
    fs::write(&temp, next).and_then(|_| fs::rename(temp, &path)).map_err(|e| e.to_string())?;
    status(home)
}

pub fn publish(home: &Path, session: &str) -> Result<(), String> {
    if session.is_empty() || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("无效 DSH 会话编号".into());
    }
    let dir = home.join("ss-session-links");
    let link = json!({"version": 1, "session_id": session}).to_string();
    fs::create_dir_all(&dir)
        .and_then(|_| fs::write(dir.join(format!("{session}.json")), link))
        .map_err(|e| e.to_string())
}

/// The DSH projects the bridge last snapshotted.
pub fn projects(home: &Path) -> Result<Vec<Value>, String> {
    let raw = match fs::read(home.join("ss-session-links/.workspaces.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        raw => raw.map_err(|_| "DSH 工作区快照不可读")?,
    };
    let value: Value = serde_json::from_slice(&raw).map_err(|_| "DSH 工作区快照损坏")?;
    if value["version"] != 1 {
        return Err("DSH 工作区快照版本不兼容".into());
    }
    let rows = value["projects"].as_array().ok_or("DSH 工作区列表无效")?;
    let valid = |p: &&Value| {
        p["id"].as_str().is_some_and(|s| s.starts_with("dsh:"))
            && p["name"].is_string()
            && p["cwd"].as_str().is_some_and(|s| Path::new(s).is_absolute() && Path::new(s).is_dir())
    };
    Ok(rows.iter().filter(valid).cloned().collect())
}

#[cfg(test)]
mod tests;
