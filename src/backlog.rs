use crate::{context_budget, engine::App, model::*};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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
    pub(crate) fn message_queue(&self, s: &State) -> (Vec<Message>, usize) {
        let active = self.analyzing_messages.lock().unwrap();
        let waiting = context_budget::pending(s)
            .into_iter()
            .filter(|m| active.get(&m.id) != Some(&fingerprint(m)))
            .collect();
        (waiting, active.len())
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
                    s.analyzed.insert(id, fingerprint);
                    skipped += 1;
                }
            }
            crate::message_progress::advance_analysis(&mut s);
            crate::message_progress::advance_filter(&mut s);
            let remaining = self.message_queue(&s).0.len();
            context_budget::mark_pending(&mut s, remaining > 0 || analyzing > 0, at);
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
            json!({"hours":hours,"count":records.len(),"records":records,"eligible":eligible,"analyzing":analyzing}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_batch_is_separate_and_skip_only_affects_waiting_revisions() {
        let root = std::env::temp_dir().join(format!("shishi-queue-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            for key in ["a", "b", "waiting"] {
                s.messages.insert(
                    key.into(),
                    Message {
                        id: key.into(),
                        text: "请核对".into(),
                        private: true,
                        time: now() - 3 * 3_600_000,
                        ..Default::default()
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
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejected_analysis_releases_message_claim_without_consuming_a_call() {
        let root = std::env::temp_dir().join(format!("shishi-rejected-queue-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = 1;
            s.config.codex = "must-not-be-invoked.exe".into();
            s.messages.insert(
                "a".into(),
                Message {
                    id: "a".into(),
                    text: "请核对".into(),
                    private: true,
                    time: now(),
                    ..Default::default()
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
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn window_and_one_time_skip_keep_raw_messages_and_new_arrivals() {
        let root = std::env::temp_dir().join(format!("shishi-backlog-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            for (id, hours) in [("old", 30), ("recent", 2)] {
                s.messages.insert(
                    id.into(),
                    Message {
                        id: id.into(),
                        private: true,
                        time: now() - hours * 3_600_000,
                        text: "请检查".into(),
                        ..Default::default()
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
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
