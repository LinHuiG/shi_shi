use crate::{context_budget, engine::App, model::*};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn waiting_messages<'a: 'b, 'b>(
    s: &'a State,
    active: &'b BTreeMap<String, String>,
) -> impl Iterator<Item = &'a Message> + 'b {
    context_budget::pending_refs(s, true).filter(move |m| {
        !active
            .get(&m.id)
            .is_some_and(|version| *version == fingerprint(m))
    })
}

/// In-flight message revisions are transient: failed/cancelled work returns to the queue.
pub(crate) struct MessageBatch<'a>(&'a App);
impl Drop for MessageBatch<'_> {
    fn drop(&mut self) {
        self.0.analyzing_messages.lock().unwrap().clear();
    }
}

impl App {
    pub(crate) fn claim_message_batch(
        &self,
        fingerprints: &BTreeMap<String, String>,
    ) -> Result<MessageBatch<'_>, String> {
        let mut active = self.analyzing_messages.lock().unwrap();
        if !active.is_empty() {
            return Err("消息分析正在处理，请稍后重试".into());
        }
        *active = fingerprints.clone();
        Ok(MessageBatch(self))
    }
    pub(crate) fn message_queue<'a>(&self, s: &'a State) -> (Vec<&'a Message>, usize) {
        let active = self.analyzing_messages.lock().unwrap();
        let waiting = waiting_messages(s, &active).collect();
        (waiting, active.len())
    }
    pub(crate) fn message_queue_counts(&self, s: &State) -> (usize, usize, usize) {
        let active = self.analyzing_messages.lock().unwrap();
        let scope = (!s.analysis_retry_scopes.is_empty())
            .then(|| crate::engine::analysis_block_fingerprint(s));
        let (mut waiting, mut manual) = (0, 0);
        for m in waiting_messages(s, &active) {
            waiting += 1;
            manual += usize::from(!context_budget::automatic_retry(s, &m.id, scope.as_ref()));
        }
        (waiting, active.len(), manual)
    }
    pub fn analysis_backlog(&self, value: &Value) -> Result<Value, String> {
        let hours = value["hours"].as_u64().unwrap_or(24);
        if !(1..=87600).contains(&hours) {
            return Err("请输入1–87600小时".into());
        }
        let apply = value["apply"] == true;
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let at = now();
        let cutoff = at - hours as i64 * 3_600_000;
        // Snapshot the precise fingerprints at preview time. Applying never skips newer arrivals.
        let (waiting, analyzing) = self.message_queue(&s);
        let eligible = waiting.len();
        let retry_scope = (!s.analysis_retry_scopes.is_empty())
            .then(|| crate::engine::analysis_block_fingerprint(&s));
        let mut retry_reasons = BTreeMap::<(bool, String), usize>::new();
        let mut manual_retry_count = 0;
        for message in &waiting {
            if let Some(reason) = s.analysis_retries.get(&message.id) {
                let automatic_retry =
                    context_budget::automatic_retry(&s, &message.id, retry_scope.as_ref());
                manual_retry_count += usize::from(!automatic_retry);
                *retry_reasons
                    .entry((automatic_retry, reason.clone()))
                    .or_default() += 1;
            }
        }
        let retry_reason_count = retry_reasons.len();
        let retry_reasons = retry_reasons.into_iter().take(3).map(|((automatic_retry, reason), count)| {
            json!({"automatic_retry":automatic_retry,"reason":reason,"count":count})
        }).collect::<Vec<_>>();
        let records = waiting
            .into_iter()
            .filter(|m| m.time > 0 && m.time < cutoff)
            .map(|m| (m.id.clone(), fingerprint(&m)))
            .collect::<std::collections::BTreeMap<_, _>>();
        if apply {
            let expected = value["records"].as_object().ok_or("请先预览要跳过的消息")?;
            let mut skipped = 0;
            for (id, fingerprint) in records {
                if expected.get(&id).and_then(Value::as_str) == Some(&fingerprint) {
                    s.analysis_retries.remove(&id);
                    s.analysis_retry_counts.remove(&id);
                    s.analysis_retry_scopes.remove(&id);
                    s.analyzed.insert(id, fingerprint);
                    skipped += 1;
                }
            }
            crate::message_progress::advance_analysis(&mut s);
            crate::message_progress::advance_filter(&mut s);
            let remaining = self.message_queue(&s).0.len();
            let pending = context_budget::pending_refs(&s, false).next().is_some() || analyzing > 0;
            context_budget::mark_pending(&mut s, pending, at);
            self.save(&s)?;
            *lock = s;
            drop(lock);
            self.log(
                "analysis_skipped",
                json!({"hours":hours,"skipped":skipped,"remaining":remaining}),
            );
            return Ok(json!({"skipped_count":skipped,"remaining":remaining}));
        }
        Ok(
            json!({"hours":hours,"count":records.len(),"records":records,"eligible":eligible,"analyzing":analyzing,
                "manual_retry_count":manual_retry_count,"retry_reasons":retry_reasons,
                "retry_reasons_more":retry_reason_count.saturating_sub(3)}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_retry_limits_do_not_retire_ordinary_failed_messages() {
        let (root, app) = crate::test_support::app("shishi-legacy-retry");
        app.commit_state(|s| {
            for (index, reason) in [
                "多次未采用，已停止自动重试；消息未标为已分析，可手动分析",
                "AI未返回动作且缺少原因，本批未采用，消息仍待处理；已保留待手动重试，未标为已分析",
            ]
            .into_iter()
            .enumerate()
            {
                let id = format!("ordinary-{index}");
                crate::message_progress::insert(
                    s,
                    crate::test_support::message(id.clone(), "Synthetic pending evidence", now()),
                );
                s.analysis_retry_counts.insert(id.clone(), 8);
                s.analysis_retries.insert(id, reason.into());
            }
            Ok(())
        })
        .unwrap();
        drop(app);
        let app = App::load(root.clone()).unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(context_budget::pending(&s).len(), 2);
        assert_eq!(app.message_queue_counts(&s), (2, 0, 0));
        drop(s);
        let queue = app.analysis_backlog(&json!({"hours":24})).unwrap();
        assert_eq!(queue["manual_retry_count"], 0);
        assert!(queue["retry_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["automatic_retry"] == true));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn manual_retry_messages_stay_visible_and_explicit_skip_clears_their_retry_state() {
        let (_root, app) = crate::test_support::scoped_app("shishi-retry-queue");
        {
            let mut state = app.state.lock().unwrap();
            state.messages.insert(
                "held".into(),
                Message {
                    private: true,
                    ..crate::test_support::message("held", "合成待核对消息", now() - 3 * 3_600_000)
                },
            );
            state.analysis_retry_counts.insert("held".into(), 3);
            state
                .analysis_retries
                .insert("held".into(), context_budget::COMMAND_RETRY_REASON.into());
            assert!(context_budget::pending(&state).is_empty());
            assert_eq!(app.message_queue(&state).0.len(), 1);
        }
        assert_eq!(app.snapshot()["pending"], 1);
        assert_eq!(app.snapshot()["manual_retry_count"], 1);
        let mut preview = app.analysis_backlog(&json!({"hours":1})).unwrap();
        assert_eq!(preview["eligible"], 1);
        assert_eq!(preview["manual_retry_count"], 1);
        assert_eq!(preview["retry_reasons"][0]["automatic_retry"], false);
        assert!(preview["retry_reasons"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("核对命令记录"));
        preview["apply"] = json!(true);
        assert_eq!(app.analysis_backlog(&preview).unwrap()["skipped_count"], 1);
        let state = app.state.lock().unwrap();
        assert!(state.analysis_retries.is_empty());
        assert!(state.analysis_retry_counts.is_empty());
        assert!(crate::message_progress::analyzed(
            &state,
            &state.messages["held"]
        ));
        assert!(!state.pending_analysis);
        drop(state);
    }
    #[test]
    fn active_batch_is_separate_and_skip_only_affects_waiting_revisions() {
        let (root, app) = crate::test_support::scoped_app("shishi-queue");
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            for key in ["a", "b", "waiting"] {
                s.messages.insert(
                    key.into(),
                    Message {
                        private: true,
                        ..crate::test_support::message(
                            key,
                            "请核对".repeat(1000),
                            now() - 3 * 3_600_000,
                        )
                    },
                );
            }
        }
        let batch = {
            let s = app.state.lock().unwrap();
            app.claim_message_batch(
                &["a", "b"]
                    .into_iter()
                    .map(|id| (id.into(), fingerprint(&s.messages[id])))
                    .collect(),
            )
            .unwrap()
        };
        assert!(app.claim_message_batch(&BTreeMap::new()).is_err());
        {
            let s = app.state.lock().unwrap();
            let (waiting, active) = app.message_queue(&s);
            assert_eq!(app.message_queue_counts(&s), (waiting.len(), active, 0));
            assert_eq!(waiting.len(), 1);
            assert!(std::ptr::eq(waiting[0], &s.messages["waiting"]));
        }
        assert_eq!(
            (
                app.snapshot()["pending"].clone(),
                app.snapshot()["analyzing"].clone()
            ),
            (json!(1), json!(2))
        );
        let mut preview = app.analysis_backlog(&json!({"hours":1})).unwrap();
        assert_eq!(preview["count"], 1);
        assert!(preview["records"].get("a").is_none());
        assert_eq!(preview["eligible"], 1);
        assert_eq!(preview["analyzing"], 2);
        preview["apply"] = json!(true);
        assert_eq!(app.analysis_backlog(&preview).unwrap()["skipped_count"], 1);
        assert_eq!(app.snapshot()["pending"], 0);
        {
            let mut s = app.state.lock().unwrap();
            let mut arrival = s.messages["a"].clone();
            arrival.id = "arrival".into();
            s.messages.insert(arrival.id.clone(), arrival);
            assert!(!s.analyzed.contains_key("a"));
            app.save(&s).unwrap();
        }
        assert_eq!(app.snapshot()["pending"], 1);
        let restored = App::load(root.clone()).unwrap();
        assert_eq!(restored.snapshot()["pending"], 3);
        assert_eq!(restored.snapshot()["analyzing"], 0);
        drop(restored);
        // Interruption or failure releases the claim without marking messages analyzed.
        drop(batch);
        assert_eq!(app.snapshot()["pending"], 3);
        assert_eq!(app.snapshot()["analyzing"], 0);
        let batch = {
            let s = app.state.lock().unwrap();
            app.claim_message_batch(
                &[("a".into(), fingerprint(&s.messages["a"]))]
                    .into_iter()
                    .collect(),
            )
            .unwrap()
        };
        {
            let mut s = app.state.lock().unwrap();
            s.messages.get_mut("a").unwrap().text = "修改后的新内容".into();
        }
        assert_eq!(
            app.snapshot()["pending"],
            3,
            "new revision waits while the old revision is running"
        );
        drop(batch);
    }
    #[test]
    fn rejected_analysis_releases_message_claim_without_consuming_a_call() {
        let (_root, app) = crate::test_support::scoped_app("shishi-rejected-queue");
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = 1;
            s.config.codex = "must-not-be-invoked.exe".into();
            s.messages.insert(
                "a".into(),
                Message {
                    private: true,
                    ..crate::test_support::message("a", "请核对", now())
                },
            );
        }
        assert!(app
            .analyze(crate::work_hours::AiMode::Manual)
            .unwrap_err()
            .starts_with(crate::ai::INPUT_LIMIT_ERROR));
        assert_eq!(app.snapshot()["pending"], 1);
        assert_eq!(app.snapshot()["analyzing"], 0);
        assert_eq!(app.state.lock().unwrap().calls, 0);
    }
    #[test]
    fn window_and_one_time_skip_keep_raw_messages_and_new_arrivals() {
        let (_root, app) = crate::test_support::scoped_app("shishi-backlog");
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            for (id, hours) in [("old", 30), ("recent", 2)] {
                s.messages.insert(
                    id.into(),
                    Message {
                        private: true,
                        ..crate::test_support::message(id, "请检查", now() - hours * 3_600_000)
                    },
                );
            }
        }
        assert_eq!(context_budget::pending(&app.state.lock().unwrap()).len(), 1);
        assert_eq!(
            app.analysis_backlog(&json!({"hours":24})).unwrap()["count"],
            0
        );
        app.state.lock().unwrap().config.analysis_lookback_hours = 48;
        let mut preview = app.analysis_backlog(&json!({"hours":24})).unwrap();
        assert_eq!(preview["count"], 1);
        {
            let mut s = app.state.lock().unwrap();
            s.config.analysis_lookback_hours = 48;
            let mut arrival = s.messages["old"].clone();
            arrival.id = "late".into();
            s.messages.insert(arrival.id.clone(), arrival);
        }
        preview["apply"] = json!(true);
        assert_eq!(app.analysis_backlog(&preview).unwrap()["skipped_count"], 1);
        let s = app.state.lock().unwrap();
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.config.analysis_lookback_hours, 48);
        assert!(s.analyzed.contains_key("old"));
        assert!(!s.analyzed.contains_key("late"));
        assert_eq!(context_budget::pending(&s).len(), 2);
        drop(s);
    }
}
