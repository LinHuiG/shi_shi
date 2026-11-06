use super::*;

#[test]
fn missing_and_malformed_files_fail() {
    assert!(rows(&json!({})).is_err());
}

fn temp(name: &str, text: &str) -> Value {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-data/json-source");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-{name}", std::process::id()));
    fs::write(&path, text).unwrap();
    json!({"file": path})
}

#[test]
fn chatlab_json_and_jsonl_map_to_the_same_messages() {
    let json = temp(
        "group.json",
        r#"{"chatlab":{"version":"0.0.2","exportedAt":1},"meta":{"name":"示例群","platform":"qq","type":"group","ownerId":"u1"},
        "members":[{"platformId":"u1","accountName":"甲"},{"platformId":"u2","accountName":"成员乙"}],
        "messages":[
          {"platformMessageId":"m1","sender":"u1","timestamp":1703001600,"type":0,"content":"请确认议程"},
          {"sender":"u2","accountName":"乙","groupNickname":"组长","timestamp":1703001610,"type":25,"content":"收到","replyToMessageId":"m1"},
          {"sender":"u2","accountName":"","groupNickname":"","timestamp":1703001610,"type":1,"content":null},
          {"sender":"u2","accountName":"乙","timestamp":1703001620,"type":81,"content":null}]}"#,
    );
    let lines = temp(
        "group.jsonl",
        "{\"_type\":\"header\",\"chatlab\":{\"version\":\"0.0.2\",\"exportedAt\":1},\"meta\":{\"name\":\"示例群\",\"platform\":\"qq\",\"type\":\"group\",\"ownerId\":\"u1\"}}\n\
         # 注释行\n\
         {\"_type\":\"member\",\"platformId\":\"u1\",\"accountName\":\"甲\"}\n\
         {\"_type\":\"message\",\"platformMessageId\":\"m1\",\"sender\":\"u1\",\"timestamp\":1703001600,\"type\":0,\"content\":\"请确认议程\"}\n\
         坏行\n\
         {\"_type\":\"message\",\"sender\":\"u2\",\"accountName\":\"乙\",\"groupNickname\":\"组长\",\"timestamp\":1703001610,\"type\":25,\"content\":\"收到\",\"replyToMessageId\":\"m1\"}\n\
         {\"_type\":\"message\",\"sender\":\"u2\",\"accountName\":\"\",\"groupNickname\":\"\",\"timestamp\":1703001610,\"type\":1,\"content\":null}\n\
         {\"_type\":\"member\",\"platformId\":\"u2\",\"accountName\":\"成员乙\"}\n\
         {\"_type\":\"message\",\"sender\":\"u2\",\"accountName\":\"乙\",\"timestamp\":1703001620,\"type\":81,\"content\":null}\n",
    );
    let rows = rows(&json).unwrap();
    assert_eq!(rows, super::rows(&lines).unwrap());
    let ids: Vec<&str> = rows.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["m1", "u2-1703001610-1", "u2-1703001610-2", "u2-1703001620-1"]);
    assert_eq!((rows[0]["time"].as_i64(), rows[0]["mine"].as_bool()), (Some(1703001600000), Some(true)));
    assert_eq!((rows[1]["sender"].as_str(), rows[1]["reply"].as_str()), (Some("组长"), Some("m1")));
    assert_eq!((rows[2]["text"].as_str(), rows[2]["private"].as_bool()), (Some("[图片]"), Some(false)));
    assert_eq!(rows[3]["withdrawn"], true);
    assert_eq!(
        [&rows[0]["sender"], &rows[2]["sender"], &rows[3]["sender"]],
        [&json!("甲"), &json!("成员乙"), &json!("乙")]
    );
    for f in [json, lines] {
        let _ = fs::remove_file(f["file"].as_str().unwrap());
    }
}
