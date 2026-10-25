use super::*;
use crate::{
    analysis_context::{self, Meter},
    credentials::CredentialStore,
};
use serde_json::json;
use shishi_api::{
    AnalyzeRequest, ApiEvent, ChatCompletionsClient, ToolFailure, ToolFailureKind, ToolHandler,
    ToolInvocation, ToolResult,
};
use std::time::Duration;

pub struct Api;
#[cfg(test)]
#[path = "workflow_tool_tests.rs"]
mod workflow_tool_tests;
pub(crate) fn key(
    c: &Connection,
    root: &Path,
) -> Result<crate::credentials::ResolvedCredential, String> {
    CredentialStore::new(crate::storage::data_dir(&root).join("credentials"))
        .resolve(
            c.credential_id.as_deref().unwrap_or(&c.id),
            c.key_env.as_deref(),
        )
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "未配置此连接的 API Key".into())
}

/// Shared retrieval bookkeeping for adapters, always from this request's view.
pub(crate) fn attach_result(value: &mut Value, snapshot: &Value, meter: &Meter) {
    value["_context_seen"] = json!(meter.seen);
    value["_context_usage"] = json!({"calls":meter.calls,"chars":meter.chars,"image_calls":meter.image_calls,"image_bytes":meter.image_bytes,"budget_exhausted":meter.exhausted});
    value["_image_notes"] = crate::images::notes(meter, value);
    match analysis_context::memory_update(snapshot, meter, value) {
        Ok(Some(update)) => value["_memory_update"] = update,
        Ok(None) => {}
        Err(error) => value["_memory_error"] = json!(error),
    }
    if let Some(map) = value.as_object_mut() {
        map.remove("memory_update");
        map.remove("image_notes");
    }
}

struct ApiTools<'a, 'r> {
    request: &'a AnalysisRequest<'r>,
    meter: &'a mut Meter,
}
impl ToolHandler for ApiTools<'_, '_> {
    fn invoke(&mut self, call: &ToolInvocation<'_>) -> Result<ToolResult, ToolFailure> {
        let r = self.request;
        if r.cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ToolFailure::new(ToolFailureKind::Failed));
        }
        let snapshot = &r.payload["_local_context"];
        let result = match call.name() {
            "read_context" => analysis_context::query(snapshot, self.meter, call.arguments())
                .map(|text| json!({"content":[{"type":"text","text":text}]})),
            "read_image" => crate::images::read(snapshot, self.meter, call.arguments()),
            _ => return Err(ToolFailure::new(ToolFailureKind::Denied)),
        };
        ToolResult::from_mcp(&result.unwrap_or_else(
            |error| json!({"isError":true,"content":[{"type":"text","text":error}]}),
        ))
    }
    fn allows_repeated_arguments(&self) -> bool {
        self.request.registered_tools.is_some()
    }
    fn input_char_limit(&self) -> Option<usize> {
        Some(self.request.config.max_input_chars)
    }
    fn validate_batch(&mut self, calls: &[ToolInvocation<'_>]) -> Result<(), ToolFailure> {
        if let Some(session) = self.request.registered_tools {
            let registered = calls
                .iter()
                .filter(|call| session.contains(call.name()))
                .copied()
                .collect::<Vec<_>>();
            session.check_batch(&registered)?;
        }
        Ok(())
    }
    fn invoke_guarded(
        &mut self,
        call: &ToolInvocation<'_>,
        cancel: &std::sync::atomic::AtomicBool,
        deadline: std::time::Instant,
    ) -> Result<ToolResult, ToolFailure> {
        if let Some(session) = self
            .request
            .registered_tools
            .filter(|session| session.contains(call.name()))
        {
            session.execute(
                call.name(),
                call.call_id(),
                call.arguments(),
                cancel,
                deadline,
            )
        } else {
            self.invoke(call)
        }
    }
}

/// Shared SS tool loop for HTTP APIs and plain model adapters.
pub(crate) fn analyze_client(
    r: &AnalysisRequest<'_>,
    event: &mut dyn FnMut(AnalysisEvent),
    client: &ChatCompletionsClient<'_>,
    secret: &str,
    stream: bool,
    native_stream_events: bool,
) -> Result<(Value, Value), String> {
    let snapshot = &r.payload["_local_context"];
    // Use the exact declarations that were measured by the application.
    // Never rebuild a different tool list here.
    let tools = match r.tools {
        Some(tools) => shishi_api::tool_definitions_from_mcp(tools).map_err(|e| e.to_string())?,
        None => vec![],
    };
    let mut meter = Meter::default();
    let mut handler = ApiTools {
        request: r,
        meter: &mut meter,
    };
    let output = client
            .analyze_with_events(AnalyzeRequest {
                model: &r.config.model,
                reasoning_effort: (!r.config.reasoning_effort.is_empty())
                    .then_some(r.config.reasoning_effort.as_str()),
                prompt: r.prompt,
                schema: r.schema,
                credential: secret,
                cancel: r.cancel,
                timeout: Duration::from_secs(r.config.ai_timeout_seconds),
                tools: &tools,
                tool_handler: Some(&mut handler),
            }, stream, &mut |e| match e {
                ApiEvent::Phase(phase) => event(AnalysisEvent::Stream{kind:"phase".into(),text:phase.into(),append:false}),
                ApiEvent::Text{kind,text,append} if native_stream_events => event(AnalysisEvent::Stream{kind:kind.into(),text,append}),
                ApiEvent::OutputTokens(count) if native_stream_events => event(AnalysisEvent::OutputTokens(count)),
                ApiEvent::StepFinished{output_tokens,decode_ms} if native_stream_events => event(AnalysisEvent::StepFinished{output_tokens,decode_ms}),
                ApiEvent::Response(value) => record_response(r.trace,value,secret,r.config.retain_ai_traces,false),
                ApiEvent::ToolRejected(value) => {
                    if let Some(session) = r.registered_tools {
                        if let Err(error) = session.record_rejection(&value) { event(AnalysisEvent::Stream{kind:"diagnostic".into(),text:error,append:false}); }
                    }
                    record_response(r.trace,value,secret,r.config.retain_ai_traces,true);
                },
                _ => {},
            })
            .map_err(|e| {
                if let Some(u) = e.usage() {
                    event(AnalysisEvent::Usage(json!({"input_tokens":u.prompt_tokens,"output_tokens":u.completion_tokens,"cached_input_tokens":u.cached_tokens,"total_tokens":u.total_tokens,"partial":true})));
                }
                if e.kind() == shishi_api::ApiErrorKind::Cancelled {
                    crate::ai_control::CANCELLED.into()
                } else if e.kind() == shishi_api::ApiErrorKind::InputLimit {
                    format!("{}：工具续轮超过全局输入预算，已停止；已执行的工具不会重放", crate::ai::INPUT_LIMIT_ERROR)
                } else {
                    let guidance = match e.kind() {
                        shishi_api::ApiErrorKind::HttpStatus if matches!(e.status(),Some(400|422)) => "；供应商拒绝请求参数，请核对模型、推理强度、工具及结构化输出支持。未自动丢弃参数，未应用本次结果",
                        shishi_api::ApiErrorKind::Refusal => "；供应商拒答，未应用本次结果；请查看本地调用详情并核对输入范围，不会自动重试",
                        shishi_api::ApiErrorKind::HttpStatus if e.status() == Some(504) => "；上游网关等待超时；延长本地 AI 总时限不能延长网关期限，未应用本次结果",
                        shishi_api::ApiErrorKind::InvalidToolArguments => "；工具参数不符合格式，未执行该批工具；请查看已保留的原始响应",
                        shishi_api::ApiErrorKind::Timeout => "；请求或网络等待超时，未应用本次结果，可检查网络与服务端状态后重试",
                        shishi_api::ApiErrorKind::SchemaInvalid => "；模型在一次纠正后仍未按完整格式返回，未应用本次结果，请换用支持结构化输出的模型或重试",
                        _ => "",
                    };
                    let parameter=e.parameter().map(|p|format!("；被拒参数：{p}")).unwrap_or_default();
                    format!("API 分析失败：{e}{parameter}{guidance}")
                }
            })?;
    let usage = json!({"input_tokens":output.usage.prompt_tokens,"output_tokens":output.usage.completion_tokens,
            "cached_input_tokens":output.usage.cached_tokens,"total_tokens":output.usage.total_tokens});
    event(AnalysisEvent::Usage(usage.clone()));
    let mut value = output.value;
    if r.config.retain_ai_traces {
        std::fs::write(r.trace.join("output.txt"), value.to_string())
            .map_err(|e| format!("分析结果跟踪保存失败：{e}"))?;
    }
    attach_result(&mut value, snapshot, &meter);
    Ok((value, usage))
}

impl Backend for Api {
    fn analyze(
        &self,
        c: &Connection,
        r: AnalysisRequest<'_>,
        event: &mut dyn FnMut(AnalysisEvent),
    ) -> Result<(Value, Value), String> {
        if r.kind == "handling" {
            return Err("API 不支持处置会话".into());
        }
        let client =
            ChatCompletionsClient::from_base_url(&c.base_url).map_err(|e| e.to_string())?;
        let secret = key(c, r.root)?;
        analyze_client(&r, event, &client, secret.expose_secret(), c.stream, true)
    }
    fn models(&self, c: &Connection, root: &Path) -> Result<Value, String> {
        models_with_key(c, root, None)
    }

    fn validate(&self, c: &Connection, config: &Config) -> Result<(), String> {
        shishi_api::Endpoint::parse(&c.base_url).map_err(|e| e.to_string())?;
        if config.model.trim().is_empty() {
            return Err("请选择或填写 API 模型".into());
        }
        Ok(())
    }
    fn status(&self, c: &Connection, root: &Path) -> Result<Value, String> {
        let status = CredentialStore::new(crate::storage::data_dir(&root).join("credentials"))
            .status(
                c.credential_id.as_deref().unwrap_or(&c.id),
                c.key_env.as_deref(),
            )
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"configured":status.configured,"status":if status.configured {"API Key 已配置"} else {"未配置 API Key"},"source":format!("{:?}",status.source)}),
        )
    }
    fn agent(&self, _: &Connection) -> Result<Arc<dyn AgentSession>, String> {
        Err("API 仅支持分析，处置请选择 Codex 或 DeepSeek Harness".into())
    }
    fn open_session(&self, _: &str) -> Result<(), String> {
        Err("API 分析没有桌面会话".into())
    }
}

/// Unsaved credentials are used only for discovery; never written or logged.
pub fn models_with_key(c: &Connection, root: &Path, draft: Option<&str>) -> Result<Value, String> {
    let client = ChatCompletionsClient::from_base_url(&c.base_url).map_err(|e| e.to_string())?;
    let stored;
    let secret = if c.key_env.is_none() && draft.is_some_and(|s| !s.is_empty()) {
        draft.unwrap()
    } else {
        stored = key(c, root)?;
        stored.expose_secret()
    };
    let rows = client
        .discover_models(secret, &AtomicBool::new(false), Duration::from_secs(15))
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"models":rows.iter().map(|m|{let efforts=m.supported_reasoning_efforts.clone().unwrap_or_else(||["none","minimal","low","medium","high","xhigh"].into_iter().map(str::to_string).collect());json!({"model":m.id,"displayName":m.display_name.as_deref().unwrap_or(&m.id),"reasoningEffortSupport":if m.supported_reasoning_efforts.is_some(){"declared"}else{"unverified"},"supportedReasoningEfforts":efforts.into_iter().map(|e|json!({"reasoningEffort":e})).collect::<Vec<_>>()})}).collect::<Vec<_>>()}),
    )
}

pub(crate) fn record_response(
    trace: &Path,
    mut value: Value,
    secret: &str,
    retain: bool,
    failed: bool,
) {
    if !retain {
        return;
    }
    crate::images::redact(&mut value);
    fn redact(v: &mut Value, secret: &str) {
        match v {
            Value::String(s) => {
                if !secret.is_empty() {
                    *s = s.replace(secret, "[已隐藏密钥]");
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|v| redact(v, secret)),
            Value::Object(m) => m.values_mut().for_each(|v| redact(v, secret)),
            _ => {}
        }
    }
    redact(&mut value, secret);
    if failed {
        use std::io::Write;
        // At most two argument-validation failures per run. Keep them apart
        // from the capped event stream so its tail cannot hide the cause.
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(trace.join("tool-failures.jsonl"))
        {
            let _ = writeln!(file, "{value}");
        }
    } else {
        crate::ai::append_trace_event(trace, &json!({"type":"provider_response","response":value}));
    }
}
#[cfg(test)]
mod response_tests {
    use super::*;
    #[test]
    fn raw_tool_failure_is_private_redacted_and_respects_trace_setting() {
        let root = std::env::temp_dir().join(format!("shishi-response-{}", crate::model::id()));
        std::fs::create_dir_all(&root).unwrap();
        let value = json!({"tool_calls":[{"function":{"name":"read_context","arguments":"{\"extra\":\"synthetic-key\"}"}}]});
        record_response(&root, value.clone(), "synthetic-key", false, true);
        assert!(!root.join("tool-failures.jsonl").exists());
        record_response(&root, value, "synthetic-key", true, true);
        let saved = std::fs::read_to_string(root.join("tool-failures.jsonl")).unwrap();
        assert!(saved.contains("extra"));
        assert!(!saved.contains("synthetic-key"));
        assert!(saved.contains("已隐藏密钥"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
