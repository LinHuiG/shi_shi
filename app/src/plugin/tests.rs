use super::*;
use std::fs;

fn package(settings: bool) -> PathBuf {
    let dir = crate::transport::test_dir("plugin-prepare");
    fs::write(dir.join("source.dll"), b"synthetic").unwrap();
    fs::write(dir.join("settings.html"), "<p>synthetic</p>").unwrap();
    let mut manifest = json!({"id": "synthetic", "name": "Synthetic", "version": "1", "api_version": 1,
        "entrypoint": "source.dll", "files": ["source.dll", "settings.html"],
        "capabilities": ["discover", "validate", "fetch_incremental", "fetch_range", "lookup_messages"]});
    if settings {
        manifest["settings"] = json!({"api_version": 1, "entrypoint": "settings.html", "methods": ["login"]});
    }
    fs::write(dir.join("plugin.json"), manifest.to_string()).unwrap();
    dir
}

fn request(dir: &std::path::Path, settings: bool, method: &str) -> Request {
    Request {
        id: "r".into(),
        manifest: dir.join("plugin.json"),
        settings,
        input: json!({"protocol": 1, "method": method, "config": {"k": "v"}, "params": {"p": 1}}),
    }
}

#[test]
fn calls_are_checked_against_the_manifest() {
    let dir = package(true);
    let call = prepare(&request(&dir, false, "discover")).unwrap();
    assert!(call.library.ends_with("source.dll") && !call.settings);
    let sent: Value = serde_json::from_slice(&call.input).unwrap();
    assert_eq!(sent["config"]["k"], "v");
    assert_eq!(prepare(&request(&dir, false, "erase")).unwrap_err(), "插件未声明此方法");
    let call = prepare(&request(&dir, true, "login")).unwrap();
    let sent: Value = serde_json::from_slice(&call.input).unwrap();
    assert_eq!(sent["context"]["timeout_ms"], 90000);
    assert_eq!(sent["params"]["p"], 1);
    assert!(sent["context"]["plugin_dir"].is_string());
    assert_eq!(prepare(&request(&dir, true, "discover")).unwrap_err(), "插件未声明此配置方法");
    let mut missing = request(&dir, false, "x");
    missing.input = json!({});
    assert_eq!(prepare(&missing).unwrap_err(), "缺少插件方法");
    fs::remove_dir_all(dir).unwrap();
    let dir = package(false);
    assert_eq!(prepare(&request(&dir, true, "login")).unwrap_err(), "插件未提供配置页面");
    fs::remove_dir_all(dir).unwrap();
}
