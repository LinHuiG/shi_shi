use crate::{engine::App, model::*, work_hours::AiMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, sync::atomic::Ordering};
pub(crate) const AUTOMATION_DISABLED: &str = "AI_AUTOMATION_DISABLED";

/// The filter is a fourth, independent AI purpose. It only decides whether the
/// next full analysis should run early; it never marks a message analyzed,
/// advances a cursor or deletes a message.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterState {
    /// Temporary acknowledgements and sparse pending holes. The watermark
    /// absorbs completed revisions; true holes survive a later scope change.
    pub checked: BTreeMap<String, String>,
    pub last_run: i64,
    pub early: bool,
    /// Actual reason for an early full analysis, kept separate from its timer.
    pub trigger: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Calls reserved but whose usage has not been reported. A non-zero value is
    /// displayed as unknown usage, never rounded to zero.
    pub unreported_calls: u64,
    pub reason: String,
}

impl FilterState {
    /// Consumed when a full request is reserved, independently of its outcome.
    /// A later decision is never cleared by an older request's completion.
    pub fn consume(&mut self, decision_at: Option<i64>) {
        if self.early && decision_at == Some(self.last_run) {
            self.early = false;
            self.trigger.clear();
        }
    }
    /// UI/log projection. The fingerprint table stays in persisted state and is
    /// never copied into every snapshot or log row.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "last_run": self.last_run,
            "early": self.early,
            "calls": self.calls,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "unreported_calls": self.unreported_calls,
            "reason": self.reason,
        })
    }

    pub(crate) fn observe_analysis(
        &mut self,
        observed: &BTreeMap<String, Message>,
        messages: &BTreeMap<String, Message>,
    ) {
        for (id, message) in observed {
            if messages
                .get(id)
                .is_some_and(|live| fingerprint(live) == fingerprint(message))
            {
                self.checked.insert(id.clone(), fingerprint(message));
            }
        }
    }
    pub(crate) fn prune(
        &mut self,
        messages: &BTreeMap<String, Message>,
        analyzed: &BTreeMap<String, String>,
        cutoff: i64,
    ) {
        // Never evict a pending hole below the watermark: widening the lookback
        // must still find it. Temporary acknowledgements can follow the scope.
        self.checked.retain(|id, stored| {
            messages.get(id).is_some_and(|m| {
                !m.withdrawn
                    && (crate::message_progress::pending_revision(m, Some(stored))
                        || ((m.time <= 0 || m.time >= cutoff)
                            && fingerprint_matches(m, Some(stored))
                            && !fingerprint_matches(m, analyzed.get(id))))
            })
        });
    }
}

impl App {
    /// One bounded filter call per new scan batch. A failed or uncertain
    /// classification falls back to the normal full analysis; the interval
    /// timer remains the unconditional safety net when the decision is "wait".
    pub(crate) fn filter_tick(&self) -> Result<(), String> {
        if self.ai_busy.load(Ordering::SeqCst) {
            return Ok(());
        }
        let at = now();
        let (batch, fingerprints) = {
            let mut locked = self.state.lock().unwrap();
            if !locked.config.early_analysis_enabled() {
                return Ok(());
            }
            // A full analysis already due must not wait for another filter.
            if locked.message_filter.early
                || crate::context_budget::due(&locked, &crate::context_budget::pending(&locked), at)
            {
                return Ok(());
            }
            if !self.work_gate(&locked, "message_filter", AiMode::Automatic, None, at) {
                return Ok(());
            }
            if at - locked.message_filter.last_run < 60_000 {
                return Ok(());
            }
            let mut chars = 0usize;
            let mut batch = Vec::new();
            let mut fingerprints = BTreeMap::new();
            let mut pending = crate::context_budget::pending(&locked);
            pending.sort_by_key(|message| (message.updated_at, message.time, message.id.clone()));
            for message in pending {
                if crate::message_progress::filtered(&locked, &message) {
                    continue;
                }
                let value = crate::context_budget::message(&message, 2_000);
                let size = value.to_string().chars().count();
                if batch.len() >= 50 || chars + size > 10_000 {
                    break;
                }
                chars += size;
                batch.push(value);
                fingerprints.insert(message.id.clone(), fingerprint(&message));
            }
            if batch.is_empty() {
                return Ok(());
            }
            let mut next = locked.clone();
            next.message_filter.last_run = at;
            self.save(&next)?;
            *locked = next;
            (batch, fingerprints)
        };
        let result = self.ask_ai_mode(
            "message_filter",
            &json!({"changes":batch,"_trigger":"scan_batch"}),
            AiMode::Automatic,
        );
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with("AI_UNCONFIGURED:") || e == AUTOMATION_DISABLED)
        {
            return Ok(());
        }
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        if !next.config.early_analysis_enabled() {
            return Ok(());
        }
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e == crate::ai_control::CANCELLED)
        {
            next.message_filter.early = false;
            next.message_filter.reason = crate::ai_control::CANCELLED.into();
            next.analysis_interrupted_at = now();
            self.save(&next)?;
            *locked = next;
            return Ok(());
        }
        for (id, value) in fingerprints {
            // Keep the fingerprint that was actually classified, even when the
            // message changes while the request is running. A newer revision
            // then becomes a new filter input on the next batch.
            if next.messages.get(&id).map(fingerprint) == Some(value.clone()) {
                next.message_filter.checked.insert(id, value);
            }
        }
        next.message_filter.prune(
            &next.messages,
            &next.analyzed,
            now() - next.config.analysis_lookback_hours as i64 * 3_600_000,
        );
        crate::message_progress::advance_filter(&mut next);
        next.message_filter.early = match &result {
            Ok(value) => match value["decision"].as_str() {
                Some("wait") => false,
                Some("analyze") | Some("uncertain") => true,
                _ => true,
            },
            Err(_) => true,
        };
        next.message_filter.trigger = if !next.message_filter.early {
            String::new()
        } else if result.is_err() {
            "message_filter_failed".into()
        } else {
            "message_filter".into()
        };
        next.message_filter.reason = match result {
            Ok(value) => value["reason"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect(),
            Err(error) => format!("过滤失败，已回退完整分析：{error}")
                .chars()
                .take(200)
                .collect(),
        };
        self.save(&next)?;
        *locked = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::State;
    use serde_json::json;

    fn message(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            text: text.into(),
            time: now(),
            private: true,
            ..Default::default()
        }
    }

    #[test]
    fn consuming_an_older_request_cannot_erase_a_new_filter_decision() {
        let mut filter = FilterState {
            early: true,
            last_run: 2,
            trigger: "message_filter".into(),
            ..Default::default()
        };
        filter.consume(Some(1));
        assert!(filter.early);
        assert_eq!(filter.trigger, "message_filter");
        filter.consume(Some(2));
        assert!(!filter.early);
        assert!(filter.trigger.is_empty());
    }
    #[test]
    fn filter_state_never_marks_full_analysis_done() {
        let mut state = State::default();
        state
            .message_filter
            .checked
            .insert("message".into(), "fingerprint".into());
        assert!(state.analyzed.is_empty());
        assert_eq!(state.calls, 0);
    }

    #[test]
    fn checked_fingerprints_are_bounded_and_follow_message_revisions() {
        let mut state = State::default();
        for i in 0..700 {
            let value = message(&format!("m{i}"), "正文");
            state
                .message_filter
                .checked
                .insert(value.id.clone(), fingerprint(&value));
            state.messages.insert(value.id.clone(), value);
        }
        state
            .message_filter
            .prune(&state.messages, &state.analyzed, now() - 86_400_000);
        assert_eq!(state.message_filter.checked.len(), 700);
        // Successful and expired messages no longer occupy the filter table.
        state
            .analyzed
            .insert("m0".into(), fingerprint(&state.messages["m0"]));
        state.messages.get_mut("m1").unwrap().time = now() - 2 * 86_400_000;
        state
            .message_filter
            .prune(&state.messages, &state.analyzed, now() - 86_400_000);
        assert_eq!(state.message_filter.checked.len(), 698);
        let mut changed = message("m699", "修改后的正文");
        changed.time = now() + 1;
        state.messages.insert("m699".into(), changed.clone());
        state
            .message_filter
            .prune(&state.messages, &state.analyzed, now() - 86_400_000);
        assert!(!state.message_filter.checked.contains_key("m699"));
        state
            .message_filter
            .checked
            .insert("m699".into(), "stale fingerprint".into());
        assert_ne!(
            state.message_filter.checked.get("m699"),
            Some(&fingerprint(&changed))
        );
    }

    #[test]
    fn filter_payload_is_reported_separately_from_full_analysis() {
        let mut state = State::default();
        state
            .message_filter
            .checked
            .insert("m".into(), "internal-fingerprint".into());
        state.message_filter.calls = 2;
        let snapshot = json!({
            "filter_usage": state.message_filter.summary(),
            "calls": state.calls,
        });
        assert_eq!(snapshot["calls"], 0);
        assert_eq!(snapshot["filter_usage"]["calls"], 2);
        assert!(snapshot["filter_usage"].get("checked").is_none());
        assert!(!snapshot.to_string().contains("internal-fingerprint"));
    }
}
