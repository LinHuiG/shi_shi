use super::super::testing::Fixture;
use super::*;
use crate::time;

#[tokio::test]
async fn toggle_keeps_progress_and_uninstall_keeps_sources_and_messages() {
    let f = Fixture::new("manage-toggle", false);
    let a = f.source("工作群", "a").await;
    let at = time::now() - 60_000;
    let rows = json!([{"id": "1", "chat": "c", "time": at, "text": "x"}]);
    f.host.reply("fetch_incremental", Ok(json!({"messages": rows, "cursor": {"n": 1}})));
    f.call("scan_only", json!({})).await.unwrap();
    let scanned = f.sources().await.remove(0);
    let off = f.call("source_toggle", json!({"id": a.id, "updated": scanned.updated, "enabled": false})).await.unwrap();
    let off: Source = serde_json::from_value(off).unwrap();
    assert_eq!((off.cursor.clone(), off.last_scan, off.enabled), (json!({"n": 1}), scanned.last_scan, false));
    let stale = f.call("source_toggle", json!({"id": a.id, "updated": scanned.updated, "enabled": true})).await;
    assert_eq!(stale.unwrap_err().0, STALE);
    f.call("source_toggle", json!({"id": a.id, "updated": off.updated, "enabled": true})).await.unwrap();

    f.call("plugin_uninstall", json!({"id": "demo"})).await.unwrap();
    let after = f.sources().await.remove(0);
    assert!(!after.enabled);
    assert_eq!(after.cursor, json!({"n": 1}));
    let count = f
        .ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(f.call("plugins", json!({})).await.unwrap()["plugins"].as_array().unwrap().is_empty());
    assert_eq!(fs::read_dir(f.ctx.root.join("tmp/removed-plugins")).unwrap().count(), 0);
}

#[tokio::test]
async fn upload_installs_and_cleans_its_temporary_file() {
    let f = Fixture::new("manage-upload", false);
    let src = f.ctx.root.join("upload-src");
    let manifest = fs::read(plugin::fixture(&src, "other", "2")).unwrap();
    let zip = super::super::zip::build(&[("plugin.json", &manifest), ("p.dll", b"MZ2")]);
    let p = json!({"name": "other.zip", "base64": base64::engine::general_purpose::STANDARD.encode(zip)});
    let result = f.call("plugin_upload", p).await.unwrap();
    assert_eq!(result["install_status"], "installed");
    assert_eq!(fs::read_dir(f.ctx.root.join("tmp/plugin-uploads")).unwrap().count(), 0);
    let bad = json!({"name": "x.zip", "base64": "aGVsbG8="});
    assert_eq!(f.call("plugin_upload", bad).await.unwrap_err().0, "插件包不是有效的 ZIP 文件");
}

#[tokio::test]
async fn removing_the_last_enabled_source_stops_automatic_scans() {
    let f = Fixture::new("manage-remove", false);
    let a = f.source("工作群", "a").await;
    f.call("scan_control", json!({"enabled": true})).await.unwrap();
    assert_eq!(f.call("source_remove", json!({"id": a.id, "updated": 0})).await.unwrap_err().0, "消息来源已变化");
    f.call("source_remove", json!({"id": a.id, "updated": a.updated})).await.unwrap();
    assert!(!f.ctx.config.get().enabled);
}
