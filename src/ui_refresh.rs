//! Small runtime snapshots keep stream updates independent of business-page rendering.
use crate::{
    engine::App,
    model::{now, State},
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

impl App {
    pub(crate) fn core_revision(&self, s: &State, page: Option<&str>) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
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
            if page == Some("logs") {
                self.activity_revision.load(Ordering::SeqCst)
            } else {
                0
            },
            s.tasks.len()
        )
    }

    pub(crate) fn snapshot_cached(
        &self,
        page: Option<&str>,
        known: Option<&str>,
        task: Option<&str>,
    ) -> Value {
        if let Some(known) = known {
            let revision = {
                let state = self.state.lock().unwrap();
                self.core_revision(&state, page)
            };
            if known == revision {
                let mut value = self.snapshot_runtime(task);
                value["_unchanged"] = json!(true);
                value["_page"] = json!(page);
                value["core_revision"] = json!(revision);
                return value;
            }
        }
        self.snapshot_page(page)
    }

    fn snapshot_runtime(&self, task: Option<&str>) -> Value {
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
        let (interrupted, handling) = {
            let state = self.state.lock().unwrap();
            (state.ai_interrupted_at, self.live_handling(&state, task))
        };
        json!({"ai_request_id":request,"ai_cancelling":cancelling,"ai_source":source,
            "ai_interrupted_at":interrupted,"ai_busy":self.ai_busy.load(Ordering::SeqCst),
            "busy":self.busy.load(Ordering::SeqCst),"ai_started":self.ai_started.load(Ordering::SeqCst),
            "ai_progress":*self.ai_progress.lock().unwrap(),"ai_live":*self.ai_live.lock().unwrap(),
            "updates":self.updates.snapshot(),"activity":*self.activity.lock().unwrap(),"handling_live":handling})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{id, Task};

    #[test]
    fn stream_updates_use_small_snapshots_and_business_commits_invalidate_them() {
        let root = std::env::temp_dir().join(format!("shishi-refresh-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.commit_state(|s| {
            s.tasks.push(Task {
                id: "task".into(),
                title: "Synthetic".into(),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
        let full = app.snapshot_cached(Some("tasks"), None, None);
        assert!(full["tasks"].is_array());
        *app.ai_progress.lock().unwrap() = "Synthetic progress".into();
        let small = app.snapshot_cached(Some("tasks"), full["core_revision"].as_str(), None);
        assert_eq!(small["_unchanged"], true);
        assert_eq!(small["ai_progress"], "Synthetic progress");
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
        // A queued log invalidates the logs page, without changing business pages.
        let tasks = changed["core_revision"].as_str().unwrap();
        let logs = app.snapshot_page(Some("logs"));
        app.log("synthetic", json!({}));
        assert_eq!(
            app.snapshot_cached(Some("tasks"), Some(tasks), None)["_unchanged"],
            true
        );
        assert!(
            app.snapshot_cached(Some("logs"), logs["core_revision"].as_str(), None)["audit"]
                .is_array()
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_generation_results_are_noops_without_an_extra_save() {
        let root = std::env::temp_dir().join(format!("shishi-stale-result-{}", id()));
        let app = App::load(root.clone()).unwrap();
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
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
