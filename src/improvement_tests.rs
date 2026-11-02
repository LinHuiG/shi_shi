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
    fn factory_defaults_use_general_parameters_without_personal_routes_or_paths() {
        let c = Config::default();
        assert!(c.enabled && c.auto_ai && c.message_filter_enabled && c.auto_update);
        assert_eq!(
            (
                c.interval_minutes,
                c.analysis_interval_minutes,
                c.analysis_lookback_hours
            ),
            (3, 30, 24)
        );
        assert_eq!(
            (c.daily_calls, c.max_input_chars, c.retrieval_budget_chars),
            (150, 80_000, 80_000)
        );
        assert_eq!(
            (
                c.work_start.as_str(),
                c.work_end.as_str(),
                c.ai_timeout_seconds
            ),
            ("08:00", "20:00", 600)
        );
        assert_eq!(c.message_retention_days, 90);
        assert!(!c.retain_ai_traces);
        assert!(
            c.codex.is_empty()
                && c.codex_models.is_empty()
                && c.ai_connections.is_empty()
                && c.leaders.is_empty()
        );
        assert_eq!(c.analysis_workdir, "ShiShi");
        for kind in PURPOSES {
            assert_eq!(
                c.for_kind(kind).selected_connection,
                UNCONFIGURED_CONNECTION
            );
        }
        let (root, app) = crate::test_support::scoped_app("shishi-factory");
        app.configure_patch(&json!({"daily_calls":149})).unwrap();
        drop(app);
        let loaded = App::load(root.clone()).unwrap();
        assert!(loaded.state.lock().unwrap().config.codex.is_empty());
        assert_eq!(
            loaded
                .state
                .lock()
                .unwrap()
                .config
                .for_kind("handling")
                .selected_connection,
            UNCONFIGURED_CONNECTION
        );
        drop(loaded);
    }

    #[test]
    fn unconfigured_operations_log_once_without_calls_cursors_drafts_or_runs() {
        let root = std::env::temp_dir().join(format!("shishi-unconfigured-{}", id()));
        let app = std::sync::Arc::new(App::load(root.clone()).unwrap());
        for kind in PURPOSES {
            app.activity.lock().unwrap().clear();
            assert!(app
                .ask_ai(kind, &json!({}))
                .unwrap_err()
                .starts_with("AI_UNCONFIGURED:"));
            let rows = app.activity.lock().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["detail"]["kind"], kind);
            assert_eq!(rows[0]["detail"]["failure_class"], "unconfigured");
        }
        for result in [
            app.handling_prepare("synthetic"),
            app.handling_start(&json!({"task_id":"synthetic"})),
            app.archive(crate::work_hours::AiMode::Manual),
        ] {
            assert!(result.unwrap_err().starts_with("AI_UNCONFIGURED:"));
        }
        assert!(app.create_generation(&json!({"text":"synthetic"})).is_err());
        let s = app.state.lock().unwrap();
        assert_eq!((s.calls, s.message_filter.calls), (0, 0));
        assert!(s.analyzed.is_empty() && s.message_filter.checked.is_empty());
        assert!(s.tasks.is_empty() && s.handling.runs.is_empty() && s.archive_attempts.is_empty());
        drop(s);
        app.activity.lock().unwrap().clear();
        let started = crate::ai_jobs::start(
            &app,
            &json!({"method":"format","params":{"text":"synthetic"}}),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let result = crate::ai_jobs::result(&started).unwrap();
            if result["done"] == true {
                assert_eq!(result["ok"], false);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.activity.lock().unwrap().len(), 1);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_sources_clears_only_their_routes_atomically_and_survives_restart() {
        let (root, app) = crate::test_support::scoped_app("shishi-clear-routes");
        {
            let mut s = app.state.lock().unwrap();
            s.config = serde_json::from_value(json!({"codex":"synthetic.exe","codex_models":[{"model":"synthetic"}],"ai_connections":[{"id":"api","name":"Synthetic","driver":"api","base_url":"https://example.com/v1","models":[{"model":"synthetic"}]}],"model_profiles":{"filter":null,"analysis":{"connection_id":"api","model":"synthetic"},"handling":{"model":"synthetic"},"review":{"connection_id":"api","model":"synthetic"}}})).unwrap();
            app.save(&s).unwrap();
        }
        crate::test_support::persistence_failure(&root, true).unwrap();
        let before = json!(app.state.lock().unwrap().config);
        assert!(app.delete_connection(&json!({"id":"api"})).is_err());
        assert_eq!(json!(app.state.lock().unwrap().config), before);
        crate::test_support::persistence_failure(&root, false).unwrap();
        let result = app.delete_connection(&json!({"id":"api"})).unwrap();
        assert!(result["config"]["model_profiles"]["analysis"].is_null());
        assert!(result["config"]["model_profiles"]["review"].is_null());
        assert!(result["config"]["model_profiles"]["handling"].is_object());
        app.configure_patch(&json!({"daily_calls":147})).unwrap();
        app.delete_connection(&json!({"id":""})).unwrap();
        drop(app);
        let restored = App::load(root.clone()).unwrap();
        let c = restored.state.lock().unwrap().config.clone();
        assert!(c.codex.is_empty() && c.ai_connections.is_empty());
        for kind in PURPOSES {
            assert_eq!(
                c.for_kind(kind).selected_connection,
                UNCONFIGURED_CONNECTION
            );
        }
        assert_eq!(c.daily_calls, 147);
        drop(restored);
    }

    #[test]
    fn settings_patch_preserves_unsubmitted_data_and_rejects_invalid_edits_atomically() {
        let (root, app) = crate::test_support::scoped_app("shishi-autosave");
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
    }

    #[test]
    fn analysis_scope_switch_persists_without_changing_progress_or_other_settings() {
        let (root, app) = crate::test_support::scoped_app("shishi-scope");
        {
            let mut s = app.state.lock().unwrap();
            assert!(!s.config.analysis_related_only);
            s.messages.insert(
                "plain".into(),
                crate::test_support::message("plain", "普通群消息", now()),
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
        assert!(s.config.message_filter_enabled);
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
    }
    #[test]
    fn context_budget_counts_errors_and_preserves_undelivered_offsets_across_restart() {
        let snapshot = json!({"budget_chars":4000,"records":[{"id":"m","kind":"message","data":{"text":"文".repeat(9000)}}]});
        let mut meter = Meter::default();
        let error_page = analysis_context::query(&snapshot, &mut meter, &json!({"invalid":true}));
        assert_eq!(meter.chars, error_page.chars().count());
        // An earlier error consumes budget, but must not discard a usable
        // final partial page. Only that delivered prefix advances coverage.
        let page = analysis_context::query(&snapshot, &mut meter, &json!({"ids":["m"]}));
        let delivered: serde_json::Value = serde_json::from_str(&page).unwrap();
        let offset = delivered["records"][0]["next_offset"].as_u64().unwrap();
        assert!(offset > 0 && offset < 9000);
        assert_eq!(meter.offsets["m"], offset as usize);
        assert_eq!(
            meter.chars,
            error_page.chars().count() + page.chars().count()
        );
        assert!(meter.chars <= 4000);
        let before = (meter.offsets.clone(), meter.pages.clone(), meter.chars);
        let mut restarted: Meter =
            serde_json::from_slice(&serde_json::to_vec(&meter).unwrap()).unwrap();
        let notice = analysis_context::query(
            &snapshot,
            &mut restarted,
            &json!({"ids":["m"],"offset":offset}),
        );
        assert!(notice.contains("覆盖不完整"));
        assert!(restarted.exhausted);
        assert_eq!(restarted.offsets, before.0);
        assert_eq!(restarted.pages, before.1);
        // The exhaustion notice is host control text, not retrieved material.
        assert_eq!(restarted.chars, before.2);
        assert!(restarted.complete.is_empty());
        let mut exhausted: Meter =
            serde_json::from_slice(&serde_json::to_vec(&restarted).unwrap()).unwrap();
        assert_eq!(
            analysis_context::query(&snapshot, &mut exhausted, &json!({"invalid":true})),
            notice
        );
        assert_eq!(exhausted.offsets, before.0);
        assert!(exhausted.complete.is_empty());
    }

    #[test]
    fn valid_primary_ignores_interrupted_temp_and_cleanup_backs_up_before_removal() {
        let root = std::env::temp_dir().join(format!("shishi-recovery-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&State::default()).unwrap(),
        )
        .unwrap();
        fs::write(root.join("data/state.next.json"), b"interrupted write").unwrap();
        assert!(crate::managed_data::inspect(&root).is_empty());
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
        let (root, app) = crate::test_support::scoped_app("shishi-scan");
        let enabled = app.state.lock().unwrap().config.enabled;
        let interval = app.state.lock().unwrap().config.interval_minutes;
        crate::test_support::persistence_failure(&root, true).unwrap();
        assert!(app
            .scan_control(&json!({"enabled":false,"interval_minutes":12}))
            .is_err());
        let state = app.state.lock().unwrap();
        assert_eq!(state.config.enabled, enabled);
        assert_eq!(state.config.interval_minutes, interval);
        drop(state);
    }

    #[test]
    fn settings_patch_save_failure_keeps_live_configuration_and_can_retry() {
        let (root, app) = crate::test_support::scoped_app("shishi-settings-failure");
        let before = json!(app.state.lock().unwrap().config);
        crate::test_support::persistence_failure(&root, true).unwrap();
        assert!(app.configure_patch(&json!({"daily_calls":42})).is_err());
        assert_eq!(json!(app.state.lock().unwrap().config), before);
        crate::test_support::persistence_failure(&root, false).unwrap();
        assert_eq!(
            app.configure_patch(&json!({"daily_calls":42})).unwrap()["config"]["daily_calls"],
            42
        );
    }

    #[test]
    fn archive_retention_keeps_current_lineage_unfinished_jobs_and_latest_outcome() {
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
        state.archive_attempts.push(ArchiveAttempt {
            id: "latest-completed".into(),
            created: old + 1,
            finished: old + 1,
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
        assert_eq!(
            state
                .archive_attempts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["latest-completed", "unfinished"]
        );
    }

    #[test]
    fn paused_scan_keeps_boundary_jobs_and_failed_claim_can_retry_once() {
        use crate::test_support::{calendar, ts_ms};
        for hour in [9, 18] {
            let (root, app) = crate::test_support::scoped_app("shishi-boundary");
            *app.calendar.lock().unwrap() = calendar();
            {
                let mut s = app.state.lock().unwrap();
                s.config.enabled = false;
                s.config.work_start = "09:00".into();
                s.config.work_end = "18:00".into();
            }
            app.work_boundary_tick(None, ts_ms(2025, 1, 2, hour - 1, 0))
                .unwrap();
            let previous = ts_ms(2025, 1, 2, hour - 1, 59) + 55_000;
            let boundary = ts_ms(2025, 1, 2, hour, 0) + 1_000;
            let before = app.state.lock().unwrap().work_scheduler.clone();
            crate::test_support::persistence_failure(&root, true).unwrap();
            assert!(app.work_boundary_tick(Some(previous), boundary).is_err());
            {
                let s = app.state.lock().unwrap();
                assert_eq!(
                    s.work_scheduler, before,
                    "failed save must not publish preview claims"
                );
                assert!(s.archive_attempts.is_empty());
                assert!(s.morning_jobs.is_empty());
            }
            crate::test_support::persistence_failure(&root, false).unwrap();
            app.work_boundary_tick(Some(previous), boundary + 5_000)
                .unwrap();
            app.work_boundary_tick(Some(boundary + 5_000), boundary + 10_000)
                .unwrap();
            {
                let s = app.state.lock().unwrap();
                assert_eq!(s.archive_attempts.len(), usize::from(hour == 18));
                assert_eq!(s.morning_jobs.len(), usize::from(hour == 9));
            }
            drop(app);
        }
    }
}
