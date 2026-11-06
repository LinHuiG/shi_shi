use shishi_api::openai::{analyze_with_model, ModelReply, ModelRound};

use super::*;

struct Pages(usize);

impl Context for Pages {
    fn read(
        &mut self,
        name: &str,
        arguments: &Value,
        _: Option<usize>,
    ) -> std::result::Result<ToolResult, ToolFailure> {
        assert_eq!(name, "read_context");
        self.0 += 1;
        Ok(ToolResult::Text(format!("第{}页：{}", self.0, arguments["ref"])))
    }
    fn progress(&self) -> usize {
        self.0
    }
    fn finish(&mut self, value: &mut Value) {
        value["_context_usage"] = json!({"pages": self.0});
    }
}

/// Asks for one page, then answers with what it saw.
struct Script(usize);

impl ModelRound for Script {
    fn complete(
        &mut self,
        body: &Value,
        _: &str,
        _: Instant,
        _: &AtomicBool,
        _: Option<u64>,
        _: &mut dyn FnMut(ApiEvent),
    ) -> std::result::Result<ModelReply, shishi_api::ApiError> {
        self.0 += 1;
        let usage = Some(json!({"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}));
        if self.0 == 1 {
            let call = json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function",
                "function":{"name":"read_context","arguments":"{\"ref\":\"m1\"}"}}]});
            return ModelReply::from_message(call, Some("tool_calls".into()), usage);
        }
        let seen = body["messages"].as_array().unwrap().iter().any(|m| m["content"] == "第1页：\"m1\"");
        ModelReply::from_message(
            json!({"role":"assistant","content":json!({"ok":seen}).to_string()}),
            Some("stop".into()),
            usage,
        )
    }
}

#[test]
fn tool_loop_answers_retrieval_from_context_and_sums_usage() {
    let (dir, config) = (crate::testing::temp_dir("provider-loop"), Config::default());
    let route = Route {
        kind: "classify".into(),
        connection: AiConnection::default(),
        model: "m".into(),
        effort: String::new(),
    };
    let schema =
        json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    let tools = json!([{"name":"read_context","description":"读取上下文",
        "inputSchema":{"type":"object","properties":{"ref":{"type":"string"}},"required":["ref"],"additionalProperties":false}}]);
    let definitions = shishi_api::tool_definitions_from_mcp(&tools).unwrap();
    let prepared = Prepared { prompt: "分析".into(), schema, tools: Some(tools), definitions, ..Default::default() };
    let mut job = fake::job(route, config, prepared, dir);
    job.trace = Some(Trace::default());
    let mut pages = Pages(0);
    let mut events = vec![];
    let (value, usage) = tool_loop(&job, "m", Some(&mut pages), &mut |e| events.push(e), "", false, |r, e| {
        analyze_with_model(r, &mut Script(0), e)
    })
    .unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["_context_usage"]["pages"], 1);
    assert_eq!(usage["input_tokens"], 14);
    assert_eq!(usage["output_tokens"], 6);
    assert!(events.contains(&Event::Usage(usage.clone())));
    assert!(job.trace.unwrap().get("output.txt").unwrap().contains("\"ok\":true"));
}

#[test]
fn api_errors_keep_reference_text() {
    use shishi_api::ApiError;
    assert_eq!(api_error(&ApiError::simple(ApiErrorKind::Cancelled)), super::super::CANCELLED);
    assert!(api_error(&ApiError::simple(ApiErrorKind::InputLimit)).ends_with("：首轮完整请求超过输入额度，未调用模型"));
    let refusal = api_error(&ApiError::simple(ApiErrorKind::Refusal));
    assert!(refusal.starts_with("API 分析失败：") && refusal.ends_with("核对输入范围"), "{refusal}");
}

#[test]
fn unknown_driver_is_rejected_before_any_call() {
    let connection = AiConnection { driver: "other".into(), ..Default::default() };
    assert_eq!(backend(&connection).err().unwrap(), "尚未支持的 AI 接入：other，未发起调用");
}

#[test]
fn redaction_hides_secret_and_pixels() {
    let mut v = json!({"a":"key sk-1 end","img":{"url":"data:image/png;base64,AAAA"}});
    redact(&mut v, "sk-1");
    assert_eq!(v, json!({"a":"key [已隐藏密钥] end","img":{"url":"[已省略图片数据]"}}));
}
