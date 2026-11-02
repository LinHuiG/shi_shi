use crate::{engine::App, model::*, work_hours::AiMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::atomic::Ordering};
pub(crate) const AUTOMATION_DISABLED: &str = "AI_AUTOMATION_DISABLED";
const FILTER_TIMEOUT: &str = "消息过滤总时限已到，未完成批次保留";

fn filter_authority(state: &State) -> Value {
    json!({"config":crate::context_budget::config_authority(&state.config, "message_filter"),
        "sources":state.chat_sources.iter().map(|source| json!({
            "id":source.id,"plugin":source.plugin_id,"account":source.account_key,
            "enabled":source.enabled,"updated":source.updated,"config":source.config
        })).collect::<Vec<_>>()})
}

fn check_filter_running(
    call: &crate::ai_control::AiCall<'_>,
    deadline: std::time::Instant,
) -> Result<(), String> {
    crate::process::check_running()?;
    if call.cancel.load(Ordering::SeqCst) {
        return Err(crate::ai_control::CANCELLED.into());
    }
    if std::time::Instant::now() >= deadline {
        return Err(FILTER_TIMEOUT.into());
    }
    Ok(())
}

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
        observed: &[&Message],
        messages: &BTreeMap<String, Message>,
    ) {
        for message in observed {
            if messages.get(&message.id).is_some_and(|live| {
                live.text == message.text && live.withdrawn == message.withdrawn
            }) {
                self.checked
                    .insert(message.id.clone(), fingerprint(message));
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
    /// Drain the versions pending at this start. A "wait" only acknowledges its
    /// own batch; a positive/uncertain result hands off to full analysis at once.
    pub(crate) fn filter_tick(&self) -> Result<(), String> {
        self.filter_tick_with(&mut |request, call| {
            self.invoke_prepared_request(request, AiMode::Automatic, call, false)
        })
    }

    fn filter_tick_with(
        &self,
        invoke: &mut dyn FnMut(
            &crate::engine::PreparedAiRequest,
            &crate::ai_control::AiCall<'_>,
        ) -> Result<serde_json::Value, String>,
    ) -> Result<(), String> {
        if self.ai_busy.load(Ordering::SeqCst) {
            return Ok(());
        }
        let at = now();
        let (mut boundary, mut context, timeout, authority, max_input) = {
            let locked = self.state.lock().unwrap().clone();
            if !locked.config.early_analysis_enabled()
                || locked.message_filter.early
                || at
                    < crate::context_budget::next_interval_at(
                        locked.message_filter.last_run,
                        60_000,
                        locked.analysis_interrupted_at,
                    )
                || !self.work_gate(&locked, "message_filter", AiMode::Automatic, None, at)
            {
                return Ok(());
            }
            let mut pending =
                crate::context_budget::pending_refs(&locked, false).collect::<Vec<_>>();
            // A full analysis already due must not wait for another filter.
            if crate::context_budget::due(&locked, pending.iter().copied(), at) {
                return Ok(());
            }
            pending.retain(|message| !crate::message_progress::filtered(&locked, message));
            pending
                .sort_by(|a, b| (a.updated_at, a.time, &a.id).cmp(&(b.updated_at, b.time, &b.id)));
            if pending.is_empty() {
                return Ok(());
            }
            let boundary = pending
                .into_iter()
                .map(|message| (message.id.clone(), fingerprint(message)))
                .collect::<std::collections::VecDeque<_>>();
            let context =
                self.capture_prepare_context_from(&locked, "message_filter", true, json!([]));
            (
                boundary,
                context,
                locked.config.ai_timeout_seconds,
                filter_authority(&locked),
                locked.config.max_input_chars,
            )
        };
        crate::process::check_running()?;
        // A competing manual request may win after the cheap idle check. Do
        // not consume the next round's frequency gate until we own the slot.
        let mut call = match self.begin_ai_for("message_filter") {
            Ok(call) => call,
            Err(_) => return Ok(()),
        };
        {
            let mut locked = self.state.lock().unwrap();
            let current = now();
            if !locked.config.early_analysis_enabled()
                || locked.message_filter.early
                || current
                    < crate::context_budget::next_interval_at(
                        locked.message_filter.last_run,
                        60_000,
                        locked.analysis_interrupted_at,
                    )
                || !self.work_gate(&locked, "message_filter", AiMode::Automatic, None, current)
            {
                return Ok(());
            }
            if filter_authority(&locked) != authority {
                return Ok(());
            }
            // The queue may have grown while acquiring the shared AI slot.
            // This is a startup gate only; continuation batches do not rescan it.
            if crate::context_budget::due(
                &locked,
                crate::context_budget::pending_refs(&locked, false),
                current,
            ) {
                return Ok(());
            }
            let mut next = locked.clone();
            next.message_filter.last_run = current;
            self.save(&next)?;
            *locked = next;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
        let _deadline_guard = crate::ai_control::DeadlineGuard::new(deadline, call.cancel.clone());

        let mut batch_index = 0usize;
        loop {
            crate::process::check_running()?;
            let mut batch = Vec::new();
            let mut fingerprints = Vec::new();
            let snapshot = self.state.lock().unwrap().clone();
            if !snapshot.config.early_analysis_enabled()
                || !self.work_gate(&snapshot, "message_filter", AiMode::Automatic, None, now())
                || filter_authority(&snapshot) != authority
            {
                return Ok(());
            }
            let eligible = |(id, version): &(String, String)| {
                snapshot.messages.get(id).filter(|message| {
                    !message.withdrawn
                        && fingerprint(message) == *version
                        && !crate::message_progress::filtered(&snapshot, message)
                })
            };
            while boundary
                .front()
                .is_some_and(|entry| eligible(entry).is_none())
            {
                boundary.pop_front();
            }
            let pending_count = boundary.len();
            // The complete prompt contains these literal message projections.
            // Their JSON alone is a lower bound, so no larger prefix can fit
            // after it exceeds the configured global limit. This bounds trial
            // preparation without adding another fixed message/character cap.
            let mut chars = 2usize;
            let mut bounded = false;
            for entry in &boundary {
                if check_filter_running(&call, deadline).is_err() {
                    break;
                }
                let Some(message) = eligible(entry) else {
                    continue;
                };
                let value = crate::context_budget::message(message, 2_000);
                let size = json_chars(&value) + usize::from(!batch.is_empty());
                if !batch.is_empty() && chars.saturating_add(size) > max_input {
                    bounded = true;
                    break;
                }
                chars += size;
                batch.push(value);
                fingerprints.push(entry.clone());
                if chars > max_input {
                    break;
                }
            }
            if batch.is_empty() && check_filter_running(&call, deadline).is_ok() {
                return Ok(());
            }
            let prepared = check_filter_running(&call, deadline)
                .and_then(|_| self.fit_filter_batch(&batch, &mut context, &call, deadline));
            let result = match prepared {
                Ok((selected, mut request)) => {
                    // Trials never consume the boundary. Validate the exact
                    // chosen revisions before removing their ordered prefix.
                    let locked = self.state.lock().unwrap();
                    if filter_authority(&locked) != authority {
                        return Ok(());
                    }
                    if fingerprints[..selected].iter().any(|(id, version)| {
                        locked.messages.get(id).is_none_or(|message| {
                            message.withdrawn
                                || fingerprint(message) != *version
                                || crate::message_progress::filtered(&locked, message)
                        })
                    }) {
                        continue;
                    }
                    let last = &fingerprints[selected - 1].0;
                    while let Some((id, _)) = boundary.pop_front() {
                        if &id == last {
                            break;
                        }
                    }
                    drop(locked);
                    fingerprints.truncate(selected);
                    batch_index += 1;
                    request.payload["_batch_metrics"] = json!({
                        "index":batch_index,"pending":pending_count,"selected":selected,"remaining":boundary.len(),
                        "initial_selected":batch.len(),"message_chars":json_chars(&batch[..selected]),
                        "total_input_chars":request.prepared.total,"max_input_chars":request.config.max_input_chars,
                        "reduced_by":if bounded || selected < batch.len() { vec!["input_budget"] } else { vec![] }
                    });
                    request.materialize_records();
                    request.config.ai_timeout_seconds = deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .as_secs()
                        .max(1);
                    check_filter_running(&call, deadline)
                        .and_then(|_| call.next_request())
                        .and_then(|_| invoke(&request, &call))
                }
                Err(error) => {
                    fingerprints.clear();
                    Err(error)
                }
            };
            // Use the same acceptance lock as interruption; no late click can
            // acknowledge a batch that was cancelled before its commit.
            let _acceptance = self.active_ai.lock().unwrap();
            if !self.commit_filter_batch(
                &fingerprints,
                result,
                boundary.is_empty(),
                &call,
                deadline,
                &authority,
            )? {
                return Ok(());
            }
        }
    }

    fn fit_filter_batch(
        &self,
        rows: &[Value],
        context: &mut crate::engine::PrepareContext,
        call: &crate::ai_control::AiCall<'_>,
        deadline: std::time::Instant,
    ) -> Result<(usize, crate::engine::PreparedAiRequest), String> {
        let mut prepare = |count: usize| {
            check_filter_running(call, deadline)?;
            self.prepare_ai_candidate_with(
                "message_filter",
                &json!({"changes":&rows[..count],"_trigger":"scan_batch"}),
                AiMode::Automatic,
                context,
            )
        };
        let all = prepare(rows.len())?;
        if all.prepared.total <= all.config.max_input_chars {
            return Ok((rows.len(), all));
        }
        let accepted = crate::context_budget::fit_prefix(rows.len().saturating_sub(1), |count| {
            let request = prepare(count)?;
            Ok((request.prepared.total <= request.config.max_input_chars).then_some(request))
        })?;
        check_filter_running(call, deadline)?;
        accepted.ok_or_else(|| {
            format!(
                "{}：单条消息及必需过滤上下文超过单次输入上限，未调用 AI",
                crate::ai::INPUT_LIMIT_ERROR
            )
        })
    }

    /// Each accepted batch is durable before the next request. Only the final
    /// batch compacts watermarks, so draining does not rescan all message history
    /// after every small filter call; exact checked revisions work in the interim.
    fn commit_filter_batch(
        &self,
        fingerprints: &[(String, String)],
        result: Result<serde_json::Value, String>,
        final_batch: bool,
        call: &crate::ai_control::AiCall<'_>,
        deadline: std::time::Instant,
        authority: &Value,
    ) -> Result<bool, String> {
        if result.as_ref().err().is_some_and(|error| {
            error.starts_with("AI_UNCONFIGURED:")
                || error == AUTOMATION_DISABLED
                || error == crate::work_hours::DEFERRED
        }) {
            return Ok(false);
        }
        let mut locked = self.state.lock().unwrap();
        if !locked.config.early_analysis_enabled() || filter_authority(&locked) != *authority {
            return Ok(false);
        }
        // begin_ai_for resets this timestamp, and the acceptance lock excludes
        // a later interruption while committing. A user's cancellation wins
        // even if the provider only returns after the total deadline.
        let timed_out = std::time::Instant::now() >= deadline;
        let result = if locked.ai_interrupted_at > 0 {
            Err(crate::ai_control::CANCELLED.into())
        } else if timed_out {
            Err(FILTER_TIMEOUT.into())
        } else if call.cancel.load(Ordering::SeqCst) {
            Err(crate::ai_control::CANCELLED.into())
        } else {
            result
        };
        let mut next = locked.clone();
        let cancelled = result
            .as_ref()
            .err()
            .is_some_and(|e| e == crate::ai_control::CANCELLED);
        if result.is_ok() {
            for (id, value) in fingerprints {
                if next
                    .messages
                    .get(id)
                    .is_some_and(|message| fingerprint(message) == *value)
                {
                    next.message_filter
                        .checked
                        .insert(id.clone(), value.clone());
                }
            }
        }
        next.message_filter.early = !cancelled
            && !result
                .as_ref()
                .is_ok_and(|value| value["decision"] == "wait");
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
            Err(error) if cancelled => error,
            Err(error) => format!("过滤失败，已回退完整分析：{error}")
                .chars()
                .take(200)
                .collect(),
        };
        if cancelled {
            next.analysis_interrupted_at = if next.ai_interrupted_at > 0 {
                next.ai_interrupted_at
            } else {
                now()
            };
        }
        let mut continue_batches = !cancelled && !next.message_filter.early;
        if final_batch || !continue_batches {
            next.message_filter.prune(
                &next.messages,
                &next.analyzed,
                now() - next.config.analysis_lookback_hours as i64 * 3_600_000,
            );
            crate::message_progress::advance_filter(&mut next);
        }
        if !cancelled && !timed_out && std::time::Instant::now() >= deadline {
            // Cloning and watermark compaction can cross the deadline. Discard
            // the whole candidate so none of this batch's revisions are accepted.
            next = locked.clone();
            next.message_filter.early = true;
            next.message_filter.trigger = "message_filter_failed".into();
            next.message_filter.reason = format!("过滤失败，已回退完整分析：{FILTER_TIMEOUT}");
            continue_batches = false;
        }
        self.save(&next)?;
        *locked = next;
        Ok(continue_batches)
    }
}

#[cfg(test)]
#[path = "message_filter_tests.rs"]
mod batching_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::State;
    use serde_json::json;

    fn message(id: &str, text: &str) -> Message {
        Message {
            private: true,
            ..crate::test_support::message(id, text, now())
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
