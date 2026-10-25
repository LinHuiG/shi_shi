use super::*;
use crate::{
    engine::App,
    model::*,
    tool_calling::ToolSession,
    tool_registry::{Parameter, ToolDefinition},
};
use shishi_api::{Endpoint, Transport, TransportError, TransportRequest, TransportResponse};
use std::{
    collections::VecDeque,
    sync::{atomic::AtomicBool, Mutex},
};

struct SyntheticTransport {
    replies: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<Value>>,
}

#[test]
fn continuation_overflow_keeps_partial_usage_and_the_application_budget_error() {
    let root = std::env::temp_dir().join(format!("ss-api-budget-{}", id()));
    let schema =
        json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    let tools = analysis_context::tools();
    let definitions = shishi_api::tool_definitions_from_mcp(&tools).unwrap();
    let config = Config {
        max_input_chars: shishi_api::openai::initial_input_chars(
            "synthetic",
            None,
            "Synthetic input",
            &schema,
            &definitions,
        ) + 20,
        model: "synthetic".into(),
        ..Default::default()
    };
    let transport = SyntheticTransport {
        replies: Mutex::new(VecDeque::from([
            json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"context-call","type":"function","function":{"name":"read_context","arguments":"{\"ids\":[\"row\"]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
        ])),
        requests: Mutex::new(vec![]),
    };
    let client = ChatCompletionsClient::with_transport(
        Endpoint::parse("https://example.com/v1").unwrap(),
        &transport,
    );
    let payload = json!({"_local_context":{"expires":now()+60_000,"records":[{"id":"row","kind":"task","data":{"title":"Synthetic context"}}]}});
    let request = AnalysisRequest {
        show_session: false,
        config: &config,
        kind: "edit",
        payload: &payload,
        root: &root,
        trace: &root,
        prompt: "Synthetic input",
        schema: &schema,
        tools: Some(&tools),
        registered_tools: None,
        cancel: &AtomicBool::new(false),
    };
    let mut usage = Value::Null;
    let error = analyze_client(
        &request,
        &mut |event| {
            if let AnalysisEvent::Usage(value) = event {
                usage = value;
            }
        },
        &client,
        "synthetic",
        false,
        false,
    )
    .unwrap_err();
    assert!(error.starts_with(crate::ai::INPUT_LIMIT_ERROR), "{error}");
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage["partial"], true);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}
impl Transport for SyntheticTransport {
    fn send(
        &self,
        request: &TransportRequest<'_>,
        _: Duration,
        _: &AtomicBool,
    ) -> Result<TransportResponse, TransportError> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(request.body().unwrap()).unwrap());
        let value = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected retry");
        Ok(TransportResponse::new(
            200,
            serde_json::to_vec(&value).unwrap(),
        ))
    }
}
fn final_value(kind: &str) -> Value {
    match kind {
        "classify" => {
            json!({"actions":[],"tag_suggestions":[],"image_notes":[],"memory_update":[]})
        }
        "format" => {
            json!({"tasks":[{"title":"Synthetic task","description":"Synthetic evidence","blocker":"","next_step":"Inspect","due":"","tags":[],"suggested_project_id":"project","suggested_project_reason":"Synthetic directory","tool_call_ids":["synthetic-call"]}],"tag_suggestions":[]})
        }
        "edit" => {
            json!({"reason":"Synthetic edit","changes":[],"tag_changes":[],"tag_suggestions":[],"suggested_project_id":"project","suggested_project_reason":"Synthetic directory"})
        }
        "archive" => {
            json!({"report":{"summary":"Synthetic report","completed":[],"attention":[],"next_steps":[]},"memory_changes":[]})
        }
        _ => unreachable!(),
    }
}

#[test]
fn all_four_purposes_send_real_declarations_first_and_continue_the_same_request() {
    for kind in ["classify", "format", "edit", "archive"] {
        let root = std::env::temp_dir().join(format!("shishi-workflow-tools-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let tool = ToolDefinition {
            id: "synthetic-tool".into(),
            name: "synthetic_lookup".into(),
            description: "Read a synthetic record".into(),
            command_template: "echo {{record}}".into(),
            parameters: vec![Parameter {
                name: "record".into(),
                description: "Synthetic record key".into(),
                ..Default::default()
            }],
            enabled: true,
            ..Default::default()
        };
        app.state.lock().unwrap().config.tools.push(tool.clone());
        let config = Config {
            selected_connection: "api".into(),
            model: format!("synthetic-{kind}"),
            ai_connections: vec![Connection {
                id: "api".into(),
                driver: "api".into(),
                base_url: "https://example.com/v1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut payload = json!({"_registered_tools":[tool],"available_projects":[{"id":"project","name":"Synthetic project","cwd":"D:/synthetic"}]});
        if ["classify", "archive"].contains(&kind) {
            analysis_context::attach_memory_text(&mut payload, "Synthetic archived memory", "");
        }
        let prepared = crate::ai::prepare(&config, kind, &payload);
        let declarations =
            shishi_api::tool_definitions_from_mcp(prepared.tools.as_ref().unwrap()).unwrap();
        let session = ToolSession::new(
            &app,
            "request",
            kind,
            "",
            vec![tool.clone()],
            &config,
            prepared.total,
        );
        let cancel = AtomicBool::new(false);
        let request = AnalysisRequest {
            show_session: false,
            config: &config,
            kind,
            payload: &payload,
            root: &root,
            trace: &root,
            prompt: &prepared.prompt,
            schema: &prepared.schema,
            tools: prepared.tools.as_ref(),
            registered_tools: Some(&session),
            cancel: &cancel,
        };
        let mut meter = Meter::default();
        let mut handler = ApiTools {
            request: &request,
            meter: &mut meter,
        };
        let expected = final_value(kind);
        let transport = SyntheticTransport {
            replies: Mutex::new(VecDeque::from([
                json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"synthetic-call","type":"function","function":{"name":"synthetic_lookup","arguments":"{\"record\":\"DEMO_001\"}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[{"message":{"role":"assistant","content":expected.to_string()},"finish_reason":"stop"}]}),
            ])),
            requests: Mutex::new(vec![]),
        };
        let client = ChatCompletionsClient::with_transport(
            Endpoint::parse("https://example.com/v1").unwrap(),
            &transport,
        );
        let output = client
            .analyze(AnalyzeRequest {
                model: &config.model,
                reasoning_effort: None,
                prompt: &prepared.prompt,
                schema: &prepared.schema,
                credential: "synthetic",
                cancel: &cancel,
                timeout: Duration::from_secs(10),
                tools: &declarations,
                tool_handler: Some(&mut handler),
            })
            .unwrap();
        assert_eq!(output.value, expected);
        session.finish(None, &Value::Null).unwrap();
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], config.model);
        assert!(requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "synthetic_lookup"
                && t["function"]["parameters"]["properties"]["record"]["type"] == "string"));
        let initial = requests[0]["messages"].as_array().unwrap();
        let continued = requests[1]["messages"].as_array().unwrap();
        assert_eq!(&continued[..initial.len()], initial.as_slice());
        assert_eq!(continued.last().unwrap()["tool_call_id"], "synthetic-call");
        assert!(continued.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("DEMO_001"));
        let run = app.tool_run_get(&json!({"run_id":"request"})).unwrap();
        assert_eq!(run["purpose"], kind);
        assert_eq!(run["request_id"], "request");
        assert_eq!(run["calls"][0]["result"]["status"], "completed");
        assert_eq!(
            app.state.lock().unwrap().calls,
            0,
            "only the workflow accounts for its AI quota"
        );
        drop(requests);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}
