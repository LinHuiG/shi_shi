//! Connection editing is independent of the settings form. Draft secrets never
//! enter Config; replacement secrets get a new reference before atomic save.
use crate::{ai_backend::Connection, credentials::CredentialStore, engine::App, model::*, prompts};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

fn validate_catalog(models: &[Value]) -> Result<(), String> {
    if models.is_empty() || models.len() > 1000 {
        return Err("请先检测模型，或填写至少一个模型编号（最多 1000 个）".into());
    }
    if serde_json::to_vec(models).map_err(|e| e.to_string())?.len() > 512_000 {
        return Err("模型目录过大".into());
    }
    let mut ids = BTreeSet::new();
    for m in models {
        let id = m["model"].as_str().unwrap_or("");
        if id.trim().is_empty() || id.chars().count() > 300 || !ids.insert(id) {
            return Err("模型编号为空、过长或重复".into());
        }
        if m.to_string().len() > 8192 {
            return Err("模型信息过长".into());
        }
    }
    Ok(())
}
fn referenced(c: &Config, id: &str) -> bool {
    PURPOSES
        .iter()
        .any(|kind| c.for_kind(kind).selected_connection == id)
}
impl App {
    pub fn connection_models(&self, p: &Value) -> Result<Value, String> {
        let mut c: Connection =
            serde_json::from_value(p["connection"].clone()).map_err(|e| e.to_string())?;
        if c.driver != "api" {
            return crate::ai_backend::backend(&c)?.models(&c, &self.root);
        }
        let previous = self
            .state
            .lock()
            .unwrap()
            .config
            .ai_connections
            .iter()
            .find(|v| v.id == c.id)
            .cloned();
        let key = p["key"].as_str().filter(|v| !v.is_empty());
        if key.is_none()
            && c.key_env.is_none()
            && previous
                .as_ref()
                .is_some_and(|old| old.base_url != c.base_url)
        {
            return Err("地址已修改，请重新填写密钥后检测".into());
        }
        c.credential_id = previous.and_then(|v| v.credential_id);
        crate::ai_backend::api::models_with_key(&c, &self.root, key)
    }
    pub fn save_connection(&self, p: &Value) -> Result<Value, String> {
        let mut c: Connection =
            serde_json::from_value(p["connection"].clone()).map_err(|e| e.to_string())?;
        validate_catalog(&c.models)?;
        if c.name.trim().is_empty() || c.name.chars().count() > 80 || c.id.len() > 100 {
            return Err("请填写连接名称（最多 80 字）".into());
        }
        crate::ai_backend::backend(&c)?;
        if c.driver == "api" {
            shishi_api::Endpoint::parse(&c.base_url).map_err(|e| e.to_string())?;
        } else if c.executable.trim().is_empty() {
            return Err("请选择程序路径".into());
        }
        if c.id.is_empty() && c.driver != "codex" {
            return Err("默认连接只支持 Codex".into());
        }
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        let previous = if c.id.is_empty() {
            Some(crate::ai_backend::default_codex_connection(&next.config))
        } else {
            next.config
                .ai_connections
                .iter()
                .find(|v| v.id == c.id)
                .cloned()
        };
        if let Some(expected) = p.get("expected") {
            let expected: Option<Connection> = serde_json::from_value(expected.clone())
                .map_err(|_| "连接编辑快照无效，请重新打开编辑")?;
            if previous != expected {
                return Err("连接已被修改，请重新打开编辑".into());
            }
        }
        if previous.is_none() && c.driver == "dsh" {
            if let Some(existing) = next.config.ai_connections.iter().find(|old| {
                old.driver == "dsh"
                    && launcher_identity(Path::new(&old.executable))
                        == launcher_identity(Path::new(&c.executable))
            }) {
                return Ok(json!({"connection":existing,"config":next.config,"reused":true}));
            }
        }
        if previous.is_none() && next.config.ai_connections.len() >= 20 {
            return Err("最多配置 20 个附加连接".into());
        }
        if previous.as_ref().is_some_and(|old| old.driver != c.driver)
            && referenced(&next.config, &c.id)
        {
            return Err("连接仍被用途引用，请先切换用途再修改接入类型".into());
        }
        // A caller cannot replace a reference with another connection's secret.
        c.credential_id = previous.as_ref().and_then(|v| v.credential_id.clone());
        let store = CredentialStore::new(crate::storage::data_dir(&self.root).join("credentials"));
        let key = p["key"].as_str().filter(|s| !s.is_empty());
        if c.driver == "api"
            && key.is_none()
            && c.key_env.is_none()
            && p["clear_key"] != true
            && previous
                .as_ref()
                .is_some_and(|old| old.base_url != c.base_url && old.driver == "api")
        {
            return Err("地址已修改，请重新填写密钥后保存".into());
        }
        if key.is_some() && p["clear_key"] == true {
            return Err("请在替换密钥和清除密钥中选择一种操作".into());
        }
        if c.driver != "api" && (key.is_some() || p["clear_key"] == true) {
            return Err("仅 API 连接支持密钥".into());
        }
        let replacing = key.is_some() || p["clear_key"] == true;
        let fresh = replacing.then(id);
        if let (Some(secret), Some(reference)) = (key, fresh.as_ref()) {
            store.set(reference, secret).map_err(|e| e.to_string())?;
        }
        if let Some(reference) = &fresh {
            c.credential_id = Some(reference.clone());
        }
        if c.id.is_empty() {
            next.config.codex = c.executable.clone();
            next.config.codex_name = c.name.clone();
            next.config.codex_models = c.models.clone();
        } else if let Some(row) = next.config.ai_connections.iter_mut().find(|v| v.id == c.id) {
            *row = c.clone();
        } else {
            next.config.ai_connections.push(c.clone());
        }
        if let Err(error) = self.save(&next) {
            if let Some(reference) = fresh {
                let _ = store.delete(&reference);
            }
            return Err(error);
        }
        *locked = next;
        if replacing {
            if let Some(old) = &previous {
                let _ = store.delete(old.credential_id.as_deref().unwrap_or(&old.id));
            }
        }
        Ok(json!({"connection":c,"config":locked.config}))
    }
    pub fn delete_connection(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("缺少连接编号")?;
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        next.config.clear_connection_routes(id);
        let removed = next
            .config
            .ai_connections
            .iter()
            .find(|v| v.id == id)
            .cloned();
        if id.is_empty() {
            next.config.codex.clear();
            next.config.codex_models.clear();
        } else {
            next.config.ai_connections.retain(|v| v.id != id);
        }
        self.save(&next)?;
        *locked = next;
        if let Some(old) = removed {
            let _ = CredentialStore::new(crate::storage::data_dir(&self.root).join("credentials"))
                .delete(old.credential_id.as_deref().unwrap_or(&old.id));
        }
        Ok(json!({"config":locked.config}))
    }
    pub fn save_prompt(&self, p: &Value) -> Result<Value, String> {
        let target = p["target"].as_str().ok_or("缺少提示词类型")?;
        let text = p["text"].as_str().ok_or("缺少提示词")?;
        if text.chars().count() > 20000 {
            return Err("每套提示词最多 20000 字".into());
        }
        let mut locked = self.state.lock().unwrap();
        if prompts::current(&locked, target).map(|v| v.id.as_str()) != p["expected"].as_str() {
            return Err("提示词已更新，请重新打开编辑".into());
        }
        let mut next = locked.clone();
        let mut config = next.config.clone();
        prompts::set_content(&mut config, target, text.into())?;
        if target == "handling" {
            crate::evolution::sync_handling_prompt(&mut next, text, "编辑处置提示词");
        }
        prompts::record_config_changes(&mut next, &config);
        next.config = config;
        self.save(&next)?;
        *locked = next;
        Ok(json!(true))
    }
}

pub fn launcher_identity(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .trim_start_matches(r"\\?\")
        .to_lowercase()
}
fn desktop_candidate(path: &Path) -> Option<Value> {
    let launcher = if path
        .file_name()
        .is_some_and(|v| v.eq_ignore_ascii_case("DeepSeek Harness.exe"))
    {
        path.parent()?.join("resources/runtime/cli/bin/dsh.cmd")
    } else if path.is_dir() {
        path.join("resources/runtime/cli/bin/dsh.cmd")
    } else {
        path.to_path_buf()
    };
    let root = launcher.parent()?.parent()?.parent()?.parent()?.parent()?;
    if !launcher.is_file()
        || !root.join("DeepSeek Harness.exe").is_file()
        || !root.join("resources/app.asar").is_file()
    {
        return None;
    }
    if launcher_identity(&launcher)
        != launcher_identity(&root.join("resources/runtime/cli/bin/dsh.cmd"))
    {
        return None;
    }
    Some(
        json!({"path":launcher.components().collect::<PathBuf>().to_string_lossy(),"label":"DeepSeek Harness 桌面版"}),
    )
}
fn candidates(paths: Vec<PathBuf>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    paths
        .iter()
        .filter_map(|p| desktop_candidate(p))
        .filter(|v| seen.insert(launcher_identity(Path::new(v["path"].as_str().unwrap()))))
        .collect()
}
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
    // Fixed PowerShell reads registry/file metadata with UTF-8 output, including
    // non-ASCII installation paths. Input paths are data in an environment value.
    let script = r#"[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
$items=@($env:SHISHI_DISCOVERY_PATHS | ConvertFrom-Json)
foreach($key in @('HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*')) {
  $items+=@(Get-ItemProperty $key -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like '*DeepSeek*Harness*' } | ForEach-Object { $_.InstallLocation })
}
$rows=@(foreach($item in $items) { if([string]::IsNullOrWhiteSpace($item)){continue}; $root=$item
  if([IO.Path]::GetFileName($item) -eq 'dsh.cmd'){ for($i=0;$i -lt 5;$i++){ $root=Split-Path -LiteralPath $root } }
  elseif([IO.Path]::GetFileName($item) -eq 'DeepSeek Harness.exe'){ $root=Split-Path -LiteralPath $item }
  $exe=Join-Path $root 'DeepSeek Harness.exe'
  $version=''; if(Test-Path -LiteralPath $exe -PathType Leaf){$version=[Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion}
  @{path=$item;version=$version}
})
ConvertTo-Json -InputObject $rows -Compress"#;
    let temp = std::env::temp_dir().join(format!("shishi-discovery-{}", id()));
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env(
            "SHISHI_DISCOVERY_PATHS",
            serde_json::to_string(&paths).unwrap(),
        );
    let metadata = crate::process::run(command, None, 10, &temp);
    let _ = std::fs::remove_dir(&temp);
    let mut versions = std::collections::BTreeMap::new();
    if let Ok((output, _)) = metadata {
        if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&output) {
            for row in rows {
                if let Some(path) = row["path"].as_str() {
                    let path = PathBuf::from(path);
                    if let Some(candidate) = desktop_candidate(&path) {
                        versions.insert(
                            launcher_identity(Path::new(candidate["path"].as_str().unwrap())),
                            row["version"].clone(),
                        );
                    }
                    paths.push(path);
                }
            }
        }
    }
    let mut result = candidates(paths);
    for row in &mut result {
        row["version"] = versions
            .get(&launcher_identity(Path::new(row["path"].as_str().unwrap())))
            .cloned()
            .unwrap_or(Value::Null);
    }
    json!({"candidates":result})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_codex_edit_uses_its_own_snapshot_and_normalizes_default_fields() {
        let root = std::env::temp_dir().join(format!("shishi-default-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut state = app.state.lock().unwrap();
            state.config.codex = "synthetic-original.exe".into();
            state.config.codex_models = vec![json!({"model":"synthetic"})];
            state.config.ai_connections.push(Connection {
                id: "api".into(),
                name: "Synthetic API".into(),
                driver: "api".into(),
                base_url: "https://example.com/v1".into(),
                ..Default::default()
            });
            state.config.selected_connection = "api".into();
        }
        let original =
            crate::ai_backend::default_codex_connection(&app.state.lock().unwrap().config);
        let mut expected = json!(original);
        expected.as_object_mut().unwrap().remove("stream"); // Older/default UI descriptor.
        let mut edited = original.clone();
        edited.name = "Renamed Codex".into();
        app.save_connection(&json!({"connection":edited,"expected":expected}))
            .unwrap();
        assert_eq!(app.state.lock().unwrap().config.codex_name, "Renamed Codex");
        assert!(app
            .save_connection(&json!({"connection":original,"expected":expected}))
            .unwrap_err()
            .contains("连接已被修改"));
        assert_eq!(
            app.state.lock().unwrap().config.ai_connections[0].name,
            "Synthetic API"
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn desktop_detection_excludes_legacy_and_deduplicates() {
        let root = std::env::temp_dir().join(format!("shishi-discovery-{}", id()));
        let launcher = root.join("resources/runtime/cli/bin/dsh.cmd");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::fs::write(&launcher, "fixture").unwrap();
        assert!(candidates(vec![root.clone()]).is_empty());
        std::fs::write(root.join("DeepSeek Harness.exe"), "fixture").unwrap();
        std::fs::write(root.join("resources/app.asar"), "fixture").unwrap();
        assert_eq!(candidates(vec![root.clone(), launcher]).len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn connection_save_requires_models_and_deletion_allows_empty_routes() {
        let root = std::env::temp_dir().join(format!("shishi-connections-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let mut c = json!({"id":"fixture","name":"Fixture","driver":"api","base_url":"https://example.com/v1"});
        assert!(app.save_connection(&json!({"connection":c})).is_err());
        c["models"] = json!([{"model":"fixture-model"}]);
        app.save_connection(&json!({"connection":c,"expected":null}))
            .unwrap();
        assert!(app
            .save_connection(&json!({"connection":c,"expected":null}))
            .is_err());
        app.delete_connection(&json!({"id":""})).unwrap();
        app.delete_connection(&json!({"id":"fixture"})).unwrap();
        assert!(App::load(root.clone())
            .unwrap()
            .state
            .lock()
            .unwrap()
            .config
            .ai_connections
            .is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_replacement_is_not_committed_when_state_save_fails() {
        let root = std::env::temp_dir().join(format!("shishi-secret-save-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let connection = json!({"id":"fixture","name":"Fixture","driver":"api","base_url":"https://example.com/v1","models":[{"model":"fixture"}]});
        let saved = app
            .save_connection(
                &json!({"connection":connection,"expected":null,"key":"synthetic-before"}),
            )
            .unwrap();
        let old = saved["connection"].clone();
        let store = CredentialStore::new(root.join("data/credentials"));
        let reference = old["credential_id"].as_str().unwrap();
        assert_eq!(
            store.get(reference).unwrap().unwrap().expose_secret(),
            "synthetic-before"
        );
        std::fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app
            .save_connection(&json!({"connection":old,"expected":old,"key":"synthetic-after"}))
            .is_err());
        assert_eq!(
            serde_json::to_value(&app.state.lock().unwrap().config.ai_connections[0]).unwrap(),
            old
        );
        assert_eq!(
            store.get(reference).unwrap().unwrap().expose_secret(),
            "synthetic-before"
        );
        assert!(!std::fs::read_to_string(root.join("data/state.json"))
            .unwrap()
            .contains("synthetic-before"));
        std::fs::remove_dir(root.join("data/state.next.json")).unwrap();
        let cleared = app
            .save_connection(&json!({"connection":old,"expected":old,"clear_key":true}))
            .unwrap();
        assert!(store
            .get(cleared["connection"]["credential_id"].as_str().unwrap())
            .unwrap()
            .is_none());
        assert!(store.get(reference).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_edit_requires_current_version_and_preserves_other_settings() {
        let root = std::env::temp_dir().join(format!("shishi-prompt-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let version = prompts::current(&app.state.lock().unwrap(), "classify")
            .unwrap()
            .id
            .clone();
        app.save_prompt(
            &json!({"target":"classify","expected":version,"text":"Synthetic revised prompt"}),
        )
        .unwrap();
        assert!(app
            .save_prompt(&json!({"target":"classify","expected":version,"text":"Stale prompt"}))
            .is_err());
        let state = app.state.lock().unwrap();
        assert_eq!(state.config.classify_prompt, "Synthetic revised prompt");
        assert_eq!(
            state.config.max_input_chars,
            Config::default().max_input_chars
        );
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_api_address_never_reuses_saved_key_for_discovery() {
        let root = std::env::temp_dir().join(format!("shishi-api-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let c = json!({"id":"fixture","name":"Fixture","driver":"api","base_url":"https://example.com/v1","models":[{"model":"fixture"}]});
        let saved = app
            .save_connection(&json!({"connection":c,"expected":null,"key":"synthetic-fixture"}))
            .unwrap();
        let mut edited = saved["connection"].clone();
        edited["base_url"] = json!("https://other.example/v1");
        assert_eq!(
            app.connection_models(&json!({"connection":edited}))
                .unwrap_err(),
            "地址已修改，请重新填写密钥后检测"
        );
        assert!(app
            .save_connection(&json!({"connection":edited,"expected":saved["connection"]}))
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
