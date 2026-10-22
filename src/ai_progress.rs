use serde::Serialize;

#[derive(Clone, Copy, Default, Serialize)]
pub struct GenerationStats {
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub steps: u64,
}
#[derive(Default, Serialize)]
pub struct LiveProgress {
    pub request_id: String,
    pub phase: String,
    pub last_received: i64,
    pub revision: u64,
    pub entries: Vec<Entry>,
    pub first_response_ms: Option<u64>,
    pub output_tokens: Option<u64>,
    pub generation: GenerationStats,
    pub usage_final: bool,
    pub usage_elapsed_ms: u64,
}
#[derive(Serialize)]
pub struct Entry {
    id: u64,
    kind: String,
    text: String,
}
impl LiveProgress {
    pub fn record_step(&mut self, request_id: &str, tokens: Option<u64>, decode_ms: Option<u64>) {
        if self.request_id != request_id || self.usage_final {
            return;
        }
        if let (Some(tokens), Some(ms)) = (tokens, decode_ms.filter(|ms| *ms > 0)) {
            if let (Some(total), Some(elapsed)) = (
                self.generation.output_tokens.checked_add(tokens),
                self.generation.elapsed_ms.checked_add(ms),
            ) {
                self.generation.output_tokens = total;
                self.generation.elapsed_ms = elapsed;
                self.generation.steps = self.generation.steps.saturating_add(1);
                self.revision = self.revision.wrapping_add(1);
            }
        }
    }
    pub fn record_content(&mut self, request_id: &str, elapsed_ms: u64) {
        if self.request_id == request_id && self.first_response_ms.is_none() {
            self.first_response_ms = Some(elapsed_ms);
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn record_usage(&mut self, request_id: &str, usage: &serde_json::Value, elapsed_ms: u64) {
        if self.request_id != request_id {
            return;
        }
        let output = if usage["partial"] != true {
            usage["output_tokens"].as_u64()
        } else {
            None
        };
        if !self.usage_final || self.output_tokens != output || self.usage_elapsed_ms != elapsed_ms
        {
            self.output_tokens = output;
            self.usage_elapsed_ms = elapsed_ms;
            self.usage_final = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn record_output_tokens(&mut self, request_id: &str, output: Option<u64>, elapsed_ms: u64) {
        if self.request_id != request_id || self.usage_final {
            return;
        }
        if self.output_tokens != output || self.usage_elapsed_ms != elapsed_ms {
            self.output_tokens = output;
            self.usage_elapsed_ms = elapsed_ms;
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn start(&mut self, id: &str) {
        *self = Self {
            request_id: id.into(),
            phase: "正在连接".into(),
            ..Self::default()
        };
    }
    pub fn accept(&mut self, kind: &str, text: &str, append: bool) {
        if text.is_empty() {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        if kind == "phase" {
            self.phase = text.into();
        } else {
            self.last_received = crate::model::now();
            self.phase = match kind {
                "reasoning" => "正在思考",
                "tool" => "读取上下文",
                _ => "接收结果",
            }
            .into();
        }
        if let Some(last) = self.entries.last_mut().filter(|e| e.kind == kind) {
            if append {
                last.text.push_str(text);
            } else {
                last.text = text.into();
            }
            last.text = tail(&last.text, 12000);
        } else {
            self.entries.push(Entry {
                id: self.revision,
                kind: kind.into(),
                text: tail(text, 12000),
            });
        }
        while self.entries.iter().map(|e| e.text.len()).sum::<usize>() > 48_000
            || self.entries.len() > 64
        {
            self.entries.remove(0);
        }
    }
}
fn tail(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_steps_pair_tokens_and_decode_time_without_unknown_or_stale_steps() {
        let mut live = LiveProgress::default();
        live.start("first");
        live.record_step("stale", Some(9999), Some(1));
        live.record_step("first", None, Some(1000));
        live.record_step("first", Some(1000), None);
        live.record_step("first", Some(1000), Some(0));
        assert_eq!(live.generation.steps, 0);
        live.record_step("first", Some(100), Some(1000));
        live.record_step("first", Some(900), Some(3000));
        live.record_step("first", Some(0), Some(1000));
        assert_eq!(live.generation.output_tokens, 1000);
        assert_eq!(live.generation.elapsed_ms, 5000);
        assert_eq!(live.generation.steps, 3);
        live.record_usage("first", &serde_json::json!({"output_tokens":1000}), 100000);
        live.record_step("first", Some(10), Some(1));
        assert_eq!(live.generation.steps, 3);
        live.start("second");
        assert_eq!(live.generation.steps, 0);
    }
    #[test]
    fn reported_metrics_keep_first_content_and_reject_stale_or_partial_usage() {
        let mut live = LiveProgress::default();
        live.start("first");
        live.accept("phase", "waiting", false);
        assert_eq!(live.first_response_ms, None);
        live.record_content("first", 1250);
        live.record_content("first", 9000);
        assert_eq!(live.first_response_ms, Some(1250));
        live.record_usage(
            "first",
            &serde_json::json!({"input_tokens":8928,"output_tokens":912}),
            2000,
        );
        assert_eq!(live.output_tokens, Some(912));
        assert_eq!(live.usage_elapsed_ms, 2000);
        live.start("second");
        live.record_content("first", 1);
        live.record_usage(
            "first",
            &serde_json::json!({"input_tokens":1,"output_tokens":999}),
            1,
        );
        assert_eq!(live.first_response_ms, None);
        assert_eq!(live.output_tokens, None);
        live.record_usage(
            "second",
            &serde_json::json!({"input_tokens":1,"output_tokens":10,"partial":true}),
            10,
        );
        assert_eq!(live.output_tokens, None);
    }
    #[test]
    fn live_output_is_independent_of_billing_and_terminal_counts_stop_sampling() {
        let mut live = LiveProgress::default();
        live.start("first");
        live.record_output_tokens("stale", Some(999), 1000);
        assert_eq!(live.output_tokens, None);
        live.record_output_tokens("first", Some(5), 1000);
        assert_eq!(live.output_tokens, Some(5));
        assert!(!live.usage_final);
        live.record_output_tokens("first", Some(25), 2000);
        assert_eq!(live.output_tokens, Some(25));
        live.record_output_tokens("first", None, 2500);
        assert_eq!(live.output_tokens, None);
        live.record_usage(
            "first",
            &serde_json::json!({"input_tokens":10,"output_tokens":40}),
            3000,
        );
        assert!(live.usage_final);
        assert_eq!(live.output_tokens, Some(40));
        live.record_output_tokens("first", Some(9000), 4000);
        assert_eq!(live.output_tokens, Some(40));
        live.start("second");
        assert!(!live.usage_final);
        assert_eq!(live.output_tokens, None);
    }
    #[test]
    fn chunks_append_snapshots_replace_and_request_reset_isolated() {
        let mut p = LiveProgress::default();
        p.start("first");
        p.accept("reasoning", "核", true);
        p.accept("reasoning", "对", true);
        assert_eq!(p.entries[0].text, "核对");
        p.accept("reasoning", "核对完成", false);
        assert_eq!(p.entries[0].text, "核对完成");
        p.accept("content", &"字".repeat(100_000), true);
        assert!(p.entries.iter().map(|e| e.text.len()).sum::<usize>() <= 48_000);
        p.start("second");
        assert!(p.entries.is_empty());
        assert_eq!(p.last_received, 0);
        p.accept("phase", "已开始处理，等待公开内容", false);
        assert_eq!(p.entries[0].kind, "phase");
        assert_eq!(
            p.last_received, 0,
            "a phase must not claim provider text arrived"
        );
    }
}
