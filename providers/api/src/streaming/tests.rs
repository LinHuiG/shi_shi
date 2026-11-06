use super::*;

fn collect(chunks: &[&str]) -> (Result<Value, ()>, Vec<String>) {
    let mut c = StreamCollector::new(Some(10), false);
    let mut seen = Vec::new();
    let mut event = |e: ApiEvent| match e {
        ApiEvent::Text { kind, text, .. } => seen.push(format!("{kind}:{text}")),
        ApiEvent::OutputTokens(n) => seen.push(format!("out:{n:?}")),
        _ => {}
    };
    for chunk in chunks {
        if c.push(chunk.as_bytes(), &mut event).is_err() {
            return (Err(()), seen);
        }
    }
    (c.finish(&mut event), seen)
}

#[test]
fn assembles_content_tools_and_usage_across_split_frames() {
    let (v, seen) = collect(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\nda",
        "ta: {\"choices\":[{\"delta\":{\"content\":\"llo\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"a\\\"\"}}]}}],\"usage\":{\"completion_tokens\":3}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ]);
    let v = v.unwrap();
    let message = &v["choices"][0]["message"];
    assert_eq!(message["content"], "hello");
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert!(seen.contains(&"out:Some(13)".to_string()) && seen.contains(&"content:llo".to_string()));
}

#[test]
fn responses_events_become_a_chat_reply() {
    let mut c = StreamCollector::new(Some(0), true);
    let lines = [
        r#"{"type":"response.output_text.delta","delta":"{\"ok\""}"#,
        r#"{"type":"response.output_text.delta","delta":":true}"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read_context","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13,"input_tokens_details":{"cached_tokens":5}}}}"#,
    ];
    for line in lines {
        c.push(format!("event: x\ndata: {line}\n\n").as_bytes(), &mut |_| {}).unwrap();
    }
    let v = c.finish(&mut |_| {}).unwrap();
    let choice = &v["choices"][0];
    assert_eq!(
        (choice["message"]["content"].as_str(), choice["finish_reason"].as_str()),
        (Some("{\"ok\":true}"), Some("tool_calls"))
    );
    assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "read_context");
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
    let mut failed = StreamCollector::new(None, true);
    assert!(failed.push(b"data: {\"type\":\"response.failed\"}\n\n", &mut |_| {}).is_err());
}

#[test]
fn plain_json_passes_and_early_ends_fail() {
    let (v, _) = collect(&["  {\"choices\":[]", "}"]);
    assert_eq!(v.unwrap()["choices"], json!([]));
    assert!(collect(&["data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"]).0.is_err());
    assert!(collect(&["data: {\"error\":{\"message\":\"x\"}}\n\n"]).0.is_err());
}
