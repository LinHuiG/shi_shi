//! Native model rounds (DSH) on the shared SS tool loop: the OpenAI-style
//! conversation is carried as one plain text frame per round ([`frame`]),
//! answered with the `{tool_call, result}` envelope ([`envelope`]); the loop
//! owns the conversation, tool validation and budgets.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{json, Value};
use shishi_api::json::chars as json_chars;
use shishi_api::openai::{analyze_with_model, ModelReply, ModelRound};
use shishi_api::{ApiError, ApiErrorKind, ApiEvent};

use super::provider::{self, Context, Event, Job, Reply};
use super::trace::Trace;
use super::CANCELLED;

/// One native round as the provider sees it; the job's deadline and cancel
/// flag bound it.
pub struct Round<'a> {
    pub prompt: &'a str,
    /// The envelope schema of this round.
    pub schema: &'a Value,
    /// `[{mimeType, data}]` attached to this round.
    pub images: &'a [Value],
    /// `round-NNNN/` of the request trace when traces are retained.
    pub trace: Option<&'a Trace>,
}

/// A provider able to answer one native round with `(envelope, usage)`.
pub trait Rounds {
    fn round(&mut self, job: &Job, round: Round<'_>, event: &mut dyn FnMut(Event)) -> Reply;
}

/// `previous + current` per counter (unknown stays unknown); a missing total
/// is input plus output.
fn usage_sum(previous: Option<&Value>, current: &Value) -> Value {
    let (input, output) = (current["input_tokens"].as_u64(), current["output_tokens"].as_u64());
    let total = current["total_tokens"].as_u64().or_else(|| input.zip(output).map(|(a, b)| a.saturating_add(b)));
    let mut value = json!({"input_tokens":input,"output_tokens":output,"cached_input_tokens":current["cached_input_tokens"].as_u64(),"total_tokens":total});
    for key in ["input_tokens", "output_tokens", "cached_input_tokens", "total_tokens"] {
        if let Some(previous) = previous {
            value[key] = json!(previous[key].as_u64().zip(value[key].as_u64()).map(|(a, b)| a.saturating_add(b)));
        }
    }
    value
}

struct Native<'a, 'e> {
    job: &'a Job,
    rounds: &'a mut dyn Rounds,
    events: &'a RefCell<&'e mut dyn FnMut(Event)>,
    counter: usize,
    completed_usage: Option<Value>,
    observed_usage: Option<Value>,
    error: Option<String>,
    id: String,
}

impl Native<'_, '_> {
    fn emit(&self, event: Event) {
        (self.events.borrow_mut())(event);
    }

    /// `Err` when the job was cancelled or its deadline passed.
    fn alive(&self) -> Result<(), String> {
        match () {
            _ if self.job.flag().load(Ordering::SeqCst) => Err(CANCELLED.into()),
            _ if Instant::now() >= self.job.deadline => Err("AI 总时限已到".into()),
            _ => Ok(()),
        }
    }

    fn round(&mut self, body: &Value, output_base: Option<u64>) -> Result<ModelReply, String> {
        self.alive()?;
        let schema = envelope(&self.job.prepared.schema, &body["tools"]);
        let (prompt, images) = frame(body);
        self.counter += 1;
        let round = self.counter;
        let trace = self.job.trace.as_ref().map(|t| t.round(round));
        if let Some(trace) = &trace {
            trace.put("input.txt", prompt.as_str());
            trace.put("schema.json", schema.to_string());
        }
        let (completed, events) = (self.completed_usage.clone(), self.events);
        let mut observed = None;
        let mut forward = |event: Event| match event {
            Event::Usage(value) => {
                let combined = usage_sum(completed.as_ref(), &value);
                observed = Some(combined.clone());
                (events.borrow_mut())(Event::Usage(combined));
            }
            Event::OutputTokens(value) => {
                (events.borrow_mut())(Event::OutputTokens(output_base.zip(value).map(|(a, b)| a.saturating_add(b))))
            }
            event => (events.borrow_mut())(event),
        };
        let result = self.rounds.round(
            self.job,
            Round { prompt: &prompt, schema: &schema, images: &images, trace: trace.as_ref() },
            &mut forward,
        );
        if observed.is_some() {
            self.observed_usage = observed;
        }
        self.alive()?;
        let (value, usage) = result?;
        let combined = usage_sum(self.completed_usage.as_ref(), &usage);
        self.completed_usage = Some(combined.clone());
        self.observed_usage = Some(combined.clone());
        self.emit(Event::Usage(combined));
        if !value.as_object().is_some_and(|v| v.len() == 2 && v.contains_key("tool_call") && v.contains_key("result")) {
            return Err("模型未返回有效的结构化调用或最终结果".into());
        }
        let (call, result) = (&value["tool_call"], &value["result"]);
        if !call.is_null() && !call.is_object() {
            return Err("工具请求必须是对象".into());
        }
        let step = usage_sum(None, &usage);
        let usage = json!({"prompt_tokens":step["input_tokens"],"completion_tokens":step["output_tokens"],"total_tokens":step["total_tokens"],
            "prompt_tokens_details":{"cached_tokens":usage["cached_input_tokens"]}});
        let reply = |message, reason: &str| {
            ModelReply::from_message(message, Some(reason.into()), Some(usage.clone())).map_err(|e| e.to_string())
        };
        if call.is_null() == result.is_null() {
            // Rejected by the loop with the error; never executable.
            let error = "模型必须返回一次工具请求或最终结果，不能同时返回或全部留空；本次封装未执行工具，也未采用结果";
            return Ok(reply(json!({"role":"assistant","content":value.to_string()}), "stop")?.with_output_error(error));
        }
        if call.is_object() {
            let call = json!({"id":format!("text:{}:{round}",self.id),"type":"function","function":{"name":call["name"],"arguments":call["arguments"]}});
            return reply(json!({"role":"assistant","content":null,"tool_calls":[call]}), "tool_calls");
        }
        reply(json!({"role":"assistant","content":result.to_string()}), "stop")
    }
}

impl ModelRound for Native<'_, '_> {
    fn complete(
        &mut self,
        body: &Value,
        _: &str,
        _: Instant,
        _: &AtomicBool,
        output_base: Option<u64>,
        _: &mut dyn FnMut(ApiEvent),
    ) -> Result<ModelReply, ApiError> {
        self.round(body, output_base).map_err(|error| {
            self.error = Some(error);
            ApiError::simple(ApiErrorKind::MalformedResponse)
        })
    }
}

/// Run `job` through native rounds; `label` replaces "API" in shared loop
/// failures ("DSH").
pub fn analyze(
    job: &Job,
    context: Option<&mut dyn Context>,
    event: &mut dyn FnMut(Event),
    label: &str,
    rounds: &mut dyn Rounds,
) -> Reply {
    let events = RefCell::new(event);
    let id = uuid::Uuid::new_v4().to_string();
    let mut model = Native {
        job,
        rounds,
        events: &events,
        counter: 0,
        completed_usage: None,
        observed_usage: None,
        error: None,
        id,
    };
    let result =
        provider::tool_loop(job, &job.route.model, context, &mut |e| (events.borrow_mut())(e), "", true, |r, e| {
            analyze_with_model(r, &mut model, e)
        });
    result.map_err(|error| {
        if let Some(mut usage) = model.observed_usage.take() {
            usage["partial"] = json!(true);
            (events.borrow_mut())(Event::Usage(usage));
        }
        model.error.take().unwrap_or_else(|| error.replacen("API 分析失败", &format!("{label} 分析失败"), 1))
    })
}

/// Output contract of one native round: either one tool call or the final
/// business result.
pub(crate) fn envelope(schema: &Value, tools: &Value) -> Value {
    let names = tools.as_array().into_iter().flatten().map(|t| t["function"]["name"].clone()).collect::<Vec<_>>();
    let calls = if names.is_empty() {
        json!({"type":"null"})
    } else {
        json!({"type":["object","null"],"additionalProperties":false,"properties":{
        "name":{"type":"string","enum":names},"arguments":{"type":"string"}},"required":["name","arguments"]})
    };
    // Business results are objects; reuse the nullable type the shared
    // validator already supports.
    let mut result = schema.clone();
    result["type"] = json!(["object", "null"]);
    json!({"type":"object","additionalProperties":false,"properties":{
        "tool_call":calls,"result":result},"required":["tool_call","result"]})
}

/// Text frame of one round and the attached images (`{mimeType, data}`).
pub(crate) fn frame(body: &Value) -> (String, Vec<Value>) {
    let mut messages = body["messages"].clone();
    let mut images = vec![];
    for message in messages.as_array_mut().into_iter().flatten() {
        for content in message["content"].as_array_mut().into_iter().flatten() {
            if content["type"] != "image_url" {
                continue;
            }
            if let Some((mime, data)) = content["image_url"]["url"]
                .as_str()
                .and_then(|u| u.strip_prefix("data:"))
                .and_then(|s| s.split_once(";base64,"))
            {
                images.push(json!({"mimeType":mime,"data":data}));
            }
            *content =
                json!({"type":"text","text":"SS has attached this read-only image to the current model request."});
        }
    }
    (
        format!(
            "{}\n\n工具目录：{}\n\n本次对话：{}",
            crate::prompts::default("model_api_protocol"),
            body["tools"],
            messages
        ),
        images,
    )
}

/// Query messages keep their ordinary JSON framing for accounting; the
/// remaining command history matches the native text frame.
pub(crate) fn body_input_chars(schema: &Value, body: &Value) -> usize {
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let message_chars = 2
        + messages.iter().map(shishi_api::openai::message_input_chars).sum::<usize>()
        + messages.len().saturating_sub(1);
    crate::prompts::default("model_api_protocol").chars().count()
        + "\n\n工具目录：\n\n本次对话：".chars().count()
        + json_chars(&body["tools"])
        + message_chars
        + json_chars(&envelope(schema, &body["tools"]))
}
