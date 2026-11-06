use super::*;
use serde_json::json;

#[test]
fn copy_switches_the_locator_and_keeps_the_source() {
    let root = canonical(&wire::test_dir("storage")).unwrap();
    let data = root.join("data");
    fs::create_dir_all(data.join("plugins/p")).unwrap();
    fs::write(data.join("shishi.db"), b"db").unwrap();
    fs::write(data.join("plugins/p/plugin.json"), b"{}").unwrap();
    fs::create_dir_all(data.join("webview")).unwrap();
    fs::write(data.join("webview/x"), b"skip").unwrap();
    let target = root.join("elsewhere");
    fs::create_dir(&target).unwrap();
    let fail = |params: Value| prepare(&root, &data, &params, vec![]).err().unwrap();
    assert!(fail(json!({"directory": data, "copy": true})).contains("不能相同"));
    assert!(fail(json!({"directory": data.join("plugins"), "copy": true})).contains("互相包含"));
    assert!(fail(json!({"directory": "relative", "copy": true})).contains("完整路径"));
    assert!(fail(json!({"directory": target})).contains("是否迁移"));
    fs::write(root.join("program-only"), b"").unwrap();
    let plan = prepare(&root, &data, &json!({"directory": target, "copy": true}), vec!["--x".into()]).unwrap();
    apply(&plan).unwrap();
    assert_eq!(fs::read(target.join("plugins/p/plugin.json")).unwrap(), b"{}");
    assert!(!target.join("webview").exists(), "the WebView profile is rebuilt, not copied");
    assert!(data.join("shishi.db").exists(), "the old data stays");
    assert_eq!(canonical(&wire::data_directory(&root).unwrap()).unwrap(), canonical(&target).unwrap());
    assert!(apply(&plan).is_err(), "a stale plan never applies twice");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn switching_requires_an_empty_or_existing_data_directory() {
    let root = canonical(&wire::test_dir("storage-switch")).unwrap();
    let data = root.join("data");
    fs::create_dir_all(&data).unwrap();
    let other = root.join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("notes.txt"), b"mine").unwrap();
    let error = prepare(&root, &data, &json!({"directory": other, "copy": false}), vec![]).err().unwrap();
    assert!(error.contains("shishi.db"));
    assert!(prepare(&root, &data, &json!({"directory": other, "copy": true}), vec![])
        .err()
        .unwrap()
        .contains("空文件夹"));
    fs::write(other.join("shishi.db"), b"db").unwrap();
    let plan = prepare(&root, &data, &json!({"directory": other, "copy": false}), vec![]).unwrap();
    apply(&plan).unwrap();
    assert_eq!(fs::read(other.join("notes.txt")).unwrap(), b"mine");
    fs::remove_dir_all(root).unwrap();
}
