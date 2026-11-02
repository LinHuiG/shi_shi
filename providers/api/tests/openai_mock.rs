use serde_json::{json, Value};
use shishi_api::{
    tool_definitions_from_mcp, AnalysisOutput, AnalyzeRequest, ApiError, ApiErrorKind,
    ChatCompletionsClient, Endpoint, FnToolHandler, HttpMethod, ToolDefinition, ToolFailure,
    ToolFailureKind, ToolHandler, Transport, TransportError, TransportErrorKind, TransportRequest,
    TransportResponse, UsageAggregate, USER_INSTRUCTION,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SYNTHETIC_KEY: &str = "synthetic-analysis-key-9f4a2c";
const SYNTHETIC_PROMPT: &str = "SENTINEL-SYSTEM-PROMPT\n## 运行上下文（最后）\n当前上海时间：2026-10-18 09:00:00 +08:00\n只输出指定JSON。";

#[derive(Clone)]
struct RequestRecord {
    method: HttpMethod,
    url: String,
    bearer: String,
    body: Vec<u8>,
    timeout: Duration,
}

impl RequestRecord {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

/// A forbidden invocation must fail immediately, even if the request later errors.
struct NoToolCalls;
impl ToolHandler for NoToolCalls {
    fn invoke(
        &mut self,
        invocation: &shishi_api::ToolInvocation<'_>,
    ) -> Result<shishi_api::ToolResult, ToolFailure> {
        panic!("unexpected tool invocation: {invocation:?}");
    }
}

enum Reply {
    Response { status: u16, body: Vec<u8> },
    Error(TransportErrorKind),
}

impl Reply {
    fn json(status: u16, value: Value) -> Self {
        Self::Response {
            status,
            body: serde_json::to_vec(&value).expect("reply is JSON"),
        }
    }

    fn raw(status: u16, body: &str) -> Self {
        Self::Response {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    fn error(kind: TransportErrorKind) -> Self {
        Self::Error(kind)
    }
}

#[derive(Default)]
struct MockTransport {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<RequestRecord>>,
}

impl MockTransport {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

impl Transport for MockTransport {
    fn send(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        _cancel: &AtomicBool,
    ) -> Result<TransportResponse, TransportError> {
        self.requests
            .lock()
            .expect("requests mutex")
            .push(RequestRecord {
                method: request.method(),
                url: request.url().to_string(),
                bearer: request.bearer().to_string(),
                body: request.body().unwrap_or_default().to_vec(),
                timeout,
            });
        let reply = self
            .replies
            .lock()
            .expect("replies mutex")
            .pop_front()
            .unwrap_or(Reply::Error(TransportErrorKind::Network));
        match reply {
            Reply::Response { status, body } => Ok(TransportResponse::new(status, body)),
            Reply::Error(kind) => Err(TransportError::new(kind)),
        }
    }
}

fn test_client(mock: &MockTransport) -> ChatCompletionsClient<'_> {
    let endpoint = Endpoint::parse("https://api.example.test/openai/v1").expect("endpoint");
    ChatCompletionsClient::with_transport(endpoint, mock)
}

fn test_request<'a, 'h>(schema: &'a Value, cancel: &'a AtomicBool) -> AnalyzeRequest<'a, 'h> {
    AnalyzeRequest {
        model: "synthetic-model",
        reasoning_effort: None,
        prompt: SYNTHETIC_PROMPT,
        schema,
        credential: SYNTHETIC_KEY,
        cancel,
        timeout: Duration::from_secs(5),
        tools: &[],
        tool_handler: None,
    }
}

#[test]
fn explicit_effort_survives_format_fallback_and_tool_continuation() {
    let mock = MockTransport::new(vec![
        Reply::json(
            400,
            json!({"error":{"message":"Unsupported parameter: 'response_format' is not supported with this model.","param":"response_format","code":"unsupported_parameter"}}),
        ),
        tool_response(vec![call("ctx", "read_context", r#"{"ids":["m1"]}"#)], None),
        response_with_usage(&final_content(), None),
    ]);
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok(String::from("{}")));
    test_client(&mock)
        .analyze(AnalyzeRequest {
            reasoning_effort: Some("high"),
            tools: &[read_context_tool()],
            tool_handler: Some(&mut handler),
            ..test_request(&schema, &cancel)
        })
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|r| r.json()["reasoning_effort"] == "high"));
    let default = MockTransport::new(vec![response_with_usage(&final_content(), None)]);
    analyze(&default, &[], None, Duration::from_secs(2)).unwrap();
    assert!(default.requests()[0]
        .json()
        .get("reasoning_effort")
        .is_none());
    let rejected = MockTransport::new(vec![Reply::json(
        400,
        json!({"error":{"param":"reasoning_effort","message":"unsupported reasoning_effort"}}),
    )]);
    assert!(test_client(&rejected)
        .analyze(AnalyzeRequest {
            reasoning_effort: Some("high"),
            timeout: Duration::from_secs(2),
            ..test_request(&schema, &cancel)
        })
        .is_err());
    assert_eq!(rejected.requests().len(), 1);
}

#[test]
fn model_effort_metadata_distinguishes_unknown_empty_and_declared() {
    let mock = MockTransport::new(vec![Reply::json(
        200,
        json!({"data":[{"id":"unknown"},{"id":"unsupported","supported_reasoning_efforts":[]},{"id":"supported","supportedReasoningEfforts":["low",{"reasoningEffort":"high"}]}]}),
    )]);
    let models = test_client(&mock)
        .discover_models(
            SYNTHETIC_KEY,
            &AtomicBool::new(false),
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(models[0].supported_reasoning_efforts, None);
    assert_eq!(models[1].supported_reasoning_efforts, Some(vec![]));
    assert_eq!(
        models[2].supported_reasoning_efforts,
        Some(vec!["low".into(), "high".into()])
    );
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "ok": {"type": "boolean"},
            "note": {"type": "string", "maxLength": 20}
        },
        "required": ["ok", "note"]
    })
}

fn final_content() -> String {
    json!({"ok": true, "note": "done"}).to_string()
}

fn analyze(
    mock: &MockTransport,
    tools: &[ToolDefinition],
    handler: Option<&mut dyn ToolHandler>,
    timeout: Duration,
) -> Result<AnalysisOutput, ApiError> {
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let client = test_client(mock);
    client.analyze(AnalyzeRequest {
        timeout,
        tools,
        tool_handler: handler,
        ..test_request(&schema, &cancel)
    })
}

fn response_with_usage(content: &str, usage: Option<Value>) -> Reply {
    let mut value = json!({
        "choices": [{
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    Reply::json(200, value)
}

fn tool_response(calls: Vec<Value>, usage: Option<Value>) -> Reply {
    let mut value = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": calls
            },
            "finish_reason": "tool_calls"
        }]
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    Reply::json(200, value)
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    })
}

fn usage(prompt: u64, completion: u64, total: u64, cached: Option<u64>) -> Value {
    let mut value = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total
    });
    if let Some(cached) = cached {
        value["prompt_tokens_details"] = json!({"cached_tokens": cached});
    }
    value
}

fn read_context_tool() -> ToolDefinition {
    ToolDefinition::new(
        "read_context",
        "Read only the frozen synthetic snapshot.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "ids": {"type": "array", "maxItems": 8, "items": {"type": "string"}},
                "query": {"type": "string", "maxLength": 80}
            },
            "required": ["ids"]
        }),
    )
}

fn read_image_tool() -> ToolDefinition {
    ToolDefinition::new(
        "read_image",
        "Read a synthetic image reference from the frozen snapshot.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "image_id": {"type": "string", "maxLength": 512}
            },
            "required": ["image_id"]
        }),
    )
}

#[test]
fn local_image_content_is_multimodal_and_all_tool_calls_are_correlated_first() {
    let mock = MockTransport::new(vec![
        tool_response(
            vec![
                call("img", "read_image", r#"{"image_id":"one"}"#),
                call("ctx", "read_context", r#"{"ids":["m1"]}"#),
            ],
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler = FnToolHandler(|invocation: &shishi_api::ToolInvocation<'_>| {
        shishi_api::ToolResult::from_mcp(&if invocation.name() == "read_image" {
            json!({"content":[{"type":"text","text":"local picture"},
                {"type":"image","mimeType":"image/png","data":"aW1n"}]})
        } else {
            json!({"records":[]})
        })
    });
    analyze(
        &mock,
        &[read_image_tool(), read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(2),
    )
    .unwrap();
    let body = mock.requests()[1].json();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "img");
    assert_eq!(messages[4]["tool_call_id"], "ctx");
    assert!(!messages[3]["content"].as_str().unwrap().contains("aW1n"));
    assert_eq!(messages[5]["role"], "user");
    assert_eq!(
        messages[5]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1n"
    );
}

#[test]
fn image_tool_rejects_remote_urls_and_malformed_base64() {
    for (mime, data) in [
        ("image/svg+xml", "aW1n"),
        ("image/png", "https://example.test/p.png"),
        ("image/png", "a==="),
    ] {
        let mock = MockTransport::new(vec![
            tool_response(
                vec![call("img", "read_image", r#"{"image_id":"one"}"#)],
                None,
            ),
            response_with_usage(&final_content(), None),
        ]);
        let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
            shishi_api::ToolResult::from_mcp(
                &json!({"content":[{"type":"image","mimeType":mime,"data":data}]}),
            )
        });
        let output = analyze(
            &mock,
            &[read_image_tool()],
            Some(&mut handler),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output.value["ok"], true);
        assert!(mock.requests()[1].json().get("tools").is_none());
        assert_eq!(mock.requests().len(), 2);
        assert!(!mock.requests()[1].json().to_string().contains(data));
    }
}

#[test]
fn continuation_budget_counts_text_but_not_native_image_pixels() {
    for (text, image, final_only) in [
        ("short".into(), "aW1n".repeat(40_000), false),
        ("中".repeat(10_001), "aW1n".into(), true),
        (
            "data:image/png;base64,".to_owned() + &"aW1n".repeat(3_000),
            "aW1n".into(),
            true,
        ),
        ("short".into(), "aW1n".repeat(1024 * 1024 + 1), true),
    ] {
        let mock = MockTransport::new(vec![
            tool_response(
                vec![call("img", "read_image", r#"{"image_id":"one"}"#)],
                Some(usage(11, 7, 18, None)),
            ),
            response_with_usage(&final_content(), None),
        ]);
        let mut handler = BudgetedHandler {
            result: shishi_api::ToolResult::from_mcp(&json!({"content":[
                {"type":"text","text":text},
                {"type":"image","mimeType":"image/png","data":image}]}))
            .unwrap(),
            query: None,
            ..Default::default()
        };
        analyze(
            &mock,
            &[read_image_tool()],
            Some(&mut handler),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(handler.calls, ["img"]);
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].json().get("tools").is_none(), final_only);
        if final_only {
            assert!(
                requests[1].body.len() < 10_000,
                "oversized result must not be replayed"
            );
        } else {
            assert!(
                requests[1].body.len() > 100_000,
                "native pixels are not charged as text"
            );
        }
    }
}

#[test]
fn independent_retrieval_never_discounts_the_transport_byte_limit() {
    let mock = MockTransport::new(vec![
        tool_response(
            vec![call("one", "read_image", r#"{"image_id":"one"}"#)],
            None,
        ),
        tool_response(
            vec![call("two", "read_image", r#"{"image_id":"two"}"#)],
            None,
        ),
    ]);
    let mut handler = BudgetedHandler {
        result: shishi_api::ToolResult::from_mcp(
            &json!({"content":[{"type":"image","mimeType":"image/png","data":"aW1n".repeat(1024 * 1024)}]}),
        ).unwrap(),
        continuation: None,
        query: Some("read_image"),
        ..Default::default()
    };
    let error = analyze(
        &mock,
        &[read_image_tool()],
        Some(&mut handler),
        Duration::from_secs(10),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ApiErrorKind::InvalidConfig);
    assert_eq!(handler.calls, ["one", "two"]);
    assert_eq!(
        mock.requests().len(),
        2,
        "the oversized third request never reaches transport"
    );
}

#[test]
fn query_calls_and_provider_prose_share_the_continuation_budget() {
    for mixed in [false, true] {
        for field in ["reasoning_content", "content"] {
            let mut calls = vec![call("read", "read_context", r#"{"ids":["m1"]}"#)];
            if mixed {
                calls.push(call("action", "synthetic_action", r#"{"ids":[]}"#));
            }
            let mut message = json!({"role":"assistant","content":null,"tool_calls":calls});
            message[field] = json!("x".repeat(100_000));
            let mock = MockTransport::new(vec![
                Reply::json(
                    200,
                    json!({"choices":[{"message":message,"finish_reason":"tool_calls"}]}),
                ),
                response_with_usage(&final_content(), None),
            ]);
            let mut handler = BudgetedHandler::default();
            analyze(
                &mock,
                &[read_context_tool(), action_tool()],
                Some(&mut handler),
                Duration::from_secs(5),
            )
            .unwrap();
            assert!(
                handler.calls.is_empty(),
                "over-budget assistant text cannot start commands"
            );
            let requests = mock.requests();
            assert_eq!(requests.len(), 2);
            assert!(requests[1].json().get("tools").is_none());
            assert!(requests[1].body.len() < 10_000);
        }
    }
}

#[test]
fn tool_loop_supports_multiple_readonly_calls_and_continuation() {
    let tool_usage = usage(10, 5, 15, Some(3));
    let final_usage = usage(4, 2, 6, Some(0));
    let mock = MockTransport::new(vec![
        tool_response(
            vec![
                call("call_ctx", "read_context", r#"{"ids":["m1"]}"#),
                call("call_img", "read_image", r#"{"image_id":"img-1"}"#),
            ],
            Some(tool_usage),
        ),
        response_with_usage(&final_content(), Some(final_usage)),
    ]);
    let tools = [read_context_tool(), read_image_tool()];
    let mut seen = Vec::new();
    let mut handler = FnToolHandler(|invocation: &shishi_api::ToolInvocation<'_>| {
        seen.push((
            invocation.name().to_string(),
            invocation.arguments().clone(),
        ));
        let result = match invocation.name() {
            "read_context" => r#"{"records":[{"id":"m1","text":"synthetic"}]}"#.to_string(),
            "read_image" => r#"{"image_id":"img-1","summary":"synthetic image"}"#.to_string(),
            _ => return Err(ToolFailure::new(ToolFailureKind::Denied)),
        };
        Ok(result)
    });

    let output = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
        .expect("tool loop succeeds");

    assert_eq!(output.value, json!({"ok":true,"note":"done"}));
    assert_eq!(output.tool_calls, 2);
    assert_eq!(output.requests, 2);
    assert!(!output.response_format_fallback);
    assert!(!output.correction_used);
    assert_eq!(
        output.usage,
        UsageAggregate {
            prompt_tokens: Some(14),
            completion_tokens: Some(7),
            total_tokens: Some(21),
            cached_tokens: Some(3),
            responses: 2,
            reported_responses: 2,
        }
    );
    assert_eq!(
        seen,
        vec![
            ("read_context".to_string(), json!({"ids":["m1"]})),
            ("read_image".to_string(), json!({"image_id":"img-1"})),
        ]
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(
            request.url,
            "https://api.example.test/openai/v1/chat/completions"
        );
        assert_eq!(request.bearer, SYNTHETIC_KEY);
        let body = request.json();
        assert_eq!(body["model"], "synthetic-model");
        assert_eq!(body["stream"].as_bool(), Some(false));
        assert_eq!(body["messages"][0]["content"], SYNTHETIC_PROMPT);
        assert!(body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .starts_with(USER_INSTRUCTION));
        assert!(body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Required JSON Schema"));
        assert_eq!(body["response_format"]["type"], "json_schema");
        let names = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read_context", "read_image"]);
        assert!(!names.contains(&"shell"));
        assert_eq!(body["tool_choice"].as_str(), Some("auto"));
    }

    let second = requests[1].json();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_ctx");
    assert_eq!(messages[2]["tool_calls"][1]["id"], "call_img");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_ctx");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_img");
    assert!(messages[3]["content"].as_str().unwrap().contains("m1"));
    assert!(messages[4]["content"].as_str().unwrap().contains("img-1"));
    assert!(requests[1].timeout <= requests[0].timeout);
}

#[test]
fn rejected_tool_batches_keep_correlated_results_without_execution() {
    for (case, stream) in [
        ("unknown", false),
        ("duplicate", false),
        ("oversized", false),
        ("oversized", true),
    ] {
        let calls = match case {
            "unknown" => vec![call("call_shell", "shell", r#"{"command":"whoami"}"#)],
            "duplicate" => vec![
                call("call_1", "read_context", r#"{"ids":["m1"]}"#),
                call("call_2", "read_context", r#"{"ids":["m1"]}"#),
            ],
            _ => (0..17)
                .map(|i| {
                    call(
                        &format!("call-{i}"),
                        "read_context",
                        &format!("{{\"ids\":[\"m{i}\"]}}"),
                    )
                })
                .collect(),
        };
        let expected = calls
            .iter()
            .map(|call| call["id"].clone())
            .collect::<Vec<_>>();
        let mock = MockTransport::new(vec![
            tool_response(calls, None),
            response_with_usage(&final_content(), None),
        ]);
        let schema = output_schema();
        let cancel = AtomicBool::new(false);
        let output = test_client(&mock)
            .analyze_with_events(
                AnalyzeRequest {
                    tools: &[read_context_tool()],
                    tool_handler: Some(&mut NoToolCalls),
                    ..test_request(&schema, &cancel)
                },
                stream,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(output.value["ok"], true, "{case}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "{case}");
        let body = requests[1].json();
        if case != "unknown" {
            assert!(body.get("tools").is_none(), "{case}");
        }
        let results = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(body["messages"][3]["tool_call_id"], expected[0], "{case}");
        assert_eq!(
            results
                .iter()
                .map(|row| row["tool_call_id"].clone())
                .collect::<Vec<_>>(),
            expected,
            "{case}"
        );
        assert!(
            results
                .iter()
                .all(|row| row["content"].as_str().unwrap().contains("No tools")),
            "{case}"
        );
    }
}

#[test]
fn invalid_arguments_get_one_adjustment_then_final_only() {
    for arguments in ["{", r#"{"ids":["m1"],"shell":"whoami"}"#] {
        let mock = MockTransport::new(vec![
            tool_response(vec![call("call_1", "read_context", arguments)], None),
            tool_response(vec![call("call_2", "read_context", arguments)], None),
            response_with_usage(&final_content(), None),
        ]);
        analyze(
            &mock,
            &[read_context_tool()],
            Some(&mut NoToolCalls),
            Duration::from_secs(5),
        )
        .unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].json()["tools"].is_array());
        assert!(requests[2].json().get("tools").is_none());
        assert!(requests[2].json()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["tool_call_id"] == "call_2"));
    }
}

#[test]
fn duplicate_query_keeps_previous_result_and_finishes_without_reexecution() {
    let mock = MockTransport::new(vec![
        tool_response(
            vec![call("call_1", "read_context", r#"{"ids":["m1"]}"#)],
            None,
        ),
        tool_response(
            vec![call("call_2", "read_context", r#"{"ids":["m1"]}"#)],
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut invoked = 0;
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
        invoked += 1;
        Ok(r#"{"records":["existing evidence"]}"#.to_string())
    });
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(invoked, 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let body = requests[2].json();
    assert!(body.get("tools").is_none());
    assert!(body.to_string().contains("existing evidence"));
    assert!(body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool_call_id"] == "call_2"));
}

#[derive(Default)]
struct ActionHandler {
    calls: Vec<String>,
}
impl ToolHandler for ActionHandler {
    fn allows_repeated_arguments(&self, name: &str) -> bool {
        name == "synthetic_action"
    }
    fn invoke(
        &mut self,
        call: &shishi_api::ToolInvocation<'_>,
    ) -> Result<shishi_api::ToolResult, ToolFailure> {
        self.calls.push(call.call_id().to_string());
        Ok(shishi_api::ToolResult::Text("{}".into()))
    }
}
fn action_tool() -> ToolDefinition {
    let mut tool = read_context_tool();
    tool.name = "synthetic_action".into();
    tool
}

#[test]
fn action_repetition_does_not_disable_query_progress_checks() {
    for same_response in [false, true] {
        for (query, arguments, definition) in [
            ("read_context", r#"{"ids":["m1"]}"#, read_context_tool()),
            ("read_image", r#"{"image_id":"one"}"#, read_image_tool()),
        ] {
            let mut replies = vec![];
            let mut repeated = vec![call("action", "synthetic_action", r#"{"ids":[]}"#)];
            if same_response {
                repeated.push(call("query-1", query, arguments));
            } else {
                replies.push(tool_response(vec![call("query-1", query, arguments)], None));
            }
            repeated.push(call("query-2", query, arguments));
            replies.push(tool_response(repeated, None));
            replies.push(response_with_usage(&final_content(), None));
            let mock = MockTransport::new(replies);
            let mut handler = ActionHandler::default();
            let output = analyze(
                &mock,
                &[definition, action_tool()],
                Some(&mut handler),
                Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(output.value["ok"], true);
            let expected = if same_response {
                vec![]
            } else {
                vec!["query-1"]
            };
            assert_eq!(
                handler.calls, expected,
                "no action in a rejected batch runs"
            );
            assert_eq!(mock.requests().len(), if same_response { 2 } else { 3 });
        }
    }
}

#[test]
fn actions_allow_repeated_arguments_but_never_repeated_call_ids() {
    for (same_response, repeated_id) in [(true, false), (false, false), (true, true), (false, true)]
    {
        let first = call("action-1", "synthetic_action", r#"{"ids":["m1"]}"#);
        let second = call(
            if repeated_id { "action-1" } else { "action-2" },
            "synthetic_action",
            r#"{"ids":["m1"]}"#,
        );
        let mut replies = if same_response {
            vec![tool_response(vec![first, second], None)]
        } else {
            vec![
                tool_response(vec![first], None),
                tool_response(vec![second], None),
            ]
        };
        replies.push(response_with_usage(&final_content(), None));
        let mock = MockTransport::new(replies);
        let mut handler = ActionHandler::default();
        let result = analyze(
            &mock,
            &[action_tool()],
            Some(&mut handler),
            Duration::from_secs(5),
        );
        if repeated_id {
            assert_eq!(
                result.unwrap_err().kind(),
                if same_response {
                    ApiErrorKind::InvalidToolCall
                } else {
                    ApiErrorKind::NoProgress
                }
            );
            assert_eq!(handler.calls.len(), usize::from(!same_response));
        } else {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(handler.calls, ["action-1", "action-2"]);
        }
    }
}

#[test]
fn cancellation_and_whole_loop_deadline_are_enforced() {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_by_handler = Arc::clone(&cancel);
    let mock = MockTransport::new(vec![tool_response(
        vec![call("call_1", "read_context", r#"{"ids":["m1"]}"#)],
        None,
    )]);
    let tools = [read_context_tool()];
    let schema = output_schema();
    let client = test_client(&mock);
    let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
        cancel_by_handler.store(true, Ordering::Relaxed);
        Ok(r#"{"records":[]}"#.to_string())
    });
    let error = client
        .analyze(AnalyzeRequest {
            tools: &tools,
            tool_handler: Some(&mut handler),
            ..test_request(&schema, cancel.as_ref())
        })
        .expect_err("cancellation must stop the loop");
    assert_eq!(error.kind(), ApiErrorKind::Cancelled);
    assert_eq!(mock.requests().len(), 1);

    let mock = MockTransport::new(vec![tool_response(
        vec![call("call_1", "read_context", r#"{"ids":["m1"]}"#)],
        None,
    )]);
    let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
        std::thread::sleep(Duration::from_millis(150));
        Ok(r#"{"records":[]}"#.to_string())
    });
    let started = Instant::now();
    let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_millis(60))
        .expect_err("deadline must stop the loop");
    assert_eq!(error.kind(), ApiErrorKind::Timeout);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn final_output_validation_has_one_bounded_correction() {
    for (invalid, second_invalid, kind, guidance) in [
        (
            r#"{"ok":"wrong","note":"done"}"#,
            r#"{"ok":"still-wrong","note":"private-synthetic-text"}"#,
            ApiErrorKind::SchemaInvalid,
            &["schema", "$.ok"][..],
        ),
        (
            "not json",
            "still not json",
            ApiErrorKind::InvalidJson,
            &["valid JSON"][..],
        ),
    ] {
        for corrected in [true, false] {
            let final_reply = if corrected {
                final_content()
            } else {
                second_invalid.into()
            };
            let mock = MockTransport::new(vec![
                response_with_usage(
                    invalid,
                    Some(json!({"prompt_tokens":10,"completion_tokens":2})),
                ),
                response_with_usage(
                    &final_reply,
                    Some(json!({"prompt_tokens":12,"completion_tokens":3})),
                ),
            ]);
            let result = analyze(&mock, &[], None, Duration::from_secs(5));
            let usage = if corrected {
                let output = result.expect("correction succeeds");
                assert_eq!(output.value, json!({"ok":true,"note":"done"}));
                assert!(output.correction_used);
                assert_eq!(output.requests, 2);
                output.usage
            } else {
                let error = result.expect_err("only one correction");
                assert_eq!(error.kind(), kind);
                if kind == ApiErrorKind::SchemaInvalid {
                    assert!(error.to_string().contains("$."));
                }
                for private in ["still-wrong", "private-synthetic-text", "still not json"] {
                    assert!(!format!("{error} {error:?}").contains(private));
                }
                error.usage().unwrap().clone()
            };
            assert_eq!(usage.prompt_tokens, Some(22));
            assert_eq!(usage.completion_tokens, Some(5));
            let requests = mock.requests();
            assert_eq!(requests.len(), 2);
            let second = requests[1].json();
            let messages = second["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[2]["role"], "assistant");
            for expected in guidance {
                assert!(messages[3]["content"].as_str().unwrap().contains(expected));
            }
        }
    }
}

#[test]
fn missing_refusal_truncated_and_malformed_outputs_are_typed() {
    let cases = [
        (
            json!({"choices":[{"message":{"role":"assistant"},"finish_reason":"stop"}]}),
            ApiErrorKind::EmptyResponse,
        ),
        (
            json!({"choices":[{"message":{"role":"assistant","content":null,"refusal":"synthetic refusal"},"finish_reason":"stop"}]}),
            ApiErrorKind::Refusal,
        ),
        (
            json!({"choices":[{"message":{"role":"assistant","content":"{}"},"finish_reason":"content_filter"}]}),
            ApiErrorKind::Refusal,
        ),
        (
            json!({"choices":[{"message":{"role":"assistant","content":"{\"ok\":tr"},"finish_reason":"length"}]}),
            ApiErrorKind::Truncated,
        ),
        (json!({"choices":[]}), ApiErrorKind::EmptyResponse),
    ];
    for (body, expected) in cases {
        let mock = MockTransport::new(vec![Reply::json(200, body)]);
        let error =
            analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("output must fail");
        assert_eq!(error.kind(), expected);
        assert_eq!(mock.requests().len(), 1);
    }
    let mock = MockTransport::new(vec![Reply::raw(200, "not a response envelope")]);
    let error = analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("malformed");
    assert_eq!(error.kind(), ApiErrorKind::MalformedResponse);
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn response_format_fallback_requires_an_explicit_unsupported_parameter() {
    let unsupported = json!({
        "error": {
            "message": "Unsupported parameter: 'response_format' is not supported with this model.",
            "param": "response_format",
            "code": "unsupported_parameter"
        }
    });
    for status in [400u16, 422] {
        let mock = MockTransport::new(vec![
            Reply::json(status, unsupported.clone()),
            response_with_usage(&final_content(), None),
        ]);
        let output = analyze(&mock, &[], None, Duration::from_secs(5))
            .expect("explicit unsupported format falls back");
        assert!(output.response_format_fallback);
        assert_eq!(output.requests, 2);
        let requests = mock.requests();
        assert!(requests[0].json().get("response_format").is_some());
        let second = requests[1].json();
        assert!(second.get("response_format").is_none());
        assert!(second["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Required JSON Schema"));
    }

    for (status, body) in [
        (
            400u16,
            json!({"error":{"message":"Bad request","code":"invalid_request"}}),
        ),
        (422u16, json!({"error":{"message":"Unprocessable entity"}})),
        (
            400u16,
            json!({"error":{"message":"response_format is invalid","param":"response_format","code":"invalid_parameter"}}),
        ),
    ] {
        let mock = MockTransport::new(vec![
            Reply::json(status, body),
            response_with_usage(&final_content(), None),
        ]);
        let error = analyze(&mock, &[], None, Duration::from_secs(5))
            .expect_err("generic 400/422 must not fall back");
        assert_eq!(error.kind(), ApiErrorKind::HttpStatus);
        assert_eq!(error.status(), Some(status));
        assert_eq!(mock.requests().len(), 1);
    }
}

#[test]
fn unavailable_response_format_type_falls_back_without_dropping_effort_or_validation() {
    let mock = MockTransport::new(vec![
        Reply::json(
            400,
            json!({"error":{"code":"invalid_parameter_error","message":"This response_format type is unavailable now"}}),
        ),
        response_with_usage(&final_content(), None),
    ]);
    let output = analyze(&mock, &[], None, Duration::from_secs(5)).unwrap();
    assert!(output.response_format_fallback);
    assert_eq!(output.requests, 2);
    let requests = mock.requests();
    let second = requests[1].json();
    assert!(second.get("response_format").is_none());
    assert!(second["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("Required JSON Schema"));
    assert_eq!(output.value, json!({"ok":true,"note":"done"}));
}

#[test]
fn http_and_transport_failures_are_never_retried() {
    for status in [401u16, 403, 404, 429, 500, 503] {
        let mock = MockTransport::new(vec![
            Reply::raw(status, r#"{"error":{"message":"synthetic failure"}}"#),
            response_with_usage(&final_content(), None),
        ]);
        let error = analyze(&mock, &[], None, Duration::from_secs(5))
            .expect_err("HTTP failure must not retry");
        assert_eq!(error.kind(), ApiErrorKind::HttpStatus);
        assert_eq!(error.status(), Some(status));
        assert_eq!(mock.requests().len(), 1);
    }
    for (kind, expected) in [
        (TransportErrorKind::Timeout, ApiErrorKind::Timeout),
        (TransportErrorKind::Cancelled, ApiErrorKind::Cancelled),
        (TransportErrorKind::Network, ApiErrorKind::Transport),
        (TransportErrorKind::Tls, ApiErrorKind::Transport),
        (TransportErrorKind::Protocol, ApiErrorKind::Transport),
        (
            TransportErrorKind::ResponseTooLarge,
            ApiErrorKind::ResponseTooLarge,
        ),
    ] {
        let mock = MockTransport::new(vec![Reply::error(kind)]);
        let error = analyze(&mock, &[], None, Duration::from_secs(5))
            .expect_err("transport failure must not retry");
        assert_eq!(error.kind(), expected);
        assert_eq!(mock.requests().len(), 1);
    }
}

#[test]
fn usage_is_optional_and_never_invented_as_zero() {
    let mock = MockTransport::new(vec![
        tool_response(
            vec![call("call_1", "read_context", r#"{"ids":["m1"]}"#)],
            Some(usage(10, 4, 14, Some(2))),
        ),
        response_with_usage(&final_content(), None),
    ]);
    let tools = [read_context_tool()];
    let mut handler =
        FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok(r#"{"records":[]}"#.to_string()));
    let output = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5)).unwrap();
    assert_eq!(output.usage.responses, 2);
    assert_eq!(output.usage.reported_responses, 1);
    assert_eq!(output.usage.prompt_tokens, None);
    assert_eq!(output.usage.completion_tokens, None);
    assert_eq!(output.usage.total_tokens, None);
    assert_eq!(output.usage.cached_tokens, None);
}

#[test]
fn invalid_tool_call_shape_is_rejected_before_any_callback() {
    let calls = [
        json!([
            call("same-id", "read_context", r#"{"ids":["m1"]}"#),
            call("same-id", "read_context", r#"{"ids":["m2"]}"#)
        ]),
        json!([{
            "id": "call_bad_type",
            "type": "text",
            "function": {"name": "read_context", "arguments": "{\"ids\":[]}"}
        }]),
        json!([{
            "id": "",
            "type": "function",
            "function": {"name": "read_context", "arguments": "{\"ids\":[]}"}
        }]),
        json!([{
            "id": "call_missing_function",
            "type": "function"
        }]),
    ];
    for calls in calls {
        let calls = calls.as_array().cloned().unwrap_or_default();
        let mock = MockTransport::new(vec![tool_response(calls, None)]);
        let tools = [read_context_tool()];
        let error = analyze(
            &mock,
            &tools,
            Some(&mut NoToolCalls),
            Duration::from_secs(5),
        )
        .expect_err("invalid tool call shape");
        assert_eq!(error.kind(), ApiErrorKind::InvalidToolCall);
        assert_eq!(mock.requests().len(), 1);
    }
}

#[test]
fn tool_failure_is_typed_without_echoing_handler_details() {
    let mock = MockTransport::new(vec![tool_response(
        vec![call("call_1", "read_context", r#"{"ids":["m1"]}"#)],
        None,
    )]);
    let tools = [read_context_tool()];
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
        Err::<String, _>(ToolFailure::new(ToolFailureKind::Failed))
    });
    let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
        .expect_err("tool failure");
    assert_eq!(error.kind(), ApiErrorKind::ToolFailed);
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn prompt_is_preserved_exactly_without_a_timestamp_prefix() {
    let mock = MockTransport::new(vec![response_with_usage(&final_content(), None)]);
    analyze(&mock, &[], None, Duration::from_secs(5)).expect("valid output");
    let body = mock.requests()[0].json();
    assert_eq!(body["messages"][0]["content"], SYNTHETIC_PROMPT);
    assert!(body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .starts_with(USER_INSTRUCTION));
    assert!(body["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("Required JSON Schema"));
    assert!(!body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("2026-"));
    assert!(body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("SENTINEL-SYSTEM-PROMPT"));
}

#[test]
fn synthetic_key_echo_does_not_appear_in_safe_errors() {
    let mock = MockTransport::new(vec![Reply::json(
        500,
        json!({"error":{"message":format!("upstream failure while using {SYNTHETIC_KEY}")}}),
    )]);
    let error = analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("HTTP 500");
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains(SYNTHETIC_KEY));
    assert!(!rendered.contains("upstream failure"));
    assert!(!rendered.contains("Authorization"));
    assert!(rendered.contains("500"));

    let mock = MockTransport::new(vec![
        response_with_usage(
            &json!({"ok": true, "note": SYNTHETIC_KEY}).to_string(),
            None,
        ),
        response_with_usage(
            &json!({"ok": true, "note": SYNTHETIC_KEY}).to_string(),
            None,
        ),
    ]);
    let error = analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("schema failure");
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains(SYNTHETIC_KEY));
    assert!(!rendered.contains("synthetic-analysis-key"));
}

#[test]
fn input_validation_prevents_network_calls() {
    let mock = MockTransport::new(vec![response_with_usage(&final_content(), None)]);
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let client = test_client(&mock);
    let tools = [read_context_tool()];
    let bad_schema = json!({"type":"string","pattern":"^x"});
    for (label, request, expected) in [
        (
            "empty credential",
            AnalyzeRequest {
                credential: "",
                ..test_request(&schema, &cancel)
            },
            ApiErrorKind::MissingCredential,
        ),
        (
            "tools require a handler",
            AnalyzeRequest {
                tools: &tools,
                ..test_request(&schema, &cancel)
            },
            ApiErrorKind::InvalidConfig,
        ),
        (
            "unsupported schema",
            test_request(&bad_schema, &cancel),
            ApiErrorKind::InvalidConfig,
        ),
    ] {
        assert_eq!(
            client.analyze(request).expect_err(label).kind(),
            expected,
            "{label}"
        );
        assert!(mock.requests().is_empty(), "{label}");
    }
}

#[test]
fn models_discovery_is_get_only_and_rejects_empty_or_malformed_lists() {
    let mock = MockTransport::new(vec![Reply::json(
        200,
        json!({"data":[{"id":"model-a","name":"Model A"},{"id":"model-b"}]}),
    )]);
    let cancel = AtomicBool::new(false);
    let models = test_client(&mock)
        .discover_models(SYNTHETIC_KEY, &cancel, Duration::from_secs(5))
        .expect("models");
    assert_eq!(
        models,
        vec![
            shishi_api::ModelInfo {
                id: "model-a".into(),
                display_name: Some("Model A".into()),
                supported_reasoning_efforts: None,
            },
            shishi_api::ModelInfo {
                id: "model-b".into(),
                display_name: None,
                supported_reasoning_efforts: None,
            },
        ]
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, HttpMethod::Get);
    assert_eq!(requests[0].url, "https://api.example.test/openai/v1/models");
    assert_eq!(requests[0].bearer, SYNTHETIC_KEY);
    assert!(requests[0].body.is_empty());

    for (reply, expected) in [
        (
            Reply::json(200, json!({"data":[]})),
            ApiErrorKind::EmptyModels,
        ),
        (
            Reply::json(200, json!({"data":[{"name":"missing id"}]})),
            ApiErrorKind::MalformedResponse,
        ),
        (Reply::raw(200, "not json"), ApiErrorKind::MalformedResponse),
        (
            Reply::raw(401, r#"{"error":{"message":"unauthorized"}}"#),
            ApiErrorKind::HttpStatus,
        ),
    ] {
        let mock = MockTransport::new(vec![reply]);
        let cancel = AtomicBool::new(false);
        let error = test_client(&mock)
            .discover_models(SYNTHETIC_KEY, &cancel, Duration::from_secs(5))
            .expect_err("models failure");
        assert_eq!(error.kind(), expected);
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, HttpMethod::Get);
    }
}

#[test]
fn mcp_tool_definitions_map_to_advertised_function_tools() {
    let mapped = tool_definitions_from_mcp(&json!({
        "tools": [
            {
                "name": "read_context",
                "description": "read",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"ids": {"type": "array", "items": {"type": "string"}}},
                    "required": ["ids"]
                }
            },
            {
                "name": "read_image",
                "description": "image",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"image_id": {"type": "string"}},
                    "required": ["image_id"]
                }
            }
        ]
    }))
    .expect("mcp tools");
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].name, "read_context");
    assert_eq!(mapped[1].name, "read_image");

    let mock = MockTransport::new(vec![
        tool_response(
            vec![call("call_ctx", "read_context", r#"{"ids":["m1"]}"#)],
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler =
        FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok(r#"{"records":[]}"#.to_string()));
    let output = analyze(&mock, &mapped, Some(&mut handler), Duration::from_secs(5))
        .expect("mapped tool loop succeeds");
    assert_eq!(output.tool_calls, 1);
    let body = mock.requests()[0].json();
    assert_eq!(
        body["tools"][0]["function"]["name"].as_str(),
        Some("read_context")
    );
}

#[test]
fn readonly_tool_arguments_get_one_correction_without_executing_invalid_batch() {
    let mock = MockTransport::new(vec![
        tool_response(
            vec![call(
                "bad",
                "read_context",
                r#"{"ids":["m1"],"extra":true}"#,
            )],
            None,
        ),
        tool_response(
            vec![call("fixed", "read_context", r#"{"ids":["m1"]}"#)],
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut invoked = 0;
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
        invoked += 1;
        Ok("{}".to_owned())
    });
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(invoked, 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].json()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap_or("").contains("No tools")));
    assert!(requests.windows(2).all(|r| r[1].timeout <= r[0].timeout));
}

#[test]
fn streaming_response_and_reasoning_replay_share_the_actual_analysis_path() {
    fn sse(v: Value) -> Reply {
        Reply::raw(200, &format!("data: {v}\n\ndata: [DONE]\n\n"))
    }
    let mock = MockTransport::new(vec![
        sse(
            json!({"choices":[{"delta":{"reasoning_content":"公开摘要","tool_calls":[{"index":0,"id":"ctx","type":"function","function":{"name":"read_context","arguments":"{\"ids\":[]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
        ),
        sse(
            json!({"choices":[{"delta":{"content":final_content()},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":5,"total_tokens":9}}),
        ),
    ]);
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let tools = [read_context_tool()];
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok("{}".to_string()));
    let mut public = String::new();
    let mut raw = 0;
    let result = test_client(&mock)
        .analyze_with_events(
            AnalyzeRequest {
                reasoning_effort: Some("low"),
                tools: &tools,
                tool_handler: Some(&mut handler),
                ..test_request(&schema, &cancel)
            },
            true,
            &mut |e| match e {
                shishi_api::ApiEvent::Text {
                    kind: "reasoning",
                    text,
                    ..
                } => public.push_str(&text),
                shishi_api::ApiEvent::Response(_) => raw += 1,
                _ => {}
            },
        )
        .unwrap();
    assert_eq!(public, "公开摘要");
    assert_eq!(raw, 2);
    assert_eq!(result.usage.total_tokens, Some(14));
    let requests = mock.requests();
    assert_eq!(requests[0].json()["stream"], true);
    assert_eq!(
        requests[1].json()["messages"][2]["reasoning_content"],
        "公开摘要"
    );
}

#[test]
fn streaming_usage_updates_do_not_double_count_tool_rounds_or_final_only_usage() {
    fn sse(frames: &[Value]) -> Reply {
        let mut wire = frames
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>();
        wire.push_str("data: [DONE]\n\n");
        Reply::raw(200, &wire)
    }
    for first_output in [Some(3), None] {
        let mock = MockTransport::new(vec![
            sse(&[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"ctx","type":"function","function":{"name":"read_context","arguments":"{\"ids\":[]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":first_output}}),
            ]),
            sse(&[
                json!({"choices":[{"delta":{"content":final_content()}}],"usage":{"prompt_tokens":4,"completion_tokens":2}}),
                json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":4}}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":5}}),
            ]),
        ]);
        let schema = output_schema();
        let cancel = AtomicBool::new(false);
        let tools = [read_context_tool()];
        let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok("{}".to_string()));
        let mut counts = vec![];
        let mut finished = vec![];
        let output = test_client(&mock)
            .analyze_with_events(
                AnalyzeRequest {
                    tools: &tools,
                    tool_handler: Some(&mut handler),
                    ..test_request(&schema, &cancel)
                },
                true,
                &mut |e| match e {
                    shishi_api::ApiEvent::OutputTokens(count) => counts.push(count),
                    shishi_api::ApiEvent::StepFinished { output_tokens, .. } => {
                        finished.push(output_tokens)
                    }
                    _ => {}
                },
            )
            .unwrap();
        assert_eq!(
            counts,
            if first_output.is_some() {
                vec![Some(5), Some(7)]
            } else {
                vec![None, None]
            }
        );
        assert_eq!(finished, vec![first_output, Some(5)]);
        assert_eq!(output.usage.completion_tokens, first_output.map(|v| v + 5));
        assert_eq!(output.usage.prompt_tokens, Some(6));
        assert_eq!(output.usage.responses, 2);
    }
}

#[test]
fn stream_refusal_and_partial_tool_arguments_never_invoke_tools() {
    for wire in ["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"refusal\"}]}\n\ndata: [DONE]\n\n", "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"x\",\"function\":{\"name\":\"read_context\",\"arguments\":\"{\"}}]}}]}\n\n"] {
        let mock=MockTransport::new(vec![Reply::raw(200,wire)]);let schema=output_schema();let cancel=AtomicBool::new(false);let tools=[read_context_tool()];
        let error=test_client(&mock).analyze_with_events(AnalyzeRequest{model:"synthetic-model",reasoning_effort:None,prompt:SYNTHETIC_PROMPT,schema:&schema,credential:SYNTHETIC_KEY,cancel:&cancel,timeout:Duration::from_secs(5),tools:&tools,tool_handler:Some(&mut NoToolCalls)},true,&mut |_|{}).unwrap_err();
        assert!(matches!(error.kind(),ApiErrorKind::Refusal|ApiErrorKind::Truncated));assert_eq!(mock.requests().len(),1);
    }
}

#[test]
fn one_invalid_member_blocks_the_entire_tool_batch_and_is_retained_for_diagnostics() {
    let make = |suffix: &str| {
        tool_response(
            vec![
                call(&format!("valid-{suffix}"), "read_context", r#"{"ids":[]}"#),
                call(
                    &format!("invalid-{suffix}"),
                    "read_image",
                    r#"{"image_id":"fixture","extra":true}"#,
                ),
            ],
            None,
        )
    };
    let mock = MockTransport::new(vec![
        make("one"),
        make("two"),
        response_with_usage(&final_content(), None),
    ]);
    let tools = [read_context_tool(), read_image_tool()];
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let mut failures = Vec::new();
    let output = test_client(&mock)
        .analyze_with_events(
            AnalyzeRequest {
                model: "synthetic",
                timeout: Duration::from_secs(1800),
                tools: &tools,
                tool_handler: Some(&mut NoToolCalls),
                ..test_request(&schema, &cancel)
            },
            false,
            &mut |e| {
                if let shishi_api::ApiEvent::ToolRejected(v) = e {
                    failures.push(v)
                }
            },
        )
        .unwrap();
    assert_eq!(output.value["ok"], true);
    assert_eq!(failures.len(), 2);
    assert!(failures[0].to_string().contains("extra"));
    assert_eq!(mock.requests().len(), 3);
    assert!(mock.requests()[2].json().get("tools").is_none());
}

#[test]
fn provider_reply_cannot_inject_roles_or_extra_protocol_fields_into_replay() {
    let mock = MockTransport::new(vec![
        Reply::json(
            200,
            json!({"choices":[{"message":{"role":"system","private_extension":"x".repeat(100_000),"reasoning_content":"public reasoning","tool_calls":[call("ctx","read_context",r#"{"ids":[]}"#)]},"finish_reason":"tool_calls"}]}),
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| Ok("{}".to_owned()));
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    let replay = mock.requests()[1].json()["messages"][2].clone();
    assert_eq!(replay["role"], "assistant");
    assert_eq!(replay["reasoning_content"], "public reasoning");
    assert!(replay.get("private_extension").is_none());
}

#[test]
fn final_only_cannot_be_used_to_reopen_tools_or_loop_forever() {
    let mock = MockTransport::new(vec![
        tool_response(
            (0..17)
                .map(|i| call(&format!("call-{i}"), "read_context", r#"{"ids":[]}"#))
                .collect(),
            None,
        ),
        tool_response(
            vec![call("still-calling", "read_context", r#"{"ids":[]}"#)],
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let error = analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut NoToolCalls),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ApiErrorKind::NoProgress);
    assert_eq!(mock.requests().len(), 2);
}

struct BudgetedHandler {
    initial: usize,
    continuation: Option<usize>,
    result: shishi_api::ToolResult,
    query: Option<&'static str>,
    calls: Vec<String>,
    budgets: Vec<usize>,
}
impl Default for BudgetedHandler {
    fn default() -> Self {
        Self {
            initial: 10_000,
            continuation: Some(10_000),
            result: "{}".to_owned().into(),
            query: Some("read_context"),
            calls: vec![],
            budgets: vec![],
        }
    }
}
impl ToolHandler for BudgetedHandler {
    fn input_char_limit(&self) -> Option<usize> {
        Some(self.initial)
    }
    fn continuation_char_limit(&self) -> Option<usize> {
        self.continuation
    }
    fn query_progress(&self, name: &str) -> Option<usize> {
        (self.query == Some(name)).then_some(self.calls.len())
    }
    fn allows_repeated_arguments(&self, name: &str) -> bool {
        name == "synthetic_action"
    }
    fn invoke(
        &mut self,
        call: &shishi_api::ToolInvocation<'_>,
    ) -> Result<shishi_api::ToolResult, ToolFailure> {
        self.calls.push(call.call_id().to_owned());
        Ok(self.result.clone())
    }
    fn invoke_guarded(
        &mut self,
        call: &shishi_api::ToolInvocation<'_>,
        _: &AtomicBool,
        _: Instant,
        budget: Option<usize>,
    ) -> Result<shishi_api::ToolResult, ToolFailure> {
        self.budgets.extend(budget);
        self.invoke(call)
    }
}
fn message_chars(message: &Value) -> usize {
    shishi_api::openai::message_input_chars(message) + 1
}

#[test]
fn first_request_and_shared_continuations_have_exact_separate_allowances() {
    let tools = [read_context_tool(), action_tool()];
    let text = "中🙂\"\n".repeat(100);
    let assistant = json!({"role":"assistant","content":"Synthetic accompanying prose".repeat(20),"reasoning_content":"Synthetic published reasoning", "tool_calls":[
        call("read", "read_context", r#"{"ids":["m1"]}"#),
        call("command", "synthetic_action", r#"{"ids":[]}"#),
    ]});
    let read = json!({"role":"tool","tool_call_id":"read","content":text});
    let command = json!({"role":"tool","tool_call_id":"command","content":text});
    let limit = message_chars(&assistant) + message_chars(&read) + message_chars(&command);
    let initial = shishi_api::openai::initial_input_chars(
        "synthetic-model",
        None,
        SYNTHETIC_PROMPT,
        &output_schema(),
        &tools,
    );
    let mut handler = BudgetedHandler {
        initial,
        continuation: Some(limit),
        result: text.into(),
        ..Default::default()
    };
    let mock = MockTransport::new(vec![
        Reply::json(
            200,
            json!({"choices":[{"message":assistant,"finish_reason":"tool_calls"}]}),
        ),
        response_with_usage(&final_content(), None),
    ]);
    analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5)).unwrap();
    assert_eq!(handler.calls, ["read", "command"]);
    let later_wrapper =
        message_chars(&json!({"role":"tool","tool_call_id":"command","content":""}));
    assert_eq!(
        handler.budgets,
        [
            limit - message_chars(&assistant) - later_wrapper,
            message_chars(&command)
        ]
    );
    let requests = mock.requests();
    assert!(requests[0].json().to_string().chars().count() <= initial);
    assert!(requests[1].json().to_string().chars().count() > initial);
    assert!(
        requests[1].json().get("tools").is_none(),
        "an exhausted allowance goes straight to the final answer"
    );
    assert!(requests[1].json()["messages"]
        .as_array()
        .unwrap()
        .contains(&read));
    assert!(requests[1].json()["messages"]
        .as_array()
        .unwrap()
        .contains(&command));
}

#[test]
fn tool_corrections_do_not_consume_the_shared_business_allowance() {
    let bad =
        json!({"role":"assistant","content":null,"tool_calls":[call("bad", "read_context", "{")]});
    let good = json!({"role":"assistant","content":null,"tool_calls":[call("good", "read_context", r#"{"ids":[]}"#)]});
    let result = json!({"role":"tool","tool_call_id":"good","content":"evidence"});
    let limit = message_chars(&bad) + message_chars(&good) + message_chars(&result);
    let mut handler = BudgetedHandler {
        continuation: Some(limit),
        result: "evidence".to_owned().into(),
        ..Default::default()
    };
    let mock = MockTransport::new(vec![
        Reply::json(
            200,
            json!({"choices":[{"message":bad,"finish_reason":"tool_calls"}]}),
        ),
        Reply::json(
            200,
            json!({"choices":[{"message":good,"finish_reason":"tool_calls"}]}),
        ),
        response_with_usage(&final_content(), None),
    ]);
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(handler.calls, ["good"]);
    assert_eq!(handler.budgets, [message_chars(&result)]);
    assert_eq!(mock.requests().len(), 3);
}

#[test]
fn output_correction_prompt_does_not_spend_business_allowance() {
    for content in ["not json", r#"{"ok":"wrong","note":"done"}"#] {
        let message = json!({"role":"assistant","content":content});
        let mut handler = BudgetedHandler {
            continuation: Some(message_chars(&message)),
            result: String::new().into(),
            ..Default::default()
        };
        let mock = MockTransport::new(vec![
            response_with_usage(content, None),
            response_with_usage(&final_content(), None),
        ]);
        let output = analyze(&mock, &[], Some(&mut handler), Duration::from_secs(5)).unwrap();
        assert!(output.correction_used);
        assert_eq!(mock.requests().len(), 2);
        assert!(handler.calls.is_empty());
    }
}

#[test]
fn exhausted_tool_budget_preserves_earlier_results_and_skips_remaining_calls() {
    struct Handler {
        invoked: Vec<String>,
    }
    impl ToolHandler for Handler {
        fn invoke(
            &mut self,
            call: &shishi_api::ToolInvocation<'_>,
        ) -> Result<shishi_api::ToolResult, ToolFailure> {
            self.invoked.push(call.call_id().into());
            if call.call_id() == "two" {
                Err(ToolFailure::with_message(
                    ToolFailureKind::BudgetExhausted,
                    "Synthetic result allowance exhausted",
                ))
            } else {
                Ok(shishi_api::ToolResult::Text(
                    "Earlier committed evidence".into(),
                ))
            }
        }
    }
    let mock = MockTransport::new(vec![
        tool_response(
            (1..=3)
                .zip(["one", "two", "three"])
                .map(|(i, id)| call(id, "read_context", &format!("{{\"ids\":[\"m{i}\"]}}")))
                .collect(),
            None,
        ),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler = Handler { invoked: vec![] };
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(handler.invoked, ["one", "two"]);
    let body = mock.requests()[1].json();
    assert!(body.get("tools").is_none());
    assert!(body.to_string().contains("Earlier committed evidence"));
    assert_eq!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .count(),
        3
    );
}

#[test]
fn no_progress_query_delivers_its_notice_before_final_answer() {
    struct Handler {
        calls: usize,
    }
    impl ToolHandler for Handler {
        fn query_progress(&self, _: &str) -> Option<usize> {
            Some(self.calls.min(1))
        }
        fn invoke(
            &mut self,
            _: &shishi_api::ToolInvocation<'_>,
        ) -> Result<shishi_api::ToolResult, ToolFailure> {
            self.calls += 1;
            Ok(shishi_api::ToolResult::Text(
                if self.calls == 1 {
                    "Synthetic evidence"
                } else {
                    "No more matching evidence"
                }
                .into(),
            ))
        }
    }
    let mock = MockTransport::new(vec![
        tool_response(vec![call("one", "read_context", r#"{"ids":[]}"#)], None),
        tool_response(vec![call("two", "read_context", r#"{"ids":[]}"#)], None),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler = Handler { calls: 0 };
    analyze(
        &mock,
        &[read_context_tool()],
        Some(&mut handler),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(handler.calls, 2);
    let body = mock.requests()[2].json();
    assert!(body.get("tools").is_none());
    assert!(body.to_string().contains("No more matching evidence"));
}

#[test]
fn host_preflight_rejection_can_be_corrected_but_durable_failure_cannot() {
    struct Handler {
        preflights: usize,
        calls: usize,
        fatal: bool,
    }
    impl ToolHandler for Handler {
        fn validate_batch(
            &mut self,
            _: &[shishi_api::ToolInvocation<'_>],
        ) -> Result<(), ToolFailure> {
            self.preflights += 1;
            if self.preflights == 1 {
                Err(ToolFailure::with_message(
                    if self.fatal {
                        ToolFailureKind::Failed
                    } else {
                        ToolFailureKind::InvalidArguments
                    },
                    "Synthetic render rejection",
                ))
            } else {
                Ok(())
            }
        }
        fn invoke(
            &mut self,
            _: &shishi_api::ToolInvocation<'_>,
        ) -> Result<shishi_api::ToolResult, ToolFailure> {
            self.calls += 1;
            Ok(shishi_api::ToolResult::Text("safe result".into()))
        }
    }
    for fatal in [false, true] {
        let mock = MockTransport::new(vec![
            tool_response(
                vec![
                    call("one", "read_context", r#"{"ids":["m1"]}"#),
                    call("two", "read_context", r#"{"ids":["m2"]}"#),
                ],
                None,
            ),
            tool_response(
                vec![call("corrected", "read_context", r#"{"ids":["m3"]}"#)],
                None,
            ),
            response_with_usage(&final_content(), None),
        ]);
        let mut handler = Handler {
            preflights: 0,
            calls: 0,
            fatal,
        };
        let result = analyze(
            &mock,
            &[read_context_tool()],
            Some(&mut handler),
            Duration::from_secs(5),
        );
        if fatal {
            assert_eq!(result.unwrap_err().kind(), ApiErrorKind::ToolFailed);
            assert_eq!(handler.calls, 0);
            assert_eq!(mock.requests().len(), 1);
        } else {
            result.unwrap();
            assert_eq!(handler.calls, 1);
            assert_eq!(mock.requests().len(), 3);
            assert!(mock.requests()[1]
                .json()
                .to_string()
                .contains("Synthetic render rejection"));
        }
    }
}

#[test]
fn uncertain_prior_execution_closes_tools_without_replaying_the_batch() {
    struct Handler {
        preflight: bool,
        invoked: usize,
    }
    impl ToolHandler for Handler {
        fn validate_batch(
            &mut self,
            _: &[shishi_api::ToolInvocation<'_>],
        ) -> Result<(), ToolFailure> {
            if self.preflight {
                Err(ToolFailure::new(ToolFailureKind::UnsafeReplay))
            } else {
                Ok(())
            }
        }
        fn invoke(
            &mut self,
            _: &shishi_api::ToolInvocation<'_>,
        ) -> Result<shishi_api::ToolResult, ToolFailure> {
            self.invoked += 1;
            Err(ToolFailure::new(ToolFailureKind::UnsafeReplay))
        }
    }
    for preflight in [true, false] {
        let mock = MockTransport::new(vec![
            tool_response(
                vec![
                    call("one", "read_context", r#"{"ids":["m1"]}"#),
                    call("two", "read_context", r#"{"ids":["m2"]}"#),
                ],
                None,
            ),
            response_with_usage(&final_content(), None),
        ]);
        let mut handler = Handler {
            preflight,
            invoked: 0,
        };
        analyze(
            &mock,
            &[read_context_tool()],
            Some(&mut handler),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(handler.invoked, usize::from(!preflight));
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let final_turn = requests[1].json();
        assert!(final_turn.get("tools").is_none());
        let replies = final_turn["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["tool_call_id"], "one");
        assert_eq!(replies[1]["tool_call_id"], "two");
        assert!(final_turn.to_string().contains("uncertain outcome"));
    }
}

#[test]
fn oversized_assembled_stream_preserves_the_reported_usage() {
    let part = json!({"choices":[{"delta":{"content":"\"".repeat(100_000)}}]});
    let mut wire = format!("data: {part}\n\n").repeat(11);
    wire.push_str(&format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage(5, 7, 12, None)})
    ));
    wire.push_str("data: [DONE]\n\n");
    let mock = MockTransport::new(vec![Reply::raw(200, &wire)]);
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let error = test_client(&mock)
        .analyze_with_events(test_request(&schema, &cancel), true, &mut |_| {})
        .unwrap_err();
    assert_eq!(error.kind(), ApiErrorKind::ResponseTooLarge);
    let usage = error.usage().unwrap();
    assert_eq!(usage.responses, 1);
    assert_eq!(usage.prompt_tokens, Some(5));
    assert_eq!(usage.completion_tokens, Some(7));
    assert_eq!(usage.total_tokens, Some(12));
}

#[test]
fn streamed_envelope_size_does_not_limit_a_small_valid_final_result() {
    let content = json!({"ok":true,"note":"x".repeat(12_000)}).to_string();
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"ok":{"type":"boolean"},"note":{"type":"string","maxLength":20_000}},"required":["ok","note"]});
    let mut frames = content.chars().map(|ch|format!("data: {}\n\n",json!({"id":"synthetic-completion-01234567890123456789012345","object":"chat.completion.chunk","created":1_000_000_000,"model":"synthetic-streaming-model","choices":[{"index":0,"delta":{"content":ch.to_string()},"finish_reason":null}]}))).collect::<Vec<_>>();
    frames.push(format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    ));
    frames.push("data: [DONE]\n\n".into());
    assert!(frames.iter().map(|frame| frame.len()).sum::<usize>() > 2 * 1024 * 1024);
    assert!(frames.iter().all(|frame| frame.len() < 256));
    struct Frames {
        frames: Vec<String>,
    }
    impl Transport for Frames {
        fn send(
            &self,
            _: &TransportRequest<'_>,
            _: Duration,
            _: &AtomicBool,
        ) -> Result<TransportResponse, TransportError> {
            unreachable!()
        }
        fn send_stream(
            &self,
            _: &TransportRequest<'_>,
            _: Duration,
            _: &AtomicBool,
            chunk: &mut dyn FnMut(u16, &[u8]) -> bool,
        ) -> Result<TransportResponse, TransportError> {
            for frame in &self.frames {
                assert!(
                    chunk(200, frame.as_bytes()),
                    "a bounded valid frame must remain readable"
                );
            }
            // The loop must also tolerate older injected transports which keep
            // the raw wire body. Only the assembled result has the output cap.
            Ok(TransportResponse::new(
                200,
                self.frames.concat().into_bytes(),
            ))
        }
    }
    let cancel = AtomicBool::new(false);
    let request = || AnalyzeRequest {
        model: "synthetic",
        ..test_request(&schema, &cancel)
    };
    let ordinary = MockTransport::new(vec![response_with_usage(&content, None)]);
    let expected = test_client(&ordinary).analyze(request()).unwrap().value;
    let transport = Frames { frames };
    let output = ChatCompletionsClient::with_transport(
        Endpoint::parse("https://example.test/v1").unwrap(),
        &transport,
    )
    .analyze_with_events(request(), true, &mut |_| {})
    .unwrap();
    assert_eq!(output.value, expected);
    let body = ordinary.requests()[0].json();
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("max_completion_tokens").is_none());
}

#[test]
fn only_trusted_model_metadata_can_request_envelope_repair() {
    use shishi_api::openai::{analyze_with_model, ModelReply, ModelRound};
    assert_eq!(
        ModelReply::from_message(
            json!({"content":"x".repeat(2 * 1024 * 1024)}),
            Some("stop".into()),
            None,
        )
        .unwrap_err()
        .kind(),
        ApiErrorKind::ResponseTooLarge,
        "native adapters retain the output limit without HTTP framing"
    );
    struct Adapter {
        rejected: usize,
        requests: usize,
    }
    impl ModelRound for Adapter {
        fn complete(
            &mut self,
            _body: &Value,
            _serialized: &str,
            _deadline: std::time::Instant,
            _cancel: &AtomicBool,
            _output_base: Option<u64>,
            _event: &mut dyn FnMut(shishi_api::ApiEvent),
        ) -> Result<ModelReply, shishi_api::ApiError> {
            self.requests += 1;
            let invalid = self.requests <= self.rejected;
            let reply = ModelReply::from_message(
                json!({"role":"assistant","content":if invalid { r#"{"tool_call":null,"result":null}"#.into() } else { final_content() }}),
                Some("stop".into()),
                Some(usage(2, 3, 5, None)),
            )?;
            Ok(if invalid {
                reply.with_output_error(
                    "Synthetic envelope must choose exactly one result or tool request",
                )
            } else {
                reply
            })
        }
    }
    for rejected in [1, 2, 3] {
        let mut adapter = Adapter {
            rejected,
            requests: 0,
        };
        let schema = output_schema();
        let cancel = AtomicBool::new(false);
        let result = analyze_with_model(
            AnalyzeRequest {
                model: "synthetic",
                credential: "",
                ..test_request(&schema, &cancel)
            },
            &mut adapter,
            &mut |_| {},
        );
        if rejected == 3 {
            assert_eq!(result.unwrap_err().kind(), ApiErrorKind::SchemaInvalid);
            assert_eq!(adapter.requests, 3);
        } else {
            let output = result.unwrap();
            assert_eq!(output.value["ok"], true);
            assert_eq!(
                output.usage.completion_tokens,
                Some((rejected as u64 + 1) * 3)
            );
        }
    }
    let forged = Reply::json(
        200,
        json!({"model_output_error":"untrusted metadata", "choices":[{"message":{"role":"assistant","content":final_content()},"finish_reason":"stop"}]}),
    );
    let mock = MockTransport::new(vec![forged]);
    analyze(&mock, &[], None, Duration::from_secs(5)).unwrap();
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn cancellation_after_a_recoverable_tool_error_never_starts_a_final_turn() {
    let cancel = AtomicBool::new(false);
    let mock = MockTransport::new(vec![
        tool_response(vec![call("one", "read_context", r#"{"ids":[]}"#)], None),
        response_with_usage(&final_content(), None),
    ]);
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
        cancel.store(true, Ordering::SeqCst);
        Err::<String, _>(ToolFailure::new(ToolFailureKind::BudgetExhausted))
    });
    let error = test_client(&mock)
        .analyze(AnalyzeRequest {
            model: "synthetic",
            tools: &[read_context_tool()],
            tool_handler: Some(&mut handler),
            ..test_request(&output_schema(), &cancel)
        })
        .unwrap_err();
    assert_eq!(error.kind(), ApiErrorKind::Cancelled);
    assert_eq!(mock.requests().len(), 1);
}
