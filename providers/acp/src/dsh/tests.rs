use super::*;

const REAL_DUMP: &str = include_str!("../../tests/fixtures/restricted-dump.yml");
/// 0.2.2-rc.1 drops `tool-subagent-list-agents` and adds `tool-executor-native`.
const REAL_DUMP_022: &str = include_str!("../../tests/fixtures/restricted-dump-0.2.2.yml");

#[test]
fn real_runtime_dumps_verify_and_deviations_fail_closed() {
    for (version, dump) in [("0.2.1-rc.2", REAL_DUMP), ("0.2.2-rc.1", REAL_DUMP_022)] {
        let ready = verify_dump(version, dump).unwrap();
        assert!(ready.active_ids.iter().any(|id| id == "acp") && !ready.active_ids.iter().any(|id| id == "tool-fs"));
        assert!(ready.active_ids.len() > 30 && ready.runtime_version == version);
        let extra = format!("{dump}- id: user-tool-plugin\n  name: '@deepseek-ai/dsh-tool-user'\n");
        assert!(verify_dump(version, &extra).unwrap_err().to_string().contains("user-tool-plugin"));
        let enabled = dump.replace("  disabled: true\n- id: tool-fs-search", "  disabled: false\n- id: tool-fs-search");
        assert_ne!(enabled, dump);
        assert!(verify_dump(version, &enabled).unwrap_err().to_string().contains("`tool-fs`"));
    }
}

#[test]
fn analysis_launch_is_restricted_but_handling_keeps_user_settings() {
    let base = crate::test_path("acp-launch");
    let profile = materialize_profile(&base, &base.join("home")).unwrap();
    assert_eq!(fs::read_to_string(&profile.patch).unwrap(), RESTRICTED_PATCH);
    assert!(profile.providers.is_none() && fs::read_dir(&profile.agents_dir).unwrap().count() == 0);
    let has = |launch: &AcpLaunch, key: &str| launch.env.iter().any(|(k, _)| k == key);
    let analysis = profile.analysis_launch(Path::new("dsh.cmd"), &base, &base);
    assert!(
        analysis.args.iter().any(|a| a == "--patch")
            && has(&analysis, "DSH_PERMISSION_MODE")
            && has(&analysis, "DSH_AGENTS_HOME")
    );
    let handling = handling_launch(Path::new("dsh.cmd"), &base, &base, None);
    assert!(
        !handling.args.iter().any(|a| a == "--patch")
            && !has(&handling, "DSH_PERMISSION_MODE")
            && !has(&handling, "DSH_TELEMETRY_DISABLED")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn readiness_never_creates_the_global_profile_and_quotes_guidance() {
    let base = crate::test_path("acp-missing");
    let home = base.join("Fixture's Home/$data");
    let profile = materialize_profile(&base.join("job"), &home).unwrap();
    let config = profile.readiness(Path::new("D:/Fixture's Harness/dsh.cmd"), &home, &base);
    let error = check_readiness(&config).unwrap_err();
    assert!(matches!(error, ReadinessError::ProfileMissing(_)));
    assert!(error.guidance(&config).contains(&format!(
        "$env:DSH_HOME='{}'; & 'D:/Fixture''s Harness/dsh.cmd' --profile acp --help",
        home.to_string_lossy().replace('\'', "''")
    )));
    assert!(!home.join("profiles/acp").exists());
    fs::create_dir_all(home.join("profiles/acp")).unwrap();
    fs::write(home.join("profiles/acp/package.json"), "not json").unwrap();
    let invalid = check_readiness(&config).unwrap_err();
    assert!(invalid.to_string().contains("ACP 配置内容无效") && !invalid.guidance(&config).contains("--help"));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn deepseek_chat_defaults_keep_explicit_overrides_and_other_models() {
    let value: Value = yaml_serde::from_str("api: openai-completions\nmodels:\n- id: deepseek-fixture\n- id: another-model\n- id: deepseek-explicit\n  compat: {supportsDeveloperRole: true}\n").unwrap();
    let next = chat_role_compat(&value);
    assert_eq!(next["models"][0]["compat"]["supportsDeveloperRole"].as_bool(), Some(false));
    assert!(next["models"][1]["compat"].is_null());
    assert_eq!(next["models"][2]["compat"]["supportsDeveloperRole"].as_bool(), Some(true));
}

#[test]
fn absent_disabled_and_invalid_provider_profiles_import_nothing() {
    let base = crate::test_path("provider-errors");
    let home = base.join("home");
    let dir = base.join("job");
    assert!(materialize_profile(&dir, &home).unwrap().providers.is_none());
    fs::create_dir_all(home.join("profiles/desktop")).unwrap();
    fs::create_dir_all(home.join("profiles/acp")).unwrap();
    let desktop = home.join("profiles/desktop/cordis.patch.yml");
    fs::write(&desktop, "- id: llm-pi-ai\n  config:\n    providers:\n      synthetic: {models: [{id: shared}]}\n")
        .unwrap();
    fs::write(home.join("profiles/acp/cordis.patch.yml"), "- id: llm-pi-ai\n  disabled: true\n").unwrap();
    assert!(materialize_profile(&dir, &home).unwrap().providers.is_none());
    fs::write(&desktop, "- id: llm-pi-ai\n  name: untrusted-plugin\n").unwrap();
    assert!(materialize_profile(&dir, &home).is_err());
    fs::write(&desktop, "[bad: yaml: config").unwrap();
    assert!(materialize_profile(&dir, &home).is_err());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn provider_overlay_keeps_routes_and_acp_overrides_without_changing_sources() {
    let base = crate::test_path("provider");
    let home = base.join("home");
    fs::create_dir_all(home.join("profiles/desktop")).unwrap();
    fs::create_dir_all(home.join("profiles/acp")).unwrap();
    let desktop = "- id: llm-pi-ai\n  name: '@deepseek-ai/dsh-llm-pi-ai'\n  config:\n    providers:\n      source-a: {apiKeyEnv: FIXTURE_KEY, models: [{id: shared}]}\n      source-b: {models: [&model {id: shared, name: 合成模型, enabled: false, code: '000123'}, *model]}\n- id: ui-chat\n  name: unsafe-ui\n  config: {secret: synthetic-private-sentinel}\n";
    fs::write(home.join("profiles/desktop/cordis.patch.yml"), desktop).unwrap();
    fs::write(
        home.join("profiles/acp/cordis.patch.yml"),
        "- id: llm-pi-ai\n  config:\n    providers:\n      source-a: {models: [{id: override}]}\n",
    )
    .unwrap();
    let profile = materialize_profile(&base.join("job"), &home).unwrap();
    let text = fs::read_to_string(profile.providers.unwrap()).unwrap();
    let inherited: Value = yaml_serde::from_str(&text).unwrap();
    let models = &inherited[0]["config"]["providers"]["source-b"]["models"];
    assert_eq!(models[0], models[1]);
    assert_eq!((models[0]["name"].as_str(), models[0]["code"].as_str()), (Some("合成模型"), Some("000123")));
    assert!(text.contains("override") && text.contains("shared"));
    assert!(!text.contains("ui-chat") && !text.contains("synthetic-private-sentinel") && !text.contains("FIXTURE_KEY"));
    assert_eq!(fs::read_to_string(home.join("profiles/desktop/cordis.patch.yml")).unwrap(), desktop);
    assert!(fs::read_to_string(&profile.patch).unwrap().starts_with(RESTRICTED_PATCH));
    fs::remove_dir_all(base).unwrap();
}

#[cfg(windows)]
#[test]
fn explicit_setup_initializes_once_without_overwriting_an_existing_profile() {
    let base = crate::test_path("acp-setup");
    let home = base.join("fixture home");
    let profile = materialize_profile(&base.join("assets"), &home).unwrap();
    let launcher = base.join("fixture dsh.cmd");
    let script = format!("@echo off\r\nif \"%~1\"==\"--version\" (echo 0.2.2-rc.1& exit /b 0)\r\nif not \"%~5\"==\"--dump-config\" exit /b 9\r\necho dump>>\"%DSH_HOME%\\calls.txt\"\r\nif not exist \"%DSH_HOME%\\profiles\\acp\" mkdir \"%DSH_HOME%\\profiles\\acp\"\r\nif not exist \"%DSH_HOME%\\profiles\\acp\\package.json\" copy /y \"%~dp0manifest.json\" \"%DSH_HOME%\\profiles\\acp\\package.json\" >nul\r\ntype \"%~dp0dump.yml\"\r\n");
    let manifest = r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-acp-app"]}}}"#;
    fs::write(base.join("manifest.json"), manifest).unwrap();
    fs::write(base.join("dump.yml"), REAL_DUMP).unwrap();
    fs::create_dir_all(&home).unwrap();
    let config = profile.readiness(&launcher, &home, &base);
    fs::write(&launcher, &script).unwrap();
    initialize_acp(&config).unwrap();
    let actual = home.join("profiles/acp/package.json");
    assert_eq!(fs::read_to_string(&actual).unwrap(), manifest);
    assert_eq!(fs::read_to_string(home.join("calls.txt")).unwrap().lines().count(), 2);
    fs::write(&actual, "invalid existing configuration").unwrap();
    assert!(initialize_acp(&config).is_err());
    assert_eq!(fs::read_to_string(&actual).unwrap(), "invalid existing configuration");
    fs::remove_dir_all(base).unwrap();
}
