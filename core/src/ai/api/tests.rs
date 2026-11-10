use super::*;

fn connection() -> AiConnection {
    AiConnection {
        id: "fixture".into(),
        name: "Fixture".into(),
        driver: "api".into(),
        base_url: "https://example.com/v1".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn keys_resolve_from_one_source_and_status_never_shows_them() {
    let db = Db::memory().unwrap();
    let mut c = connection();
    assert_eq!(key(&db, &c).await.err().unwrap(), "未配置此连接的 API Key");
    assert_eq!(status(&db, &c).await.unwrap()["status"], "未配置 API Key");
    db.set_credential("fixture".into(), "synthetic-secret".into()).await.unwrap();
    assert_eq!(key(&db, &c).await.unwrap().expose_secret(), "synthetic-secret");
    let shown = status(&db, &c).await.unwrap();
    assert_eq!((shown["configured"].as_bool(), shown["source"].as_str()), (Some(true), Some("Some(Dpapi)")));
    assert!(!shown.to_string().contains("synthetic-secret"));

    // A selected variable never falls back to the stored key.
    let name = format!("SHISHI_TEST_API_KEY_{}", std::process::id());
    c.key_env = Some(name.clone());
    assert_eq!(key(&db, &c).await.err().unwrap(), "未配置此连接的 API Key");
    assert_eq!(status(&db, &c).await.unwrap()["source"], "Some(Environment)");
    std::env::set_var(&name, "synthetic-env");
    assert_eq!(key(&db, &c).await.unwrap().expose_secret(), "synthetic-env");
    assert_eq!(status(&db, &c).await.unwrap()["configured"], true);
    std::env::remove_var(&name);
    for bad in ["", "1BAD", "BAD-NAME", "A B"] {
        c.key_env = Some(bad.into());
        assert!(key(&db, &c).await.err().unwrap().contains("invalid environment variable name"));
    }

    assert_eq!(validate(&c, " ").unwrap_err(), "请选择或填写 API 模型");
    assert!(validate(&c, "m").is_ok());
}
