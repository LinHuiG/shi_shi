//! Desktop-only native workspace bridge. Installation is explicitly requested;
//! normal analysis only publishes ids for already-persisted native sessions.
use serde_json::{json, Value};
use std::{fs, path::Path};
const PLUGIN: &str = include_str!("../../../examples/dsh-workspace-bridge/index.js");
const PACKAGE: &str = include_str!("../../../examples/dsh-workspace-bridge/package.json");
const ID: &str = "shishi-workspace-bridge";
fn plugin_row(rows: &[serde_yaml_ng::Value]) -> Option<serde_yaml_ng::Value> {
    let mut result: Option<serde_yaml_ng::Value> = None;
    for row in rows {
        let entries = row["insert"]
            .as_sequence()
            .map(|a| a.as_slice())
            .unwrap_or(std::slice::from_ref(row));
        for entry in entries {
            if entry["id"].as_str() != Some(ID) {
                continue;
            }
            if let Some(current) = result
                .as_mut()
                .and_then(serde_yaml_ng::Value::as_mapping_mut)
            {
                if let Some(next) = entry.as_mapping() {
                    for (k, v) in next {
                        current.insert(k.clone(), v.clone());
                    }
                }
            } else {
                result = Some(entry.clone());
            }
        }
    }
    result
}

fn rows(home: &Path) -> Result<(std::path::PathBuf, String, Vec<serde_yaml_ng::Value>), String> {
    let path = home.join("profiles/desktop/cordis.patch.yml");
    let text = match fs::read_to_string(&path) {
        Ok(s) if s.len() <= 2_000_000 => s,
        Ok(_) => return Err("DSH desktop 配置过大".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err("DSH desktop 配置不可读".into()),
    };
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&text).map_err(|_| "DSH desktop YAML 损坏，未修改")?;
    let rows = if value.is_null() {
        vec![]
    } else {
        value
            .as_sequence()
            .ok_or("DSH desktop 配置不是列表")?
            .clone()
    };
    Ok((path, text, rows))
}
pub fn status(home: &Path) -> Result<Value, String> {
    let (_, _, rows) = rows(home)?;
    let plugin = home.join("plugins/shishi-workspace-bridge/index.js");
    let installed = plugin_row(&rows).as_ref().is_some_and(|r| {
        r["id"].as_str() == Some(ID)
            && r["disabled"].as_bool() != Some(true)
            && r["name"]
                .as_str()
                .and_then(|s| Path::new(s).canonicalize().ok())
                == plugin.canonicalize().ok()
            && plugin.is_file()
    });
    Ok(
        json!({"installed":installed && fs::read_to_string(&plugin).ok().as_deref()==Some(PLUGIN) && fs::read_to_string(plugin.with_file_name("package.json")).ok().as_deref()==Some(PACKAGE),"index_dir":home.join("ss-session-links").to_string_lossy(),"restart_required":true}),
    )
}
pub fn install(home: &Path) -> Result<Value, String> {
    let (path, text, rows) = rows(home)?;
    if status(home)?["installed"] == true {
        return status(home);
    }
    if plugin_row(&rows).is_some() {
        return Err("已有 SS 插件配置不兼容或被禁用，请检查 DSH 插件设置；未覆盖".into());
    }
    let dir = home.join("plugins/shishi-workspace-bridge");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("index.js"), PLUGIN).map_err(|e| e.to_string())?;
    fs::write(dir.join("package.json"), PACKAGE).map_err(|e| e.to_string())?;
    let index = home.join("ss-session-links");
    fs::create_dir_all(&index).map_err(|e| e.to_string())?;
    let row = json!({"id":ID,"name":dir.join("index.js").to_string_lossy(),"config":{"indexDir":index.to_string_lossy()}});
    let addition =
        serde_yaml_ng::to_string(&vec![json!({"insert":[row]})]).map_err(|e| e.to_string())?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    // Preserve the exact user document and keep a recoverable pre-install copy.
    if !text.is_empty() {
        let backup = path.with_file_name(format!(
            "cordis.patch.ss-backup-{}.yml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::write(backup, &text).map_err(|e| e.to_string())?;
    }
    if fs::read_to_string(&path).unwrap_or_default() != text {
        return Err("DSH 配置已变化，请重试安装".into());
    }
    let next = if text.trim().is_empty() || text.trim() == "[]" || text.trim() == "null" {
        addition
    } else {
        format!(
            "{}\n{}",
            text.trim_end_matches(|c| c == '\n' || c == '\r'),
            addition
        )
    };
    // Parse the composed document before publication (also rejects YAML end markers).
    let check: Vec<serde_yaml_ng::Value> = serde_yaml_ng::from_str(&next)
        .map_err(|_| "无法安全追加插件，请在 DSH 中手动安装；原配置未修改")?;
    if check.len() != rows.len() + 1 {
        return Err("追加插件校验失败，原配置未修改".into());
    }
    let temp = path.with_extension("ss-install.tmp");
    fs::write(&temp, next).map_err(|e| e.to_string())?;
    fs::rename(temp, &path).map_err(|e| e.to_string())?;
    status(home)
}
pub fn publish(home: &Path, session: &str) -> Result<(), String> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("无效 DSH 会话编号".into());
    }
    let dir = home.join("ss-session-links");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(
        dir.join(format!("{session}.json")),
        json!({"version":1,"session_id":session}).to_string(),
    )
    .map_err(|e| e.to_string())
}
pub fn projects(home: &Path) -> Result<Vec<Value>, String> {
    let path = home.join("ss-session-links/.workspaces.json");
    let raw = match fs::read(&path) {
        Ok(raw) if raw.len() <= 1_000_000 => raw,
        Ok(_) => return Err("DSH 工作区快照过大".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err("DSH 工作区快照不可读".into()),
    };
    let value: Value = serde_json::from_slice(&raw).map_err(|_| "DSH 工作区快照损坏")?;
    if value["version"] != 1 {
        return Err("DSH 工作区快照版本不兼容".into());
    }
    let rows = value["projects"]
        .as_array()
        .filter(|r| r.len() <= 1000)
        .ok_or("DSH 工作区列表无效")?;
    Ok(rows
        .iter()
        .filter(|p| {
            p["id"].as_str().is_some_and(|s| s.starts_with("dsh:"))
                && p["name"].is_string()
                && p["cwd"]
                    .as_str()
                    .is_some_and(|s| Path::new(s).is_absolute() && Path::new(s).is_dir())
        })
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ss-workspace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn explicit_install_preserves_user_yaml_is_idempotent_and_checks_files() {
        let root = root();
        let profile = root.join("profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        let original = "# user comment\n- id: fixture\n  name: fixture-plugin\n";
        fs::write(profile.join("cordis.patch.yml"), original).unwrap();
        assert_eq!(status(&root).unwrap()["installed"], false);
        assert!(!root.join("plugins").exists());
        assert_eq!(install(&root).unwrap()["installed"], true);
        let once = fs::read_to_string(profile.join("cordis.patch.yml")).unwrap();
        assert!(once.starts_with(original));
        assert_eq!(install(&root).unwrap()["installed"], true);
        assert_eq!(
            fs::read_to_string(profile.join("cordis.patch.yml")).unwrap(),
            once
        );
        fs::remove_file(root.join("plugins/shishi-workspace-bridge/index.js")).unwrap();
        assert_eq!(status(&root).unwrap()["installed"], false);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn malformed_configuration_and_unsafe_session_ids_fail_without_overwrite() {
        let root = root();
        let path = root.join("profiles/desktop/cordis.patch.yml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[ broken").unwrap();
        assert!(install(&root).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[ broken");
        assert!(publish(&root, "../outside").is_err());
        publish(&root, "synthetic-session").unwrap();
        let link: Value = serde_json::from_slice(
            &fs::read(root.join("ss-session-links/synthetic-session.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(link, json!({"version":1,"session_id":"synthetic-session"}));
        fs::remove_dir_all(root).unwrap();
    }
}
