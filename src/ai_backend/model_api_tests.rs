use super::*;
use crate::{
    engine::App,
    model::*,
    tool_calling::ToolSession,
    tool_registry::{Parameter, ToolDefinition},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

fn cli() -> &'static Path {
    static CLI: OnceLock<PathBuf> = OnceLock::new();
    CLI.get_or_init(|| {
        let root=std::env::temp_dir().join(format!("ss-model-cli-{}",id()));fs::create_dir_all(&root).unwrap();
        let source=root.join("fixture.rs");
        fs::write(&source,r##"use std::{fs,io::{self,BufRead,Read,Write}};
fn main(){let args=std::env::args().collect::<Vec<_>>();
 if args.iter().any(|a|a=="--help"){println!("--ignore-user-config --ignore-rules");return;}
 if args.iter().any(|a|a=="app-server") {
  let executable=std::env::current_exe().unwrap();
  let model=if executable.parent().unwrap().file_name().unwrap()=="0000000000000001" {"legacy"} else {"synthetic"};
  for line in io::stdin().lock().lines() {
   let line=line.unwrap();
   let id=line.split("\"id\":").nth(1).unwrap_or("").trim_start().chars().take_while(char::is_ascii_digit).collect::<String>();
   if id.is_empty(){continue;}
   let result=if line.contains("\"method\":\"initialize\"") {"{}".into()}
    else if line.contains("\"method\":\"model/list\"") {format!(r#"{{"data":[{{"model":"{model}","hidden":false,"supportedReasoningEfforts":[{{"reasoningEffort":"low"}}]}}],"nextCursor":null}}"#)}
    else {panic!("unexpected metadata method: {line}")};
   println!("{{\"id\":{id},\"result\":{result}}}");io::stdout().flush().unwrap();
  }
  return;
 }
 assert!(args.iter().any(|a|a=="mcp_servers={}"));
 assert!(!args.iter().any(|a|a.starts_with("mcp_servers.")));
 assert!(std::env::var_os("CODEX_THREAD_ID").is_none());
 assert!(std::env::var_os("CODEX_APP_TOOLS_PIPE_PATH").is_none());
 let mut prompt=String::new();io::stdin().read_to_string(&mut prompt).unwrap();
 if prompt.contains("scenario:cancel"){std::thread::sleep(std::time::Duration::from_secs(30));}
 if prompt.contains("scenario:native-tool") {
  println!("{}",r#"{"type":"item.started","item":{"type":"mcp_tool_call"}}"#);io::stdout().flush().unwrap();
  std::thread::sleep(std::time::Duration::from_secs(30));return;
 }
 let queried=prompt.contains("\"role\":\"tool\"");
 let value=if prompt.contains("scenario:unknown") {r#"{"tool_call":{"name":"undeclared","arguments":"{}"},"result":null}"#}
 else if prompt.contains("scenario:bad-args") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":7}"},"result":null}"#}
 else if prompt.contains("scenario:both") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{}"},"result":{"answer":"invalid"}}"#}
 else if prompt.contains("scenario:empty") {r#"{"tool_call":null,"result":null}"#}
 else if prompt.contains("scenario:repair-") {
  if !prompt.contains("model response envelope was invalid") {
   if prompt.contains("scenario:repair-both") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":\"demo\"}"},"result":{"answer":"discard"}}"#}
   else {r#"{"tool_call":null,"result":null}"#}
  } else if !prompt.contains("SS_CMD_DONE") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":\"demo\"}"},"result":null}"#}
  else {r#"{"tool_call":null,"result":{"answer":"SS_CMD_DONE"}}"#}
 }
 else if prompt.contains("scenario:after-command") && prompt.contains("SS_CMD_DONE") {
  if !prompt.contains("model response envelope was invalid") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":\"demo\"}"},"result":{"answer":"discard"}}"#}
  else {r#"{"tool_call":null,"result":{"answer":"SS_CMD_DONE"}}"#}
 }
 else if !queried || prompt.contains("scenario:repeat-context") {r#"{"tool_call":{"name":"read_context","arguments":"{\"ids\":[\"ctx-row\"]}"},"result":null}"#}
 else if !prompt.contains("SS_CMD_DONE") && !prompt.contains("scenario:query-only") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":\"demo\"}"},"result":null}"#}
 else {r#"{"tool_call":null,"result":{"answer":"SS_CMD_DONE"}}"#};
 let output=&args[args.iter().position(|a|a=="--output-last-message").unwrap()+1];fs::write(output,value).unwrap();
 println!("{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{:?}}}}}",value);
 println!("{}",r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5,"cached_input_tokens":2}}"#);io::stdout().flush().unwrap();}
"##).unwrap();
        let executable=root.join("synthetic-cli.exe");
        let output=Command::new("rustc").args(["--edition=2021"]).arg(source).arg("-o").arg(&executable).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));executable
    }).as_path()
}
struct Fixture {
    app: App,
    config: Config,
    connection: Connection,
    tools: Value,
    payload: Value,
}
impl Fixture {
    fn new() -> Self {
        let app =
            App::load(std::env::temp_dir().join(format!("ss-model-exchange-{}", id()))).unwrap();
        let tool = ToolDefinition {
            id: "synthetic".into(),
            name: "synthetic_echo".into(),
            description: "Synthetic local echo".into(),
            enabled: true,
            command_template: "echo SS_CMD_DONE {{value}}".into(),
            parameters: vec![Parameter {
                name: "value".into(),
                description: "Synthetic string".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        app.state.lock().unwrap().config.tools.push(tool.clone());
        let mut config = app.state.lock().unwrap().config.clone();
        config.codex = cli().to_string_lossy().into();
        config.model = "synthetic".into();
        config.analysis_workdir = app.root.join("workspace").to_string_lossy().into();
        config.retain_ai_traces = true;
        let connection = Connection {
            executable: config.codex.clone(),
            ..Default::default()
        };
        let mut tools = crate::analysis_context::tools();
        tools["tools"].as_array_mut().unwrap().push(
            json!({"name":tool.name,"description":tool.description,"inputSchema":tool.schema()}),
        );
        let payload = json!({"_local_context":{"expires":now()+60_000,"records":[{"id":"ctx-row","kind":"task","data":{"id":"ctx-row","title":"Synthetic context"}}]}});
        Self {
            app,
            config,
            connection,
            tools,
            payload,
        }
    }
    fn invoke(&self, prompt: &str, cancel: &AtomicBool) -> Result<(Value, Value), String> {
        self.invoke_events(prompt, cancel, &mut |_| {})
    }
    fn invoke_events(
        &self,
        prompt: &str,
        cancel: &AtomicBool,
        event: &mut dyn FnMut(AnalysisEvent),
    ) -> Result<(Value, Value), String> {
        let tool = crate::tool_registry::effective(&self.app.state.lock().unwrap());
        let session = ToolSession::new(&self.app, "text-fixture", "edit", "", tool, &self.config);
        let schema = json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]});
        let trace = self.app.root.join("trace");
        fs::create_dir_all(&trace).unwrap();
        let result = super::analyze(
            &self.connection,
            AnalysisRequest {
                show_session: false,
                config: &self.config,
                kind: "edit",
                payload: &self.payload,
                root: &self.app.root,
                trace: &trace,
                prompt,
                schema: &schema,
                tools: Some(&self.tools),
                registered_tools: Some(&session),
                cancel,
            },
            event,
        );
        session
            .finish(result.as_ref().err().map(String::as_str), &Value::Null)
            .unwrap();
        result
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        *self.app.store.lock().unwrap() = None;
        let _ = fs::remove_dir_all(&self.app.root);
    }
}

#[test]
fn codex_text_queries_and_commands_use_the_api_loop_and_aggregate_usage() {
    let fixture = Fixture::new();
    let (value, usage) = fixture
        .invoke("scenario:normal", &AtomicBool::new(false))
        .unwrap();
    assert_eq!(value["answer"], "SS_CMD_DONE");
    assert_eq!(usage["input_tokens"], 30);
    assert_eq!(usage["output_tokens"], 15);
    assert_eq!(usage["cached_input_tokens"], 6);
    assert!(value["_context_seen"]
        .as_array()
        .unwrap()
        .contains(&json!("ctx-row")));
    let state = fixture.app.state.lock().unwrap();
    assert_eq!(state.tool_runs.len(), 1);
    assert_eq!(state.tool_runs[0].calls.len(), 1);
    let call = &state.tool_runs[0].calls[0];
    assert_eq!(call.tool, "synthetic_echo");
    assert!(call.call_id.starts_with("text:"));
    assert_eq!(call.arguments, json!({"value":"demo"}));
    assert_eq!(
        call.result.as_ref().unwrap().stdout.trim(),
        "SS_CMD_DONE \"demo\""
    );
}
#[test]
fn context_only_queries_do_not_create_command_history() {
    let fixture = Fixture::new();
    fixture
        .invoke("scenario:query-only", &AtomicBool::new(false))
        .unwrap();
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
}

#[test]
fn optional_round_traces_do_not_block_model_progress_or_usage() {
    for blocked in ["directory", "input.txt", "output.txt"] {
        let fixture = Fixture::new();
        let trace = fixture.app.root.join("trace");
        fs::create_dir_all(&trace).unwrap();
        let round = trace.join("round-0001");
        if blocked == "directory" {
            fs::write(&round, b"blocked").unwrap();
        } else {
            fs::create_dir_all(round.join(blocked)).unwrap();
        }
        let mut events = vec![];
        let (value, usage) = fixture
            .invoke_events(
                "scenario:query-only",
                &AtomicBool::new(false),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(value["answer"], "SS_CMD_DONE");
        assert_eq!(usage["input_tokens"], 20);
        assert_eq!(usage["output_tokens"], 10);
        assert!(events.iter().any(|event| matches!(event,
            AnalysisEvent::Stream { kind, text, .. } if kind == "diagnostic" && text.contains("跟踪保存失败"))));
        assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
    }
}

#[test]
fn optional_request_trace_failure_still_invokes_the_prepared_model_request() {
    let fixture = Fixture::new();
    let mut state = fixture.app.state.lock().unwrap().clone();
    state.config = fixture.config.clone();
    state.config.selected_connection.clear();
    state.config.model_profiles = None;
    let mut context = fixture
        .app
        .capture_prepare_context_from(&state, "edit", false, json!([]));
    let mut request = fixture
        .app
        .prepare_ai_request_with(
            "edit",
            &fixture.payload,
            crate::work_hours::AiMode::Manual,
            &mut context,
        )
        .unwrap();
    request.payload = fixture.payload.clone();
    request.prepared.prompt = "scenario:query-only".into();
    request.prepared.schema = json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]});
    request.prepared.tools = Some(fixture.tools.clone());
    fs::write(
        crate::storage::data_dir(&fixture.app.root).join("ai-traces"),
        b"blocked",
    )
    .unwrap();
    let mut events = vec![];
    let (value, usage) = crate::ai::invoke_prepared(
        &request,
        &fixture.app.root,
        "trace-fixture",
        &AtomicBool::new(false),
        None,
        &mut |event| events.push(event),
    )
    .unwrap();
    assert_eq!(value["answer"], "SS_CMD_DONE");
    assert_eq!(usage["output_tokens"], 10);
    assert!(events.iter().any(|event| matches!(event,
        AnalysisEvent::Stream { kind, text, .. } if kind == "diagnostic" && text.contains("跟踪保存失败"))));
}

#[test]
fn native_tool_isolation_stops_the_process_and_preserves_user_cancellation() {
    let fixture = Fixture::new();
    let payload = json!({});
    let schema = json!({"type":"object","properties":{}});
    for cancel_on_violation in [false, true] {
        let cancel = AtomicBool::new(false);
        let mut violation_at = None;
        let error = super::super::codex::analyze_round(
            AnalysisRequest {
                show_session: false,
                config: &fixture.config,
                kind: "edit",
                payload: &payload,
                root: &fixture.app.root,
                trace: &fixture.app.root,
                prompt: "scenario:native-tool",
                schema: &schema,
                tools: None,
                registered_tools: None,
                cancel: &cancel,
            },
            &fixture.connection,
            &mut |event| {
            if matches!(event,
                AnalysisEvent::Stream { kind, text, .. } if kind == "phase" && text.contains("未授权的原生工具")) {
                violation_at = Some(Instant::now());
                if cancel_on_violation {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
        }).unwrap_err();
        if cancel_on_violation {
            assert_eq!(error, crate::ai_control::CANCELLED);
        } else {
            assert!(error.contains("未授权的原生工具"), "{error}");
            assert!(!error.contains("超限"));
        }
        // Measure termination after the violation, excluding unrelated CLI
        // discovery/startup contention when the whole workspace runs together.
        assert!(
            violation_at
                .expect("native-tool violation was reported")
                .elapsed()
                < Duration::from_secs(5)
        );
    }
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
}

#[test]
fn managed_cli_upgrade_recovers_metadata_and_tool_rounds_without_changing_configuration() {
    let mut fixture = Fixture::new();
    let root = fixture.app.root.join("OpenAI/Codex/bin");
    let old = root.join("0000000000000001/codex.exe");
    let current = root.join("0000000000000002/codex.exe");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::copy(cli(), &old).unwrap();
    fixture.config.codex = old.to_string_lossy().into();
    fixture.connection.executable = fixture.config.codex.clone();
    let saved_config = serde_json::to_value(&fixture.config).unwrap();
    let saved_connection = fixture.connection.clone();

    let validate = |path: &str, model: &str| {
        crate::ai_backend::codex::validate_selection(path, model, &fixture.config.reasoning_effort)
    };
    // Cache the previous release's different catalogue before simulating removal.
    let catalog = crate::ai_backend::codex::list_models(&fixture.config.codex).unwrap();
    assert_eq!(catalog["models"][0]["model"], "legacy");
    assert!(validate(&fixture.config.codex, &fixture.config.model).is_err());
    fs::create_dir_all(current.parent().unwrap()).unwrap();
    fs::copy(cli(), &current).unwrap();
    fs::remove_file(&old).unwrap();
    assert!(!old.exists());

    // The old version's model cache must not reject the newly selected binary.
    validate(&fixture.config.codex, &fixture.config.model).unwrap();
    let catalog = crate::ai_backend::codex::list_models(&fixture.config.codex).unwrap();
    assert_eq!(catalog["models"][0]["model"], "synthetic");
    validate(&fixture.config.codex, &fixture.config.model).unwrap();
    for path in [&old, &current] {
        assert!(
            validate(&path.to_string_lossy(), "not-in-current-catalogue").is_err(),
            "catalogue lookup and insertion both use the actual executable"
        );
    }

    let cancel = AtomicBool::new(false);
    let client =
        crate::codex_bridge::Client::connect_guarded(&fixture.config.codex, Some(&cancel)).unwrap();
    assert_eq!(
        client.call("model/list", json!({})).unwrap()["data"][0]["model"],
        "synthetic"
    );
    client.close().unwrap();
    let (value, usage) = fixture.invoke("scenario:query-only", &cancel).unwrap();
    assert_eq!(value["answer"], "SS_CMD_DONE");
    assert_eq!(value["_context_seen"], json!(["ctx-row"]));
    assert_eq!(
        usage["output_tokens"], 10,
        "both model rounds use the recovered path"
    );
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
    assert_eq!(serde_json::to_value(&fixture.config).unwrap(), saved_config);
    assert_eq!(fixture.connection, saved_connection);
}
#[test]
fn malformed_calls_never_execute_commands() {
    for scenario in [
        "scenario:unknown",
        "scenario:bad-args",
        "scenario:both",
        "scenario:empty",
    ] {
        let fixture = Fixture::new();
        let mut usage = Value::Null;
        assert!(fixture
            .invoke_events(scenario, &AtomicBool::new(false), &mut |event| {
                if let AnalysisEvent::Usage(value) = event {
                    usage = value
                }
            })
            .is_err());
        assert_eq!(usage["input_tokens"], 30);
        assert_eq!(usage["output_tokens"], 15);
        assert_eq!(usage["partial"], true);
        assert!(fixture
            .app
            .state
            .lock()
            .unwrap()
            .tool_runs
            .iter()
            .flat_map(|r| &r.calls)
            .all(|call| call.result.as_ref().is_none_or(|r| r.status != "completed")));
    }
}

#[test]
fn invalid_envelopes_can_be_corrected_without_executing_or_replaying_commands() {
    for (scenario, rounds) in [
        ("scenario:repair-both", 3),
        ("scenario:repair-empty", 3),
        ("scenario:after-command", 4),
    ] {
        let fixture = Fixture::new();
        let (value, usage) = fixture.invoke(scenario, &AtomicBool::new(false)).unwrap();
        assert_eq!(value["answer"], "SS_CMD_DONE");
        assert_eq!(usage["input_tokens"], 10 * rounds);
        assert_eq!(usage["output_tokens"], 5 * rounds);
        let state = fixture.app.state.lock().unwrap();
        assert_eq!(state.tool_runs.len(), 1);
        assert_eq!(state.tool_runs[0].calls.len(), 1, "{scenario}");
        assert_eq!(
            state.tool_runs[0].calls[0].result.as_ref().unwrap().status,
            "completed"
        );
    }
}
#[test]
fn cancellation_and_total_deadline_stop_plain_model_rounds() {
    let fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::SeqCst);
        });
        assert!(fixture.invoke("scenario:cancel", &cancel).is_err());
    });
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
    let mut fixture = Fixture::new();
    fixture.config.ai_timeout_seconds = 2;
    let started = Instant::now();
    assert!(fixture
        .invoke("scenario:cancel", &AtomicBool::new(false))
        .is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
}
#[test]
fn tool_image_content_is_forwarded_as_native_images_not_base64_prompt_text() {
    let body = json!({"tools":[],"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}}]}]});
    let (prompt, images) = frame(&body);
    assert!(!prompt.contains("aW1hZ2U="));
    assert_eq!(
        images,
        json!([{"mimeType":"image/png","data":"aW1hZ2U="}])
            .as_array()
            .unwrap()
            .clone()
    );
}

#[test]
fn plain_model_independent_retrieval_can_follow_a_nearly_full_initial_request() {
    let mut fixture = Fixture::new();
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]});
    fixture.config.max_input_chars = input_chars(
        &fixture.config,
        "scenario:query-only",
        &schema,
        Some(&fixture.tools),
    ) + 20;
    let mut usage = Value::Null;
    let (value, _) = fixture
        .invoke_events(
            "scenario:query-only",
            &AtomicBool::new(false),
            &mut |event| {
                if let AnalysisEvent::Usage(value) = event {
                    usage = value;
                }
            },
        )
        .unwrap();
    assert_eq!(value["answer"], "SS_CMD_DONE");
    assert_eq!(value["_context_seen"], json!(["ctx-row"]));
    assert_eq!(usage["output_tokens"], 10);
    assert_ne!(usage["partial"], true);
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
}

#[test]
fn native_input_meter_matches_frame_and_envelope_after_independent_query_credits() {
    let schema =
        json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    let declarations =
        shishi_api::tool_definitions_from_mcp(&crate::analysis_context::tools()).unwrap();
    let mut body = shishi_api::openai::initial_request_body(
        "synthetic",
        None,
        "Unicode 甲 / \\ \"",
        &schema,
        &declarations,
    );
    let native_chars = |body: &Value| {
        frame(body).0.chars().count()
            + envelope(&schema, &body["tools"])
                .to_string()
                .chars()
                .count()
    };
    assert_eq!(body_input_chars(&schema, &body), native_chars(&body));
    let assistant = json!({"role":"assistant","content":null,"reasoning_content":"preserve this","tool_calls":[{"id":"cmd","type":"function","function":{"name":"synthetic_echo","arguments":"{\"value\":\"甲\\\\path\"}"}}]});
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(assistant.clone());
    body["messages"].as_array_mut().unwrap().push(
        json!({"role":"tool","tool_call_id":"cmd","content":"{\"stdout\":\"甲\\\\path\\r\\n\"}"}),
    );
    assert_eq!(body_input_chars(&schema, &body), native_chars(&body));
    let charged = native_chars(&body);
    let mixed = &mut body["messages"][2];
    mixed["tool_calls"].as_array_mut().unwrap().push(json!({"id":"query","type":"function","function":{"name":"read_image","arguments":"{\"image_id\":\"synthetic\"}"}}));
    let mut credits = shishi_api::openai::message_input_chars(mixed)
        - shishi_api::openai::message_input_chars(&assistant);
    for message in [
        json!({"role":"tool","tool_call_id":"query","content":"Synthetic read result"}),
        json!({"role":"user","content":[{"type":"text","text":"Synthetic image"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}}]}),
    ] {
        credits += shishi_api::openai::message_input_chars(&message) + 1;
        body["messages"].as_array_mut().unwrap().push(message);
    }
    assert_eq!(body_input_chars(&schema, &body) - credits, charged);
    assert_eq!(frame(&body).1.len(), 1);
}

#[test]
fn model_envelope_defaults_do_not_relax_tool_arguments_or_business_fields() {
    let schema = json!({"type":"object","additionalProperties":false,"properties":{
        "status":{"type":"string","enum":["pending"]},
        "summary":{"type":"string","maxLength":3,"default":""}},"required":["status","summary"]});
    let wrapped = envelope(&schema, &json!([{"function":{"name":"synthetic_echo"}}]));
    let value = shishi_api::schema_validate::validate_output(
        &wrapped,
        json!({"tool_call":null,"result":{"status":"pending","summary":["bad"]}}),
    )
    .unwrap();
    assert_eq!(value["result"]["summary"], "");
    let root = std::env::temp_dir().join(format!("ss-envelope-submit-{}", id()));
    let (paths, ticket) =
        shishi_acp::submit::SubmitPaths::create(&root, "synthetic", "classify", wrapped.clone())
            .unwrap();
    let submitted = paths
        .submit(
            &ticket,
            &json!({"tool_call":null,"result":{"status":"pending"}}),
        )
        .unwrap();
    assert_eq!(submitted["isError"], false);
    let accepted = paths
        .read_accepted("synthetic", "classify", &wrapped)
        .unwrap();
    assert_eq!(accepted["result"]["summary"], "");
    fs::remove_dir_all(root).unwrap();
    assert!(shishi_api::schema_validate::validate_output(
        &wrapped,
        json!({"tool_call":{"name":"synthetic_echo","arguments":{}},"result":null})
    )
    .is_err());
    assert!(shishi_api::schema_validate::validate_output(
        &wrapped,
        json!({"tool_call":null,"result":{"status":"unknown"}})
    )
    .is_err());
}

#[test]
fn automatic_query_without_progress_stops_without_consuming_messages_or_pausing_for_budget() {
    use crate::work_hours::AiMode;
    let fixture = Fixture::new();
    {
        let mut state = fixture.app.state.lock().unwrap();
        state.config = fixture.config.clone();
        state.config.selected_connection.clear();
        state.config.model_profiles = None;
        state.config.max_input_chars = 20_000;
        state.config.classify_prompt = "scenario:repeat-context".into();
        state.config.ai_timeout_seconds = 30;
        state.messages.insert(
            "message".into(),
            Message {
                id: "message".into(),
                source_id: "synthetic".into(),
                chat: "synthetic".into(),
                text: "Synthetic pending task".into(),
                private: true,
                time: now(),
                updated_at: now(),
                ..Default::default()
            },
        );
    }
    fixture.app.test_force_working();
    let error = fixture.app.analyze(AiMode::Automatic).unwrap_err();
    assert!(error.contains("made no progress"), "{error}");
    {
        let state = fixture.app.state.lock().unwrap();
        assert!(!state.auto_analysis_blocked);
        assert!(state.pending_analysis);
        assert!(state.analyzed.is_empty());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.calls, 1, "no progress happened after submission");
        assert!(
            state.tool_runs.is_empty(),
            "context reads never execute commands"
        );
    }
    let response = fixture.app.analyze_after_scan(AiMode::Automatic).unwrap();
    assert!(response["skipped"]
        .as_str()
        .unwrap()
        .contains("等待分析间隔"));
    assert_eq!(fixture.app.state.lock().unwrap().calls, 1);
    assert!(
        !crate::managed_data::load_state(&fixture.app.root)
            .unwrap()
            .auto_analysis_blocked
    );
}
