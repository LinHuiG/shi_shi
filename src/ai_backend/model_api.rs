//! Plain model text is adapted to the same SS-owned tool loop as an HTTP API.
use super::{AnalysisEvent, AnalysisRequest, Connection};
use serde_json::{json, Value};
use shishi_api::{
    ChatCompletionsClient, Endpoint, Transport, TransportError, TransportErrorKind,
    TransportRequest, TransportResponse,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex,
    },
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
    let schema = envelope(schema, &body["tools"]);
    frame(&body).0.chars().count() + schema.to_string().chars().count()
}

struct ModelTransport<'a, 'r> {
    connection: &'a Connection,
    request: &'a AnalysisRequest<'r>,
    events: mpsc::SyncSender<AnalysisEvent>,
    counter: Mutex<usize>,
    completed_output: Mutex<Option<u64>>,
    completed_usage: Mutex<Option<Value>>,
    observed_usage: Mutex<Option<Value>>,
    error: Mutex<Option<String>>,
    id: String,
}
impl Transport for ModelTransport<'_, '_> {
    fn send(
        &self,
        request: &TransportRequest<'_>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<TransportResponse, TransportError> {
        let failed = || TransportError::new(TransportErrorKind::Protocol);
        let result = (|| -> Result<TransportResponse, String> {
            let body: Value = serde_json::from_slice(request.body().ok_or("缺少模型请求")?)
                .map_err(|e| e.to_string())?;
            let schema = envelope(self.request.schema, &body["tools"]);
            let (prompt, images) = frame(&body);
            if prompt.chars().count() + schema.to_string().chars().count()
                > self.request.config.max_input_chars
            {
                return Err(format!(
                    "{}：工具续轮超过全局输入预算，已停止；已执行的工具不会重放",
                    crate::ai::INPUT_LIMIT_ERROR
                ));
            }
            let round = {
                let mut n = self.counter.lock().unwrap();
                *n += 1;
                *n
            };
            let trace = self.request.trace.join(format!("round-{round:04}"));
            if self.request.config.retain_ai_traces {
                std::fs::create_dir_all(&trace).map_err(|e| e.to_string())?;
                std::fs::write(trace.join("input.txt"), &prompt).map_err(|e| e.to_string())?;
                std::fs::write(trace.join("schema.json"), schema.to_string())
                    .map_err(|e| e.to_string())?;
            }
            let mut config = self.request.config.clone();
            config.ai_timeout_seconds = timeout.as_secs().min(config.ai_timeout_seconds);
            if config.ai_timeout_seconds == 0 {
                return Err("AI 总时限已到".into());
            }
            let payload = json!({"_model_only":true,"_model_images":images});
            // DSH's submission tool is a structured response channel, not an
            // executor. No query or registered command is exposed over MCP.
            let submission = json!({"tools":[{"name":"submit_analysis","description":"Submit the single model response object.","inputSchema":schema}]});
            let stopped = AtomicBool::new(false);
            let local_cancel = AtomicBool::new(cancel.load(Ordering::SeqCst));
            let deadline = Instant::now() + timeout;
            let backend = super::backend(self.connection)?;
            let raw = std::thread::scope(|scope| {
                scope.spawn(|| {
                    while !stopped.load(Ordering::SeqCst) {
                        if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                            local_cancel.store(true, Ordering::SeqCst);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
                let result = backend.analyze(
                    self.connection,
                    AnalysisRequest {
                        show_session: self.request.show_session,
                        config: &config,
                        kind: self.request.kind,
                        payload: &payload,
                        root: self.request.root,
                        trace: &trace,
                        prompt: &prompt,
                        schema: &schema,
                        tools: (self.connection.driver == "dsh").then_some(&submission),
                        registered_tools: None,
                        cancel: &local_cancel,
                    },
                    &mut |event| match event {
                        AnalysisEvent::Usage(value) => {
                            let combined =
                                usage_sum(self.completed_usage.lock().unwrap().as_ref(), &value);
                            *self.observed_usage.lock().unwrap() = Some(combined.clone());
                            let _ = self.events.send(AnalysisEvent::Usage(combined));
                        }
                        AnalysisEvent::OutputTokens(value) => {
                            let previous = *self.completed_output.lock().unwrap();
                            let _ = self.events.send(AnalysisEvent::OutputTokens(
                                previous.zip(value).map(|(a, b)| a.saturating_add(b)),
                            ));
                        }
                        event => {
                            let _ = self.events.send(event);
                        }
                    },
                );
                stopped.store(true, Ordering::SeqCst);
                result
            })?;
            let (value, usage) = raw;
            let combined = usage_sum(self.completed_usage.lock().unwrap().as_ref(), &usage);
            *self.completed_usage.lock().unwrap() = Some(combined.clone());
            *self.observed_usage.lock().unwrap() = Some(combined.clone());
            let _ = self.events.send(AnalysisEvent::Usage(combined));
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
            if call.is_null() == result.is_null() {
                return Err("模型必须返回一次工具请求或最终结果，不能同时返回或全部留空".into());
            }
            let message = if call.is_object() {
                json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("text:{}:{round}",self.id),"type":"function","function":{"name":call["name"],"arguments":call["arguments"]}}]})
            } else {
                json!({"role":"assistant","content":result.to_string()})
            };
            let input = usage["input_tokens"].as_u64();
            let output = usage["output_tokens"].as_u64();
            {
                let mut completed = self.completed_output.lock().unwrap();
                *completed = completed.zip(output).map(|(a, b)| a.saturating_add(b));
            }
            let response = json!({"choices":[{"index":0,"finish_reason":if call.is_object(){"tool_calls"}else{"stop"},"message":message}],
                "usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":usage["total_tokens"].as_u64().or_else(||input.zip(output).map(|(a,b)|a.saturating_add(b))),"prompt_tokens_details":{"cached_tokens":usage["cached_input_tokens"]}}});
            Ok(TransportResponse::new(
                200,
                serde_json::to_vec(&response).map_err(|e| e.to_string())?,
            ))
        })();
        result.map_err(|error| {
            *self.error.lock().unwrap() = Some(error);
            failed()
        })
    }
}

pub(crate) fn analyze(
    connection: &Connection,
    request: AnalysisRequest<'_>,
    event: &mut dyn FnMut(AnalysisEvent),
) -> Result<(Value, Value), String> {
    let (tx, rx) = mpsc::sync_channel(64);
    let transport = ModelTransport {
        connection,
        request: &request,
        events: tx.clone(),
        counter: Mutex::new(0),
        completed_output: Mutex::new(Some(0)),
        completed_usage: Mutex::new(None),
        observed_usage: Mutex::new(None),
        error: Mutex::new(None),
        id: crate::model::id(),
    };
    let client = ChatCompletionsClient::with_transport(
        Endpoint::parse("https://ss-model.invalid/v1").unwrap(),
        &transport,
    );
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            super::api::analyze_client(
                &request,
                &mut |e| {
                    let _ = tx.send(e);
                },
                &client,
                "local-model-adapter",
                false,
                false,
            )
        });
        while !worker.is_finished() {
            if let Ok(value) = rx.recv_timeout(Duration::from_millis(20)) {
                event(value);
            }
        }
        let result = worker
            .join()
            .map_err(|_| "模型接入工作线程已停止".to_string())?;
        for value in rx.try_iter() {
            event(value);
        }
        result
    });
    match result {
        Err(error) => {
            if let Some(mut usage) = transport.observed_usage.lock().unwrap().clone() {
                usage["partial"] = json!(true);
                event(AnalysisEvent::Usage(usage));
            }
            Err(transport.error.lock().unwrap().take().unwrap_or_else(|| {
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
            }))
        }
        value => value,
    }
}

#[cfg(test)]
#[path = "model_api_tests.rs"]
mod tests;
