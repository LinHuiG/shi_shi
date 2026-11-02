//! Bounded SSE assembly. Tool arguments remain inert until the complete reply
//! has passed the caller's tool-name and schema validation.
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

pub enum ApiEvent {
    Phase(&'static str),
    /// Reported cumulative output across this analysis, excluding terminal-only
    /// usage. Unknown when a preceding response did not report output usage.
    OutputTokens(Option<u64>),
    /// One assembled response, with provider usage and measured decode time.
    StepFinished {
        output_tokens: Option<u64>,
        decode_ms: Option<u64>,
    },
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
struct ToolBuffer {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(crate) struct StreamCollector {
    pending: Vec<u8>,
    frame_bytes: usize,
    assembled_bytes: usize,
    json_mode: Option<bool>,
    data: String,
    content: String,
    reasoning: String,
    refusal: String,
    tools: BTreeMap<u64, ToolBuffer>,
    finish: Option<String>,
    usage: Option<Value>,
    output_base: Option<u64>,
    done: bool,
    first_token: Option<Instant>,
}
impl StreamCollector {
    pub fn with_output_base(output_base: Option<u64>) -> Self {
        Self {
            output_base,
            ..Self::default()
        }
    }
    pub fn push(&mut self, mut bytes: &[u8], event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        if self.json_mode.is_none() {
            let prefix = bytes
                .iter()
                .position(|b| !b.is_ascii_whitespace())
                .unwrap_or(bytes.len());
            // Empty keepalive lines do not accumulate while format detection
            // waits for the first payload. A single unfinished line is bounded.
            self.push_sse(&bytes[..prefix], event)?;
            bytes = &bytes[prefix..];
            let Some(first) = bytes.first() else {
                return Ok(());
            };
            self.json_mode = Some(*first == b'{');
        }
        if self.json_mode == Some(true) {
            if self.pending.len().saturating_add(bytes.len())
                > super::openai::DEFAULT_MAX_RESPONSE_BYTES
            {
                return Err(());
            }
            self.pending.extend_from_slice(bytes);
            return Ok(());
        }
        self.push_sse(bytes, event)
    }
    fn push_sse(&mut self, mut bytes: &[u8], event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        // Consume complete lines directly from the input slice. Keep only an
        // unfinished line, rather than copying and draining the whole stream.
        while !bytes.is_empty() {
            let length = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |end| end + 1);
            let part = &bytes[..length];
            self.frame_bytes = self.frame_bytes.saturating_add(length);
            if self.frame_bytes > super::openai::DEFAULT_MAX_RESPONSE_BYTES {
                return Err(());
            }
            if self.pending.is_empty() && part.ends_with(b"\n") {
                self.line(part, event)?;
            } else {
                self.pending.extend_from_slice(part);
                if part.ends_with(b"\n") {
                    let mut line = std::mem::take(&mut self.pending);
                    self.line(&line, event)?;
                    line.clear();
                    self.pending = line;
                }
            }
            bytes = &bytes[length..];
        }
        Ok(())
    }
    fn line(&mut self, bytes: &[u8], event: &mut dyn FnMut(ApiEvent)) -> Result<(), ()> {
        let line = std::str::from_utf8(bytes)
            .map_err(|_| ())?
            .trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            self.frame_bytes = 0;
            if !self.data.is_empty() {
                let mut data = std::mem::take(&mut self.data);
                data.pop(); // Remove only the separator after the final data line.
                self.accept(&data, event)?;
                data.clear();
                self.data = data;
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            self.data.push('\n');
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
        if !choices.is_empty()
            && (choices.len() != 1 || choices[0]["index"].as_u64().unwrap_or(0) != 0)
        {
            return Err(());
        }
        if let Some(reason) = choices.first().and_then(|c| c["finish_reason"].as_str()) {
            self.finish = Some(reason.to_owned());
        }
        if v["usage"].is_object() {
            // A final usage chunk must never look like one second of output.
            if self.finish.is_none() {
                event(ApiEvent::OutputTokens(
                    self.output_base
                        .zip(v["usage"]["completion_tokens"].as_u64())
                        .and_then(|(base, current)| base.checked_add(current)),
                ));
            }
        }
        if choices.is_empty() {
            return Ok(());
        }
        let c = &choices[0];
        let delta = &c["delta"];
        for (key, kind, target) in [
            ("reasoning_content", "reasoning", &mut self.reasoning),
            ("content", "content", &mut self.content),
            ("refusal", "refusal", &mut self.refusal),
        ] {
            if let Some(text) = delta[key].as_str() {
                self.assembled_bytes = self.assembled_bytes.saturating_add(text.len());
                if self.assembled_bytes > super::openai::DEFAULT_MAX_RESPONSE_BYTES {
                    return Err(());
                }
                target.push_str(text);
                if !text.is_empty() {
                    self.first_token.get_or_insert_with(Instant::now);
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
            if calls.len() > super::openai::MAX_TOOL_CALLS_IN_RESPONSE {
                return Err(());
            }
            for call in calls {
                if ["name", "arguments"].iter().any(|key| {
                    call["function"][key]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                }) {
                    self.first_token.get_or_insert_with(Instant::now);
                }
                let index = call["index"].as_u64().ok_or(())?;
                if index >= super::openai::MAX_TOOL_CALLS_IN_RESPONSE as u64 {
                    return Err(());
                }
                let entry = self.tools.entry(index).or_default();
                if call["type"].as_str().is_some_and(|kind| kind != "function") {
                    return Err(());
                }
                let limit = super::openai::MAX_TOOL_ARGUMENT_BYTES;
                for (value, buffer, limit) in [
                    (&call["id"], &mut entry.id, usize::MAX),
                    (&call["function"]["name"], &mut entry.name, limit),
                    (&call["function"]["arguments"], &mut entry.arguments, limit),
                ] {
                    if let Some(value) = value.as_str() {
                        if buffer.len() + value.len() > limit {
                            return Err(());
                        }
                        self.assembled_bytes = self.assembled_bytes.saturating_add(value.len());
                        if self.assembled_bytes > super::openai::DEFAULT_MAX_RESPONSE_BYTES {
                            return Err(());
                        }
                        buffer.push_str(value);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }
    pub fn finish(self, event: &mut dyn FnMut(ApiEvent)) -> Result<Value, ()> {
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
            message["tool_calls"] = Value::Array(self.tools
                .into_values()
                .map(|tool| json!({"id":tool.id,"type":"function","function":{"name":tool.name,"arguments":tool.arguments}}))
                .collect());
        }
        event(ApiEvent::StepFinished {
            output_tokens: self
                .usage
                .as_ref()
                .and_then(|u| u["completion_tokens"].as_u64()),
            decode_ms: self
                .first_token
                .map(|first| first.elapsed().as_millis().min(u64::MAX as u128) as u64),
        });
        Ok(json!({"choices":[{"message":message,"finish_reason":self.finish}],"usage":self.usage}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_overhead_does_not_limit_a_small_complete_response() {
        let content = json!({"note":"x".repeat(12_000)}).to_string();
        let mut frames = content
            .chars()
            .map(|ch| {
                format!(
                    "data: {}\n\n",
                    json!({"id":"synthetic-completion-01234567890123456789012345",
                    "object":"chat.completion.chunk","created":1_000_000_000,
                    "model":"synthetic-streaming-model","choices":[{"index":0,
                    "delta":{"content":ch.to_string()},"finish_reason":null}]})
                )
            })
            .collect::<Vec<_>>();
        frames.push(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                .into(),
        );
        let wire = frames.concat();
        assert!(wire.len() > super::super::openai::DEFAULT_MAX_RESPONSE_BYTES);
        for fragmented in [false, true] {
            let mut collector = StreamCollector::default();
            if fragmented {
                for frame in &frames {
                    for chunk in frame.as_bytes().chunks(17) {
                        collector.push(chunk, &mut |_| {}).unwrap();
                    }
                    assert!(collector.pending.is_empty() && collector.data.is_empty());
                }
            } else {
                collector
                    .push(
                        &vec![b'\n'; super::super::openai::DEFAULT_MAX_RESPONSE_BYTES + 1],
                        &mut |_| {},
                    )
                    .unwrap();
                assert!(collector.pending.is_empty());
                collector.push(wire.as_bytes(), &mut |_| {}).unwrap();
            }
            assert_eq!(collector.assembled_bytes, content.len());
            assert_eq!(
                collector.finish(&mut |_| {}).unwrap()["choices"][0]["message"]["content"],
                content
            );
        }
    }

    #[test]
    fn assembled_content_and_each_unfinished_frame_stay_bounded() {
        let limit = super::super::openai::DEFAULT_MAX_RESPONSE_BYTES;
        let delta = |text: &str| {
            format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":text}}]})
            )
        };
        let mut collector = StreamCollector::default();
        let chunk = delta(&"x".repeat(1024));
        for _ in 0..limit / 1024 {
            collector.push(chunk.as_bytes(), &mut |_| {}).unwrap();
        }
        assert!(collector.push(delta("x").as_bytes(), &mut |_| {}).is_err());
        assert_eq!(collector.content.len(), limit);

        let mut collector = StreamCollector::default();
        assert!(collector
            .push(
                format!("data: {}", "x".repeat(limit)).as_bytes(),
                &mut |_| {}
            )
            .is_err());
        assert!(collector.pending.len() <= limit);

        let mut collector = StreamCollector::default();
        let line = format!("data: {}\n", " ".repeat(1024));
        while collector.push(line.as_bytes(), &mut |_| {}).is_ok() {}
        assert!(collector.pending.len() + collector.data.len() <= limit);

        let mut collector = StreamCollector::default();
        assert!(collector
            .push(
                json!({"content":"x".repeat(limit)}).to_string().as_bytes(),
                &mut |_| {}
            )
            .is_err());
    }

    #[test]
    fn tool_count_can_be_rejected_by_the_loop_without_unbounded_assembly() {
        let limit = super::super::openai::MAX_TOOL_CALLS_IN_RESPONSE;
        let mut collector = StreamCollector::default();
        for index in 0..limit {
            collector.push(format!("data: {}\n\n", json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":format!("call-{index}"),"function":{"name":"read_context","arguments":"{}"}}]}}]})).as_bytes(), &mut |_| {}).unwrap();
        }
        assert_eq!(collector.tools.len(), limit);
        assert!(collector
            .push(
                format!(
                    "data: {}\n\n",
                    json!({"choices":[{"delta":{"tool_calls":[{"index":limit,"id":"overflow"}]}}]})
                )
                .as_bytes(),
                &mut |_| {}
            )
            .is_err());
        assert_eq!(collector.tools.len(), limit);
    }
    #[test]
    fn many_tool_fragments_reuse_storage_and_keep_utf8_and_argument_limits() {
        let mut collector = StreamCollector::default();
        let mut buffer = String::with_capacity(super::super::openai::MAX_TOOL_ARGUMENT_BYTES);
        let pointer = buffer.as_ptr();
        buffer.push('{');
        collector.tools.entry(0).or_default().arguments = buffer;
        for _ in 0..8_000 {
            collector.push(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"中🙂\"}}]}}]}\n\n".as_bytes(),
                &mut |_| {},
            ).unwrap();
        }
        let args = collector.tools[&0].arguments.as_str();
        assert_eq!(args, format!("{{{}", "中🙂".repeat(8_000)));
        assert_eq!(
            args.as_ptr(),
            pointer,
            "appending within capacity must not copy the prefix"
        );
        let remainder = super::super::openai::MAX_TOOL_ARGUMENT_BYTES - args.len();
        let delta = |text: &str| {
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":text}}]}}]}).to_string()
        };
        collector
            .accept(&delta(&"x".repeat(remainder)), &mut |_| {})
            .unwrap();
        assert!(collector.accept(&delta("x"), &mut |_| {}).is_err());
        assert_eq!(
            collector.tools[&0].arguments.len(),
            super::super::openai::MAX_TOOL_ARGUMENT_BYTES
        );
    }

    #[test]
    fn finished_step_uses_reported_tokens_and_first_delta_to_assembly_time() {
        let mut c = StreamCollector::with_output_base(Some(0));
        c.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"{}\"}}]}\n\n",
            &mut |_| {},
        )
        .unwrap();
        assert!(c.first_token.is_some());
        c.first_token = Some(Instant::now() - std::time::Duration::from_secs(2));
        c.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"completion_tokens\":46}}\n\ndata: [DONE]\n\n",&mut |_|{}).unwrap();
        let mut stats = Vec::new();
        c.finish(&mut |e| {
            if let ApiEvent::StepFinished {
                output_tokens,
                decode_ms,
            } = e
            {
                stats.push((output_tokens, decode_ms));
            }
        })
        .unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, Some(46));
        assert!(stats[0].1.unwrap() >= 2000);
        let mut c = StreamCollector::default();
        c.push(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            &mut |_| {},
        )
        .unwrap();
        c.finish(&mut |e| {
            if let ApiEvent::StepFinished {
                output_tokens,
                decode_ms,
            } = e
            {
                assert_eq!((output_tokens, decode_ms), (None, None));
            }
        })
        .unwrap();
    }
    #[test]
    fn ongoing_usage_is_cumulative_and_terminal_totals_are_never_live_samples() {
        for (base, expected) in [
            (Some(30), vec![Some(32), Some(35), None]),
            (None, vec![None, None, None]),
        ] {
            let mut collector = StreamCollector::with_output_base(base);
            let mut counts = Vec::new();
            let mut event = |e| {
                if let ApiEvent::OutputTokens(count) = e {
                    counts.push(count);
                }
            };
            for frame in [
                json!({"choices":[{"delta":{"content":"{"}}],"usage":{"completion_tokens":2}}),
                json!({"choices":[{"delta":{"content":"}"}}],"usage":{"completion_tokens":5}}),
                json!({"choices":[],"usage":{"completion_tokens":-1}}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"completion_tokens":900}}),
                json!({"choices":[],"usage":{"completion_tokens":1000}}),
            ] {
                let bytes = format!("data: {frame}\n\n");
                for chunk in bytes.as_bytes().chunks(7) {
                    collector.push(chunk, &mut event).unwrap();
                }
            }
            collector.push(b"data: [DONE]\n\n", &mut event).unwrap();
            assert_eq!(
                collector.finish(&mut event).unwrap()["usage"]["completion_tokens"],
                1000
            );
            assert_eq!(counts, expected);
        }
    }
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
