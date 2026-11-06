use super::*;
fn root() -> std::path::PathBuf {
    let root = crate::test_path("workspace");
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
    assert_eq!(fs::read_to_string(profile.join("cordis.patch.yml")).unwrap(), once);
    // An older bridge is updated in place, keeping its single row.
    fs::write(root.join("plugins/shishi-workspace-bridge/index.js"), "// older bridge").unwrap();
    assert_eq!(status(&root).unwrap()["outdated"], true);
    assert_eq!(install(&root).unwrap()["installed"], true);
    assert_eq!(fs::read_to_string(profile.join("cordis.patch.yml")).unwrap(), once);
    assert_eq!(fs::read_to_string(root.join("plugins/shishi-workspace-bridge/index.js")).unwrap(), PLUGIN);
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
    let link: Value =
        serde_json::from_slice(&fs::read(root.join("ss-session-links/synthetic-session.json")).unwrap()).unwrap();
    assert_eq!(link, json!({"version":1,"session_id":"synthetic-session"}));
    fs::remove_dir_all(root).unwrap();
}
