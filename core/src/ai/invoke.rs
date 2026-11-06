//! One prepared request through the provider: final budget check,
//! reservation, `ai_start`, the driver call, usage accounting and the
//! `ai_success` / `ai_failed` / `ai_cancelled` log.
//!
//! Interface for the other domains (analysis engine, archive, handling,
//! task edits, prompt review):
//!
//! 1. hold the AI slot ([`crate::exec::Executor::try_call`] /
//!    `wait_call` / `run_ai`) or a manual request (`run_manual`); the
//!    request id is the one of the [`Cancel`] passed in;
//! 2. [`build`] (or, with policies already read, [`render`]) the
//!    [`Invocation`] for a purpose (`kind`), mode, trigger and payload,
//!    then set the optional parts (retrieval [`Context`], registered
//!    tools, reservation hook). A layer that fits a domain request keeps
//!    the render in a [`Measured`] and sends exactly that one;
//! 3. [`invoke`] sends it. `Err` is already logged and accounted; `Ok`
//!    gives an [`Outcome`] with the raw JSON reply;
//! 4. apply the domain result, then [`Outcome::accept`], or
//!    [`Outcome::reject`] with the [`Stage`] that failed (evidence id
//!    decoding is `Validation`, writing domain rows is `Persistence`).
//!
//! [`call`] does 2–4 for callers without domain finalization.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shishi_api::json::chars as json_chars;

use super::prompt::{self, Policies, Prepared, Route};
use super::provider::{self, Context, Done, Event, Job};
use super::trace::{self, Trace};
use super::{usage, Mode, CANCELLED};
use crate::exec::{Cancel, Live};
use crate::tools::session::Session;
use crate::tools::Tool;
use crate::{time, Config, Ctx, Error, Result};

/// One fully prepared request; see the module documentation.
pub struct Invocation {
    pub kind: String,
    pub mode: Mode,
    pub trigger: String,
    pub payload: Value,
    /// The settings the request was rendered with; a later edit cannot
    /// change an accepted request.
    pub config: Arc<Config>,
    pub route: Route,
    pub prepared: Prepared,
    pub runtime: Value,
    pub context: Option<Box<dyn Context>>,
    /// Registered SS tools frozen for this request; a tool run is opened
    /// when not empty.
    pub tools: Vec<Tool>,
    pub tool_task: String,
    /// The filter decision a classify call takes, recorded inside the
    /// reservation transaction after the call is counted.
    pub reserve: Option<crate::analysis::FilterTaken>,
}

/// Render and measure `payload` for `kind` with `policies`: the request
/// preparation of every call layer. No route gives the `AI_UNCONFIGURED:`
/// error ([`unconfigured`] logs it).
pub fn render(
    config: Arc<Config>,
    policies: &Policies,
    kind: &str,
    mode: Mode,
    trigger: &str,
    payload: Value,
) -> Result<Invocation> {
    let route = prompt::route(&config, kind)?;
    let runtime = prompt::runtime_context(&config, kind, trigger, &payload, time::now());
    let prepared = prompt::prepare(policies, &route, &payload, &runtime)?;
    let (kind, trigger) = (kind.into(), trigger.into());
    Ok(Invocation {
        kind,
        mode,
        trigger,
        payload,
        config,
        route,
        prepared,
        runtime,
        context: None,
        tools: vec![],
        tool_task: String::new(),
        reserve: None,
    })
}

/// Log `error` when it is the unconfigured-purpose error of `kind`.
pub async fn unconfigured(ctx: &Ctx, kind: &str, error: &Error) -> Result<()> {
    if !error.0.starts_with(super::UNCONFIGURED_PREFIX) {
        return Ok(());
    }
    ctx.db.log_ai_unconfigured(kind.to_string(), error.0.clone()).await
}

/// [`render`] with the adopted policies of `kind`; an unconfigured purpose
/// is logged.
pub async fn build(ctx: &Ctx, kind: &str, mode: Mode, trigger: &str, payload: Value) -> Result<Invocation> {
    let policies = ctx.db.ai_policy(kind.to_string()).await?;
    let rendered = render(ctx.config.get(), &policies, kind, mode, trigger, payload);
    if let Err(e) = &rendered {
        unconfigured(ctx, kind, e).await?;
    }
    rendered
}

/// A domain request measured by its call layer. The layer keeps its render
/// here, so the request a domain chose by fitting is sent as measured.
pub struct Measured<R> {
    pub request: R,
    /// Characters of the rendered request ([`Prepared::total`]).
    pub total: usize,
    rendered: Option<Box<dyn Any + Send>>,
}

impl<R> Measured<R> {
    /// A measurement without a render (models that render nothing).
    pub fn new(request: R, total: usize) -> Self {
        Self { request, total, rendered: None }
    }

    pub fn rendered<T: Any + Send>(request: R, total: usize, rendered: T) -> Self {
        Self { request, total, rendered: Some(Box::new(rendered)) }
    }

    /// The layer's render; an error when the request was measured elsewhere.
    pub fn take<T: Any>(&mut self) -> Result<T> {
        let rendered = self.rendered.take().and_then(|r| r.downcast::<T>().ok());
        rendered.map(|r| *r).ok_or_else(|| Error::new("请求未经本调用层测量"))
    }
}

/// A received reply waiting for the caller's domain finalization.
pub struct Outcome {
    pub value: Value,
    pub usage: Value,
    pub request_id: String,
    /// The logged detail; callers may add fields before accepting.
    pub detail: Value,
    session: Option<Session>,
    trace: Option<Trace>,
}

/// Where a received reply failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Decoding or checking the reply (evidence ids, contract).
    Validation,
    /// Writing the domain result.
    Persistence,
}

impl Outcome {
    /// Log `ai_success` and close the tool run.
    pub async fn accept(self, ctx: &Ctx) -> Result<Value> {
        phase(ctx, &self.request_id, "分析完成");
        ctx.db.log("ai_success", self.detail).await?;
        if let Some(session) = self.session {
            let usage = self.usage.clone();
            crate::exec::blocking(move || session.finish(None, &usage)).await?;
        }
        Ok(self.value)
    }

    /// Log the failed finalization; returns the error to propagate.
    pub async fn reject(self, ctx: &Ctx, error: Error, stage: Stage) -> Error {
        let class = (stage == Stage::Validation).then_some("response_validation");
        let stage = match stage {
            Stage::Validation => "response_validation",
            Stage::Persistence => "result_persistence",
        };
        fail(ctx, &self.request_id, self.trace.as_ref(), self.detail, &error.0, stage, class).await;
        if let Some(session) = self.session {
            let (usage, text) = (self.usage, error.0.clone());
            let _ = crate::exec::blocking(move || session.finish(Some(&text), &usage)).await;
        }
        error
    }
}

/// [`invoke`] and accept without domain finalization.
pub async fn call(ctx: &Arc<Ctx>, invocation: Invocation, cancel: &Cancel) -> Result<Value> {
    invoke(ctx, invocation, cancel).await?.accept(ctx).await
}

/// What the provider thread observed besides the reply.
#[derive(Default)]
struct Observed {
    usage: Value,
    thread: String,
    invalid: bool,
}

/// Send one invocation on the slot the caller holds.
pub async fn invoke(ctx: &Arc<Ctx>, invocation: Invocation, cancel: &Cancel) -> Result<Outcome> {
    let Invocation {
        kind,
        mode,
        trigger,
        payload,
        config,
        route,
        prepared,
        runtime,
        context,
        tools,
        tool_task,
        reserve,
    } = invocation;
    let request_id = cancel.request_id();
    if cancel.is_cancelled() || request_id.is_empty() {
        crate::bail!("{CANCELLED}");
    }
    let source = prompt::source_info(&route);
    if let Err(error) = prompt::validate_prepared(&config, &payload, &prepared) {
        let detail = json!({"request_id": request_id, "kind": kind, "source": source, "error": error.0});
        ctx.db.log("ai_rejected", detail).await?;
        return Err(error);
    }
    let driver = provider::driver(ctx, &route.connection)?;
    let attempt = payload["_archive_attempt_id"].as_str().map(str::to_string);
    ctx.db.reserve_ai_call(config.clone(), kind.clone(), mode, attempt, reserve).await?;

    let messages = payload["changes"].as_array().map_or(0, Vec::len);
    let start = time::now();
    ctx.db
        .record_activity(
            "ai_start",
            json!({"request_id": request_id, "kind": kind, "source": source, "trigger": trigger, "model": route.model,
            "reasoning_effort": route.effort, "messages": messages, "batch": payload["_batch_metrics"],
            "runtime": runtime, "input_chars": json_chars(&prompt::public_payload(&payload)),
            "input_sizes": prompt::input_sizes(&config, &prepared)}),
        )
        .await;

    let trace = config.retain_ai_traces.then(|| Trace::new(start));
    let session = (!tools.is_empty()).then(|| Session::new(ctx, &request_id, &kind, &tool_task, tools, source.clone()));
    let observed = Arc::new(Mutex::new(Observed::default()));
    if let Some(trace) = &trace {
        trace::request(trace, &route, &prepared);
        save(ctx, &request_id, trace).await;
    }
    let events = {
        let (ctx, request_id, observed, trace) = (ctx.clone(), request_id.clone(), observed.clone(), trace.clone());
        Box::new(move |event: Event| on_event(&ctx, &request_id, start, trace.as_ref(), &observed, event))
    };
    // The API key is read here, not by the driver; a missing one fails
    // like a provider error without sending.
    let key = match route.connection.driver.as_str() {
        "api" => Some(super::api::key(&ctx.db, &route.connection).await),
        _ => None,
    };
    let Done { mut reply, session } = match key.transpose() {
        Err(e) => Done { reply: Err(e), session },
        Ok(key) => {
            let deadline = Instant::now() + Duration::from_secs(config.ai_timeout_seconds);
            let (kind, route, config, root, trace, cancel) =
                (kind.clone(), route.clone(), config.clone(), ctx.root.clone(), trace.clone(), cancel.clone());
            driver
                .send(Job {
                    kind,
                    route,
                    config,
                    prepared,
                    session,
                    root,
                    trace,
                    key,
                    cancel,
                    deadline,
                    context,
                    events,
                })
                .await
        }
    };
    phase(ctx, &request_id, if reply.is_ok() { "校验结果" } else { "请求失败" });
    let response_received = reply.is_ok();
    let observed = std::mem::take(&mut *observed.lock().unwrap());
    let usage = Some(observed.usage)
        .filter(Value::is_object)
        .or_else(|| reply.as_ref().ok().map(|(_, u)| u.clone()))
        .unwrap_or_default();
    let known = usage::known(&usage);
    if known {
        if let Err(e) = ctx.db.record_ai_usage(kind.clone(), usage.clone()).await {
            reply = Err(format!("AI usage persistence failed: {}", e.0));
        }
    }
    if cancel.is_cancelled() {
        reply = Err(CANCELLED.into());
    }
    let elapsed_ms = (time::now() - start).max(0);
    let mut live = Live::default();
    ctx.exec.live_for(&request_id, |l, _| {
        l.output_tokens = if usage["partial"] != true { usage["output_tokens"].as_u64().unwrap_or(0) } else { 0 };
        l.usage_elapsed_ms = elapsed_ms;
        l.usage_final = true;
        l.revision += 1;
        live = l.clone();
    });
    let mut detail = json!({"request_id": request_id, "kind": kind, "source": source, "trigger": trigger,
        "model": route.model, "reasoning_effort": route.effort, "elapsed_ms": elapsed_ms,
        "first_response_ms": live.first_response_ms, "generation": generation(&live), "usage": usage,
        "batch": payload["_batch_metrics"], "thread_id": observed.thread,
        "usage_status": usage::status(&usage, reply.is_err()), "response_received": response_received});
    match reply {
        Ok((mut value, _)) => {
            detail["context_usage"] = value["_context_usage"].clone();
            detail["analysis_degraded"] = value["_analysis_degraded"].clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("_tool_run_id");
            }
            if let Some(session) = &session {
                if !ctx.db.tool_calls(session.run_id.clone()).await?.is_empty() {
                    value["_tool_run_id"] = json!(request_id);
                }
            }
            if let Some(trace) = &trace {
                save(ctx, &request_id, trace).await;
            }
            Ok(Outcome { value, usage, request_id, detail, session, trace })
        }
        Err(error) => {
            let class = observed.invalid.then_some("response_validation");
            fail(ctx, &request_id, trace.as_ref(), detail, &error, "provider", class).await;
            if let Some(session) = session {
                let text = error.clone();
                let _ = crate::exec::blocking(move || session.finish(Some(&text), &usage)).await;
            }
            Err(Error(error))
        }
    }
}

fn generation(live: &Live) -> Value {
    if live.generation.is_null() {
        json!({"output_tokens": 0, "elapsed_ms": 0, "steps": 0})
    } else {
        live.generation.clone()
    }
}

async fn fail(
    ctx: &Ctx,
    request_id: &str,
    trace: Option<&Trace>,
    mut detail: Value,
    error: &str,
    stage: &str,
    class: Option<&'static str>,
) {
    let received = detail["response_received"] == true;
    let text = match () {
        _ if error == CANCELLED => "已中断",
        _ if super::failure_class(error) == "provider_refusal" => "供应商拒答",
        _ if received => "结果处理失败",
        _ => "请求失败",
    };
    phase(ctx, request_id, text);
    if let Some(trace) = trace {
        trace.put("error.txt", error);
        save(ctx, request_id, trace).await;
    }
    detail["failure_stage"] = json!(if received { stage } else { "provider" });
    detail["error"] = json!(error);
    detail["failure_class"] = json!(class.unwrap_or_else(|| super::failure_class(error)));
    let kind = if error == CANCELLED { "ai_cancelled" } else { "ai_failed" };
    let _ = ctx.db.log_ai_failure(kind, detail, error.to_string()).await;
}

/// Save the trace's new entries; a failure is only a diagnostic.
async fn save(ctx: &Ctx, request_id: &str, trace: &Trace) {
    if let Err(e) = trace::save(&ctx.db, request_id, trace).await {
        diagnostic(ctx, request_id, &format!("分析跟踪保存失败，继续本次分析：{}", e.0));
    }
}

fn phase(ctx: &Ctx, request_id: &str, text: &str) {
    ctx.exec.live_for(request_id, |l, _| {
        l.phase = text.into();
        l.revision += 1;
    });
}

fn diagnostic(ctx: &Ctx, request_id: &str, text: &str) {
    let detail = json!({"request_id": request_id, "text": text});
    let _ = ctx.db.log_blocking("ai_diagnostic", detail);
    accept(ctx, request_id, "diagnostic", text);
}

/// Streamed text into the live view; only reasoning and content count as
/// the first response.
fn accept(ctx: &Ctx, request_id: &str, kind: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    ctx.exec.live_for(request_id, |l, started| {
        if kind == "phase" {
            l.phase = text.into();
            l.revision += 1;
            return;
        }
        let first = l.first_response_ms;
        l.push(kind, text, started);
        if !matches!(kind, "reasoning" | "content") {
            l.first_response_ms = first;
        }
        l.phase = (match kind {
            "reasoning" => "正在思考",
            "tool" => "读取上下文",
            _ => "接收结果",
        })
        .into();
    });
}

fn on_event(ctx: &Ctx, request_id: &str, start: i64, trace: Option<&Trace>, observed: &Mutex<Observed>, event: Event) {
    match event {
        Event::SessionStarted(thread) => observed.lock().unwrap().thread = thread,
        Event::OutputValidationFailed => observed.lock().unwrap().invalid = true,
        Event::Usage(usage) => observed.lock().unwrap().usage = usage,
        Event::OutputTokens(count) => {
            let elapsed = (time::now() - start).max(0);
            ctx.exec.live_for(request_id, |l, _| {
                if !l.usage_final {
                    l.output_tokens = count.unwrap_or(0);
                    l.usage_elapsed_ms = elapsed;
                    l.revision += 1;
                }
            });
        }
        Event::StepFinished { output_tokens, decode_ms } => {
            let (Some(tokens), Some(ms)) = (output_tokens, decode_ms.filter(|ms| *ms > 0)) else {
                return;
            };
            ctx.exec.live_for(request_id, |l, _| {
                if l.usage_final {
                    return;
                }
                let mut g = generation(l);
                g["output_tokens"] = json!(g["output_tokens"].as_u64().unwrap_or(0).saturating_add(tokens));
                g["elapsed_ms"] = json!(g["elapsed_ms"].as_u64().unwrap_or(0).saturating_add(ms));
                g["steps"] = json!(g["steps"].as_u64().unwrap_or(0).saturating_add(1));
                l.generation = g;
                l.revision += 1;
            });
        }
        Event::Stream { kind, text } => {
            if kind == "diagnostic" {
                diagnostic(ctx, request_id, &text);
                return;
            }
            accept(ctx, request_id, &kind, &text);
            if let (Some(trace), "phase") = (trace, kind.as_str()) {
                trace.event(&json!({"type": "phase", "text": text}));
            }
        }
        Event::VisibilityIssue { thread_id, error } => {
            let detail = json!({"type": "codex.visibility", "thread_id": thread_id, "error": error});
            let _ = ctx.db.log_blocking("codex_visibility", detail);
        }
    }
}
