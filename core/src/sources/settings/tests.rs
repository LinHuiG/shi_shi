use super::super::testing::Fixture;
use serde_json::json;

#[tokio::test]
async fn completion_saves_source_once_and_keeps_session_on_failure() {
    let f = Fixture::new("settings", true);
    let opened = f.call("source_discover", json!({"plugin_id": "demo", "config": {}})).await.unwrap();
    let session = opened["settings"]["session"].as_str().unwrap().to_string();
    assert!(opened["settings"]["html"].as_str().unwrap().contains("demo"));

    let source = json!({"id": "", "plugin_id": "demo", "name": "", "enabled": false, "updated": 0});
    let complete = json!({"session": session, "action": "complete", "config": {"path": "x"}, "source": source});
    let err = f.call("plugin_settings", complete.clone()).await.unwrap_err();
    assert_eq!(err.0, "来源名称应为 1–100 字");

    let mut complete = complete;
    complete["source"]["name"] = "工作群".into();
    assert_eq!(f.call("plugin_settings", complete.clone()).await.unwrap(), json!({"path": "x"}));
    assert_eq!(f.call("plugins", json!({})).await.unwrap()["sources"].as_array().unwrap().len(), 1);
    let err = f.call("plugin_settings", complete).await.unwrap_err();
    assert_eq!(err.0, "插件配置已关闭，请重新打开");
}

#[tokio::test]
async fn settings_calls_are_declared_and_bounded() {
    let f = Fixture::new("settings-call", true);
    let opened = f.call("source_discover", json!({"plugin_id": "demo", "config": {}})).await.unwrap();
    let session = opened["settings"]["session"].as_str().unwrap().to_string();
    let call = |method: &str| json!({"session": session, "action": "call", "method": method, "params": {}});
    assert_eq!(f.call("plugin_settings", call("other")).await.unwrap_err().0, "插件未声明此配置方法");
    f.host.reply("probe", Ok(json!({"ok": 1})));
    assert_eq!(f.call("plugin_settings", call("probe")).await.unwrap(), json!({"ok": 1}));
    let bad = json!({"session": session, "action": "noop"});
    assert_eq!(f.call("plugin_settings", bad).await.unwrap_err().0, "不支持的插件配置操作");
    let cancel = json!({"session": session, "action": "cancel"});
    assert_eq!(f.call("plugin_settings", cancel).await.unwrap(), json!(true));
    assert_eq!(f.call("plugin_settings", call("probe")).await.unwrap_err().0, "插件配置已关闭，请重新打开");
}
