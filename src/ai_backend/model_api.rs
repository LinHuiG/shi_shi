//! Native model rounds share the tool loop directly, without an HTTP adapter.
use super::{AnalysisEvent, AnalysisRequest, Connection};
use serde_json::{json, Value};
use shishi_api::json::chars as json_chars;
use shishi_api::openai::{analyze_with_model, ModelReply, ModelRound};
use shishi_api::{ApiError, ApiErrorKind, ApiEvent};
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn envelope(schema: &Value, tools: &Value) -> Value {
    let names = tools
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| tool["function"]["name"].clone())
        .collect::<Vec<_>>();
    let calls = if names.is_empty() {
        json!({"type":"null"})
    } else {
        json!({"type":["object","null"],"additionalProperties":false,"properties":{
        "name":{"type":"string","enum":names},"arguments":{"type":"string"}},"required":["name","arguments"]})
    };
    // Business results are objects. Use the nullable type already supported by
    // the shared validator, including its explicit presentation defaults.
    let mut result = schema.clone();
    result["type"] = json!(["object", "null"]);
    json!({"type":"object","additionalProperties":false,"properties":{
        "tool_call":calls,"result":result},"required":["tool_call","result"]})
}

fn frame(body: &Value) -> (String, Vec<Value>) {
    let mut messages = body["messages"].clone();
    let mut images = vec![];
    for message in messages.as_array_mut().into_iter().flatten() {
        for content in message["content"].as_array_mut().into_iter().flatten() {
            if content["type"] != "image_url" {
                continue;
            }
            if let Some(url) = content["image_url"]["url"].as_str() {
                if let Some((mime, data)) = url
                    .strip_prefix("data:")
                    .and_then(|s| s.split_once(";base64,"))
                {
                    images.push(json!({"mimeType":mime,"data":data}));
                }
            }
            *content = json!({"type":"text","text":"SS has attached this read-only image to the current model request."});
        }
    }
    (
        format!(
            "{}\n\n工具目录：{}\n\n本次对话：{}",
            crate::prompt_config::get("model_api_protocol"),
            body["tools"],
            messages
        ),
        images,
    )
}

/// Query messages keep their ordinary JSON framing for accounting. The tool
/// loop credits them in full under the independent retrieval/image budgets;
/// the remaining command history therefore matches the native text frame.
pub(crate) fn body_input_chars(schema: &Value, body: &Value) -> usize {
    let messages = body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let message_chars = 2
        + messages
            .iter()
            .map(shishi_api::openai::message_input_chars)
            .sum::<usize>()
        + messages.len().saturating_sub(1);
    crate::prompt_config::get("model_api_protocol")
        .chars()
        .count()
        + "\n\n工具目录：\n\n本次对话：".chars().count()
        + json_chars(&body["tools"])
        + message_chars
        + json_chars(&envelope(schema, &body["tools"]))
}

fn usage_step(value: &Value) -> Value {
    let input = value["input_tokens"].as_u64();
    let output = value["output_tokens"].as_u64();
    json!({"input_tokens":input,"output_tokens":output,"cached_input_tokens":value["cached_input_tokens"].as_u64(),
        "total_tokens":value["total_tokens"].as_u64().or_else(||input.zip(output).map(|(a,b)|a.saturating_add(b)))})
}
fn usage_sum(previous: Option<&Value>, current: &Value) -> Value {
    let mut value = usage_step(current);
    if let Some(previous) = previous {
        for key in [
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "total_tokens",
        ] {
            value[key] = json!(previous[key]
                .as_u64()
                .zip(value[key].as_u64())
                .map(|(a, b)| a.saturating_add(b)));
        }
    }
    value
}

pub(crate) fn input_chars(
    config: &crate::model::Config,
    prompt: &str,
    schema: &Value,
    declarations: Option<&Value>,
) -> usize {
    let tools = declarations
        .and_then(|v| shishi_api::tool_definitions_from_mcp(v).ok())
        .unwrap_or_default();
    let body = shishi_api::openai::initial_request_body(
        &config.model,
        (!config.reasoning_effort.is_empty()).then_some(config.reasoning_effort.as_str()),
        prompt,
        schema,
        &tools,
    );
    body_input_chars(schema, &body)
}

struct NativeModel<'a, 'r, 'e> {
    connection: &'a Connection,
    request: &'a AnalysisRequest<'r>,
    events: &'a RefCell<&'e mut dyn FnMut(AnalysisEvent)>,
    counter: usize,
    completed_usage: Option<Value>,
    observed_usage: Option<Value>,
    error: Option<String>,
    dsh: Option<super::dsh::AnalysisProcess>,
    id: String,
}
impl NativeModel<'_, '_, '_> {
    fn round(
        &mut self,
        body: &Value,
        deadline: Instant,
        cancel: &AtomicBool,
        output_base: Option<u64>,
    ) -> Result<ModelReply, String> {
        if cancel.load(Ordering::SeqCst) {
            return Err(crate::ai_control::CANCELLED.into());
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        let schema = envelope(self.request.schema, &body["tools"]);
        let (prompt, images) = frame(&body);
        // The shared tool loop has measured this native frame's chargeable
        // history. Independently metered query results remain in the full
        // prompt and must not be charged again against the initial limit.
        self.counter += 1;
        let round = self.counter;
        let trace = self.request.trace.join(format!("round-{round:04}"));
        if self.request.config.retain_ai_traces {
            let recorded = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&trace)?;
                std::fs::write(trace.join("input.txt"), &prompt)?;
                std::fs::write(trace.join("schema.json"), schema.to_string())
            })();
            if let Err(error) = recorded {
                (self.events.borrow_mut())(AnalysisEvent::Stream {
                    kind: "diagnostic".into(),
                    text: format!("分析跟踪保存失败，继续本次分析：{error}"),
                    append: false,
                });
            }
        }
        let mut config = self.request.config.clone();
        config.ai_timeout_seconds = timeout
            .as_secs()
            .saturating_add(u64::from(timeout.subsec_nanos() != 0))
            .min(config.ai_timeout_seconds);
        if config.ai_timeout_seconds == 0 {
            return Err("AI 总时限已到".into());
        }
        let payload = json!({"_model_images":images});
        let stopped = AtomicBool::new(false);
        let local_cancel = AtomicBool::new(cancel.load(Ordering::SeqCst));
        let raw = std::thread::scope(|scope| {
            if self.connection.driver != "dsh" {
                scope.spawn(|| {
                    while !stopped.load(Ordering::SeqCst) {
                        if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                            local_cancel.store(true, Ordering::SeqCst);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
            }
            let model_request = AnalysisRequest {
                show_session: self.request.show_session,
                config: &config,
                kind: self.request.kind,
                payload: &payload,
                root: self.request.root,
                trace: &trace,
                prompt: &prompt,
                schema: &schema,
                tools: None,
                registered_tools: None,
                cancel: if self.connection.driver == "dsh" {
                    cancel
                } else {
                    &local_cancel
                },
            };
            let mut forward = |event| match event {
                AnalysisEvent::Usage(value) => {
                    let combined = usage_sum(self.completed_usage.as_ref(), &value);
                    self.observed_usage = Some(combined.clone());
                    (self.events.borrow_mut())(AnalysisEvent::Usage(combined));
                }
                AnalysisEvent::OutputTokens(value) => {
                    (self.events.borrow_mut())(AnalysisEvent::OutputTokens(
                        output_base.zip(value).map(|(a, b)| a.saturating_add(b)),
                    ));
                }
                event => {
                    (self.events.borrow_mut())(event);
                }
            };
            let result = (|| {
                if self.connection.driver == "dsh" {
                    let process = &mut self.dsh;
                    if process.is_none() {
                        *process = Some(super::dsh::AnalysisProcess::prepare(
                            self.connection,
                            &model_request,
                            deadline,
                        )?);
                    }
                    let result =
                        process
                            .as_ref()
                            .unwrap()
                            .analyze(model_request, deadline, &mut forward);
                    if result.is_err() || !process.as_ref().unwrap().is_connected() {
                        // A failed or cancelled process never survives into
                        // another round; tools are not replayed.
                        process.take();
                    }
                    result
                } else {
                    super::codex::analyze_round(model_request, self.connection, &mut forward)
                }
            })();
            stopped.store(true, Ordering::SeqCst);
            if cancel.load(Ordering::SeqCst) {
                Err(crate::ai_control::CANCELLED.into())
            } else if Instant::now() >= deadline {
                Err("AI 总时限已到".into())
            } else {
                result
            }
        })?;
        let (value, usage) = raw;
        let combined = usage_sum(self.completed_usage.as_ref(), &usage);
        self.completed_usage = Some(combined.clone());
        self.observed_usage = Some(combined.clone());
        (self.events.borrow_mut())(AnalysisEvent::Usage(combined));
        if !value.as_object().is_some_and(|v| {
            v.len() == 2 && v.contains_key("tool_call") && v.contains_key("result")
        }) {
            return Err("模型未返回有效的结构化调用或最终结果".into());
        }
        let call = &value["tool_call"];
        let result = &value["result"];
        if !call.is_null() && !call.is_object() {
            return Err("工具请求必须是对象".into());
        }
        let model_output_error = (call.is_null() == result.is_null()).then_some(
                "模型必须返回一次工具请求或最终结果，不能同时返回或全部留空；本次封装未执行工具，也未采用结果",
            );
        let is_tool = model_output_error.is_none() && call.is_object();
        let message = if model_output_error.is_some() {
            // Preserve the rejected reply for one bounded correction in
            // the shared loop. It must never become an executable call.
            json!({"role":"assistant","content":value.to_string()})
        } else if is_tool {
            json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("text:{}:{round}",self.id),"type":"function","function":{"name":call["name"],"arguments":call["arguments"]}}]})
        } else {
            json!({"role":"assistant","content":result.to_string()})
        };
        let input = usage["input_tokens"].as_u64();
        let output = usage["output_tokens"].as_u64();
        let usage = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":usage["total_tokens"].as_u64().or_else(||input.zip(output).map(|(a,b)|a.saturating_add(b))),"prompt_tokens_details":{"cached_tokens":usage["cached_input_tokens"]}});
        let reply = ModelReply::from_message(
            message,
            Some(if is_tool { "tool_calls" } else { "stop" }.into()),
            Some(usage),
        )
        .map_err(|e| e.to_string())?;
        Ok(match model_output_error {
            Some(error) => reply.with_output_error(error),
            None => reply,
        })
    }
}
impl ModelRound for NativeModel<'_, '_, '_> {
    fn complete(
        &mut self,
        body: &Value,
        _serialized: &str,
        deadline: Instant,
        cancel: &AtomicBool,
        output_base: Option<u64>,
        _event: &mut dyn FnMut(ApiEvent),
    ) -> Result<ModelReply, ApiError> {
        self.round(body, deadline, cancel, output_base)
            .map_err(|error| {
                self.error = Some(error);
                ApiError::simple(ApiErrorKind::MalformedResponse)
            })
    }
}

pub(crate) fn analyze(
    connection: &Connection,
    request: AnalysisRequest<'_>,
    event: &mut dyn FnMut(AnalysisEvent),
) -> Result<(Value, Value), String> {
    let events = RefCell::new(event);
    let mut model = NativeModel {
        connection,
        request: &request,
        events: &events,
        counter: 0,
        completed_usage: None,
        observed_usage: None,
        error: None,
        dsh: None,
        id: crate::model::id(),
    };
    let result = super::api::analyze_with(
        &request,
        &mut |e| (events.borrow_mut())(e),
        "",
        false,
        |request, event| analyze_with_model(request, &mut model, event),
    );
    result.map_err(|error| {
        if let Some(mut usage) = model.observed_usage.take() {
            usage["partial"] = json!(true);
            (events.borrow_mut())(AnalysisEvent::Usage(usage));
        }
        model.error.take().unwrap_or_else(|| {
            error.replacen(
                "API 分析失败",
                &format!(
                    "{} 分析失败",
                    if connection.driver == "codex" {
                        "Codex"
                    } else {
                        "DSH"
                    }
                ),
                1,
            )
        })
    })
}

#[cfg(test)]
#[path = "model_api_tests.rs"]
mod tests;
