//! Synthetic replies only: a scripted model and a loopback HTTP server.

use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

const KEY: &str = "synthetic-key";

fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{"ok":{"type":"boolean"}},"required":["ok"]})
}

fn tools() -> Vec<ToolDefinition> {
    tool_definitions_from_mcp(&json!({"tools":[{"name":"read_context","description":"read","inputSchema":
        {"type":"object","additionalProperties":false,"properties":{"ref":{"type":"string"}},"required":["ref"]}}]}))
    .unwrap()
}

fn call(id: &str, arguments: &str) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"read_context","arguments":arguments}}]})
}

fn answer(text: &str) -> Value {
    json!({"role":"assistant","content":text})
}

/// Replays replies and records every body it was sent.
struct Script(Vec<Result<Value, ApiErrorKind>>, Vec<Value>);

impl ModelRound for Script {
    fn complete(
        &mut self,
        body: &Value,
        _: &str,
        _: Instant,
        _: &AtomicBool,
        _: Option<u64>,
        _: &mut dyn FnMut(ApiEvent),
    ) -> Result<ModelReply, ApiError> {
        self.1.push(body.clone());
        let message = self.0.remove(0).map_err(ApiError::simple)?;
        let reason = if message["tool_calls"].is_null() { "stop" } else { "tool_calls" };
        ModelReply::from_message(
            message,
            Some(reason.into()),
            Some(json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12})),
        )
    }
}

/// Pages `ref` until it has served `pages`; progress counts served pages.
struct Pages {
    served: usize,
    pages: usize,
    limit: Option<usize>,
}

impl ToolHandler for Pages {
    fn invoke(
        &mut self,
        call: &ToolInvocation<'_>,
        _: &AtomicBool,
        _: Instant,
        _: Option<usize>,
    ) -> Result<ToolResult, ToolFailure> {
        if self.served < self.pages {
            self.served += 1;
        }
        Ok(ToolResult::Text(format!("page {} of {}", self.served, call.arguments["ref"])))
    }
    fn query_progress(&self, _: &str) -> Option<usize> {
        Some(self.served)
    }
}

fn analyze(script: &mut Script, handler: &mut Pages) -> Result<AnalysisOutput, ApiError> {
    let (schema, tools, cancel, limit) = (schema(), tools(), AtomicBool::new(false), handler.limit);
    let request = AnalyzeRequest {
        model: "m",
        reasoning_effort: Some("high"),
        prompt: "SYSTEM",
        schema: &schema,
        credential: KEY,
        cancel: &cancel,
        timeout: Duration::from_secs(5),
        tools: &tools,
        tool_handler: handler,
        input_limit: None,
        continuation_limit: limit,
    };
    analyze_with_model(request, script, &mut |_| {})
}

#[test]
fn the_loop_reads_tools_then_validates_the_final_value() {
    let mut script = Script(vec![Ok(call("c1", r#"{"ref":"a"}"#)), Ok(answer(r#"{"ok":true}"#))], vec![]);
    let mut pages = Pages { served: 0, pages: 9, limit: None };
    let out = analyze(&mut script, &mut pages).unwrap();
    assert_eq!((out.value, script.1.len(), pages.served), (json!({"ok":true}), 2, 1));
    assert_eq!((out.usage.prompt_tokens, out.usage.total_tokens), (Some(20), Some(24)));
    let second = &script.1[1]["messages"];
    assert_eq!(second[3], json!({"role":"tool","tool_call_id":"c1","content":"page 1 of \"a\""}));
    assert_eq!(script.1[0]["reasoning_effort"], "high");
}

#[test]
fn a_rejected_batch_is_corrected_once_then_tools_close() {
    let replies = vec![Ok(call("c1", "{}")), Ok(call("c2", r#"{"x":1}"#)), Ok(answer(r#"{"ok":false}"#))];
    let mut script = Script(replies, vec![]);
    let out = analyze(&mut script, &mut Pages { served: 0, pages: 9, limit: None }).unwrap();
    assert_eq!(out.value["ok"], false);
    assert!(script.1[1]["tools"].is_array() && script.1[2].get("tools").is_none());
    let last = script.1[2]["messages"].as_array().unwrap().last().unwrap().clone();
    assert!(last["content"].as_str().unwrap().contains("No further tools"));
}

#[test]
fn a_repeated_query_without_progress_and_a_spent_budget_close_tools() {
    let replies =
        vec![Ok(call("c1", r#"{"ref":"a"}"#)), Ok(call("c2", r#"{"ref":"a"}"#)), Ok(answer(r#"{"ok":true}"#))];
    let mut script = Script(replies, vec![]);
    analyze(&mut script, &mut Pages { served: 0, pages: 1, limit: None }).unwrap();
    assert!(script.1[2].get("tools").is_none());

    let mut script = Script(vec![Ok(call("c1", r#"{"ref":"a"}"#)), Ok(answer(r#"{"ok":true}"#))], vec![]);
    analyze(&mut script, &mut Pages { served: 0, pages: 9, limit: Some(10) }).unwrap();
    let messages = script.1[1]["messages"].as_array().unwrap();
    assert!(messages.iter().all(|m| m["role"] != "tool") && script.1[1].get("tools").is_none());

    let mut script = Script(vec![Ok(call("c1", r#"{"ref":"a"}"#)), Ok(call("c1", r#"{"ref":"b"}"#))], vec![]);
    let err = analyze(&mut script, &mut Pages { served: 0, pages: 9, limit: None }).unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::NoProgress);
}

#[test]
fn failures_are_typed_and_carry_usage() {
    for (reply, kind) in [
        (answer("not json"), ApiErrorKind::InvalidJson),
        (answer(r#"{"ok":1}"#), ApiErrorKind::SchemaInvalid),
        (json!({"role":"assistant","content":null,"refusal":"no"}), ApiErrorKind::Refusal),
        (answer(""), ApiErrorKind::InvalidJson),
    ] {
        let err =
            analyze(&mut Script(vec![Ok(reply)], vec![]), &mut Pages { served: 0, pages: 1, limit: None }).unwrap_err();
        assert_eq!(err.kind, kind);
        assert_eq!(err.usage.unwrap().completion_tokens, Some(2));
    }
    let mut script = Script(vec![Err(ApiErrorKind::UnsupportedResponseFormat), Ok(answer(r#"{"ok":true}"#))], vec![]);
    assert_eq!(
        analyze(&mut script, &mut Pages { served: 0, pages: 1, limit: None }).unwrap().value,
        json!({"ok":true})
    );
    assert!(script.1[0].get("response_format").is_some() && script.1[1].get("response_format").is_none());
    let unknown = UsageAggregate::default();
    let mut usage = unknown;
    usage.record(Some(&json!({"prompt_tokens":3})));
    usage.record(None);
    assert_eq!((usage.prompt_tokens, usage.responses), (None, 2));
    let mut deepseek = UsageAggregate::default();
    deepseek.record(Some(&json!({"prompt_tokens":10,"prompt_cache_hit_tokens":8,"prompt_cache_miss_tokens":2})));
    assert_eq!(deepseek.cached_tokens, Some(8));
}

/// Answer one request with `response`, handing its text to the test.
fn serve(response: String) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/v1", listener.local_addr().unwrap().port());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = stream.read(&mut buf).unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break;
                }
            }
        }
        tx.send(String::from_utf8_lossy(&request).to_string()).unwrap();
        stream.write_all(response.as_bytes()).unwrap();
    });
    (url, rx)
}

fn sse(events: &[Value]) -> String {
    let body: String =
        events.iter().map(|e| format!("data: {}\n\n", e.as_str().map_or(e.to_string(), String::from))).collect();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn run_client(client: &Client) -> Result<AnalysisOutput, ApiError> {
    let (schema, cancel) = (schema(), AtomicBool::new(false));
    let request = AnalyzeRequest {
        model: "m",
        reasoning_effort: Some("low"),
        prompt: "SYSTEM",
        schema: &schema,
        credential: KEY,
        cancel: &cancel,
        timeout: Duration::from_secs(5),
        tools: &[],
        tool_handler: &mut Pages { served: 0, pages: 0, limit: None },
        input_limit: None,
        continuation_limit: None,
    };
    client.analyze_with_events(request, true, &mut |_| {})
}

#[test]
fn chat_completions_stream_over_http() {
    let chunk = json!({"choices":[{"delta":{"content":"{\"ok\":true}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}});
    let (url, seen) = serve(sse(&[chunk, json!("[DONE]")]));
    let client = Client::new(&url, Wire::Chat).unwrap();
    let out = run_client(&client);
    let request = seen.recv().unwrap();
    assert!(
        request.starts_with("POST /v1/chat/completions") && request.contains(&format!("authorization: Bearer {KEY}")),
        "{request}"
    );
    let out = out.unwrap();
    assert_eq!((out.value["ok"].as_bool(), out.usage.total_tokens), (Some(true), Some(6)));
}

#[test]
fn a_chatgpt_sign_in_speaks_responses_with_its_headers_and_instructions() {
    let events = [
        json!({"type":"response.output_text.delta","delta":"{\"ok\":true}"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}),
    ];
    let (url, seen) = serve(sse(&events));
    let wire = Wire::Codex {
        instructions: "CODEX".into(),
        headers: vec![("chatgpt-account-id".into(), "acct".into()), ("version".into(), "0.159.0".into())],
    };
    let out = run_client(&Client::new(&url, wire).unwrap()).unwrap();
    assert_eq!((out.value["ok"].as_bool(), out.usage.prompt_tokens), (Some(true), Some(7)));
    let request = seen.recv().unwrap();
    let (head, body) = request.split_once("\r\n\r\n").unwrap();
    assert!(
        head.starts_with("POST /v1/responses")
            && head.contains("chatgpt-account-id: acct")
            && head.contains("session-id: shishi-"),
        "{head}"
    );
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        (body["instructions"].as_str(), body["store"].as_bool(), body["reasoning"]["effort"].as_str()),
        (Some("CODEX"), Some(false), Some("low"))
    );
    assert_eq!(
        body["input"][0],
        json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"SYSTEM"}]})
    );
    assert_eq!(body["text"]["format"]["type"], "json_schema");
}

#[test]
fn chat_bodies_become_responses_items_and_models_parse() {
    let mut body = initial_request_body("m", None, "S", &schema(), &tools());
    body["messages"].as_array_mut().unwrap().extend([
        call("c1", "{}"),
        json!({"role":"tool","tool_call_id":"c1","content":"r"}),
        json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}),
    ]);
    let out = responses_body(&body, "I", "k");
    let input = out["input"].as_array().unwrap();
    assert_eq!(input[2], json!({"type":"function_call","call_id":"c1","name":"read_context","arguments":"{}"}));
    assert_eq!(input[3], json!({"type":"function_call_output","call_id":"c1","output":"r"}));
    assert_eq!(input[4]["content"][0], json!({"type":"input_image","image_url":"data:image/png;base64,AA=="}));
    assert_eq!(out["tools"][0]["name"], "read_context");
    let codex = parse_models(
        &json!({"models":[{"slug":"b","priority":2},{"slug":"a","priority":1,"base_instructions":"X",
        "supported_reasoning_levels":[{"effort":"low"}]},{"slug":"h","visibility":"hide"}]}),
        true,
    );
    assert_eq!(codex.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(
        (codex[0].instructions.as_deref(), codex[0].supported_reasoning_efforts.clone()),
        (Some("X"), Some(vec!["low".to_string()]))
    );
    let api = parse_models(&json!({"data":[{"id":"m1","name":"M"},{"id":""}]}), false);
    assert_eq!((api.len(), api[0].display_name.as_deref()), (1, Some("M")));
    assert!(unsupported_response_format(&json!({"error":{"code":"unsupported_parameter","param":"response_format"}})));
    assert!(!unsupported_response_format(&json!({"error":{"code":"unsupported_parameter","param":"tools"}})));
}

#[test]
fn base_urls_are_https_and_plain() {
    assert_eq!(
        Client::new(" https://api.example.com/v1/ ", Wire::Chat).unwrap().url("/models"),
        "https://api.example.com/v1/models"
    );
    for bad in [
        "http://api.example.com",
        "https://a.example/v1?x=1",
        "https://u:p@a.example",
        "https://a.example/v1/models",
        "x",
    ] {
        assert!(Endpoint::parse(bad).is_err(), "{bad}");
    }
    assert!(Endpoint::parse("http://127.0.0.1:9/v1").is_ok());
}
