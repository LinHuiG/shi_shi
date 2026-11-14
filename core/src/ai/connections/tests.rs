use super::*;
use crate::config::{ConfigStore, ModelChoice};

fn ctx(label: &str) -> (PathBuf, Arc<Ctx>) {
    let root = crate::testing::temp_dir(label);
    let ctx = Ctx::open(root.clone(), Arc::new(crate::NoHost)).unwrap();
    (root, ctx)
}

fn api(id: &str) -> Value {
    json!({"id":id,"name":"Fixture","driver":"api","base_url":"https://example.com/v1","models":[{"model":"fixture"}]})
}

#[test]
fn desktop_detection_excludes_legacy_and_deduplicates() {
    let root = crate::testing::temp_dir("dsh-discovery");
    let launcher = root.join("resources/runtime/cli/bin/dsh.cmd");
    std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    std::fs::write(&launcher, "fixture").unwrap();
    assert!(candidates(vec![root.clone()]).is_empty());
    std::fs::write(root.join("DeepSeek Harness.exe"), "fixture").unwrap();
    std::fs::write(root.join("resources/app.asar"), "fixture").unwrap();
    assert_eq!(candidates(vec![root.clone(), launcher.clone(), root.join("DeepSeek Harness.exe")]).len(), 1);
    let found = detect_dsh(&launcher.to_string_lossy());
    assert_eq!(found["candidates"][0]["label"], "DeepSeek Harness 桌面版");
    assert!(found["candidates"][0].get("version").is_some());
}

#[tokio::test]
async fn connection_save_requires_models_and_deletion_allows_empty_routes() {
    let (root, ctx) = ctx("connections-save");
    let mut c = json!({"id":"fixture","name":"Fixture","driver":"api","base_url":"https://example.com/v1"});
    let e = save_connection(&ctx, json!({"connection":c})).await.unwrap_err();
    assert_eq!(e.0, "请先检测模型，或填写至少一个模型编号（最多 1000 个）");
    c["models"] = json!([{"model":"fixture-model"}]);
    save_connection(&ctx, json!({"connection":c,"expected":null})).await.unwrap();
    let e = save_connection(&ctx, json!({"connection":c,"expected":null})).await.unwrap_err();
    assert_eq!(e.0, "连接已被修改，请重新打开编辑");
    delete_connection(&ctx, &json!({"id":""})).await.unwrap();
    delete_connection(&ctx, &json!({"id":"fixture"})).await.unwrap();
    assert_eq!(delete_connection(&ctx, &json!({})).await.unwrap_err().0, "缺少连接编号");
    let saved = ConfigStore::load(root.join("config.json")).unwrap().get();
    assert!(saved.ai_connections.is_empty());
    for kind in ["message_filter", "classify", "archive"] {
        assert!(!super::super::prompt::configured(&saved, kind), "{kind} route was cleared");
    }
}

#[tokio::test]
async fn default_codex_edit_uses_its_own_snapshot_and_normalizes_default_fields() {
    let (_root, ctx) = ctx("connections-default");
    ctx.config
        .update(&ctx.db, |c| {
            c.codex = "synthetic-original.exe".into();
            c.codex_models = vec![json!({"model":"synthetic"})];
            c.ai_connections.push(serde_json::from_value(api("api")).unwrap());
            Ok(())
        })
        .await
        .unwrap();
    let original = default_codex(&ctx.config.get());
    let mut expected = json!(original);
    expected.as_object_mut().unwrap().remove("stream"); // Older/default UI descriptor.
    let mut edited = original.clone();
    edited.name = "Renamed Codex".into();
    save_connection(&ctx, json!({"connection":edited,"expected":expected})).await.unwrap();
    assert_eq!(ctx.config.get().codex_name, "Renamed Codex");
    let e = save_connection(&ctx, json!({"connection":original,"expected":expected})).await.unwrap_err();
    assert_eq!(e.0, "连接已被修改，请重新打开编辑");
    assert_eq!(ctx.config.get().ai_connections[0].name, "Fixture");
    let mut wrong = json!(original);
    wrong["driver"] = json!("api");
    wrong["base_url"] = json!("https://example.com/v1");
    assert_eq!(save_connection(&ctx, json!({"connection":wrong})).await.unwrap_err().0, "默认连接只支持 Codex");
    let e = save_connection(&ctx, json!({"connection":original,"expected":"bad"})).await.unwrap_err();
    assert_eq!(e.0, "连接编辑快照无效，请重新打开编辑");
}

#[tokio::test]
async fn changed_api_address_never_reuses_saved_key() {
    let (_root, ctx) = ctx("connections-api");
    let saved = save_connection(&ctx, json!({"connection":api("fixture"),"expected":null})).await.unwrap();
    let mut edited = saved["connection"].clone();
    edited["base_url"] = json!("https://other.example/v1");
    let e = connection_models(&ctx, &json!({"connection":edited})).await.unwrap_err();
    assert_eq!(e.0, "地址已修改，请重新填写密钥后检测");
    let e = save_connection(&ctx, json!({"connection":edited,"expected":saved["connection"]})).await.unwrap_err();
    assert_eq!(e.0, "地址已修改，请重新填写密钥后保存");
    let both = json!({"connection":saved["connection"],"key":"synthetic","clear_key":true});
    assert_eq!(save_connection(&ctx, both).await.unwrap_err().0, "请在替换密钥和清除密钥中选择一种操作");
}

#[tokio::test]
async fn non_api_keys_drivers_and_dsh_reuse() {
    let (root, ctx) = ctx("connections-dsh");
    let launcher = root.join("dsh.cmd");
    std::fs::write(&launcher, "").unwrap();
    let dsh = json!({"id":"d1","name":"Harness","driver":"dsh","executable":launcher,"models":[{"model":"m"}]});
    let e = save_connection(&ctx, json!({"connection":dsh,"key":"synthetic"})).await.unwrap_err();
    assert_eq!(e.0, "仅 API 连接支持密钥");
    save_connection(&ctx, json!({"connection":dsh})).await.unwrap();
    let mut again = dsh.clone();
    again["id"] = json!("d2");
    let reused = save_connection(&ctx, json!({"connection":again})).await.unwrap();
    assert_eq!(reused["reused"], true);
    assert_eq!(reused["connection"]["id"], "d1");
    assert_eq!(ctx.config.get().ai_connections.len(), 1);
    let mut empty = dsh.clone();
    empty["executable"] = json!(" ");
    assert_eq!(save_connection(&ctx, json!({"connection":empty})).await.unwrap_err().0, "请选择程序路径");
    let mut unknown = dsh.clone();
    unknown["driver"] = json!("other");
    assert_eq!(
        save_connection(&ctx, json!({"connection":unknown})).await.unwrap_err().0,
        "尚未支持的 AI 接入：other，未发起调用"
    );
    let mut nameless = dsh;
    nameless["name"] = json!(" ");
    assert_eq!(
        save_connection(&ctx, json!({"connection":nameless})).await.unwrap_err().0,
        "请填写连接名称（最多 80 字）"
    );
    // A referenced connection keeps its driver.
    ctx.config
        .update(&ctx.db, |c| {
            c.model_profiles.as_mut().unwrap().analysis =
                Some(ModelChoice { connection_id: "d1".into(), model: "m".into(), reasoning_effort: String::new() });
            Ok(())
        })
        .await
        .unwrap();
    let current = json!(ctx.config.get().ai_connections[0]);
    let mut switched = api("d1");
    switched["name"] = json!("Harness");
    let e = save_connection(&ctx, json!({"connection":switched,"expected":current})).await.unwrap_err();
    assert_eq!(e.0, "连接仍被用途引用，请先切换用途再修改接入类型");
}

#[test]
fn validate_connections_checks_connections_then_routes() {
    let mut c = Config::default();
    assert!(validate_connections(&c).is_ok());
    c.ai_timeout_seconds = 10;
    assert_eq!(validate_connections(&c).unwrap_err().0, "AI 总时限须为 30—1800 秒");
    c.ai_timeout_seconds = 600;
    c.ai_connections.push(serde_json::from_value(api("a")).unwrap());
    c.ai_connections.push(serde_json::from_value(api("a")).unwrap());
    assert_eq!(validate_connections(&c).unwrap_err().0, "AI 连接编号为空或重复");
    c.ai_connections.pop();
    c.ai_connections[0].base_url = "not a url".into();
    assert!(validate_connections(&c).is_err());
    c.ai_connections[0].base_url = "https://example.com/v1".into();
    let api_route =
        Some(ModelChoice { connection_id: "a".into(), model: "fixture".into(), reasoning_effort: String::new() });
    assert!(validate_connections(&c).is_ok());
    c.codex = "codex.exe".into();
    assert!(validate_connections(&c).is_ok());
    c.model_profiles.as_mut().unwrap().analysis =
        Some(ModelChoice { connection_id: "missing".into(), ..ModelChoice::default() });
    assert_eq!(validate_connections(&c).unwrap_err().0, "所选 AI 连接不存在，请重新选择接入");
    c.model_profiles.as_mut().unwrap().analysis = api_route;
    c.model_profiles.as_mut().unwrap().analysis.as_mut().unwrap().model = " ".into();
    assert_eq!(validate_connections(&c).unwrap_err().0, "请选择或填写 API 模型");
}

#[test]
fn session_open_accepts_only_codex_uuids() {
    assert_eq!(open_session(&json!({})).unwrap_err().0, "缺少会话编号");
    assert_eq!(open_session(&json!({"thread_id":"x"})).unwrap_err().0, "无效会话编号");
    let thread = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        open_session(&json!({"thread_id":thread,"driver":"dsh"})).unwrap_err().0,
        "此接入不支持自动打开会话，请在对应客户端按会话编号查看"
    );
}

#[tokio::test]
async fn credential_replacement_is_not_committed_when_config_save_fails() {
    let (root, ctx) = ctx("connections-secret");
    let saved = save_connection(&ctx, json!({"connection":api("fixture"),"expected":null,"key":"synthetic-before"}))
        .await
        .unwrap();
    let old = saved["connection"].clone();
    let reference = old["credential_id"].as_str().unwrap().to_string();
    let get = |id: String| {
        let db = ctx.db.clone();
        async move { db.credential(id).await.unwrap().map(|k| k.expose_secret().to_string()) }
    };
    assert_eq!(get(reference.clone()).await.as_deref(), Some("synthetic-before"));
    // A directory in place of the config file makes the publish fail.
    std::fs::remove_file(root.join("config.json")).unwrap();
    std::fs::create_dir_all(root.join("config.json").join("x")).unwrap();
    let after = json!({"connection":old,"expected":old,"key":"synthetic-after"});
    assert!(save_connection(&ctx, after).await.is_err());
    assert_eq!(json!(ctx.config.get().ai_connections[0]), old);
    assert_eq!(get(reference.clone()).await.as_deref(), Some("synthetic-before"));
    let rows: i64 =
        ctx.db.test_read(|c| Ok(c.query_row("SELECT count(*) FROM credentials", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(rows, 1, "the fresh entry of the failed save is removed");
    std::fs::remove_dir_all(root.join("config.json")).unwrap();
    let cleared = save_connection(&ctx, json!({"connection":old,"expected":old,"clear_key":true})).await.unwrap();
    assert!(get(cleared["connection"]["credential_id"].as_str().unwrap().into()).await.is_none());
    assert!(get(reference).await.is_none());
    assert!(!std::fs::read_to_string(root.join("config.json")).unwrap().contains("synthetic"));
}
