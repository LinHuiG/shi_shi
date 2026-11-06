//! Provider drivers and the shared SS tool loop. A driver turns one prepared
//! request into `(business JSON, usage)`; usage counters are snake_case
//! (`input_tokens`, `output_tokens`, `cached_input_tokens`, `total_tokens`)
//! and an absent counter is unknown, never zero. Events carry no provider
//! protocol fields. The caller awaits [`Driver::send`]: a blocking driver
//! holds one blocking-pool thread while it waits, a driver able to await
//! its own I/O holds none.
//!
//! Retrieval (`read_context` / `read_image`) is answered by the caller's
//! [`Context`]; registered tools by the request's tool
//! [`Session`](crate::tools::session::Session). Everything else is denied.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shishi_api::openai::{body_input_chars, message_input_chars};
use shishi_api::schema_validate::ValidationError;
use shishi_api::{
    AnalysisOutput, AnalyzeRequest, ApiError, ApiErrorKind, ApiEvent, ToolFailure, ToolFailureKind, ToolHandler,
    ToolInvocation, ToolResult, UsageAggregate,
};

use super::credentials::ResolvedCredential;
use super::prompt::{Prepared, Route};
use super::trace::Trace;
use crate::config::Connection as AiConnection;
use crate::exec::Cancel;
use crate::tools::session::Session;
use crate::{Config, Ctx, Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    SessionStarted(String),
    OutputValidationFailed,
    Usage(Value),
    /// Live cumulative output only; never a persisted billing total.
    OutputTokens(Option<u64>),
    StepFinished {
        output_tokens: Option<u64>,
        decode_ms: Option<u64>,
    },
    /// `kind` is `phase`, `diagnostic`, `reasoning` or `content`.
    Stream {
        kind: String,
        text: String,
    },
    VisibilityIssue {
        thread_id: String,
        error: String,
    },
}

/// The caller's read-only retrieval for one request. The implementation
/// keeps its own meter: a page counts as read only when it is returned.
pub trait Context: Send {
    /// Answer `read_context` / `read_image`. `budget` is the remaining
    /// allowance for this tool message's content when the loop shares one.
    fn read(
        &mut self,
        name: &str,
        arguments: &Value,
        budget: Option<usize>,
    ) -> std::result::Result<ToolResult, ToolFailure>;
    /// Changes only when a query delivered new evidence.
    fn progress(&self) -> usize;
    /// Cross-field business checks of the final value.
    fn validate_output(&mut self, _kind: &str, _value: &mut Value) -> std::result::Result<(), ValidationError> {
        Ok(())
    }
    /// Attach retrieval bookkeeping (`_context_usage`, memory updates, ...)
    /// to an accepted result.
    fn finish(&mut self, _value: &mut Value) {}
}

pub type Reply = std::result::Result<(Value, Value), String>;

/// One request handed to [`Driver::send`], owned so the driver can wait
/// for it without borrowing the caller.
pub struct Job {
    pub kind: String,
    pub route: Route,
    pub config: Arc<Config>,
    pub prepared: Prepared,
    /// Registered SS tools of this request.
    pub session: Option<Session>,
    pub root: PathBuf,
    /// The trace when traces are retained.
    pub trace: Option<Trace>,
    /// The API key the caller resolved for an API connection.
    pub key: Option<ResolvedCredential>,
    /// The request's context: its id and cancel flag.
    pub cancel: Cancel,
    /// One total deadline for every round of the request.
    pub deadline: Instant,
    pub context: Option<Box<dyn Context>>,
    pub events: Box<dyn FnMut(Event) + Send>,
}

/// What [`Driver::send`] gives back: the reply and the tool session the
/// caller still finalizes.
pub struct Done {
    pub reply: Reply,
    pub session: Option<Session>,
}

pub type Sending = Pin<Box<dyn Future<Output = Done> + Send>>;

impl Job {
    /// Time left before the total deadline.
    pub fn timeout(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn flag(&self) -> &AtomicBool {
        self.cancel.flag()
    }

    /// Answer the job with a blocking driver on this thread.
    pub fn run<D: Driver + ?Sized>(mut self, driver: &D) -> Done {
        let mut context = self.context.take();
        let mut events = std::mem::replace(&mut self.events, Box::new(|_| {}));
        let reply = driver.analyze(&self, context.as_deref_mut().map(|c| c as &mut dyn Context), &mut *events);
        Done { reply, session: self.session }
    }
}

/// [`Driver::send`] of a blocking driver: [`Job::run`] on the blocking pool.
pub fn send_blocking<D: Driver + ?Sized>(driver: Arc<D>, job: Job) -> Sending {
    Box::pin(async move {
        let done = tokio::task::spawn_blocking(move || job.run(&*driver)).await;
        done.unwrap_or_else(|_| Done { reply: Err("后台调用异常结束".into()), session: None })
    })
}

pub trait Driver: Send + Sync + 'static {
    fn analyze(&self, job: &Job, context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply;
    /// Wait for the reply of `job`. The default runs [`Driver::analyze`]
    /// on the blocking pool; a driver able to await its own I/O overrides
    /// this and holds no thread while it waits.
    fn send(self: Arc<Self>, job: Job) -> Sending {
        send_blocking(self, job)
    }
}

/// AI layer runtime on [`Ctx`]: the driver override.
#[derive(Default)]
pub struct Runtime {
    pub(crate) driver: Mutex<Option<Arc<dyn Driver>>>,
}

impl Runtime {
    /// Replace every driver (tests and synthetic regression only).
    pub fn set_driver(&self, driver: Option<Arc<dyn Driver>>) {
        *self.driver.lock().unwrap() = driver;
    }
}

/// The driver of a connection, without an override.
pub fn backend(connection: &AiConnection) -> std::result::Result<Arc<dyn Driver>, String> {
    match connection.driver.as_str() {
        "codex" => Ok(Arc::new(super::codex::Codex)),
        "api" => Ok(Arc::new(super::api::Api)),
        "dsh" => Ok(Arc::new(super::dsh::Dsh)),
        _ => Err(unsupported(connection)),
    }
}

/// The error of a connection whose driver this build does not know.
pub fn unsupported(connection: &AiConnection) -> String {
    format!("尚未支持的 AI 接入：{}，未发起调用", connection.driver)
}

/// The driver used for a connection: the override when set, else
/// [`backend`]. Unknown drivers are rejected either way.
pub fn driver(ctx: &Ctx, connection: &AiConnection) -> Result<Arc<dyn Driver>> {
    let backend = backend(connection).map_err(Error)?;
    Ok(ctx.ai.driver.lock().unwrap().clone().unwrap_or(backend))
}

const RETRIEVAL: [&str; 2] = ["read_context", "read_image"];

struct Tools<'a, 'c> {
    job: &'a Job,
    context: Option<&'c mut dyn Context>,
    degraded: bool,
    native: bool,
}

impl ToolHandler for Tools<'_, '_> {
    fn invoke(
        &mut self,
        call: &ToolInvocation<'_>,
        cancel: &AtomicBool,
        deadline: Instant,
        budget: Option<usize>,
    ) -> std::result::Result<ToolResult, ToolFailure> {
        if let Some(session) = self.job.session.as_ref().filter(|s| s.contains(call.name)) {
            return session
                .call(call.name, call.call_id, call.arguments, cancel, deadline, budget)
                .map(ToolResult::Text)
                .map_err(Into::into);
        }
        if self.job.cancel.is_cancelled() {
            return Err(ToolFailure::new(ToolFailureKind::Failed));
        }
        let context = self
            .context
            .as_deref_mut()
            .filter(|_| RETRIEVAL.contains(&call.name))
            .ok_or(ToolFailure::new(ToolFailureKind::Denied))?;
        let empty = json!({"role":"tool","tool_call_id":call.call_id,"content":""});
        context.read(
            call.name,
            call.arguments,
            budget.map(|remaining| remaining.saturating_sub(message_input_chars(&empty) + 1)),
        )
    }

    fn validate_output(&mut self, value: &mut Value) -> std::result::Result<(), ValidationError> {
        let Some(context) = self.context.as_deref_mut() else { return Ok(()) };
        let had = value.get("_analysis_degraded").is_some();
        context.validate_output(&self.job.kind, value)?;
        if !had && value.as_object_mut().and_then(|m| m.remove("_analysis_degraded")).is_some() {
            self.degraded = true;
        }
        Ok(())
    }

    fn allows_repeated_arguments(&self, name: &str) -> bool {
        self.job.session.as_ref().is_some_and(|s| s.contains(name))
    }

    fn query_progress(&self, name: &str) -> Option<usize> {
        RETRIEVAL.contains(&name).then(|| self.context.as_deref().map_or(0, Context::progress))
    }

    fn input_chars(&self, body: &Value, serialized_chars: usize) -> usize {
        // Query images are credited by the loop; their pixels are bounded
        // separately and never use the text allowance.
        if self.native {
            super::native::body_input_chars(&self.job.prepared.schema, body)
        } else {
            body_input_chars(body, serialized_chars)
        }
    }

    fn validate_batch(&mut self, calls: &[ToolInvocation<'_>]) -> std::result::Result<(), ToolFailure> {
        let Some(session) = &self.job.session else { return Ok(()) };
        let batch: Vec<(&str, &str, &Value)> =
            calls.iter().filter(|c| session.contains(c.name)).map(|c| (c.call_id, c.name, c.arguments)).collect();
        session.check_batch(&batch).map_err(ToolFailure::from)
    }
}

fn redact(value: &mut Value, secret: &str) {
    match value {
        Value::String(s) if !secret.is_empty() => *s = s.replace(secret, "[已隐藏密钥]"),
        Value::Array(a) => a.iter_mut().for_each(|v| redact(v, secret)),
        Value::Object(m) => {
            for (key, v) in m.iter_mut() {
                // Image pixels never enter a trace.
                if key == "url" && v.as_str().is_some_and(|u| u.starts_with("data:")) {
                    *v = json!("[已省略图片数据]");
                } else {
                    redact(v, secret);
                }
            }
        }
        _ => {}
    }
}

/// Keep a raw provider response in the private trace, without the secret.
fn record_response(trace: Option<&Trace>, mut value: Value, secret: &str, failed: bool) {
    let Some(trace) = trace else { return };
    redact(&mut value, secret);
    if failed {
        trace.tool_failure(&value);
    } else {
        trace.event(&json!({"type":"provider_response","response":value}));
    }
}

fn usage_json(u: &UsageAggregate) -> Value {
    json!({"input_tokens":u.prompt_tokens,"output_tokens":u.completion_tokens,"cached_input_tokens":u.cached_tokens,"total_tokens":u.total_tokens})
}

/// The shared SS tool loop for HTTP APIs and native rounds. `run` drives
/// the provider with `model`; `native` rounds stream no text of their own.
pub fn tool_loop(
    job: &Job,
    model: &str,
    context: Option<&mut dyn Context>,
    event: &mut dyn FnMut(Event),
    secret: &str,
    native: bool,
    run: impl FnOnce(AnalyzeRequest<'_, '_>, &mut dyn FnMut(ApiEvent)) -> std::result::Result<AnalysisOutput, ApiError>,
) -> Reply {
    let mut handler = Tools { job, context, degraded: false, native };
    let effort = &job.route.effort;
    let prepared = &job.prepared;
    let request = AnalyzeRequest {
        model,
        reasoning_effort: (!effort.is_empty()).then_some(effort.as_str()),
        prompt: &prepared.prompt,
        schema: &prepared.schema,
        credential: secret,
        cancel: job.flag(),
        timeout: job.timeout(),
        tools: &prepared.definitions,
        tool_handler: &mut handler,
        input_limit: Some(job.config.max_input_chars),
        continuation_limit: Some(job.config.retrieval_budget_chars),
    };
    let trace = job.trace.as_ref();
    let output = run(request, &mut |e| match e {
        ApiEvent::Phase(phase) => event(Event::Stream { kind: "phase".into(), text: phase.into() }),
        ApiEvent::Text { kind, text } if !native => event(Event::Stream { kind: kind.into(), text }),
        ApiEvent::OutputTokens(count) if !native => event(Event::OutputTokens(count)),
        ApiEvent::StepFinished { output_tokens, decode_ms } if !native => {
            event(Event::StepFinished { output_tokens, decode_ms })
        }
        ApiEvent::Response(value) => record_response(trace, value, secret, false),
        ApiEvent::ToolRejected(value) => {
            if let Some(Err(error)) = job.session.as_ref().map(|s| s.record_rejection(&value)) {
                event(Event::Stream { kind: "diagnostic".into(), text: error.0 });
            }
            record_response(trace, value, secret, true);
        }
        _ => {}
    })
    .map_err(|e| {
        if matches!(e.kind, ApiErrorKind::InvalidJson | ApiErrorKind::SchemaInvalid) {
            event(Event::OutputValidationFailed);
        }
        if let Some(u) = &e.usage {
            let mut usage = usage_json(u);
            usage["partial"] = json!(true);
            event(Event::Usage(usage));
        }
        api_error(&e)
    })?;
    let usage = usage_json(&output.usage);
    event(Event::Usage(usage.clone()));
    let mut value = output.value;
    if handler.degraded {
        value["_analysis_degraded"] = json!(true);
    }
    if let Some(trace) = trace {
        trace.put("output.txt", value.to_string());
    }
    if let Some(context) = handler.context {
        context.finish(&mut value);
    }
    Ok((value, usage))
}

/// Public text of a failed API loop.
pub fn api_error(e: &ApiError) -> String {
    let guidance = match (e.kind, e.status) {
        (ApiErrorKind::Cancelled, _) => return super::CANCELLED.into(),
        (ApiErrorKind::InputLimit, _) => {
            return format!("{}：首轮完整请求超过输入额度，未调用模型", super::INPUT_LIMIT_ERROR)
        }
        (ApiErrorKind::HttpStatus, Some(400 | 422)) => {
            "；供应商拒绝请求参数，请核对模型、推理强度、工具及结构化输出支持。未自动丢弃参数，未应用本次结果"
        }
        (ApiErrorKind::Refusal, _) => "；供应商拒答，未应用本次结果；请查看本地调用详情并核对输入范围",
        (ApiErrorKind::HttpStatus, Some(504)) => {
            "；上游网关等待超时；延长本地 AI 总时限不能延长网关期限，未应用本次结果"
        }
        (ApiErrorKind::Timeout, _) => "；请求或网络等待超时，未应用本次结果，可检查网络与服务端状态后重试",
        (ApiErrorKind::SchemaInvalid, _) => "；模型未按完整格式返回，未应用本次结果，请换用支持结构化输出的模型或重试",
        _ => "",
    };
    let parameter = e.parameter.map(|p| format!("；被拒参数：{p}")).unwrap_or_default();
    format!("API 分析失败：{e}{parameter}{guidance}")
}

#[cfg(test)]
pub mod fake;

#[cfg(test)]
mod tests;
