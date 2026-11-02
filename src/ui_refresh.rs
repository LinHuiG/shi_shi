//! Small runtime snapshots keep stream updates independent of business-page rendering.
use crate::{
    engine::App,
    model::{now, State},
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

impl App {
    pub(crate) fn core_revision(&self, s: &State) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.saves_revision(),
            now() / 60_000,
            s.revision,
            s.last_scan,
            self.busy.load(Ordering::SeqCst),
            self.ai_busy.load(Ordering::SeqCst),
            self.analyzing_messages.lock().unwrap().len(),
            s.archive_attempts
                .last()
                .map(|a| a.status.as_str())
                .unwrap_or(""),
            s.tasks.len()
        )
    }

    #[cfg(test)]
    pub(crate) fn snapshot_cached(
        &self,
        page: Option<&str>,
        known: Option<&str>,
        task: Option<&str>,
    ) -> Value {
        self.snapshot_request(&json!({"page":page,"core_revision":known,"task":task}))
    }

    pub(crate) fn snapshot_request(&self, request: &Value) -> Value {
        let page = request["page"].as_str();
        let task = request["task"].as_str();
        let selected = request["selected_task"].as_str();
        let log_day = request["log_day"].as_str();
        let scope = json!([page, task, selected, log_day]);
        let revision = {
            let state = self.state.lock().unwrap();
            format!("{}:{scope}", self.core_revision(&state))
        };
        let preview_revision = self.handling_preview_revision.load(Ordering::SeqCst);
        if let Some(known) = request["core_revision"].as_str() {
            if known == revision {
                let activity_revision = if page == Some("logs") {
                    Some(self.activity_day_revision(
                        log_day,
                        request["activity_day_revision"].get("audit"),
                    ))
                } else {
                    None
                };
                let mut value = self.snapshot_runtime(
                    task,
                    selected,
                    request["handling_revision"].as_u64() != Some(preview_revision),
                );
                value["_unchanged"] = json!(true);
                value["_page"] = json!(page);
                value["core_revision"] = json!(revision);
                value["handling_revision"] = json!(preview_revision);
                if let Some(revision) = activity_revision {
                    value["activity_day_revision"] = revision;
                }
                return value;
            }
        }
        let mut value = self.snapshot_page_scope(page, task, selected);
        // The projection takes its own state lock. Keep the revision produced
        // with that data so a concurrent commit cannot make fresh data stale.
        value["core_revision"] = json!(format!(
            "{}:{scope}",
            value["core_revision"].as_str().unwrap_or("")
        ));
        value["handling_revision"] = json!(preview_revision);
        if page == Some("logs") {
            value["activity_day_revision"] = self.activity_day_revision(log_day, None);
        }
        value
    }

    fn snapshot_runtime(
        &self,
        task: Option<&str>,
        selected: Option<&str>,
        previews_changed: bool,
    ) -> Value {
        let mut value = self.runtime_fields();
        let (interrupted, handling) = {
            let state = self.state.lock().unwrap();
            (
                state.ai_interrupted_at,
                if previews_changed {
                    self.live_handling(&state, task, selected)
                } else {
                    Vec::new()
                },
            )
        };
        value["ai_interrupted_at"] = json!(interrupted);
        value["handling_live"] = json!(handling);
        value
    }

    pub(crate) fn runtime_fields(&self) -> Value {
        let (request, cancelling, source) = {
            let active = self.active_ai.lock().unwrap();
            active
                .as_ref()
                .map(|a| {
                    (
                        a.id.clone(),
                        a.cancel.load(Ordering::SeqCst),
                        a.source.clone(),
                    )
                })
                .unwrap_or_default()
        };
        let mut value = json!({"ai_request_id":request,"ai_cancelling":cancelling,"ai_source":source,
            "ai_busy":self.ai_busy.load(Ordering::SeqCst),
            "busy":self.busy.load(Ordering::SeqCst),"ai_started":self.ai_started.load(Ordering::SeqCst),
            "updates":self.updates.snapshot(),"activity":*self.activity.lock().unwrap()});
        let live = self.ai_live.lock().unwrap();
        value["ai_progress"] = json!(live.phase);
        value["ai_live"] = json!(*live);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Task;

    #[test]
    fn stream_updates_use_small_snapshots_and_business_commits_invalidate_them() {
        let (_root, app) = crate::test_support::scoped_app("shishi-refresh");
        app.commit_state(|s| {
            s.tasks.push(crate::test_support::task("task", "Synthetic"));
            Ok(())
        })
        .unwrap();
        let first = app.state_version();
        assert_eq!(first, app.state_version());
        let full = app.snapshot_cached(Some("tasks"), None, None);
        assert_eq!(full["_page"], "tasks");
        assert!(full.get("reports").is_none());
        assert!(full["tasks"].is_array());
        app.ai_live
            .lock()
            .unwrap()
            .accept("phase", "Synthetic progress", false);
        assert_ne!(first, app.state_version());
        let small = app.snapshot_cached(Some("tasks"), full["core_revision"].as_str(), None);
        assert_eq!(small["_unchanged"], true);
        assert_eq!(small["ai_progress"], "Synthetic progress");
        assert_eq!(small["ai_progress"], small["ai_live"]["phase"]);
        for key in [
            "tasks", "messages", "config", "reports", "audit", "handling",
        ] {
            assert!(
                small.get(key).is_none(),
                "{key} must stay out of runtime snapshots"
            );
        }
        app.commit_state(|s| {
            s.tasks[0].title = "Saved title".into();
            Ok(())
        })
        .unwrap();
        let changed = app.snapshot_cached(Some("tasks"), full["core_revision"].as_str(), None);
        assert_ne!(changed["_unchanged"], true);
        assert_eq!(changed["tasks"][0]["title"], "Saved title");
        // A queued log changes only the selected day's log revision.
        let tasks = changed["core_revision"].as_str().unwrap();
        let logs = app.snapshot_cached(Some("logs"), None, None);
        app.log("synthetic", json!({}));
        assert_eq!(
            app.snapshot_cached(Some("tasks"), Some(tasks), None)["_unchanged"],
            true
        );
        let fresh_logs = app.snapshot_cached(Some("logs"), logs["core_revision"].as_str(), None);
        assert_eq!(fresh_logs["_unchanged"], true);
        assert_ne!(
            fresh_logs["activity_day_revision"],
            logs["activity_day_revision"]
        );
    }

    #[test]
    fn page_and_selection_scope_large_sections_without_losing_cross_page_selection() {
        let (_root, app) = crate::test_support::scoped_app("shishi-projection");
        app.commit_state(|state| {
            for id in ["one", "two"] {
                state.tasks.push(Task {
                    id: id.into(),
                    title: id.into(),
                    description: "large task detail".repeat(50),
                    ..Default::default()
                });
                state.handling.runs.push(crate::handling::Run {
                    id: format!("run-{id}"),
                    task_id: id.into(),
                    updated: 10,
                    rounds: vec![crate::handling::Round {
                        id: id.into(),
                        steps: vec![json!({"text":format!("full-history-{id}")})].into(),
                        ..Default::default()
                    }
                    .into()]
                    .into(),
                    ..Default::default()
                });
            }
            Ok(())
        })
        .unwrap();
        let settings = app.snapshot_request(&json!({"page":"settings"}));
        for key in [
            "messages",
            "handling",
            "evolution",
            "reports",
            "audit",
            "schedules",
        ] {
            assert!(settings.get(key).is_none(), "{key}");
        }
        assert!(settings["tasks"][0].get("description").is_none());
        let first =
            app.snapshot_request(&json!({"page":"tasks","task":"one","selected_task":"one"}));
        assert_eq!(
            first["handling"][0]["rounds"][0]["steps"][0]["text"],
            "full-history-one"
        );
        assert_eq!(first["handling"][1]["_details"], false);
        assert!(first["handling"][1].get("rounds").is_none());
        assert!(first["tasks"][1]["description"].is_string());
        let second = app.snapshot_request(&json!({"page":"tasks","task":"one","selected_task":"two","core_revision":first["core_revision"]}));
        assert_ne!(second["_unchanged"], true);
        assert!(second["handling"]
            .as_array()
            .unwrap()
            .iter()
            .all(|run| run["_details"] == true));
        let other_page = app.snapshot_request(&json!({"page":"logs","task":"one"}));
        assert_eq!(other_page["handling"].as_array().unwrap().len(), 1);
        assert_eq!(other_page["handling"][0]["task_id"], "one");
        assert!(other_page["tasks"][0]["description"].is_string());
        assert!(other_page["tasks"][1].get("description").is_none());
        assert!(app.snapshot()["messages"].is_array());
    }

    #[test]
    fn legacy_message_projection_keeps_latest_two_hundred_with_stable_ties() {
        let (_root, app) = crate::test_support::scoped_app("shishi-message-projection");
        {
            let mut state = app.state.lock().unwrap();
            for index in 0..1000 {
                let id = format!("message-{index:04}");
                state.messages.insert(
                    id.clone(),
                    crate::model::Message {
                        private: true,
                        ..crate::test_support::message(id, "synthetic message", (index / 3) as i64)
                    },
                );
            }
        }
        let snapshot = app.snapshot_sections(Some(&json!(["messages"])));
        let rows = snapshot["messages"].as_array().unwrap();
        let mut expected = (0..1000).collect::<Vec<_>>();
        expected.sort_by_key(|index| (std::cmp::Reverse(index / 3), *index));
        let ids = rows
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .take(200)
            .map(|index| format!("message-{index:04}"))
            .collect::<Vec<_>>();
        assert_eq!(ids, expected);
    }

    #[test]
    fn stale_generation_results_are_noops_without_an_extra_save() {
        let (_root, app) = crate::test_support::scoped_app("shishi-stale-result");
        let before = app.saves_revision();
        app.complete_generation(
            &Task {
                id: "missing".into(),
                ..Default::default()
            },
            Err("stale".into()),
        )
        .unwrap();
        assert_eq!(app.saves_revision(), before);
    }
}
