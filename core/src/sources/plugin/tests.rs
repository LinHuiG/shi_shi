use super::*;

#[test]
fn zip_install_accepts_root_and_nested_packages_without_loading_code() {
    let root = crate::testing::temp_dir("plugin-zip");
    let src = root.join("src");
    let manifest = fs::read(fixture(&src, "demo", "1")).unwrap();
    for prefix in ["", "demo/"] {
        let zip = super::super::zip::build(&[
            (&format!("{prefix}plugin.json"), &manifest),
            (&format!("{prefix}p.dll"), b"MZ1"),
        ]);
        let path = root.join("demo.zip");
        fs::write(&path, zip).unwrap();
        let status = install(&root, &path).unwrap().status;
        assert!(status == "installed" || status == "unchanged");
    }
    assert_eq!(catalog(&root).0.len(), 1);
    assert_eq!(fs::read_dir(root.join("tmp/plugin-packages")).unwrap().count(), 0);
}

#[test]
fn invalid_zip_packages_leave_no_partial_installation() {
    let root = crate::testing::temp_dir("plugin-bad");
    let path = root.join("bad.zip");
    fs::write(&path, super::super::zip::build(&[("plugin.json", b"{\"id\":\"x\"}")])).unwrap();
    assert!(install(&root, &path).is_err());
    assert!(catalog(&root).0.is_empty());
    assert_eq!(fs::read_dir(root.join("tmp/plugin-packages")).unwrap().count(), 0);
    let plugins = fs::read_dir(plugins_dir(&root)).map(|d| d.count()).unwrap_or(0);
    assert_eq!(plugins, 0);
}

#[test]
fn unsafe_manifest_paths_are_rejected() {
    let dir = crate::testing::temp_dir("plugin-unsafe");
    let path = fixture(&dir, "x", "1");
    for bad in ["../p.dll", "/abs.dll", "a\\b.dll", "c:/x.dll"] {
        let mut m: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        m["files"] = json!([bad, "p.dll"]);
        fs::write(&path, m.to_string()).unwrap();
        assert_eq!(Plugin::read(&path).unwrap_err().0, "插件文件名必须为包内相对路径");
    }
}

#[test]
fn source_archives_and_missing_binaries_have_actionable_errors() {
    let dir = crate::testing::temp_dir("plugin-missing");
    let path = fixture(&dir, "x", "1");
    fs::remove_file(dir.join("p.dll")).unwrap();
    assert!(Plugin::read(&path).unwrap_err().0.contains("插件包缺少文件“p.dll”"));
    let root = crate::testing::temp_dir("plugin-src");
    let zip = root.join("src.zip");
    fs::write(&zip, super::super::zip::build(&[("Cargo.toml", b""), ("src/lib.rs", b"")])).unwrap();
    assert_eq!(install(&root, &zip).unwrap_err().0, super::super::zip::ONE_MANIFEST);
}

#[test]
fn upgrade_keeps_undeclared_files_and_reports_status() {
    let root = crate::testing::temp_dir("plugin-upgrade");
    let v1 = fixture(&root.join("v1"), "demo", "1");
    assert_eq!(install(&root, &v1).unwrap().status, "installed");
    assert_eq!(install(&root, &v1).unwrap().status, "unchanged");
    fs::write(plugins_dir(&root).join("demo/user.txt"), "mine").unwrap();
    let v2 = fixture(&root.join("v2"), "demo", "2");
    assert_eq!(install(&root, &v2).unwrap().status, "updated");
    let found = find(&root, "demo").unwrap();
    assert_eq!(found.manifest.version, "2");
    assert_eq!(fs::read(found.dir.join("p.dll")).unwrap(), b"MZ2");
    // The undeclared file moved with the previous directory and stays there.
    let leftovers: Vec<_> = fs::read_dir(plugins_dir(&root))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".previous-"))
        .collect();
    assert_eq!(leftovers.len(), 1);
    assert_eq!(fs::read_to_string(leftovers[0].path().join("user.txt")).unwrap(), "mine");
    assert!(!leftovers[0].path().join("p.dll").exists());
}

#[test]
fn field_validation_messages() {
    let dir = crate::testing::temp_dir("plugin-fields");
    let p = Plugin::read(&fixture(&dir, "x", "1")).unwrap();
    assert_eq!(p.validate_fields(&json!([])).unwrap_err().0, "配置需要是对象");
    assert_eq!(p.validate_fields(&json!({"path": " "})).unwrap_err().0, "请填写文件");
    p.validate_fields(&json!({"path": "a"})).unwrap();
}
