use super::*;

#[test]
fn missing_file_gets_defaults_and_bad_field_is_reported_not_reset() {
    let dir = crate::testing::temp_dir("config");
    let path = dir.join("config.json");
    let store = ConfigStore::load(path.clone()).unwrap();
    assert_eq!(store.get().interval_minutes, 3);
    assert!(path.exists());

    std::fs::write(&path, r#"{"interval_minutes": "soon"}"#).unwrap();
    let err = ConfigStore::load(path.clone()).err().unwrap();
    assert!(err.0.contains("interval_minutes") || err.0.contains("invalid type"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"interval_minutes": "soon"}"#);

    // An unknown or retired key is ignored rather than blocking startup.
    std::fs::write(&path, r#"{"intervall_minutes": 9, "interval_minutes": 5}"#).unwrap();
    assert_eq!(ConfigStore::load(path).unwrap().get().interval_minutes, 5);
}

#[tokio::test]
async fn save_rejects_invalid_values_and_replaces_file_atomically() {
    let dir = crate::testing::temp_dir("config-save");
    let path = dir.join("config.json");
    let store = ConfigStore::load(path.clone()).unwrap();
    let db = crate::Db::memory().unwrap();
    let err = store
        .update(&db, |c| {
            c.work_start = "8点".into();
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(err.0.contains("work_start"));
    assert_eq!(store.get().work_start, "08:00");
    store
        .update(&db, |c| {
            c.interval_minutes = 7;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(parse(&std::fs::read_to_string(&path).unwrap()).unwrap().interval_minutes, 7);
    assert!(!dir.join(".config.json.tmp").exists());
    assert_eq!(db.revision(), 1);
}

#[test]
fn model_routes_follow_purpose_and_unconfigured_profile() {
    let mut c = Config::default();
    c.ai_connections.push(Connection { id: "api".into(), ..Default::default() });
    assert_eq!(c.model_choice("archive").reasoning_effort, "high");
    assert!(serde_json::to_value(&c.model_profiles).unwrap().get("handling").is_none());
    assert_eq!(c.model_choice("classify").connection_id, "api");
    c.model_profiles.as_mut().unwrap().review = None;
    assert_eq!(c.model_choice("report").connection_id, UNCONFIGURED_CONNECTION);
}
