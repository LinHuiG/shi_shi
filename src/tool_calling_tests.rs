use super::*;
use crate::tool_registry::Parameter;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

struct Fixture(App);
impl Fixture {
    fn new() -> Self {
        Self(App::load(std::env::temp_dir().join(format!("shishi-tools-{}", id()))).unwrap())
    }
    fn register(&self, command: &str) -> ToolDefinition {
        let mut tool: ToolDefinition=serde_json::from_value(self.0.tool_save(&json!({"tool":ToolDefinition { name:format!("demo_{}", self.0.tool_list().as_array().unwrap().len()),description:"Synthetic tool".into(),command_template:command.into(),..Default::default() }})).unwrap()).unwrap();
        tool = serde_json::from_value(
            self.0
                .tool_toggle(&json!({"id":tool.id,"updated":tool.updated,"enabled":true}))
                .unwrap(),
        )
        .unwrap();
        tool
    }
    fn task(&self, tools: &[ToolDefinition]) -> Task {
        serde_json::from_value(self.0.task_edit(&json!({"task":{"title":"Synthetic task","description":"Synthetic context","status":"待处理","enabled_tool_ids":tools.iter().map(|t|&t.id).collect::<Vec<_>>()}})).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        *self.0.store.lock().unwrap() = None;
        let _ = fs::remove_dir_all(&self.0.root);
    }
}
fn helper() -> &'static Path {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER.get_or_init(|| {
        let root=std::env::temp_dir().join(format!("shishi tool fixture 中文 {}",id()));
        fs::create_dir_all(&root).unwrap();
        let source=root.join("helper.rs");
        fs::write(&source,r#"use std::{io::{self,Write},time::Duration};
fn main(){ let mut args=std::env::args().skip(1); match args.next().as_deref(){
 Some("dump")=>{ for arg in args {println!("ARG:{}",arg.as_bytes().iter().map(|b|format!("{b:02x}")).collect::<String>());} },
 Some("flood")=>{let t=std::thread::spawn(||{for _ in 0..10000 {eprintln!("stderr data");}});for _ in 0..10000 {println!("stdout data");}t.join().unwrap();},
 Some("fail")=>{println!("ordinary text");eprintln!("synthetic error");std::process::exit(7);},
 Some("encoding")=>{io::stdout().write_all(&[0xd6,0xd0,0xce,0xc4]).unwrap();},
 Some("utf8")=>{println!("中文输出");eprintln!("中文错误");},
 Some("wait")=>{std::thread::sleep(Duration::from_secs(30));},
 Some("child")=>{let child=std::process::Command::new(std::env::current_exe().unwrap()).arg("wait").spawn().unwrap();println!("{}",child.id());io::stdout().flush().unwrap();if let Some(marker)=args.next(){std::fs::write(marker,child.id().to_string()).unwrap();}std::thread::sleep(Duration::from_secs(30));},
 _=>std::process::exit(2) }}"#).unwrap();
        let output=Command::new("rustc").arg("--edition=2021").arg(&source).arg("-o").arg(root.join("synthetic.exe")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        root.join("synthetic.exe")
    }).as_path()
}
fn executable(mode: &str) -> String {
    format!("\"{}\" {mode}", helper().display())
}
fn execute(tool: &ToolDefinition, args: Value) -> CommandResult {
    tool_runner::execute(
        tool,
        "synthetic-call",
        &args,
        &AtomicBool::new(false),
        Instant::now() + Duration::from_secs(10),
    )
}
fn tool(command: &str) -> ToolDefinition {
    ToolDefinition {
        name: "synthetic_tool".into(),
        description: "Synthetic tool".into(),
        command_template: command.into(),
        ..Default::default()
    }
}

#[test]
fn registrations_and_prompts_are_atomic_and_old_bindings_have_no_authority() {
    let fixture = Fixture::new();
    let app = &fixture.0;
    assert!(app.tool_list().as_array().unwrap().is_empty());
    let legacy: Config =
        serde_json::from_value(json!({"classify_prompt":"Keep this","model_profiles":null}))
            .unwrap();
    assert!(!PURPOSES.contains(&"tool_call"));
    assert_eq!(legacy.classify_prompt, "Keep this");
    let draft = app
        .tool_save(&json!({"tool":ToolDefinition::default()}))
        .unwrap();
    assert_eq!(draft["enabled"], false);
    assert!(app
        .tool_toggle(&json!({"id":draft["id"],"updated":draft["updated"],"enabled":true}))
        .is_err());
    let saved = fixture.register("echo synthetic");
    fixture.task(&[saved.clone()]);
    let before = serde_json::to_value(app.state.lock().unwrap().clone()).unwrap();
    crate::test_support::persistence_failure(&app.root, true).unwrap();
    assert!(app
        .tool_toggle(&json!({"id":saved.id,"updated":saved.updated,"enabled":false}))
        .is_err());
    assert_eq!(
        serde_json::to_value(app.state.lock().unwrap().clone()).unwrap(),
        before
    );
    crate::test_support::persistence_failure(&app.root, false).unwrap();
    assert_eq!(
        tool_registry::effective(&app.state.lock().unwrap()).len(),
        1
    );
    let version = crate::prompts::current(&app.state.lock().unwrap(), "tool_call")
        .unwrap()
        .id
        .clone();
    app.save_prompt(
        &json!({"target":"tool_call","expected":version,"text":"Synthetic custom policy"}),
    )
    .unwrap();
    assert!(app
        .tool_delete(&json!({"id":saved.id,"updated":saved.updated}))
        .is_ok());
    assert!(app.state.lock().unwrap().tasks[0]
        .enabled_tool_ids
        .contains(&saved.id));
    assert!(tool_registry::effective(&app.state.lock().unwrap()).is_empty());
    let reloaded = App::load(app.root.clone()).unwrap();
    assert_eq!(
        reloaded.state.lock().unwrap().config.tool_call_prompt,
        "Synthetic custom policy"
    );
    assert!(reloaded.state.lock().unwrap().tasks[0]
        .enabled_tool_ids
        .contains(&saved.id));
}

#[test]
fn actual_exe_and_cmd_wrapper_receive_lossless_values_and_unsupported_inputs_fail_before_spawn() {
    let script = helper().parent().unwrap().join("fixture wrapper.cmd");
    fs::write(&script, "@echo off\r\n@\"%~dp0synthetic.exe\" dump %*\r\n").unwrap();
    for template in [
        format!("{} {{{{value}}}}", executable("dump")),
        format!("\"{}\" {{{{value}}}}", script.display()),
    ] {
        let mut t = tool(&template);
        t.parameters = vec![Parameter {
            name: "value".into(),
            ..Default::default()
        }];
        for value in [
            "DEMO 001",
            "中文 与 空格",
            "a&b|c<d>e^f!g(h)[i]{j}=;,+",
            "",
            r"C:\synthetic\file.txt",
        ] {
            let result = execute(&t, json!({"value":value}));
            assert_eq!(result.status, "completed", "{result:?}");
            let expected = format!(
                "ARG:{}",
                value
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            assert_eq!(
                result.stdout.trim(),
                expected,
                "template={template}; value={value}"
            );
        }
        for value in ["double\"quote", "%PATH%", "line\nbreak", "tail\\"] {
            assert_eq!(execute(&t, json!({"value":value})).status, "rejected");
        }
        for args in [json!({"value":1}), json!({"value":"ok","extra":true})] {
            assert_eq!(execute(&t, args).status, "rejected");
        }
    }
    let mut cmd = tool("echo {{value}}");
    cmd.parameters = vec![Parameter {
        name: "value".into(),
        ..Default::default()
    }];
    // Unsupported historical encodings cannot run until explicitly resaved.
    cmd.output_encoding = "utf-16le".into();
    cmd.command_template = "cmd /d /v:off /u /c echo {{value}}".into();
    let result = execute(&cmd, json!({"value":"中文 & safe"}));
    assert_eq!(result.status, "rejected", "{result:?}");
    let ps = helper().parent().unwrap().join("fixture interpreter.ps1");
    fs::write(&ps,"param([string]$Text)\n[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)\n[Console]::Write($Text)").unwrap();
    cmd.command_template=format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" {{{{value}}}}",ps.display());
    cmd.output_encoding = "utf-8".into();
    let result = execute(&cmd, json!({"value":"DEMO 中文 001"}));
    assert_eq!(result.stdout, "DEMO 中文 001", "{result:?}");
}

#[test]
fn string_arguments_empty_omissions_and_legacy_conversion_are_explicit() {
    let mut t = tool("echo {{count}} {{optional}}");
    t.parameters = vec![
        Parameter {
            name: "count".into(),
            ..Default::default()
        },
        Parameter {
            name: "optional".into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        tool_runner::render(&t, &json!({"count":"2"})).unwrap(),
        "echo \"2\" \"\""
    );
    for value in [json!(2), json!(true), json!({}), json!([]), Value::Null] {
        assert!(tool_runner::render(&t, &json!({"count":value})).is_err());
    }
    assert_eq!(t.schema()["properties"]["count"]["type"], "string");
    assert!(t.schema()["required"].as_array().unwrap().is_empty());
    t.parameters[0].kind = "integer".into();
    t.parameters[0].default = Some(json!(2));
    assert!(t.validate(true).is_err());
    let old = json!(t);
    let fixture = Fixture::new();
    let mut legacy = t.clone();
    legacy.id = "legacy".into();
    fixture.0.state.lock().unwrap().config.tools.push(legacy);
    assert!(!fixture.0.tool_list()[0]["compatibility_issue"].is_null());
    assert!(fixture
        .0
        .tool_toggle(&json!({"id":"legacy","updated":0,"enabled":true}))
        .is_err());
    assert_eq!(json!(t), old);
    t.simplify();
    assert!(t.validate(true).is_ok());
    assert_eq!(t.arguments(&json!({})).unwrap()["count"], "");
    for template in [
        "echo {{missing}}",
        "echo \"{{count}}\"",
        "echo --n={{count}}",
    ] {
        t.command_template = template.into();
        assert!(t.validate(true).is_err());
    }
}

#[test]
fn making_prompt_works_before_implementation_and_has_no_execution_or_registration_side_effects() {
    let fixture = Fixture::new();
    let draft = ToolDefinition {
        name: "synthetic_lookup".into(),
        description: "Synthetic purpose {{tool_name}}".into(),
        ..Default::default()
    };
    let before = json!(fixture.0.state.lock().unwrap().clone());
    let copied = fixture
        .0
        .tool_making_prompt(&json!({"tool":draft}))
        .unwrap();
    let text = copied["text"].as_str().unwrap();
    assert!(text.contains("synthetic_lookup"));
    assert!(text.contains("待填写"));
    assert!(text.contains("{{参数名}}"));
    assert!(text.contains("Synthetic purpose {{tool_name}}"));
    assert_eq!(json!(fixture.0.state.lock().unwrap().clone()), before);
}

#[test]
fn output_failures_encoding_limits_and_deadlines_are_truthful() {
    let utf8 = execute(&tool(&executable("utf8")), json!({}));
    assert_eq!(utf8.status, "completed");
    assert!(utf8.stdout.contains("中文输出") && utf8.stderr.contains("中文错误"));
    let failed = execute(&tool(&executable("fail")), json!({}));
    assert_eq!(failed.status, "nonzero_exit");
    assert_eq!(failed.exit_code, Some(7));
    assert!(failed.stderr.contains("synthetic error"));
    assert!(failed.stdout.contains("ordinary text"));
    let flood = execute(&tool(&executable("flood")), json!({}));
    assert_eq!(flood.status, "completed");
    assert!(flood.truncated);
    assert_eq!(flood.stdout.len(), tool_runner::OUTPUT_LIMIT);
    assert_eq!(flood.stderr.len(), tool_runner::OUTPUT_LIMIT);
    let missing = execute(&tool("shishi-synthetic-does-not-exist.exe"), json!({}));
    assert_eq!(missing.status, "nonzero_exit");
    assert_ne!(missing.exit_code, Some(0));
    let mut encoded = tool(&executable("encoding"));
    assert_eq!(execute(&encoded, json!({})).status, "output_error");
    encoded.output_encoding = "936".into();
    assert_eq!(execute(&encoded, json!({})).status, "rejected");
    encoded.working_directory = helper()
        .parent()
        .unwrap()
        .join("not-created")
        .to_string_lossy()
        .into();
    assert_eq!(execute(&encoded, json!({})).status, "rejected");
    let wait = tool(&executable("wait"));
    let started = Instant::now();
    let timeout = tool_runner::execute(
        &wait,
        "fixed-deadline",
        &json!({}),
        &AtomicBool::new(false),
        Instant::now() + Duration::from_millis(150),
    );
    assert!(timeout.timed_out);
    assert!(started.elapsed() < Duration::from_secs(5));
    let global = tool_runner::execute(
        &wait,
        "global",
        &json!({}),
        &AtomicBool::new(false),
        Instant::now() + Duration::from_millis(100),
    );
    assert!(global.timed_out);
    assert!(global.duration_ms < 1500);
    let cancel = AtomicBool::new(true);
    let result = tool_runner::execute(
        &wait,
        "cancel",
        &json!({}),
        &cancel,
        Instant::now() + Duration::from_secs(5),
    );
    assert!(result.cancelled);
}

#[test]
fn cancellation_kills_the_owned_tree_and_preserves_bounded_partial_output() {
    let marker = helper()
        .parent()
        .unwrap()
        .join(format!("child-ready-{}", id()));
    let t = tool(&format!("{} \"{}\"", executable("child"), marker.display()));
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let ready = marker.clone();
    let thread = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(8);
        while !ready.exists() {
            assert!(
                Instant::now() < until,
                "synthetic child did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        trigger.store(true, Ordering::SeqCst);
    });
    let result = tool_runner::execute(
        &t,
        "cancel-tree",
        &json!({}),
        &cancel,
        Instant::now() + Duration::from_secs(10),
    );
    thread.join().unwrap();
    assert!(result.cancelled, "{result:?}");
    let pid = result.stdout.trim().parse::<u32>().unwrap();
    fs::remove_file(marker).unwrap();
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !process.is_null() {
            let mut code = 259;
            for _ in 0..40 {
                GetExitCodeProcess(process, &mut code);
                if code != 259 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            CloseHandle(process);
            assert_ne!(code, 259);
        }
    }
}

fn session_result(session: &ToolSession<'_>, name: &str, call: &str, arguments: &Value) -> Value {
    match session
        .execute(
            name,
            call,
            arguments,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap()
    {
        ToolResult::Text(text) => serde_json::from_str(&text).unwrap(),
        _ => panic!("unexpected result"),
    }
}

#[test]
fn sessions_freeze_templates_recheck_live_switches_and_persist_intents_before_execution() {
    let fixture = Fixture::new();
    let t = fixture.register("echo frozen_template");
    let task = fixture.task(&[t.clone()]);
    let config = fixture.0.state.lock().unwrap().config.clone();
    let session = ToolSession::new(
        &fixture.0,
        "request",
        "edit",
        &task.id,
        vec![t.clone()],
        &config,
        0,
    );
    fixture.0.state.lock().unwrap().config.tools[0].command_template =
        "echo changed_template".into();
    assert!(
        session_result(&session, &t.name, "first", &json!({}))["stdout"]
            .as_str()
            .unwrap()
            .contains("frozen_template")
    );
    fixture.0.state.lock().unwrap().tasks[0]
        .enabled_tool_ids
        .clear();
    assert_eq!(
        session_result(&session, &t.name, "empty_legacy_list", &json!({}))["status"],
        "completed"
    );
    fixture.0.state.lock().unwrap().config.tools[0].enabled = false;
    assert_eq!(
        session_result(&session, &t.name, "disabled", &json!({}))["status"],
        "rejected"
    );
    assert!(session
        .execute(
            &t.name,
            "first",
            &json!({}),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        )
        .is_err());
    assert_eq!(fixture.0.state.lock().unwrap().tool_runs[0].calls.len(), 3);
    crate::test_support::persistence_failure(&fixture.0.root, true).unwrap();
    assert!(session
        .execute(
            &t.name,
            "not_saved",
            &json!({}),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        )
        .is_err());
    assert_eq!(fixture.0.state.lock().unwrap().tool_runs[0].calls.len(), 3);
}

#[test]
fn local_tests_need_no_model_and_share_fixed_rates_cancellation_and_audit() {
    let fixture = Fixture::new();
    let t = fixture.register("echo synthetic");
    fixture.0.tool_test_cancel(&json!({"test_id":"cancelled"}));
    assert_eq!(
        fixture
            .0
            .tool_test(&json!({"tool":t,"arguments":{},"test_id":"cancelled"}))
            .unwrap()["status"],
        "cancelled"
    );
    assert_eq!(
        fixture
            .0
            .tool_test(&json!({"tool":t,"arguments":{},"test_id":"first"}))
            .unwrap()["status"],
        "completed"
    );
    assert_eq!(
        fixture
            .0
            .tool_test(&json!({"tool":t,"arguments":{"extra":"invalid"},"test_id":"invalid"}))
            .unwrap()["status"],
        "rejected"
    );
    fixture.0.tool_runtime.rates.lock().unwrap().insert(
        t.id.clone(),
        std::iter::repeat_with(Instant::now)
            .take(tool_registry::CALLS_PER_MINUTE)
            .collect(),
    );
    assert_eq!(
        fixture
            .0
            .tool_test(&json!({"tool":t,"arguments":{},"test_id":"limited"}))
            .unwrap()["status"],
        "rejected"
    );
    let history = fixture
        .0
        .tool_run_history(&json!({"tool_id":t.id,"purpose":"tool_test","day":date()}))
        .unwrap();
    assert_eq!(history["rows"].as_array().unwrap().len(), 4);
    assert_eq!(history["tools"][0]["id"], t.id);
    let mut s = fixture.0.state.lock().unwrap();
    s.config.tools[0].name = "renamed_tool".into();
    s.config.tools.clear();
    drop(s);
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"tool_id":t.id}))
            .unwrap()["rows"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert!(fixture
        .0
        .tool_run_history(&json!({"day":"invalid"}))
        .is_err());
    assert_eq!(fixture.0.state.lock().unwrap().calls, 0);
}

#[test]
fn rejected_provider_arguments_are_queryable_without_inventing_legacy_metadata() {
    let fixture = Fixture::new();
    let t = fixture.register("echo synthetic");
    let config = fixture.0.state.lock().unwrap().config.clone();
    let session = ToolSession::new(
        &fixture.0,
        "request",
        "classify",
        "",
        vec![t.clone()],
        &config,
        0,
    );
    session.record_rejection(&json!({"response":{"tool_calls":[{"id":"bad","function":{"name":t.name,"arguments":"{\"value\":2}"}}]},"error":"Invalid tool arguments"})).unwrap();
    session
        .finish(Some("Invalid tool arguments"), &Value::Null)
        .unwrap();
    let run = fixture
        .0
        .tool_run_get(&json!({"run_id":"request"}))
        .unwrap();
    assert_eq!(run["calls"][0]["arguments"]["value"], 2);
    assert_eq!(run["calls"][0]["result"]["status"], "rejected");
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"purpose":"classify","tool_id":t.id}))
            .unwrap()["rows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.0.state.lock().unwrap().tool_runs.push(serde_json::from_value(json!({"id":"old","task_id":"old-task","status":"completed","tools":[t],"calls":[{"tool":t.name,"call_id":"old-call","arguments":{},"started":1}]})).unwrap());
    let old = fixture.0.tool_run_get(&json!({"run_id":"old"})).unwrap();
    assert_eq!(old["purpose"], "");
    assert_eq!(old["source"], Value::Null);
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"tool_id":t.id}))
            .unwrap()["rows"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn exhausted_continuation_budget_records_a_rejection_without_executing() {
    let fixture = Fixture::new();
    let t = fixture.register("echo must_not_execute");
    let mut config = fixture.0.state.lock().unwrap().config.clone();
    config.max_input_chars = 100;
    let session = ToolSession::new(
        &fixture.0,
        "request",
        "archive",
        "",
        vec![t.clone()],
        &config,
        99,
    );
    assert!(session
        .execute(
            &t.name,
            "over_budget",
            &json!({}),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2)
        )
        .is_err());
    assert!(session.budget_exhausted.load(Ordering::SeqCst));
    let s = fixture.0.state.lock().unwrap();
    let result = s.tool_runs[0].calls[0].result.as_ref().unwrap();
    assert_eq!(result.status, "rejected");
    assert!(result.stdout.is_empty());
}
#[test]
fn additive_table_upgrade_preserves_legacy_configuration_and_history_queries_are_scoped_and_paged()
{
    let fixture = Fixture::new();
    let task = fixture.task(&[]);
    fixture.0.persist().unwrap();
    *fixture.0.store.lock().unwrap() = None;
    let path = crate::storage::data_dir(&fixture.0.root).join(crate::state_store::FILE);
    {
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("DROP TABLE tool_runs").unwrap();
    }
    let restored = App::load(fixture.0.root.clone()).unwrap();
    assert_eq!(restored.state.lock().unwrap().tasks[0].id, task.id);
    assert!(restored.state.lock().unwrap().config.tools.is_empty());
    restored
        .commit_state(|state| {
            for i in 0..25 {
                state.tool_runs.push(ToolRun {
                    id: format!("run-{i}"),
                    task_id: task.id.clone(),
                    status: "completed".into(),
                    ..Default::default()
                });
            }
            state.tool_runs.push(ToolRun {
                id: "other-run".into(),
                task_id: "other-task".into(),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let first = restored
        .tool_run_history(&json!({"task_id":task.id}))
        .unwrap();
    assert_eq!(first["rows"].as_array().unwrap().len(), 20);
    assert_eq!(first["rows"][0]["id"], "run-24");
    assert_eq!(first["pages"], 2);
    let last = restored
        .tool_run_history(&json!({"task_id":task.id,"page":99}))
        .unwrap();
    assert_eq!(last["rows"].as_array().unwrap().len(), 5);
    let again = App::load(restored.root.clone()).unwrap();
    assert_eq!(again.state.lock().unwrap().tool_runs.len(), 26);
}

#[test]
fn call_history_filters_before_paging_and_opens_the_exact_call_across_midnight() {
    let fixture = Fixture::new();
    let mut registered = tool("echo synthetic");
    registered.id = "history-tool".into();
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-25T00:00:00+08:00")
        .unwrap()
        .timestamp_millis();
    let mut calls = (0..26)
        .map(|i| CallRecord {
            task_ids: vec![],
            call_id: format!("call-{i}"),
            tool_id: if i == 25 {
                String::new()
            } else {
                registered.id.clone()
            },
            tool: registered.name.clone(),
            arguments: json!({"record":format!("DEMO {i:02}")}),
            started: if i == 0 { start - 1 } else { start + i },
            finished: start + i + 1,
            result: Some(CommandResult {
                status: if i == 25 { "nonzero_exit" } else { "completed" }.into(),
                exit_code: Some(if i == 25 { 7 } else { 0 }),
                stdout: if i == 25 {
                    "Unique synthetic output"
                } else {
                    "ordinary text"
                }
                .into(),
                ..Default::default()
            }),
        })
        .collect::<Vec<_>>();
    calls.push(CallRecord {
        tool: "unrelated_tool".into(),
        tool_id: "other".into(),
        started: start + 30,
        arguments: json!({"record":"DEMO 25"}),
        ..Default::default()
    });
    fixture.0.state.lock().unwrap().tool_runs.push(ToolRun {
        id: "history-run".into(),
        task_id: "task-a".into(),
        purpose: "edit".into(),
        tools: vec![registered.clone()],
        started: start - 100,
        status: "completed".into(),
        calls,
        ..Default::default()
    });
    let first = fixture
        .0
        .tool_run_history(
            &json!({"view":"calls","task_id":"task-a","tool_id":registered.id,"day":"2026-10-25"}),
        )
        .unwrap();
    assert_eq!(first["total"], 25);
    assert_eq!(first["rows"].as_array().unwrap().len(), 20);
    assert_eq!(first["rows"][0]["call_index"], 25);
    assert_eq!(first["rows"][0]["status"], "nonzero_exit");
    assert_eq!(first["rows"][0]["tool"], registered.name);
    assert_eq!(first["rows"][0]["arguments"]["record"], "DEMO 25");
    assert!(first["rows"][0].get("stdout").is_none());
    let last = fixture
        .0
        .tool_run_history(
            &json!({"view":"calls","tool_id":registered.id,"day":"2026-10-25","page":99}),
        )
        .unwrap();
    assert_eq!(last["page"], 2);
    assert_eq!(last["rows"].as_array().unwrap().len(), 5);
    let found=fixture.0.tool_run_history(&json!({"view":"calls","tool_id":registered.id,"purpose":"edit","query":"SYNTHETIC unique DEMO 25 非零退出"})).unwrap();
    assert_eq!(found["total"], 1);
    let detail = fixture
        .0
        .tool_run_get(&json!({"run_id":"history-run","call_index":25}))
        .unwrap();
    assert_eq!(detail["calls"].as_array().unwrap().len(), 1);
    assert_eq!(
        detail["calls"][0]["result"]["stdout"],
        "Unique synthetic output"
    );
    assert!(fixture
        .0
        .tool_run_get(&json!({"run_id":"history-run","call_index":999}))
        .is_err());
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":"unrelated","query":"DEMO"}))
            .unwrap()["total"],
        0
    );
    assert!(fixture
        .0
        .tool_run_history(&json!({"view":"calls","query":"x".repeat(201)}))
        .is_err());
    fixture.0.state.lock().unwrap().config.tools.clear();
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","tool_id":registered.id,"query":"DEMO 25"}))
            .unwrap()["total"],
        1
    );
}

#[test]
fn generation_links_actual_calls_only_with_the_committed_tasks_and_preserves_old_lists() {
    let fixture = Fixture::new();
    let draft = fixture.task(&[]);
    let unrelated = fixture.task(&[]);
    {
        let mut state = fixture.0.state.lock().unwrap();
        state
            .tasks
            .iter_mut()
            .find(|t| t.id == draft.id)
            .unwrap()
            .generation = "pending".into();
        state.tool_runs.push(ToolRun {
            id: "synthetic-generation".into(),
            calls: vec![
                CallRecord {
                    call_id: "first-call".into(),
                    tool: "synthetic_tool".into(),
                    ..Default::default()
                },
                CallRecord {
                    call_id: "second-call".into(),
                    tool: "synthetic_tool".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
    }
    let result = json!({"_tool_run_id":"synthetic-generation","tasks":[{"title":"First synthetic task","tool_call_ids":["first-call"]},{"title":"Second synthetic task","tool_call_ids":["second-call"]}]});
    crate::test_support::persistence_failure(&fixture.0.root, true).unwrap();
    assert!(fixture
        .0
        .complete_generation(&draft, Ok(result.clone()))
        .is_err());
    assert!(fixture.0.state.lock().unwrap().tool_runs[0]
        .calls
        .iter()
        .all(|call| call.task_ids.is_empty()));
    crate::test_support::persistence_failure(&fixture.0.root, false).unwrap();
    fixture.0.complete_generation(&draft, Ok(result)).unwrap();
    let snapshot = fixture.0.snapshot();
    for task in snapshot["tasks"].as_array().unwrap() {
        assert_eq!(task["has_tool_calls"], task["id"] != unrelated.id);
    }
    assert!(fixture.0.state.lock().unwrap().tool_runs[0]
        .calls
        .iter()
        .all(|call| call.task_ids.len() == 1));
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":draft.id}))
            .unwrap()["total"],
        1
    );
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":unrelated.id}))
            .unwrap()["total"],
        0
    );
    let restored = App::load(fixture.0.root.clone()).unwrap();
    assert!(restored.snapshot()["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["has_tool_calls"] == true));
}

#[test]
fn same_batch_calls_keep_task_scope_through_search_detail_and_legacy_history() {
    let fixture = Fixture::new();
    fixture.0.state.lock().unwrap().tool_runs.push(ToolRun {
        id: "batch".into(),
        related_task_ids: vec!["task-a".into(), "task-b".into()],
        calls: vec![
            CallRecord {
                call_id: "call-a".into(),
                task_ids: vec!["task-a".into()],
                tool: "shared_tool".into(),
                arguments: json!({"record":"Alpha"}),
                ..Default::default()
            },
            CallRecord {
                call_id: "call-b".into(),
                task_ids: vec!["task-b".into()],
                tool: "shared_tool".into(),
                arguments: json!({"record":"Beta"}),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    fixture.0.state.lock().unwrap().tool_runs.push(ToolRun {
        id: "manual-test".into(),
        purpose: "tool_test".into(),
        calls: vec![CallRecord {
            tool: "shared_tool".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    for (task, call) in [("task-a", "call-a"), ("task-b", "call-b")] {
        let rows = fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":task,"page":99}))
            .unwrap();
        assert_eq!(rows["total"], 1);
        assert_eq!(rows["rows"][0]["call_id"], call);
        let run = fixture
            .0
            .tool_run_get(&json!({"run_id":"batch","task_id":task}))
            .unwrap();
        assert_eq!(run["calls"].as_array().unwrap().len(), 1);
        assert_eq!(run["calls"][0]["call_id"], call);
        assert_eq!(run["call_count"], 1);
        assert_eq!(
            fixture
                .0
                .tool_run_history(&json!({"task_id":task}))
                .unwrap()["rows"][0]["call_count"],
            1
        );
    }
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":"task-a","query":"Beta"}))
            .unwrap()["total"],
        0
    );
    assert!(fixture
        .0
        .tool_run_get(&json!({"run_id":"batch","task_id":"task-a","call_index":1}))
        .is_err());
    assert!(fixture
        .0
        .tool_run_get(&json!({"run_id":"manual-test","task_id":"task-a","call_index":0}))
        .is_err());
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls"}))
            .unwrap()["total"],
        3
    );
    for call in &mut fixture.0.state.lock().unwrap().tool_runs[0].calls {
        call.task_ids.clear();
    }
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":"task-a"}))
            .unwrap()["total"],
        0
    );
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls"}))
            .unwrap()["total"],
        3
    );
    fixture.0.state.lock().unwrap().tool_runs[0].related_task_ids = vec!["task-a".into()];
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":"task-a"}))
            .unwrap()["total"],
        2
    );
    link_tasks(
        &mut fixture.0.state.lock().unwrap(),
        &json!({"_tool_run_id":"batch","tool_call_ids":["not-executed"]}),
        &["task-c".into()],
    );
    assert_eq!(
        fixture
            .0
            .tool_run_history(&json!({"view":"calls","task_id":"task-c"}))
            .unwrap()["total"],
        0
    );
}
