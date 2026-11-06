use super::*;

fn schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["reason", "changes"], "properties": {
        "reason": {"type": "string", "maxLength": 10}, "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}}})
}

fn store(label: &str, schema: Value) -> (PathBuf, SubmitPaths, Value) {
    let base = crate::test_path(label);
    let paths = SubmitPaths::create(&base, "req", "edit", schema).unwrap();
    let ticket = SubmitPaths::load(&paths.ticket_path).unwrap().1;
    (base, paths, ticket)
}

#[test]
fn one_valid_result_is_accepted_once_and_read_back_strictly() {
    let (base, paths, ticket) = store("valid", schema());
    assert!(matches!(paths.read_accepted("req", "edit", &schema()), Err(SubmitError::Missing)));
    assert_eq!(paths.submit(&ticket, &json!({"reason": "ok", "changes": ["a"]})).unwrap()["isError"], false);
    assert_eq!(paths.read_accepted("req", "edit", &schema()).unwrap()["reason"], "ok");
    assert!(matches!(paths.submit(&ticket, &json!({"reason": "ok", "changes": []})), Err(SubmitError::Duplicate)));
    assert!(matches!(paths.read_accepted("other", "edit", &schema()), Err(SubmitError::Protocol(_))));
    let mut stricter = schema();
    stricter["properties"]["reason"]["maxLength"] = json!(1);
    assert!(matches!(paths.read_accepted("req", "edit", &stricter), Err(SubmitError::Schema(_))));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn one_correction_is_allowed_then_the_tool_closes() {
    let (base, paths, ticket) = store("correct", schema());
    assert_eq!(paths.submit(&ticket, &json!({"reason": "ok", "changes": [], "extra": true})).unwrap()["isError"], true);
    assert!(!paths.result_path.exists());
    assert_eq!(paths.submit(&ticket, &json!({"reason": "ok", "changes": []})).unwrap()["isError"], false);
    fs::remove_dir_all(base).unwrap();

    let (base, paths, ticket) = store("exhaust", schema());
    paths.submit(&ticket, &json!({"reason": "ok"})).unwrap();
    assert!(matches!(paths.submit(&ticket, &json!({"reason": "ok"})), Err(SubmitError::Closed(_))));
    assert!(matches!(paths.read_accepted("req", "edit", &schema()), Err(SubmitError::Closed(_))));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn presentation_defaults_are_saved_before_strict_readback() {
    let mut schema = schema();
    schema["properties"]["summary"] = json!({"type": "string", "maxLength": 3, "default": ""});
    schema["required"].as_array_mut().unwrap().push(json!("summary"));
    let (base, paths, ticket) = store("default", schema.clone());
    assert_eq!(
        paths.submit(&ticket, &json!({"reason": "ok", "changes": [], "summary": ["x"]})).unwrap()["isError"],
        false
    );
    assert_eq!(paths.read_accepted("req", "edit", &schema).unwrap()["summary"], "");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn prepared_server_passes_only_the_ticket() {
    let base = crate::test_path("prepare");
    let spec = AnalysisSpec {
        request_id: "r".into(),
        kind: "edit".into(),
        schema: schema(),
        prompt: "p".into(),
        images: vec![],
        workdir: base.clone(),
        app_exe: base.join("shishi.exe"),
        model: None,
        effort: None,
    };
    let prepared = prepare(&base, &spec).unwrap();
    assert_eq!(prepared.server.name, "shishi_submit");
    assert_eq!(
        prepared.server.args,
        ["--analysis-submit-mcp".to_string(), prepared.paths.ticket_path.to_string_lossy().into()]
    );
    assert!(prepared.paths.ticket_path.is_file());
    fs::remove_dir_all(base).unwrap();
}
