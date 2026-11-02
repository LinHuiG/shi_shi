use super::*;
use crate::{
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

fn api_config() -> Config {
    Config {
        selected_connection: "api".into(),
        model: "synthetic".into(),
        ai_connections: vec![Connection {
            id: "api".into(),
            driver: "api".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl SyntheticTransport {
    fn client(&self) -> ChatCompletionsClient<'_> {
        ChatCompletionsClient::with_transport(
            Endpoint::parse("https://example.com/v1").unwrap(),
            self,
        )
    }
}

#[test]
fn independent_retrieval_pages_repeated_queries_after_a_nearly_full_initial_request() {
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
        ..api_config()
    };
    for finish in [true, false] {
        let mut replies = (0..if finish { 2 } else { 3 }).map(|index| {
        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":format!("context-{index}"),"type":"function","function":{"name":"read_context","arguments":"{\"kind\":\"task\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}})
    }).collect::<VecDeque<_>>();
        replies.push_back(json!({"choices":[{"message":{"role":"assistant","content":"{\"answer\":\"complete\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}));
        let transport = SyntheticTransport {
            replies: Mutex::new(replies),
            requests: Mutex::new(vec![]),
        };
        let client = transport.client();
        let payload = json!({"_local_context":{"expires":now()+60_000,"records":(0..16).map(|index|json!({"id":format!("row-{index:02}"),"kind":"task","data":{"title":format!("Synthetic context {index}"),"description":"x".repeat(300)}})).collect::<Vec<_>>()}});
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
        let result = analyze_client(
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
        );
        let (value, _) = result.unwrap();
        assert_eq!(value["answer"], "complete");
        assert_eq!(value["_context_seen"].as_array().unwrap().len(), 16);
        assert!(value["_context_usage"]["chars"].as_u64().unwrap() > 0);
        assert_eq!(usage["output_tokens"], if finish { 16 } else { 23 });
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), if finish { 3 } else { 4 });
        if !finish {
            assert!(requests[3].get("tools").is_none());
            assert!(requests[3]["messages"]
                .to_string()
                .contains("No further tools"));
        }
        assert!(requests[0].to_string().chars().count() <= config.max_input_chars);
        assert!(requests[2].to_string().chars().count() > config.max_input_chars);
        let pages = requests[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(pages.len(), 2);
        assert_ne!(pages[0], pages[1]);
    }
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

#[test]
fn room_alias_tool_continuation_reads_only_the_bound_source_and_chat() {
    let root = std::env::temp_dir().join(format!("ss-api-room-alias-{}", id()));
    let config = Config {
        retain_ai_traces: false,
        ..api_config()
    };
    let changes = [
        ("target-message", "source-a", "TARGET_CONTEXT_ONLY"),
        ("other-message", "source-b", "OTHER_SOURCE_ONLY"),
    ]
    .into_iter()
    .map(|(id, source, text)| Message {
        id: id.into(),
        source_id: source.into(),
        source_name: "Synthetic source".into(),
        chat: "shared-room".into(),
        group: "Shared room".into(),
        text: text.into(),
        time: now(),
        ..Default::default()
    })
    .collect::<Vec<_>>();
    let state = State {
        config: config.clone(),
        ..Default::default()
    };
    let payload = crate::context_budget::classify(&state, &changes).payload;
    assert_eq!(payload["rooms"][0]["id"], "r1");
    assert_eq!(
        payload["_local_context"]["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let prepared = crate::ai::prepare(&config, "classify", &payload);
    let expected = final_value("classify");
    let transport = SyntheticTransport {
        replies: Mutex::new(VecDeque::from([
            json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"room-context","type":"function","function":{"name":"read_context","arguments":"{\"chat\":\"r1\",\"kind\":\"message\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"message":{"role":"assistant","content":expected.to_string()},"finish_reason":"stop"}]}),
        ])),
        requests: Mutex::new(vec![]),
    };
    let client = transport.client();
    let request = AnalysisRequest {
        show_session: false,
        config: &config,
        kind: "classify",
        payload: &payload,
        root: &root,
        trace: &root,
        prompt: &prepared.prompt,
        schema: &prepared.schema,
        tools: prepared.tools.as_ref(),
        registered_tools: None,
        cancel: &AtomicBool::new(false),
    };
    let (value, _) =
        analyze_client(&request, &mut |_| {}, &client, "synthetic", false, false).unwrap();
    assert_eq!(value["actions"], expected["actions"]);
    assert_eq!(value["_context_seen"], json!(["target-message"]));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let initial = requests[0]["messages"].as_array().unwrap();
    let continued = requests[1]["messages"].as_array().unwrap();
    assert_eq!(&continued[..initial.len()], initial.as_slice());
    let response = continued.last().unwrap();
    assert_eq!(response["role"], "tool");
    assert_eq!(response["tool_call_id"], "room-context");
    let content = response["content"].as_str().unwrap();
    let result: Value = serde_json::from_str(content).unwrap();
    assert_eq!(result["returned"], 1);
    assert_eq!(result["records"][0]["id"], "target-message");
    assert_eq!(result["records"][0]["source"], "source-a");
    assert_eq!(result["records"][0]["chat"], "shared-room");
    assert_eq!(result["records"][0]["data"]["text"], "TARGET_CONTEXT_ONLY");
    assert!(!content.contains("OTHER_SOURCE_ONLY"));
    assert!(!content.contains("source-b"));
}

fn final_value(kind: &str) -> Value {
    match kind {
        "classify" => {
            json!({"actions":[],"no_action_reason":"本批没有需要更新的事项","tag_suggestions":[],"image_notes":[],"memory_update":[]})
        }
        "format" => {
            json!({"tasks":[{"title":"Synthetic task","description":"Synthetic evidence","blocker":"","due":"","tags":[],"suggested_project_id":"project","suggested_project_reason":"Synthetic directory","tool_call_ids":["synthetic-call"]}],"tag_suggestions":[]})
        }
        "edit" => {
            json!({"reason":"Synthetic edit","changes":[],"tag_changes":[],"tag_suggestions":[],"suggested_project_id":"project","suggested_project_reason":"Synthetic directory"})
        }
        "archive" => {
            json!({"report":{"summary":"Synthetic report","completed":[],"attention":[],"next_steps":[]},"archived_memory":[]})
        }
        _ => unreachable!(),
    }
}

#[test]
fn empty_analysis_conclusions_correct_once_then_accept_with_an_explicit_degraded_marker() {
    let conclusion = |reason: &str| {
        let mut value = final_value("classify");
        value["no_action_reason"] = json!(reason);
        value
    };
    let semantic_cases = [
        (
            Some(""),
            Some("No action is required by the synthetic evidence."),
            false,
        ),
        (
            Some(" \n\t"),
            Some("The synthetic acknowledgement needs no change."),
            false,
        ),
        (Some(""), Some(""), true),
        (None, Some(" \t"), true),
        (Some(""), None, false),
    ]
    .into_iter()
    .map(|(initial, correction, degraded)| {
        (
            initial.map(&conclusion),
            correction.map(&conclusion),
            degraded,
        )
    });
    let malformed_cases = [
        None,
        Some(Value::Null),
        Some(json!(7)),
        Some(json!("🙂".repeat(crate::ai::NO_ACTION_REASON_LIMIT + 1))),
    ]
    .into_iter()
    .map(|reason| {
        let mut value = final_value("classify");
        if let Some(reason) = reason {
            value["no_action_reason"] = reason;
        } else {
            value.as_object_mut().unwrap().remove("no_action_reason");
        }
        (Some(value.clone()), Some(value), false)
    });
    for (initial, correction, degraded) in semantic_cases.chain(malformed_cases) {
        let config = Config {
            model: "synthetic".into(),
            retain_ai_traces: false,
            ..Default::default()
        };
        let payload = json!({});
        let prepared = crate::ai::prepare(&config, "classify", &payload);
        let reply = |value: &Option<Value>| {
            let content = value
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "invalid JSON".into());
            json!({"choices":[{"message":{"role":"assistant","content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}})
        };
        let transport = SyntheticTransport {
            replies: Mutex::new([reply(&initial), reply(&correction)].into()),
            requests: Mutex::new(vec![]),
        };
        let client = transport.client();
        let request = AnalysisRequest {
            show_session: false,
            config: &config,
            kind: "classify",
            payload: &payload,
            root: std::path::Path::new("."),
            trace: std::path::Path::new("."),
            prompt: &prepared.prompt,
            schema: &prepared.schema,
            tools: prepared.tools.as_ref(),
            registered_tools: None,
            cancel: &AtomicBool::new(false),
        };
        let mut usage = Value::Null;
        let mut invalid = false;
        let result = analyze_client(
            &request,
            &mut |event| match event {
                AnalysisEvent::Usage(value) => usage = value,
                AnalysisEvent::OutputValidationFailed => invalid = true,
                _ => {}
            },
            &client,
            "synthetic",
            false,
            false,
        );
        if let Some(reason) = correction
            .as_ref()
            .and_then(|value| value["no_action_reason"].as_str())
            .filter(|reason| reason.chars().count() <= crate::ai::NO_ACTION_REASON_LIMIT)
        {
            let value = result.unwrap().0;
            assert_eq!(
                value["no_action_reason"],
                if degraded {
                    crate::ai::DEGRADED_ANALYSIS_REASON
                } else {
                    reason
                }
            );
            assert_eq!(value["_analysis_degraded"] == true, degraded);
            assert_eq!(value["actions"], json!([]));
            assert!(!invalid);
        } else {
            assert!(result.is_err());
            assert!(invalid);
        }
        assert_eq!(usage["output_tokens"], 14);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["messages"][0], requests[1]["messages"][0]);
        if initial
            .as_ref()
            .is_some_and(crate::ai::empty_analysis_without_reason)
        {
            assert!(
                requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("must be non-blank when actions is empty")
            );
        }
    }
    assert!(!crate::ai::empty_analysis_without_reason(
        &json!({"actions":[{}],"no_action_reason":""})
    ));
}

#[test]
fn all_four_purposes_send_real_declarations_first_and_continue_the_same_request() {
    for kind in ["classify", "format", "edit", "archive"] {
        let (root, app) = crate::test_support::scoped_app("shishi-workflow-tools");
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
        let session = ToolSession::new(&app, "request", kind, "", vec![tool.clone()], &config);
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
            degraded: false,
        };
        assert!(handler.allows_repeated_arguments("synthetic_lookup"));
        assert!(!handler.allows_repeated_arguments("read_context"));
        assert!(!handler.allows_repeated_arguments("read_image"));
        let expected = final_value(kind);
        let transport = SyntheticTransport {
            replies: Mutex::new(VecDeque::from([
                json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"synthetic-call","type":"function","function":{"name":"synthetic_lookup","arguments":"{\"record\":\"DEMO_001\"}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[{"message":{"role":"assistant","content":expected.to_string()},"finish_reason":"stop"}]}),
            ])),
            requests: Mutex::new(vec![]),
        };
        let client = transport.client();
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
    }
}

#[test]
fn commands_context_and_model_prose_share_one_continuation_allowance() {
    let (root, app) = crate::test_support::scoped_app("ss-api-shared-budget");
    let tool = ToolDefinition {
        id: "shared-echo".into(),
        name: "synthetic_echo".into(),
        description: "Echo a synthetic result".into(),
        command_template: "echo synthetic".into(),
        enabled: true,
        ..Default::default()
    };
    app.state.lock().unwrap().config.tools.push(tool.clone());
    let mut tools = analysis_context::tools();
    tools["tools"].as_array_mut().unwrap().push(json!({
        "name":tool.name,"description":"Synthetic echo","inputSchema":tool.schema()
    }));
    let schema =
        json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    let declarations = shishi_api::tool_definitions_from_mcp(&tools).unwrap();
    let config = Config {
        max_input_chars: shishi_api::openai::initial_input_chars(
            "synthetic",
            None,
            "Synthetic input",
            &schema,
            &declarations,
        ),
        retrieval_budget_chars: 2_000,
        retain_ai_traces: false,
        ..api_config()
    };
    let proposal = |id: &str, name: &str, content: String| {
        json!({
            "role":"assistant","content":content,
        "tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":if name == "read_context" {"{\"kind\":\"task\"}"} else {"{}"}}}]
        })
    };
    let last_proposal = proposal("not-executed", "synthetic_echo", "x".repeat(1_500));
    let transport = SyntheticTransport {
        replies: Mutex::new(VecDeque::from([
            json!({"choices":[{"message":proposal("once", "synthetic_echo", "x".repeat(350)),"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"message":proposal("context", "read_context", String::new()),"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"message":last_proposal,"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"message":{"role":"assistant","content":"{\"answer\":\"complete\"}"},"finish_reason":"stop"}]}),
        ])),
        requests: Mutex::new(vec![]),
    };
    let payload = json!({"_local_context":{"expires":now()+60_000,"records":[{
        "id":"context-record","kind":"task","data":{"description":"x".repeat(400)}
    }]}});
    let session = ToolSession::new(&app, "shared-budget", "edit", "", vec![tool], &config);
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
        registered_tools: Some(&session),
        cancel: &AtomicBool::new(false),
    };
    let client = transport.client();
    let (value, _) =
        analyze_client(&request, &mut |_| {}, &client, "synthetic", false, false).unwrap();
    assert_eq!(value["answer"], "complete");
    assert_eq!(value["_context_seen"], json!(["context-record"]));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let initial = requests[0]["messages"].as_array().unwrap().len();
    let used = requests[2]["messages"].as_array().unwrap()[initial..]
        .iter()
        .map(|message| shishi_api::openai::message_input_chars(message) + 1)
        .sum::<usize>();
    let proposed = shishi_api::openai::message_input_chars(&last_proposal) + 1;
    assert!(used < config.retrieval_budget_chars && proposed < config.retrieval_budget_chars);
    assert!(used + proposed > config.retrieval_budget_chars);
    assert!(requests[3].get("tools").is_none());
    assert!(requests[3]["messages"]
        .to_string()
        .contains("No tools in that reply were executed"));
    let run = app
        .tool_run_get(&json!({"run_id":"shared-budget"}))
        .unwrap();
    let calls = run["calls"].as_array().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["result"]["status"] == "completed")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .find(|call| call["call_id"] == "not-executed")
            .unwrap()["result"]["status"],
        "rejected"
    );
    drop(requests);
    drop(session);
}

#[test]
fn command_result_fits_the_real_continuation_without_replaying_or_losing_local_output() {
    let (root, app) = crate::test_support::scoped_app("ss-api-result-budget");
    let tool = ToolDefinition {
        id: "echo".into(),
        name: "synthetic_echo".into(),
        description: "Synthetic bounded echo".into(),
        command_template: "echo {{value}}".into(),
        enabled: true,
        parameters: vec![Parameter {
            name: "value".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    app.state.lock().unwrap().config.tools.push(tool.clone());
    let schema =
        json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    let tools = json!({"tools":[{"name":tool.name,"description":tool.description,"inputSchema":tool.schema()}]});
    let declarations = shishi_api::tool_definitions_from_mcp(&tools).unwrap();
    // Backslashes, the command's quotes and CRLF all expand in the nested JSON.
    let echoed = "path\\segment ".repeat(200);
    let assistant = json!({"role":"assistant","content":null,"tool_calls":[{"id":"echo-once","type":"function","function":{"name":"synthetic_echo","arguments":json!({"value":echoed}).to_string()}}]});
    let initial = shishi_api::openai::initial_input_chars(
        "synthetic",
        None,
        "Synthetic input",
        &schema,
        &declarations,
    );
    let config = Config {
        max_input_chars: initial,
        retrieval_budget_chars: assistant.to_string().chars().count() + 1_000,
        ..api_config()
    };
    let transport = SyntheticTransport {
        replies: Mutex::new(VecDeque::from([
            json!({"choices":[{"message":assistant,"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"message":{"role":"assistant","content":"{\"answer\":\"complete\"}"},"finish_reason":"stop"}]}),
        ])),
        requests: Mutex::new(vec![]),
    };
    let client = transport.client();
    let session = ToolSession::new(&app, "budget", "edit", "", vec![tool], &config);
    let payload = json!({});
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
        registered_tools: Some(&session),
        cancel: &AtomicBool::new(false),
    };
    analyze_client(&request, &mut |_| {}, &client, "synthetic", false, false).unwrap();
    session.finish(None, &Value::Null).unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let continued = &requests[1];
    let initial = requests[0]["messages"].as_array().unwrap().len();
    // A full allowance closes tools and adds a free host control message.
    // Count retained business messages, independent of declarations/controls.
    let chars = continued["messages"].as_array().unwrap()[initial..]
        .iter()
        .filter(|message| matches!(message["role"].as_str(), Some("assistant" | "tool")))
        .map(|message| shishi_api::openai::message_input_chars(message) + 1)
        .sum::<usize>();
    assert!(
        chars <= config.retrieval_budget_chars,
        "{chars} > {}",
        config.retrieval_budget_chars
    );
    assert!(
        config.retrieval_budget_chars - chars < 8,
        "the useful output fills the actual remaining space"
    );
    let result: Value = serde_json::from_str(
        continued["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["truncated"], true);
    assert!(!result["stdout"].as_str().unwrap().is_empty());
    let state = app.state.lock().unwrap();
    assert_eq!(state.tool_runs.len(), 1);
    assert_eq!(state.tool_runs[0].calls.len(), 1);
    let local = state.tool_runs[0].calls[0].result.as_ref().unwrap();
    assert_eq!(local.status, "completed");
    assert!(local.stdout.contains(&echoed));
    assert!(local.stdout.len() > result["stdout"].as_str().unwrap().len());
    drop(state);
    drop(requests);
    drop(session);
}
