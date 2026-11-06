use super::*;

fn source() -> Source {
    Source { id: "s1".into(), name: "示例来源".into(), plugin_id: "p".into(), last_scan: 10, ..Default::default() }
}

fn page(account: &str, cursor: Value, until: i64, rows: &[&str], next: Value) -> Value {
    let messages: Vec<Value> = rows.iter().map(|id| json!({"id": id, "chat": "c", "time": 50, "text": id})).collect();
    json!({"valid": true, "account_key": account, "cursor": cursor, "until": until, "messages": messages, "next_page": next})
}

#[test]
fn message_identity_is_scoped_by_source_and_chat_and_external_id() {
    let a = normalize(
        &source(),
        &json!({"messages": [{"id": "1", "chat": "a", "time": 5}, {"id": "1", "chat": "b", "time": 5}]}),
    )
    .unwrap();
    assert_ne!(a[0].id, a[1].id);
    let mut other = source();
    other.id = "s2".into();
    let b = normalize(&other, &json!({"messages": [{"id": "1", "chat": "a", "time": 5}]})).unwrap();
    assert_ne!(a[0].id, b[0].id);
    assert_eq!(a[0].external_id, "1");
}

#[test]
fn source_namespaces_replies_and_rejects_invalid_messages() {
    let rows = normalize(
        &source(),
        &json!({"messages": [{"id": "1", "chat": "c", "time": 5, "from": "u", "to": "v", "reply": "0",
            "text": "x", "withdrawn": true, "images": [{"path": "/a.png"}]}]}),
    )
    .unwrap();
    let m = &rows[0];
    assert_eq!((m.chat.as_str(), m.from.as_str(), m.to.as_str()), ("s1:c", "s1:u", "s1:v"));
    assert_eq!(m.reply, key("s1", "c", "0"));
    assert!(m.text.is_empty() && m.images.is_empty());
    let images =
        normalize(&source(), &json!({"messages": [{"id": "1", "time": 5, "images": [{"path": "rel.png"}]}]})).unwrap();
    assert_eq!(images[0].images[0].path, "");
    assert_eq!(images[0].images[0].id, format!("{}:image:0", images[0].id));
    for bad in [
        json!({"id": " ", "time": 5}),
        json!({"id": "1", "time": 0}),
        json!({"id": "1", "time": time::now() + 400_000}),
    ] {
        let e = normalize(&source(), &json!({"messages": [{"id": "ok", "time": 5}, bad]})).unwrap_err();
        assert_eq!(e.0, "插件返回的消息标识或时间无效");
    }
    assert!(normalize(&source(), &json!({"messages": 3})).unwrap_err().0.starts_with("消息格式无效："));
}

#[test]
fn identity_requires_valid_answer_and_same_account() {
    let mut s = source();
    assert_eq!(identity(&s, &json!({"valid": false})).unwrap_err().0, "插件配置校验失败");
    assert_eq!(identity(&s, &json!({"valid": false, "message": "坏了"})).unwrap_err().0, "坏了");
    assert_eq!(identity(&s, &json!({"valid": true})).unwrap_err().0, "插件未返回来源身份标识");
    assert_eq!(identity(&s, &json!({"valid": true, "account_key": "a"})).unwrap(), "a");
    s.account_key = "b".into();
    assert_eq!(
        identity(&s, &json!({"valid": true, "account_key": "a"})).unwrap_err().0,
        "文件属于另一个账号，请新建消息来源"
    );
}

fn run(pages: Vec<Value>) -> Result<Fetched> {
    let mut pages = pages.into_iter();
    scan_pages(&source(), 10, 100, vec![], |_| Ok(pages.next().unwrap()))
}

#[test]
fn frozen_scan_pages_validate_identity_progress_and_boundary() {
    let ok =
        run(vec![page("a", json!(1), 90, &["1"], json!("p2")), page("a", json!(1), 90, &["2"], Value::Null)]).unwrap();
    assert_eq!((ok.0.len(), ok.1.clone(), ok.2), (2, json!(1), 90));
    let cases = [
        (
            vec![page("a", json!(1), 90, &["1"], json!("p2")), page("b", json!(1), 90, &["2"], Value::Null)],
            "插件续页的账号、游标或检查点发生变化",
        ),
        (
            vec![page("a", json!(1), 90, &["1"], json!("p2")), page("a", json!(2), 90, &["2"], Value::Null)],
            "插件续页的账号、游标或检查点发生变化",
        ),
        (
            vec![page("a", json!(1), 90, &["1"], json!("p2")), page("a", json!(1), 90, &["1"], Value::Null)],
            "插件冻结分页返回了重复消息",
        ),
        (vec![page("a", json!(1), 90, &[], json!("p2"))], "插件续页没有读取进展"),
        (vec![page("a", json!(1), 90, &["1"], json!(""))], "插件续页令牌无效"),
        (
            vec![page("a", json!(1), 90, &["1"], json!("p")), page("a", json!(1), 90, &["2"], json!("p"))],
            "插件续页令牌重复",
        ),
        (vec![page("a", json!(1), 200, &["1"], Value::Null)], "插件返回的增量检查点无效"),
        (vec![page("a", json!(1), 5, &["1"], Value::Null)], "插件返回的增量检查点无效"),
    ];
    for (pages, error) in cases {
        assert_eq!(run(pages).unwrap_err().0, error);
    }
    let mut no_cursor = page("a", json!(1), 90, &["1"], Value::Null);
    no_cursor.as_object_mut().unwrap().remove("cursor");
    assert_eq!(run(vec![no_cursor]).unwrap_err().0, "插件未返回增量游标");
}

#[test]
fn incremental_evidence_is_not_looked_up_again_but_other_chats_are() {
    let rows = normalize(&source(), &json!({"messages": [{"id": "1", "chat": "a", "time": 5}]})).unwrap();
    let tracked = vec![
        (key("s1", "a", "1"), "1".to_string()),
        (key("s1", "b", "1"), "1".to_string()),
        (key("s1", "b", "2"), "2".to_string()),
        (key("s1", "c", "2"), "2".to_string()),
    ];
    assert_eq!(missing_tracked(tracked, &rows), vec!["1", "2"]);
}
