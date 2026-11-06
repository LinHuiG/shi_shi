use super::*;
#[test]
fn data_directory_defaults_and_rejects_damaged_settings() {
    let root = test_dir("location");
    assert_eq!(data_directory(&root).unwrap(), root.join("data"));
    fs::write(root.join("data-location.json"), "{").unwrap();
    assert_eq!(data_directory(&root).unwrap_err(), "数据目录设置损坏，未启动新后台");
    fs::write(root.join("data-location.json"), r#"{"version":1,"directory":"relative"}"#).unwrap();
    assert!(data_directory(&root).is_err());
    let custom = root.join("custom");
    fs::create_dir(&custom).unwrap();
    fs::write(root.join("data-location.json"), json!({"version":1,"directory":custom}).to_string()).unwrap();
    assert_eq!(data_directory(&root).unwrap(), custom);
    fs::write(root.join("data-location.json"), json!({"version":1,"directory":custom,"x":1}).to_string()).unwrap();
    assert!(data_directory(&root).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn route_requires_loopback_protocol_and_long_token() {
    let root = test_dir("route");
    let data = root.join("data");
    fs::create_dir(&data).unwrap();
    assert!(route(&root).unwrap().is_none());
    let good = Route {
        protocol: PROTOCOL,
        address: "127.0.0.1:9".parse().unwrap(),
        token: "t".repeat(32),
        instance: "i".into(),
        pid: 1,
    };
    fs::write(data.join(ROUTE_FILE), serde_json::to_vec(&good).unwrap()).unwrap();
    assert_eq!(route(&root).unwrap().unwrap().token, good.token);
    for bad in [
        Route { address: "192.0.2.1:9".parse().unwrap(), ..good.clone() },
        Route { token: "short".into(), ..good.clone() },
        Route { protocol: 2, ..good.clone() },
    ] {
        fs::write(data.join(ROUTE_FILE), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(route(&root).is_err());
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unavailable_explicit_directory_never_falls_back() {
    let root = test_dir("ss-cli-location");
    fs::write(root.join("data-location.json"), json!({"version":1,"directory":root.join("missing")}).to_string())
        .unwrap();
    assert!(data_directory(&root).is_err());
    assert!(!root.join("data").exists());
    fs::remove_dir_all(root).unwrap();
}
