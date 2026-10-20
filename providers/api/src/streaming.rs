//! Bounded SSE assembly. Tool arguments remain inert until the complete reply
//! has passed the caller's tool-name and schema validation.
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub enum ApiEvent {
    Phase(&'static str),
    Text {
        kind: &'static str,
        text: String,
        append: bool,
    },
    /// Explicit local trace callback only; never formatted into public errors.
    Response(Value),
    ToolRejected(Value),
}

#[derive(Default)]
pub(crate) struct StreamCollector {
    pending: Vec<u8>,
    received_bytes: usize,
    json_mode: Option<bool>,
    data: Vec<String>,
    content: String,
    reasoning: String,
    refusal: String,
    tools: BTreeMap<u64, Value>,
    finish: Option<String>,
    usage: Option<Value>,
    done: bool,
}
impl StreamCollector {
    pub fn push(&mut self, bytes: &[u8], event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        self.received_bytes = self.received_bytes.saturating_add(bytes.len());
        if self.received_bytes > super::openai::DEFAULT_MAX_RESPONSE_BYTES {
            return Err(());
        }
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > super::openai::DEFAULT_MAX_RESPONSE_BYTES {
            return Err(());
        }
        if self.json_mode.is_none() {
            if let Some(b) = self.pending.iter().find(|b| !b.is_ascii_whitespace()) {
                self.json_mode = Some(*b == b'{');
            }
        }
        if self.json_mode != Some(false) {
            return Ok(());
        }
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .map_err(|_| ())?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data).join("\n");
                    self.accept(&data, event)?;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
        }
        Ok(())
    }
    fn accept(&mut self, data: &str, event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        if self.done {
            return Err(());
        }
        let v: Value = serde_json::from_str(data).map_err(|_| ())?;
        event(ApiEvent::Response(v.clone()));
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(());
        }
        if v["usage"].is_object() {
            self.usage = Some(v["usage"].clone());
        }
        let choices = v["choices"].as_array().ok_or(())?;
        if choices.is_empty() {
            return Ok(());
        }
        if choices.len() != 1 || choices[0]["index"].as_u64().unwrap_or(0) != 0 {
            return Err(());
        }
        let c = &choices[0];
        if let Some(reason) = c["finish_reason"].as_str() {
            self.finish = Some(reason.to_owned());
        }
        let delta = &c["delta"];
        for (key, kind, target) in [
            ("reasoning_content", "reasoning", &mut self.reasoning),
            ("content", "content", &mut self.content),
            ("refusal", "refusal", &mut self.refusal),
        ] {
            if let Some(text) = delta[key].as_str() {
                target.push_str(text);
                if !text.is_empty() {
                    event(ApiEvent::Text {
                        kind,
                        text: text.to_owned(),
                        append: true,
                    });
                }
            } else if !delta[key].is_null() {
                return Err(());
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            if !calls.is_empty() {
                event(ApiEvent::Text {
                    kind: "tool",
                    text: "正在接收工具参数".into(),
                    append: false,
                });
            }
            if calls.len() > super::openai::MAX_TOOL_CALLS_PER_RESPONSE {
                return Err(());
            }
            for call in calls {
                let index = call["index"].as_u64().ok_or(())?;
                if index >= super::openai::MAX_TOOL_CALLS_PER_RESPONSE as u64 {
                    return Err(());
                }
                let entry = self.tools.entry(index).or_insert_with(
                    || json!({"id":"","type":"function","function":{"name":"","arguments":""}}),
                );
                for key in ["id", "type"] {
                    if let Some(value) = call[key].as_str() {
                        if key == "type" {
                            if value != "function" {
                                return Err(());
                            }
                        } else {
                            let old = entry[key].as_str().unwrap_or("");
                            entry[key] = json!(format!("{old}{value}"));
                        }
                    }
                }
                for key in ["name", "arguments"] {
                    if let Some(value) = call["function"][key].as_str() {
                        let old = entry["function"][key].as_str().unwrap_or("");
                        if old.len() + value.len() > super::openai::MAX_TOOL_ARGUMENT_BYTES {
                            return Err(());
                        }
                        entry["function"][key] = json!(format!("{old}{value}"));
                    }
                }
            }
        }
        Ok(())
    }
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }
    pub fn finish(mut self, event: &mut dyn FnMut(ApiEvent)) -> Result<Value, ()> {
        if self.json_mode == Some(true) {
            let v: Value = serde_json::from_slice(&self.pending).map_err(|_| ())?;
            event(ApiEvent::Response(v.clone()));
            return Ok(v);
        }
        if !self.pending.iter().all(u8::is_ascii_whitespace)
            || !self.data.is_empty()
            || !self.done
            || self.finish.is_none()
        {
            return Err(());
        }
        let mut message =
            json!({"role":"assistant","content":self.content,"reasoning_content":self.reasoning});
        if !self.refusal.is_empty() {
            message["refusal"] = json!(self.refusal);
        }
        if !self.tools.is_empty() {
            message["tool_calls"] = json!(std::mem::take(&mut self.tools)
                .into_values()
                .collect::<Vec<_>>());
        }
        Ok(json!({"choices":[{"message":message,"finish_reason":self.finish}],"usage":self.usage}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_utf8_multiline_heartbeat_and_final_usage() {
        let wire = ": ping\r\n\r\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"核对\"}}]}\r\n\r\ndata: {\"choices\":[\ndata: {\"delta\":{\"content\":\"{}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"total_tokens\":9}}\n\ndata: [DONE]\n\n";
        let mut collector = StreamCollector::default();
        let mut text = String::new();
        for b in wire.as_bytes() {
            collector
                .push(&[*b], &mut |e| {
                    if let ApiEvent::Text { text: t, .. } = e {
                        text.push_str(&t)
                    }
                })
                .unwrap();
        }
        let v = collector.finish(&mut |_| {}).unwrap();
        assert_eq!(text, "核对{}");
        assert_eq!(v["usage"]["total_tokens"], 9);
        assert_eq!(v["choices"][0]["message"]["reasoning_content"], "核对");
    }
    #[test]
    fn tool_fragments_are_assembled_by_index_and_disconnect_is_rejected() {
        let mut c = StreamCollector::default();
        for delta in [
            json!({"tool_calls":[{"index":1,"id":"b","function":{"name":"read_image","arguments":"{\"image_id\":"}},{"index":0,"id":"a","function":{"name":"read_context","arguments":"{"}}]}),
            json!({"tool_calls":[{"index":0,"function":{"arguments":"}"}},{"index":1,"function":{"arguments":"\"fixture\"}"}}]}),
        ] {
            c.push(
                format!("data: {}\n\n", json!({"choices":[{"delta":delta}]})).as_bytes(),
                &mut |_| {},
            )
            .unwrap();
        }
        c.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",&mut |_|{}).unwrap();
        let v = c.finish(&mut |_| {}).unwrap();
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        let mut c = StreamCollector::default();
        c.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"{}\"}}]}\n\n",
            &mut |_| {},
        )
        .unwrap();
        assert!(c.finish(&mut |_| {}).is_err());
    }
}
