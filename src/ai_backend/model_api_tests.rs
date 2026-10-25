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
        fs::write(&source,r##"use std::{fs,io::{self,Read,Write}};
fn main(){let args=std::env::args().collect::<Vec<_>>();
 if args.iter().any(|a|a=="--help"){println!("--ignore-user-config --ignore-rules");return;}
 assert!(args.iter().any(|a|a=="mcp_servers={}"));
 assert!(!args.iter().any(|a|a.starts_with("mcp_servers.")));
 assert!(std::env::var_os("CODEX_THREAD_ID").is_none());
 assert!(std::env::var_os("CODEX_APP_TOOLS_PIPE_PATH").is_none());
 let mut prompt=String::new();io::stdin().read_to_string(&mut prompt).unwrap();
 if prompt.contains("scenario:cancel"){std::thread::sleep(std::time::Duration::from_secs(30));}
 let queried=prompt.contains("\"role\":\"tool\"");
 let value=if prompt.contains("scenario:unknown") {r#"{"tool_call":{"name":"undeclared","arguments":"{}"},"result":null}"#}
 else if prompt.contains("scenario:bad-args") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{\"value\":7}"},"result":null}"#}
 else if prompt.contains("scenario:both") {r#"{"tool_call":{"name":"synthetic_echo","arguments":"{}"},"result":{"answer":"invalid"}}"#}
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
        let session =
            ToolSession::new(&self.app, "text-fixture", "edit", "", tool, &self.config, 0);
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
fn malformed_calls_never_execute_commands() {
    for scenario in ["scenario:unknown", "scenario:bad-args", "scenario:both"] {
        let fixture = Fixture::new();
        let mut usage = Value::Null;
        assert!(fixture
            .invoke_events(scenario, &AtomicBool::new(false), &mut |event| {
                if let AnalysisEvent::Usage(value) = event {
                    usage = value
                }
            })
            .is_err());
        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 5);
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
fn plain_model_continuation_overflow_preserves_the_budget_class_and_usage() {
    let mut fixture = Fixture::new();
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]});
    fixture.config.max_input_chars = input_chars(
        &fixture.config,
        "scenario:query-only",
        &schema,
        Some(&fixture.tools),
    ) + 20;
    let mut usage = Value::Null;
    let error = fixture
        .invoke_events(
            "scenario:query-only",
            &AtomicBool::new(false),
            &mut |event| {
                if let AnalysisEvent::Usage(value) = event {
                    usage = value;
                }
            },
        )
        .unwrap_err();
    assert!(error.starts_with(crate::ai::INPUT_LIMIT_ERROR), "{error}");
    assert_eq!(usage["output_tokens"], 5);
    assert_eq!(usage["partial"], true);
    assert!(fixture.app.state.lock().unwrap().tool_runs.is_empty());
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
    assert_eq!(accepted.value["result"]["summary"], "");
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
fn automatic_continuation_overflow_pauses_without_consuming_or_retrying_messages() {
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
    assert!(error.starts_with(crate::ai::INPUT_LIMIT_ERROR), "{error}");
    {
        let state = fixture.app.state.lock().unwrap();
        assert!(state.auto_analysis_blocked);
        assert!(state.pending_analysis);
        assert!(state.analyzed.is_empty());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.calls, 1, "overflow happened after submission");
        assert!(
            state.tool_runs.is_empty(),
            "context reads never execute commands"
        );
    }
    let response = fixture.app.analyze_after_scan(AiMode::Automatic).unwrap();
    assert!(response["skipped"]
        .as_str()
        .unwrap()
        .contains("自动分析已暂停"));
    assert_eq!(fixture.app.state.lock().unwrap().calls, 1);
    assert!(
        crate::managed_data::load_state(&fixture.app.root)
            .unwrap()
            .auto_analysis_blocked
    );
}
