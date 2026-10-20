//! Offline regressions for the coordinated settings/retention/recovery release.
#[cfg(test)]
mod tests {
    use crate::{
        analysis_context::{self, Meter},
        engine::App,
        model::*,
    };
    use serde_json::json;
    use std::fs;

    #[test]
    fn settings_patch_preserves_unsubmitted_data_and_rejects_invalid_edits_atomically() {
        let root = std::env::temp_dir().join(format!("shishi-autosave-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let before = json!(app.state.lock().unwrap().config);
        let saved = app.configure_patch(&json!({"daily_calls":43})).unwrap();
        assert_eq!(saved["config"]["daily_calls"], 43);
        let mut expected = before.clone();
        expected["daily_calls"] = json!(43);
        assert_eq!(saved["config"], expected);
        assert!(app
            .configure_patch(&json!({"daily_calls":44,"interval_minutes":0}))
            .is_err());
        assert!(app
            .configure_patch(&json!({"classify_prompt":"unexpected"}))
            .is_err());
        assert_eq!(json!(app.state.lock().unwrap().config), expected);
        let route = before["model_profiles"]["analysis"].clone();
        let saved = app
            .configure_patch(&json!({"model_profiles":{"analysis":route}}))
            .unwrap();
        assert_eq!(
            saved["config"]["model_profiles"]["review"],
            before["model_profiles"]["review"]
        );
        drop(app);
        let loaded = App::load(root.clone()).unwrap();
        assert_eq!(loaded.state.lock().unwrap().config.daily_calls, 43);
        drop(loaded);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn analysis_scope_switch_persists_without_changing_progress_or_other_settings() {
        let root = std::env::temp_dir().join(format!("shishi-scope-{}", id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            assert!(!s.config.analysis_related_only);
            s.messages.insert(
                "plain".into(),
                Message {
                    id: "plain".into(),
                    text: "普通群消息".into(),
                    time: now(),
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        let saved = app
            .configure_patch(&json!({"analysis_related_only":true}))
            .unwrap();
        assert_eq!(saved["config"]["analysis_related_only"], true);
        assert_eq!(app.snapshot()["pending"], 0);
        let loaded = App::load(root.clone()).unwrap();
        assert!(loaded.state.lock().unwrap().config.analysis_related_only);
        drop(loaded);
        app.configure_patch(&json!({"analysis_related_only":false}))
            .unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.analyzed.is_empty());
        assert!(!s.config.message_filter_enabled);
        assert!(s.pending_analysis);
        assert_eq!(s.messages["plain"].text, "普通群消息");
        drop(s);
        assert_eq!(app.snapshot()["pending"], 1);
        assert!(app
            .configure_patch(&json!({"auto_adopt_prompt":true}))
            .is_err());
        assert!(app
            .configure_patch(&json!({"auto_adopt_advice":true}))
            .is_err());
        drop(app);
        let reload = App::load(root.clone()).unwrap();
        assert!(!reload.state.lock().unwrap().config.analysis_related_only);
        drop(reload);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn context_budget_counts_errors_and_preserves_undelivered_offsets_across_restart() {
        let snapshot = json!({"budget_chars":4000,"records":[{"id":"m","kind":"message","data":{"text":"文".repeat(9000)}}]});
        let mut meter = Meter::default();
        let error_page =
            analysis_context::query(&snapshot, &mut meter, &json!({"invalid":true})).unwrap();
        assert_eq!(meter.chars, error_page.chars().count());
        let before = meter.offsets.clone();
        let notice = analysis_context::query(&snapshot, &mut meter, &json!({"ids":["m"]})).unwrap();
        assert!(notice.contains("覆盖不完整"));
        assert!(meter.exhausted);
        assert_eq!(meter.offsets, before);
        let mut restarted: Meter =
            serde_json::from_slice(&serde_json::to_vec(&meter).unwrap()).unwrap();
        assert!(
            analysis_context::query(&snapshot, &mut restarted, &json!({"ids":["m"]}))
                .unwrap()
                .contains("覆盖不完整")
        );
        assert!(restarted.chars <= 4000);
        assert!(restarted.complete.is_empty());
    }

    #[test]
    fn valid_primary_ignores_interrupted_temp_and_cleanup_backs_up_before_removal() {
        let root = std::env::temp_dir().join(format!("shishi-recovery-{}", id()));
        let app = App::load(root.clone()).unwrap();
        fs::write(root.join("data/state.next.json"), b"interrupted write").unwrap();
        assert!(crate::managed_data::inspect(&root).is_empty());
        drop(app);
        fs::write(root.join("data/state.json"), b"incompatible").unwrap();
        let inspection = crate::managed_data::inspect(&root);
        assert!(inspection
            .incompatible
            .iter()
            .any(|f| f.relative == "data/state.next.json"));
        crate::managed_data::cleanup(&root, &inspection).unwrap();
        let backup = fs::read_dir(root.join("data/legacy"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::read(backup.join("state.json")).unwrap(),
            b"incompatible"
        );
        assert!(!root.join("data/state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_primary_does_not_silently_discard_a_valid_recovery_candidate() {
        let root = std::env::temp_dir().join(format!("shishi-candidate-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/state.next.json"),
            serde_json::to_vec(&State::default()).unwrap(),
        )
        .unwrap();
        let inspection = crate::managed_data::inspect(&root);
        assert_eq!(inspection.incompatible.len(), 1);
        assert!(crate::managed_data::startup_with(&root, false, |_| false).is_err());
        assert!(root.join("data/state.next.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_save_failure_does_not_publish_false_toggle_or_change_other_config() {
        let root = std::env::temp_dir().join(format!("shishi-scan-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let enabled = app.state.lock().unwrap().config.enabled;
        let interval = app.state.lock().unwrap().config.interval_minutes;
        fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app
            .scan_control(&json!({"enabled":false,"interval_minutes":12}))
            .is_err());
        let state = app.state.lock().unwrap();
        assert_eq!(state.config.enabled, enabled);
        assert_eq!(state.config.interval_minutes, interval);
        drop(state);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_patch_save_failure_keeps_live_configuration_and_can_retry() {
        let root = std::env::temp_dir().join(format!("shishi-settings-failure-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let before = json!(app.state.lock().unwrap().config);
        fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app.configure_patch(&json!({"daily_calls":42})).is_err());
        assert_eq!(json!(app.state.lock().unwrap().config), before);
        fs::remove_dir(root.join("data/state.next.json")).unwrap();
        assert_eq!(
            app.configure_patch(&json!({"daily_calls":42})).unwrap()["config"]["daily_calls"],
            42
        );
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_retention_keeps_current_transitive_lineage_and_unfinished_jobs() {
        let mut state = State::default();
        let at = now();
        let old = at - 100 * 86_400_000;
        for (id, restored, created) in [
            ("drop", "", old),
            ("base", "", old),
            ("current", "base", at),
        ] {
            state.archive_versions.push(ArchiveGroup {
                id: id.into(),
                restored_from: restored.into(),
                created,
                ..Default::default()
            });
        }
        state.archive_attempts.push(ArchiveAttempt {
            id: "unfinished".into(),
            created: old,
            status: "queued".into(),
            ..Default::default()
        });
        state.archive_attempts.push(ArchiveAttempt {
            id: "drop".into(),
            created: old,
            status: "succeeded".into(),
            ..Default::default()
        });
        state.archive_attempts.reverse();
        assert!(crate::retention::prune(&mut state, at));
        assert_eq!(
            state
                .archive_versions
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["base", "current"]
        );
        assert_eq!(state.archive_attempts.len(), 1);
        assert_eq!(state.archive_attempts[0].id, "unfinished");
    }

    #[test]
    fn view_version_is_stable_between_idle_ticks_and_progress_invalidates_it() {
        let root = std::env::temp_dir().join(format!("shishi-signals-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let first = app.state_version();
        assert_eq!(first, app.state_version());
        *app.ai_progress.lock().unwrap() = "synthetic progress".into();
        assert_ne!(first, app.state_version());
        let tasks = app.snapshot_page(Some("tasks"));
        assert_eq!(tasks["_page"], "tasks");
        assert!(tasks.get("reports").is_none());
        assert!(tasks["tasks"].is_array());
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paused_scan_keeps_boundary_jobs_and_failed_claim_can_retry_once() {
        use crate::test_support::{calendar, ts_ms};
        let root = std::env::temp_dir().join(format!("shishi-boundary-{}", id()));
        let app = App::load(root.clone()).unwrap();
        *app.calendar.lock().unwrap() = calendar();
        {
            let mut s = app.state.lock().unwrap();
            s.config.enabled = false;
            s.config.work_start = "09:00".into();
            s.config.work_end = "18:00".into();
        }
        let at = ts_ms(2025, 1, 2, 10, 0);
        app.work_boundary_tick(None, at).unwrap();
        let previous = ts_ms(2025, 1, 2, 17, 59) + 55_000;
        let end = ts_ms(2025, 1, 2, 18, 0) + 1_000;
        fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app.work_boundary_tick(Some(previous), end).is_err());
        assert!(app.state.lock().unwrap().archive_attempts.is_empty());
        fs::remove_dir(root.join("data/state.next.json")).unwrap();
        app.work_boundary_tick(Some(previous), end + 5_000).unwrap();
        assert_eq!(app.state.lock().unwrap().archive_attempts.len(), 1);
        app.work_boundary_tick(Some(end + 5_000), end + 10_000)
            .unwrap();
        assert_eq!(app.state.lock().unwrap().archive_attempts.len(), 1);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
}
