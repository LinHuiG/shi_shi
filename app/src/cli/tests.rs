use super::*;

#[test]
fn discovery_contains_shared_mutations_and_versioned_queries() {
    let catalogue = methods();
    for name in [
        "edit",
        "action",
        "task_batch_action",
        "memory_read",
        "memory_save",
        "tasks_query",
        "task_get",
        "storage_change",
        "tool_save",
        "schedule_edit",
        "cli_skill",
        "ai_job_result",
    ] {
        assert!(catalogue["methods"].as_array().unwrap().contains(&json!(name)), "{name}");
    }
}

#[test]
fn skill_quotes_the_instance_paths_for_powershell() {
    let text = skill(Path::new("C:\\it's")).get("text").and_then(Value::as_str).unwrap().to_string();
    assert!(text.starts_with("---\nname: shishi-cli"));
    assert!(text.contains("--root 'C:\\it''s' --help"));
}
#[test]
fn json_file_and_inline_are_mutually_exclusive() {
    assert!(parse(["--input", "a.json", "--params", "{}"].map(str::to_owned)).is_err());
}
#[test]
fn forced_stop_is_explicit_and_default_is_no() {
    let o = parse(["backend", "stop"].map(str::to_owned)).unwrap();
    assert!(!o.force);
    assert_eq!(o.words, ["backend", "stop"]);
}
#[test]
fn accepts_global_options_after_subcommand() {
    let o = parse(["tasks", "get", "事项-a", "--root", r"C:\Synthetic Data"].map(str::to_owned)).unwrap();
    assert_eq!(o.words, ["tasks", "get", "事项-a"]);
    assert!(o.root.is_some());
}
#[test]
fn rejects_unknown_flags_and_unisolated_test_options() {
    assert_eq!(parse(["--x".to_string()]).unwrap_err(), "未知选项：--x");
    assert!(parse(["--no-background-jobs".to_string()]).is_err());
    assert_eq!(parse(["--version".to_string()]).unwrap().words, ["version"]);
    assert!(parse(["--root".to_string()]).is_err());
}
#[test]
fn inline_params_must_be_an_object_and_bom_is_ignored() {
    let mut o = parse(["call", "x", "--params", "\u{feff}{\"a\":1}"].map(str::to_owned)).unwrap();
    assert_eq!(payload(&o).unwrap()["a"], 1);
    o.params = Some("[1]".into());
    assert_eq!(payload(&o).unwrap_err(), "参数必须是 JSON 对象");
    o.params = Some("{".into());
    assert!(payload(&o).unwrap_err().starts_with("输入不是有效 JSON"));
}
#[test]
fn commands_select_the_console_and_options_the_desktop() {
    let r = |a: &[&str]| requested(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert!(r(&["tasks", "list"]) && r(&["--root", "R", "backend", "status"]) && r(&["--help"]) && r(&["-V"]));
    assert!(!r(&[]) && !r(&["--background", "--root", "R"]) && !r(&["--plugin-worker"]));
    assert!(!r(&["--apply-update", "plan"]) && !r(&["--root", "tasks"]));
}
