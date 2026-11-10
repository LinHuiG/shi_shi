//! SSE assembly of one streamed reply, Chat Completions or Responses, into
//! the Chat message a non-streaming reply would carry. A server that answers
//! with plain JSON instead of SSE is accepted as is.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

pub enum ApiEvent {
    Phase(&'static str),
    /// Cumulative output across the analysis; unknown when a previous
    /// response did not report it.
    OutputTokens(Option<u64>),
    StepFinished {
        output_tokens: Option<u64>,
        decode_ms: Option<u64>,
    },
    Text {
        kind: &'static str,
        text: String,
    },
    /// Raw provider JSON for the local trace only.
    Response(Value),
    ToolRejected(Value),
    /// A final result sent back to the model for its one correction.
    OutputRejected(Value),
}

fn receiving_tool(event: &mut dyn FnMut(ApiEvent)) {
    event(ApiEvent::Text { kind: "tool", text: "正在接收工具参数".into() });
}

#[derive(Default)]
pub(crate) struct StreamCollector {
    pending: Vec<u8>,
    json: Option<bool>,
    data: String,
    text: [String; 3],
    tools: BTreeMap<u64, [String; 3]>,
    finish: Option<String>,
    usage: Option<Value>,
    output_base: Option<u64>,
    /// Responses events (`response.*`) instead of Chat chunks.
    responses: bool,
    done: bool,
    first_token: Option<Instant>,
}

impl StreamCollector {
    pub fn new(output_base: Option<u64>, responses: bool) -> Self {
        Self { output_base, responses, ..Self::default() }
    }

    pub fn push(&mut self, bytes: &[u8], event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        self.pending.extend_from_slice(bytes);
        if self.json.is_none() {
            match self.pending.iter().find(|b| !b.is_ascii_whitespace()) {
                Some(b) => self.json = Some(*b == b'{'),
                None => return Ok(()),
            }
        }
        if self.json == Some(true) {
            return Ok(());
        }
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = std::str::from_utf8(&line).map_err(|_| ())?.trim_end_matches(['\r', '\n']).to_owned();
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    self.accept(data.trim_end_matches('\n'), event)?;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                self.data.push('\n');
            }
        }
        Ok(())
    }

    fn accept(&mut self, data: &str, event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).map_err(|_| ())?;
        event(ApiEvent::Response(v.clone()));
        if self.done || !v["error"].is_null() {
            return Err(());
        }
        if self.responses {
            return self.response_event(&v, event);
        }
        let choice = &v["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.into());
        }
        if v["usage"].is_object() {
            self.usage = Some(v["usage"].clone());
            // A final usage chunk must never look like one second of output.
            if self.finish.is_none() {
                let current = v["usage"]["completion_tokens"].as_u64();
                event(ApiEvent::OutputTokens(self.output_base.zip(current).map(|(a, b)| a + b)));
            }
        }
        let delta = &choice["delta"];
        for (i, (key, kind)) in
            [("reasoning_content", "reasoning"), ("content", "content"), ("refusal", "refusal")].into_iter().enumerate()
        {
            self.text(i, kind, delta[key].as_str().unwrap_or(""), event);
        }
        if let Some(calls) = delta["tool_calls"].as_array().filter(|c| !c.is_empty()) {
            receiving_tool(event);
            self.first_token.get_or_insert_with(Instant::now);
            for call in calls {
                let entry = self.tools.entry(call["index"].as_u64().ok_or(())?).or_default();
                for (buffer, value) in
                    entry.iter_mut().zip([&call["id"], &call["function"]["name"], &call["function"]["arguments"]])
                {
                    buffer.push_str(value.as_str().unwrap_or(""));
                }
            }
        }
        Ok(())
    }

    fn text(&mut self, i: usize, kind: &'static str, text: &str, event: &mut dyn FnMut(ApiEvent)) {
        if !text.is_empty() {
            self.text[i].push_str(text);
            self.first_token.get_or_insert_with(Instant::now);
            event(ApiEvent::Text { kind, text: text.into() });
        }
    }

    /// One Responses event (magpie's `responses.go` reads the same set).
    fn response_event(&mut self, v: &Value, event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        let slot = match v["type"].as_str().unwrap_or("") {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => Some((0, "reasoning")),
            "response.output_text.delta" => Some((1, "content")),
            "response.refusal.delta" => Some((2, "refusal")),
            "response.function_call_arguments.delta" => {
                receiving_tool(event);
                None
            }
            "response.output_item.done" if v["item"]["type"] == "function_call" => {
                let item = &v["item"];
                let field = |k: &str| item[k].as_str().unwrap_or("").to_owned();
                self.tools.insert(self.tools.len() as u64, [field("call_id"), field("name"), field("arguments")]);
                None
            }
            kind @ ("response.completed" | "response.incomplete") => {
                let r = &v["response"];
                let u = &r["usage"];
                if u.is_object() {
                    self.usage = Some(json!({"prompt_tokens":u["input_tokens"],"completion_tokens":u["output_tokens"],
                        "total_tokens":u["total_tokens"],"prompt_tokens_details":{"cached_tokens":u["input_tokens_details"]["cached_tokens"]}}));
                }
                self.finish = Some(
                    match (kind, r["incomplete_details"]["reason"].as_str()) {
                        ("response.incomplete", Some("content_filter")) => "content_filter",
                        ("response.incomplete", _) => "length",
                        _ if !self.tools.is_empty() => "tool_calls",
                        _ => "stop",
                    }
                    .into(),
                );
                self.done = true;
                None
            }
            "response.failed" | "error" => return Err(()),
            _ => None,
        };
        if let Some((i, kind)) = slot {
            self.text(i, kind, v["delta"].as_str().unwrap_or(""), event);
        }
        Ok(())
    }

    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    /// The assembled reply; `Err` when the stream ended early.
    pub fn finish(self, event: &mut dyn FnMut(ApiEvent)) -> Result<Value, ()> {
        if self.json == Some(true) {
            let v: Value = serde_json::from_slice(&self.pending).map_err(|_| ())?;
            event(ApiEvent::Response(v.clone()));
            return Ok(v);
        }
        if !self.done || self.finish.is_none() || !self.data.is_empty() {
            return Err(());
        }
        let [reasoning, content, refusal] = self.text;
        let mut message = json!({"role":"assistant","content":content,"reasoning_content":reasoning});
        if !refusal.is_empty() {
            message["refusal"] = json!(refusal);
        }
        if !self.tools.is_empty() {
            message["tool_calls"] = self.tools.into_values()
                .map(|[id, name, arguments]| json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}))
                .collect();
        }
        event(ApiEvent::StepFinished {
            output_tokens: self.usage.as_ref().and_then(|u| u["completion_tokens"].as_u64()),
            decode_ms: self.first_token.map(|t| t.elapsed().as_millis() as u64),
        });
        Ok(json!({"choices":[{"message":message,"finish_reason":self.finish}],"usage":self.usage}))
    }
}

#[cfg(test)]
mod tests;
