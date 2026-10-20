//! Share only the built-in provider definitions, never Desktop plugins or tools.
use crate::restricted::MaterializedProfile;
use serde_yaml_ng::{Mapping, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn inherit_desktop_providers(
    profile: &MaterializedProfile,
    home: &Path,
) -> Result<Option<PathBuf>, String> {
    let mut providers = Mapping::new();
    let mut acp_config = Mapping::new();
    for name in ["desktop", "acp"] {
        let path = home.join("profiles").join(name).join("cordis.patch.yml");
        let text = match fs::read_to_string(&path) {
            Ok(text) if text.len() <= 2_000_000 => text,
            Ok(_) => return Err("DSH 供应商配置过大".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("DSH 供应商配置不可读，请检查 profile 权限".into()),
        };
        let document: Value = serde_yaml_ng::from_str(&text)
            .map_err(|_| "DSH profile YAML 配置无效；未修改原文件")?;
        if document.is_null() {
            continue;
        }
        let rows = document
            .as_sequence()
            .ok_or("DSH profile 必须为配置行列表")?;
        for row in rows {
            if row["id"].as_str() != Some("llm-pi-ai") {
                continue;
            }
            if row["name"]
                .as_str()
                .is_some_and(|n| n != "@deepseek-ai/dsh-llm-pi-ai")
            {
                return Err("DSH 内置供应商行使用了未验证插件，拒绝继承".into());
            }
            let disabled =
                row["disabled"].as_bool() == Some(true) || row["disabled"].as_str() == Some("true");
            if disabled {
                if name == "acp" {
                    providers.clear();
                }
                continue;
            }
            if name == "acp" {
                if let Some(config) = row["config"].as_mapping() {
                    acp_config = config.clone();
                }
            }
            if let Some(entries) = row["config"]["providers"].as_mapping() {
                for (key, value) in entries {
                    if key.as_str().is_none() {
                        return Err("DSH 供应商编号必须为文本".into());
                    }
                    providers.insert(key.clone(), chat_role_compat(value));
                }
            }
        }
    }
    if providers.is_empty() {
        return Ok(None);
    }
    let mut config = acp_config;
    config.insert(Value::String("providers".into()), Value::Mapping(providers));
    let mut row = Mapping::new();
    row.insert(
        Value::String("id".into()),
        Value::String("llm-pi-ai".into()),
    );
    row.insert(
        Value::String("name".into()),
        Value::String("@deepseek-ai/dsh-llm-pi-ai".into()),
    );
    row.insert(Value::String("config".into()), Value::Mapping(config));
    let text = serde_yaml_ng::to_string(&vec![Value::Mapping(row)])
        .map_err(|_| "无法生成供应商临时配置")?;
    let patch = profile.patch.with_file_name("providers.patch.yml");
    fs::write(&patch, &text).map_err(|_| "无法写入供应商临时配置")?;
    let restricted = fs::read_to_string(&profile.patch).map_err(|_| "无法读取受限配置")?;
    fs::write(&profile.patch, format!("{restricted}\n{text}"))
        .map_err(|_| "无法生成受限供应商配置")?;
    Ok(Some(patch))
}

// DeepSeek Chat Completions accepts system, not developer. Only fill an
// omitted compatibility default in the temporary overlay; explicit user
// overrides and every unrelated model/protocol remain unchanged.
fn chat_role_compat(value: &Value) -> Value {
    let mut next = value.clone();
    let route_api = value["api"].as_str().unwrap_or("");
    let route_explicit = value["compat"]["supportsDeveloperRole"].as_bool().is_some();
    if route_explicit {
        return next;
    }
    if let Some(models) = next.get_mut("models").and_then(Value::as_sequence_mut) {
        for model in models {
            let id = model["id"].as_str().unwrap_or("").to_ascii_lowercase();
            let api = model["api"].as_str().unwrap_or(route_api);
            if !id.contains("deepseek") || api != "openai-completions" {
                continue;
            }
            let Some(map) = model.as_mapping_mut() else {
                continue;
            };
            let compat = map
                .entry(Value::String("compat".into()))
                .or_insert_with(|| Value::Mapping(Mapping::new()));
            if let Some(compat) = compat.as_mapping_mut() {
                compat
                    .entry(Value::String("supportsDeveloperRole".into()))
                    .or_insert(Value::Bool(false));
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deepseek_chat_defaults_keep_explicit_overrides_and_other_models() {
        let value:Value=serde_yaml_ng::from_str("api: openai-completions\nmodels:\n- id: deepseek-fixture\n- id: another-model\n- id: deepseek-explicit\n  compat: {supportsDeveloperRole: true}\n").unwrap();
        let next = chat_role_compat(&value);
        assert_eq!(
            next["models"][0]["compat"]["supportsDeveloperRole"].as_bool(),
            Some(false)
        );
        assert!(next["models"][1]["compat"].is_null());
        assert_eq!(
            next["models"][2]["compat"]["supportsDeveloperRole"].as_bool(),
            Some(true)
        );
        assert!(value["models"][0]["compat"].is_null());
    }
    #[test]
    fn absent_disabled_and_invalid_profiles_do_not_import_plugins_or_change_sources() {
        let base = std::env::temp_dir().join(format!(
            "shishi-provider-errors-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = crate::restricted::materialize_profile(&base.join("job")).unwrap();
        let home = base.join("home");
        assert!(inherit_desktop_providers(&profile, &home)
            .unwrap()
            .is_none());
        fs::create_dir_all(home.join("profiles/desktop")).unwrap();
        fs::create_dir_all(home.join("profiles/acp")).unwrap();
        let desktop = home.join("profiles/desktop/cordis.patch.yml");
        fs::write(&desktop,"- id: llm-pi-ai\n  config:\n    providers:\n      synthetic: {models: [{id: shared}]}\n").unwrap();
        fs::write(
            home.join("profiles/acp/cordis.patch.yml"),
            "- id: llm-pi-ai\n  disabled: true\n",
        )
        .unwrap();
        assert!(inherit_desktop_providers(&profile, &home)
            .unwrap()
            .is_none());
        fs::write(&desktop, "- id: llm-pi-ai\n  name: untrusted-plugin\n").unwrap();
        assert!(inherit_desktop_providers(&profile, &home).is_err());
        fs::write(&desktop, "[bad: yaml: config").unwrap();
        assert!(inherit_desktop_providers(&profile, &home).is_err());
        assert_eq!(
            fs::read_to_string(&profile.patch).unwrap(),
            crate::restricted::RESTRICTED_PATCH
        );
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn provider_only_overlay_keeps_routes_and_acp_overrides_without_changing_source() {
        let base = std::env::temp_dir().join(format!(
            "shishi-provider-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        fs::create_dir_all(home.join("profiles/desktop")).unwrap();
        fs::create_dir_all(home.join("profiles/acp")).unwrap();
        let desktop="- id: llm-pi-ai\n  name: '@deepseek-ai/dsh-llm-pi-ai'\n  config:\n    providers:\n      source-a: {apiKeyEnv: FIXTURE_KEY, models: [{id: shared}]}\n      source-b: {models: [{id: shared}]}\n- id: ui-chat\n  name: unsafe-ui\n  config: {secret: synthetic-private-sentinel}\n";
        fs::write(home.join("profiles/desktop/cordis.patch.yml"), desktop).unwrap();
        fs::write(home.join("profiles/acp/cordis.patch.yml"),"- id: llm-pi-ai\n  config:\n    providers:\n      source-a: {models: [{id: override}]}\n").unwrap();
        let profile = crate::restricted::materialize_profile(&base.join("job")).unwrap();
        let patch = inherit_desktop_providers(&profile, &home).unwrap().unwrap();
        let text = fs::read_to_string(patch).unwrap();
        assert!(
            text.contains("source-a")
                && text.contains("source-b")
                && text.contains("override")
                && text.contains("shared")
        );
        assert!(
            !text.contains("ui-chat")
                && !text.contains("synthetic-private-sentinel")
                && !text.contains("FIXTURE_KEY")
        );
        assert_eq!(
            fs::read_to_string(home.join("profiles/desktop/cordis.patch.yml")).unwrap(),
            desktop
        );
        assert!(fs::read_to_string(&profile.patch)
            .unwrap()
            .starts_with(crate::restricted::RESTRICTED_PATCH));
        fs::remove_dir_all(base).unwrap();
    }
}
