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
            model: "synthetic-model",
            reasoning_effort: Some("high"),
            prompt: SYNTHETIC_PROMPT,
            schema: &schema,
            credential: SYNTHETIC_KEY,
            cancel: &cancel,
            timeout: Duration::from_secs(5),
            tools: &[read_context_tool()],
            tool_handler: Some(&mut handler),
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
            model: "synthetic-model",
            reasoning_effort: Some("high"),
            prompt: SYNTHETIC_PROMPT,
            schema: &schema,
            credential: SYNTHETIC_KEY,
            cancel: &cancel,
            timeout: Duration::from_secs(2),
            tools: &[],
            tool_handler: None
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
    analyze_prompt(mock, tools, handler, timeout, SYNTHETIC_PROMPT)
}

fn analyze_prompt(
    mock: &MockTransport,
    tools: &[ToolDefinition],
    handler: Option<&mut dyn ToolHandler>,
    timeout: Duration,
    prompt: &str,
) -> Result<AnalysisOutput, ApiError> {
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let client = test_client(mock);
    client.analyze(AnalyzeRequest {
        model: "synthetic-model",
        reasoning_effort: None,
        prompt,
        schema: &schema,
        credential: SYNTHETIC_KEY,
        cancel: &cancel,
        timeout,
        tools,
        tool_handler: handler,
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
        let mock = MockTransport::new(vec![tool_response(
            vec![call("img", "read_image", r#"{"image_id":"one"}"#)],
            None,
        )]);
        let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
            shishi_api::ToolResult::from_mcp(
                &json!({"content":[{"type":"image","mimeType":mime,"data":data}]}),
            )
        });
        let err = analyze(
            &mock,
            &[read_image_tool()],
            Some(&mut handler),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(err.kind(), ApiErrorKind::ToolFailed);
        assert_eq!(mock.requests().len(), 1);
        assert!(!format!("{err:?}").contains(data));
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
    let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let seen_by_handler = Arc::clone(&seen);
    let mut handler = FnToolHandler(move |invocation: &shishi_api::ToolInvocation<'_>| {
        seen_by_handler.lock().unwrap().push((
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
        *seen.lock().unwrap(),
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
fn unknown_tool_is_denied_without_calling_the_handler() {
    let mock = MockTransport::new(vec![tool_response(
        vec![call("call_shell", "shell", r#"{"command":"whoami"}"#)],
        None,
    )]);
    let tools = [read_context_tool()];
    let calls = Arc::new(Mutex::new(0usize));
    let calls_by_handler = Arc::clone(&calls);
    let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
        *calls_by_handler.lock().unwrap() += 1;
        Ok("unexpected".to_string())
    });
    let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
        .expect_err("shell is not advertised");
    assert_eq!(error.kind(), ApiErrorKind::UnknownTool);
    assert_eq!(*calls.lock().unwrap(), 0);
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn malformed_and_schema_invalid_tool_arguments_are_typed_failures() {
    for (arguments, expected) in [
        ("{", ApiErrorKind::InvalidToolArguments),
        (
            r#"{"ids":["m1"],"shell":"whoami"}"#,
            ApiErrorKind::InvalidToolArguments,
        ),
    ] {
        let mock = MockTransport::new(vec![
            tool_response(vec![call("call_1", "read_context", arguments)], None),
            tool_response(vec![call("call_2", "read_context", arguments)], None),
        ]);
        let tools = [read_context_tool()];
        let calls = Arc::new(Mutex::new(0usize));
        let calls_by_handler = Arc::clone(&calls);
        let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
            *calls_by_handler.lock().unwrap() += 1;
            Ok("unexpected".to_string())
        });
        let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
            .expect_err("invalid tool arguments");
        assert_eq!(error.kind(), expected);
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(mock.requests().len(), 2);
    }
}

#[test]
fn duplicate_no_progress_tool_query_stops_before_second_handler_call() {
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
    let tools = [read_context_tool()];
    let calls = Arc::new(Mutex::new(0usize));
    let calls_by_handler = Arc::clone(&calls);
    let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
        *calls_by_handler.lock().unwrap() += 1;
        Ok(r#"{"records":[]}"#.to_string())
    });
    let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
        .expect_err("duplicate query must stop");
    assert_eq!(error.kind(), ApiErrorKind::NoProgress);
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(mock.requests().len(), 2);
}

#[test]
fn duplicate_query_in_one_response_is_rejected_before_any_callback() {
    let mock = MockTransport::new(vec![tool_response(
        vec![
            call("call_1", "read_context", r#"{"ids":["m1"]}"#),
            call("call_2", "read_context", r#"{"ids":["m1"]}"#),
        ],
        None,
    )]);
    let tools = [read_context_tool()];
    let calls = Arc::new(Mutex::new(0usize));
    let calls_by_handler = Arc::clone(&calls);
    let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
        *calls_by_handler.lock().unwrap() += 1;
        Ok(r#"{"records":[]}"#.to_string())
    });
    let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
        .expect_err("duplicate query must stop");
    assert_eq!(error.kind(), ApiErrorKind::NoProgress);
    assert_eq!(*calls.lock().unwrap(), 0);
    assert_eq!(mock.requests().len(), 1);
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
            model: "synthetic-model",
            reasoning_effort: None,
            prompt: SYNTHETIC_PROMPT,
            schema: &schema,
            credential: SYNTHETIC_KEY,
            cancel: cancel.as_ref(),
            timeout: Duration::from_secs(5),
            tools: &tools,
            tool_handler: Some(&mut handler),
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
    let mock = MockTransport::new(vec![
        response_with_usage(r#"{"ok":"wrong","note":"done"}"#, None),
        response_with_usage(&final_content(), None),
    ]);
    let output = analyze(&mock, &[], None, Duration::from_secs(5)).expect("correction succeeds");
    assert_eq!(output.value, json!({"ok":true,"note":"done"}));
    assert!(output.correction_used);
    assert_eq!(output.requests, 2);
    let requests = mock.requests();
    let second = requests[1].json();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2]["role"], "assistant");
    assert!(messages[3]["content"].as_str().unwrap().contains("schema"));
    assert!(messages[3]["content"].as_str().unwrap().contains("$.ok"));
}

#[test]
fn second_invalid_output_is_not_retried_again() {
    let mock = MockTransport::new(vec![
        response_with_usage(r#"{"ok":"wrong","note":"done"}"#, None),
        response_with_usage(r#"{"ok":"still-wrong","note":"done"}"#, None),
    ]);
    let error = analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("only one correction");
    assert_eq!(error.kind(), ApiErrorKind::SchemaInvalid);
    assert_eq!(mock.requests().len(), 2);
    assert!(error.to_string().contains("$."));
    assert!(!error.to_string().contains("still-wrong"));
}

#[test]
fn rejected_final_output_keeps_valid_reported_usage_without_exposing_body() {
    let mock = MockTransport::new(vec![
        response_with_usage(
            r#"{"ok":"wrong","note":"private-synthetic-text"}"#,
            Some(json!({"prompt_tokens":10,"completion_tokens":2})),
        ),
        response_with_usage(
            r#"{"ok":"wrong","note":"private-synthetic-text"}"#,
            Some(json!({"prompt_tokens":12,"completion_tokens":3})),
        ),
    ]);
    let error =
        analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("schema remains strict");
    let usage = error.usage().unwrap();
    assert_eq!(usage.prompt_tokens, Some(22));
    assert_eq!(usage.completion_tokens, Some(5));
    assert!(!error.to_string().contains("private-synthetic-text"));
}

#[test]
fn invalid_json_gets_one_correction_then_typed_failure() {
    let mock = MockTransport::new(vec![
        response_with_usage("not json", None),
        response_with_usage(&final_content(), None),
    ]);
    let output = analyze(&mock, &[], None, Duration::from_secs(5)).expect("correction succeeds");
    assert!(output.correction_used);
    assert_eq!(output.requests, 2);
    let requests = mock.requests();
    let second = requests[1].json();
    assert!(second["messages"][3]["content"]
        .as_str()
        .unwrap()
        .contains("valid JSON"));

    let mock = MockTransport::new(vec![
        response_with_usage("not json", None),
        response_with_usage("still not json", None),
    ]);
    let error = analyze(&mock, &[], None, Duration::from_secs(5)).expect_err("only one correction");
    assert_eq!(error.kind(), ApiErrorKind::InvalidJson);
    assert_eq!(mock.requests().len(), 2);
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
        let invoked = Arc::new(Mutex::new(0usize));
        let invoked_by_handler = Arc::clone(&invoked);
        let mut handler = FnToolHandler(move |_: &shishi_api::ToolInvocation<'_>| {
            *invoked_by_handler.lock().unwrap() += 1;
            Ok("unexpected".to_string())
        });
        let error = analyze(&mock, &tools, Some(&mut handler), Duration::from_secs(5))
            .expect_err("invalid tool call shape");
        assert_eq!(error.kind(), ApiErrorKind::InvalidToolCall);
        assert_eq!(*invoked.lock().unwrap(), 0);
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
    analyze_prompt(&mock, &[], None, Duration::from_secs(5), SYNTHETIC_PROMPT)
        .expect("valid output");
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
    let error = client
        .analyze(AnalyzeRequest {
            model: "synthetic-model",
            reasoning_effort: None,
            prompt: SYNTHETIC_PROMPT,
            schema: &schema,
            credential: "",
            cancel: &cancel,
            timeout: Duration::from_secs(5),
            tools: &[],
            tool_handler: None,
        })
        .expect_err("empty credential");
    assert_eq!(error.kind(), ApiErrorKind::MissingCredential);
    assert_eq!(mock.requests().len(), 0);

    let error = client
        .analyze(AnalyzeRequest {
            model: "synthetic-model",
            reasoning_effort: None,
            prompt: SYNTHETIC_PROMPT,
            schema: &schema,
            credential: SYNTHETIC_KEY,
            cancel: &cancel,
            timeout: Duration::from_secs(5),
            tools: &tools,
            tool_handler: None,
        })
        .expect_err("tools require a handler");
    assert_eq!(error.kind(), ApiErrorKind::InvalidConfig);
    assert_eq!(mock.requests().len(), 0);

    let bad_schema = json!({"type":"string","pattern":"^x"});
    let error = client
        .analyze(AnalyzeRequest {
            model: "synthetic-model",
            reasoning_effort: None,
            prompt: SYNTHETIC_PROMPT,
            schema: &bad_schema,
            credential: SYNTHETIC_KEY,
            cancel: &cancel,
            timeout: Duration::from_secs(5),
            tools: &[],
            tool_handler: None,
        })
        .expect_err("unsupported schema");
    assert_eq!(error.kind(), ApiErrorKind::InvalidConfig);
    assert_eq!(mock.requests().len(), 0);
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
                model: "synthetic-model",
                reasoning_effort: Some("low"),
                prompt: SYNTHETIC_PROMPT,
                schema: &schema,
                credential: SYNTHETIC_KEY,
                cancel: &cancel,
                timeout: Duration::from_secs(5),
                tools: &tools,
                tool_handler: Some(&mut handler),
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
fn stream_refusal_and_partial_tool_arguments_never_invoke_tools() {
    for wire in ["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"refusal\"}]}\n\ndata: [DONE]\n\n", "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"x\",\"function\":{\"name\":\"read_context\",\"arguments\":\"{\"}}]}}]}\n\n"] {
        let mock=MockTransport::new(vec![Reply::raw(200,wire)]);let schema=output_schema();let cancel=AtomicBool::new(false);let tools=[read_context_tool()];let mut invoked=0;
        let mut handler=FnToolHandler(|_:&shishi_api::ToolInvocation<'_>|{invoked+=1;Ok("{}".to_owned())});
        let error=test_client(&mock).analyze_with_events(AnalyzeRequest{model:"synthetic-model",reasoning_effort:None,prompt:SYNTHETIC_PROMPT,schema:&schema,credential:SYNTHETIC_KEY,cancel:&cancel,timeout:Duration::from_secs(5),tools:&tools,tool_handler:Some(&mut handler)},true,&mut |_|{}).unwrap_err();
        assert!(matches!(error.kind(),ApiErrorKind::Refusal|ApiErrorKind::Truncated));assert_eq!(invoked,0);assert_eq!(mock.requests().len(),1);
    }
}

#[test]
fn one_invalid_member_blocks_the_entire_tool_batch_and_is_retained_for_diagnostics() {
    let make = || {
        tool_response(
            vec![
                call("valid", "read_context", r#"{"ids":[]}"#),
                call(
                    "invalid",
                    "read_image",
                    r#"{"image_id":"fixture","extra":true}"#,
                ),
            ],
            None,
        )
    };
    let mock = MockTransport::new(vec![make(), make()]);
    let tools = [read_context_tool(), read_image_tool()];
    let mut invoked = 0;
    let mut handler = FnToolHandler(|_: &shishi_api::ToolInvocation<'_>| {
        invoked += 1;
        Ok("{}".to_owned())
    });
    let schema = output_schema();
    let cancel = AtomicBool::new(false);
    let mut failures = Vec::new();
    let err = test_client(&mock)
        .analyze_with_events(
            AnalyzeRequest {
                model: "synthetic",
                reasoning_effort: None,
                prompt: SYNTHETIC_PROMPT,
                schema: &schema,
                credential: SYNTHETIC_KEY,
                cancel: &cancel,
                timeout: Duration::from_secs(1800),
                tools: &tools,
                tool_handler: Some(&mut handler),
            },
            false,
            &mut |e| {
                if let shishi_api::ApiEvent::ToolRejected(v) = e {
                    failures.push(v)
                }
            },
        )
        .unwrap_err();
    assert_eq!(err.kind(), ApiErrorKind::InvalidToolArguments);
    assert_eq!(invoked, 0);
    assert_eq!(failures.len(), 2);
    assert!(failures[0].to_string().contains("extra"));
    assert_eq!(mock.requests().len(), 2);
}

#[test]
fn provider_reply_cannot_inject_roles_or_extra_protocol_fields_into_replay() {
    let mock = MockTransport::new(vec![
        Reply::json(
            200,
            json!({"choices":[{"message":{"role":"system","private_extension":"must-not-replay","reasoning_content":"public reasoning","tool_calls":[call("ctx","read_context",r#"{"ids":[]}"#)]},"finish_reason":"tool_calls"}]}),
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
