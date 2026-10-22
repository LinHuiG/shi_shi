//! Reviewed defaults live in configuration resources, not in application code.
use std::{collections::BTreeMap, path::Path, sync::OnceLock};

type Catalog = BTreeMap<String, String>;
static CATALOG: OnceLock<Catalog> = OnceLock::new();
const DEFAULTS: &str = include_str!("../config/prompts.json");

fn defaults() -> Catalog {
    serde_json::from_str(DEFAULTS).expect("invalid bundled prompt configuration")
}

fn merge(base: &mut Catalog, text: &str) -> Result<(), String> {
    let overrides: Catalog =
        serde_json::from_str(text).map_err(|_| "提示词配置必须是 JSON 字符串映射".to_string())?;
    for (key, value) in &overrides {
        if !base.contains_key(key) {
            return Err(format!("未知提示词配置项：{key}"));
        }
        // Protocol boundaries are versioned with the executable. Local policy edits
        // must not silently redefine the tools' actual security constraints.
        if [
            "handling_boundary",
            "tags",
            "context",
            "memory",
            "policy_boundary",
            "memory_io",
            "archive_permissions",
            "images",
            "concise",
            "wire_format",
        ]
        .contains(&key.as_str())
            || key.starts_with("handling_frame_")
        {
            return Err(format!("协议配置项 {key} 只能随源码审查和构建更新"));
        }
        if value.trim().is_empty() || value.chars().count() > 40_000 {
            return Err(format!("提示词 {key} 不能为空或超过 40000 字符"));
        }
    }
    base.extend(overrides);
    Ok(())
}

pub fn initialize(root: &Path) -> Result<(), String> {
    let catalog = load_directory(&crate::storage::data_dir(root))?;
    CATALOG
        .set(catalog)
        .map_err(|_| "提示词配置已加载".to_string())
}
pub(crate) fn validate_directory(directory: &Path) -> Result<(), String> {
    load_directory(directory).map(|_| ())
}
fn load_directory(directory: &Path) -> Result<Catalog, String> {
    let mut catalog = defaults();
    let path = directory.join("config/prompts.local.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if text.len() > 1_000_000 {
                return Err("提示词配置文件过大".into());
            }
            merge(&mut catalog, &text)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("无法读取本地提示词配置：{e}")),
    }
    Ok(catalog)
}

pub fn get(kind: &str) -> &'static str {
    let catalog = CATALOG.get_or_init(defaults);
    catalog.get(kind).unwrap_or_else(|| &catalog["classify"])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_complete_and_local_policies_cannot_replace_protocols() {
        let mut base = defaults();
        for key in [
            "archive",
            "classify",
            "format",
            "edit",
            "schedule",
            "handling",
            "handling_boundary",
            "prompt_review",
            "learning",
            "report",
            "advice_revision",
            "handling_learning",
            "tags",
            "context",
            "memory",
            "policy_boundary",
            "memory_io",
            "archive_permissions",
            "images",
            "concise",
            "message_filter",
            "dsh_submit",
        ] {
            assert!(!base[key].trim().is_empty(), "{key}");
        }
        assert!(merge(&mut base, r#"{"classify":"用户自定义规则"}"#).is_ok());
        assert_eq!(base["classify"], "用户自定义规则");
        let before = base.clone();
        for invalid in [
            r#"{"classify":""}"#,
            r#"{"unknown":"rule"}"#,
            r#"{"handling_boundary":"override"}"#,
            "[]",
        ] {
            assert!(merge(&mut base, invalid).is_err());
            assert_eq!(base, before);
        }
    }
    #[test]
    fn saved_user_prompt_wins_over_factory_configuration() {
        let config = crate::model::Config {
            classify_prompt: "用户已保存的规则".into(),
            ..Default::default()
        };
        assert_eq!(
            crate::ai::effective_policy(&config, "classify"),
            "用户已保存的规则"
        );
        assert!(!crate::model::Config::default().auto_adopt_advice);
    }
}
