use super::*;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

fn fixture(count: usize) -> (crate::test_support::TestRoot, App) {
    let (root, app) = crate::test_support::scoped_app("shishi-filter-batches");
    app.test_force_working();
    {
        let mut state = app.state.lock().unwrap();
        state.config.enabled = true;
        state.config.auto_ai = true;
        state.config.message_filter_enabled = true;
        state.config.model_profiles = None;
        // Only the filter is due; the full-analysis timer is not the trigger.
        state.last_analysis_at = now();
        state.analysis_pending_since = now();
        for index in 0..count {
            crate::message_progress::insert(&mut state, message(&format!("m{index:04}"), "body"));
        }
        // Make continuation tests deterministic through the real request
        // budget. Fifty is a fixture size, not a product batch limit.
        let rows = (0..50)
            .map(|index| {
                crate::context_budget::message(&message(&format!("m{index:04}"), "body"), 2_000)
            })
            .collect::<Vec<_>>();
        let mut context =
            app.capture_prepare_context_from(&state, "message_filter", true, json!([]));
        let request = app
            .prepare_ai_candidate_with(
                "message_filter",
                &json!({"changes":rows,"_trigger":"scan_batch"}),
                AiMode::Automatic,
                &mut context,
            )
            .unwrap();
        state.config.max_input_chars = request.prepared.total + 8;
        app.save(&state).unwrap();
    }
    (root, app)
}

fn message(id: &str, text: &str) -> Message {
    Message {
        private: true,
        ..crate::test_support::message(id, text, now())
    }
}

fn filtered_ids(app: &App) -> BTreeSet<String> {
    let state = app.state.lock().unwrap();
    state
        .messages
        .values()
        .filter(|message| crate::message_progress::filtered(&state, message))
        .map(|message| message.id.clone())
        .collect()
}

fn wait() -> serde_json::Value {
    json!({"decision":"wait","reason":"Synthetic non-actionable messages"})
}

#[test]
fn filter_drains_all_wait_batches_in_one_run_and_reuses_cancellation() {
    let (root, app) = fixture(105);
    let mut ids = Vec::new();
    let mut request_ids = BTreeSet::new();
    let mut token = None;
    let mut starts = BTreeSet::new();
    app.filter_tick_with(&mut |request, call| {
        assert!(app.ai_busy.load(Ordering::SeqCst));
        assert!(app.begin_ai_for("classify").is_err());
        assert!(request_ids.insert(call.id.clone()));
        if let Some(previous) = &token {
            assert!(Arc::ptr_eq(previous, &call.cancel));
        } else {
            token = Some(call.cancel.clone());
        }
        starts.insert(app.state.lock().unwrap().message_filter.last_run);
        let rows = request.payload["changes"].as_array().unwrap();
        assert!(!rows.is_empty());
        assert!(request.prepared.total <= request.config.max_input_chars);
        assert_eq!(
            request.payload["_batch_metrics"]["total_input_chars"],
            request.prepared.total
        );
        assert_eq!(
            request.payload["_batch_metrics"]["max_input_chars"],
            request.config.max_input_chars
        );
        ids.extend(
            rows.iter()
                .map(|row| row["id"].as_str().unwrap().to_owned()),
        );
        app.reserve_ai_inner(AiMode::Automatic, "message_filter", None, None, None)?;
        Ok(wait())
    })
    .unwrap();
    assert_eq!(request_ids.len(), 3);
    assert_eq!(ids.len(), 105);
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 105);
    assert_eq!(
        starts.len(),
        1,
        "continuation must not renew the start gate"
    );
    assert_eq!(filtered_ids(&app).len(), 105);
    assert!(!app.ai_busy.load(Ordering::SeqCst));
    {
        let state = app.state.lock().unwrap();
        assert!(!state.message_filter.early);
        assert_eq!(state.message_filter.calls, 3);
        assert!(state.analyzed.is_empty());
        assert_eq!(state.analysis_updated_at, 0);
    }
    drop(app);
    let restored = App::load(root.clone()).unwrap();
    assert_eq!(filtered_ids(&restored).len(), 105);
    drop(restored);
}

#[test]
fn positive_uncertain_and_failed_filter_results_handoff_without_tail_calls() {
    for decision in ["analyze", "uncertain", "failure"] {
        let (_root, app) = fixture(105);
        let mut calls = 0;
        let mut first = BTreeSet::new();
        app.filter_tick_with(&mut |request, _| {
            calls += 1;
            if calls == 1 {
                first.extend(
                    request.payload["changes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| row["id"].as_str().unwrap().to_owned()),
                );
                return Ok(wait());
            }
            if decision == "failure" {
                Err("synthetic provider failure".into())
            } else {
                Ok(json!({"decision":decision,"reason":"Synthetic actionable reply"}))
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        let state = app.state.lock().unwrap();
        assert!(state.message_filter.early);
        assert_eq!(
            state.message_filter.trigger,
            if decision == "failure" {
                "message_filter_failed"
            } else {
                "message_filter"
            }
        );
        assert!(crate::context_budget::automatic_due(
            &state,
            &crate::context_budget::pending(&state),
            now(),
            now()
        ));
        drop(state);
        assert_eq!(
            filtered_ids(&app).len(),
            if decision == "failure" {
                first.len()
            } else {
                100
            }
        );
        assert!(!app.ai_busy.load(Ordering::SeqCst));
    }
}

#[test]
fn boundary_never_chases_new_messages_or_acknowledges_changed_versions() {
    let (_root, app) = fixture(105);
    let mut seen = BTreeSet::new();
    let mut calls = 0;
    app.filter_tick_with(&mut |request, _| {
        calls += 1;
        seen.extend(
            request.payload["changes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_owned()),
        );
        if calls == 1 {
            let mut state = app.state.lock().unwrap();
            crate::message_progress::insert(&mut state, message("new-arrival", "new message"));
            crate::message_progress::insert(&mut state, message("m0000", "edited in flight"));
            crate::message_progress::insert(
                &mut state,
                message("m0055", "edited before its batch"),
            );
            app.save(&state).unwrap();
        }
        Ok(wait())
    })
    .unwrap();
    assert_eq!(calls, 3);
    assert!(!seen.contains("new-arrival"));
    assert!(!seen.contains("m0055"));
    let filtered = filtered_ids(&app);
    assert_eq!(filtered.len(), 103);
    for id in ["new-arrival", "m0000", "m0055"] {
        assert!(!filtered.contains(id));
    }
}

#[test]
fn cancellation_preserves_progress_and_schedule_even_after_a_late_response() {
    for late_response in [false, true] {
        let (_root, app) = fixture(105);
        if late_response {
            app.state.lock().unwrap().config.ai_timeout_seconds = 1;
        }
        let mut calls = 0;
        app.filter_tick_with(&mut |_, call| {
            calls += 1;
            if calls == 2 {
                // Simulate a round that started over a minute ago, without
                // sleeping or changing the system clock. The start gate alone
                // cannot protect this cancelled run from a 200 ms retry.
                app.state.lock().unwrap().message_filter.last_run = now() - 60_001;
                app.interrupt_ai(&call.id)?;
                if late_response {
                    std::thread::sleep(Duration::from_millis(1_100));
                }
            }
            Ok(wait())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(filtered_ids(&app).len(), 50);
        let state = app.state.lock().unwrap();
        assert!(!state.message_filter.early);
        assert_eq!(state.message_filter.reason, crate::ai_control::CANCELLED);
        assert!(state.analysis_interrupted_at > 0);
        assert_eq!(
            crate::context_budget::next_interval_at(
                state.message_filter.last_run,
                60_000,
                state.analysis_interrupted_at,
            ),
            state.message_filter.last_run + 120_000,
        );
        drop(state);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        app.filter_tick_with(&mut |_, _| panic!("cancelled run must not retry immediately"))
            .unwrap();
    }
}

#[test]
fn expired_acceptance_preserves_prior_filter_progress_without_acknowledging_the_batch() {
    let (root, app) = fixture(2);
    let fingerprints = {
        let mut state = app.state.lock().unwrap();
        let prior = fingerprint(&state.messages["m0000"]);
        state.message_filter.checked.insert("m0000".into(), prior);
        vec![("m0001".into(), fingerprint(&state.messages["m0001"]))]
    };
    let authority = filter_authority(&app.state.lock().unwrap());
    let call = app.begin_ai_for("message_filter").unwrap();
    {
        let _acceptance = app.active_ai.lock().unwrap();
        assert!(!app
            .commit_filter_batch(
                &fingerprints,
                Ok(wait()),
                true,
                &call,
                std::time::Instant::now(),
                &authority,
            )
            .unwrap());
    }
    drop(call);
    assert_eq!(filtered_ids(&app), BTreeSet::from(["m0000".into()]));
    {
        let state = app.state.lock().unwrap();
        assert!(state.message_filter.early);
        assert_eq!(state.message_filter.trigger, "message_filter_failed");
        assert!(state.message_filter.reason.contains(FILTER_TIMEOUT));
    }
    drop(app);
    let restored = App::load(root.clone()).unwrap();
    assert_eq!(filtered_ids(&restored), BTreeSet::from(["m0000".into()]));
    drop(restored);
}

#[test]
fn failed_batch_commit_does_not_publish_progress_and_preserves_prior_batches() {
    let (root, app) = fixture(105);
    let mut calls = 0;
    let result = app.filter_tick_with(&mut |_, _| {
        calls += 1;
        if calls == 2 {
            crate::test_support::persistence_failure(&root, true)?;
        }
        Ok(wait())
    });
    assert!(result.is_err());
    assert_eq!(calls, 2);
    assert_eq!(filtered_ids(&app).len(), 50);
    assert!(!app.ai_busy.load(Ordering::SeqCst));
    crate::test_support::persistence_failure(&root, false).unwrap();
    drop(app);
    let restored = App::load(root.clone()).unwrap();
    assert_eq!(filtered_ids(&restored).len(), 50);
    drop(restored);
}

#[test]
fn shared_filter_deadline_limits_later_batch_and_keeps_committed_progress() {
    let (_root, app) = fixture(105);
    app.state.lock().unwrap().config.ai_timeout_seconds = 3;
    let mut calls = 0;
    app.filter_tick_with(&mut |request, _| {
        calls += 1;
        if calls == 1 {
            std::thread::sleep(Duration::from_millis(900));
        } else {
            assert!(request.config.ai_timeout_seconds <= 2);
            std::thread::sleep(Duration::from_millis(2200));
        }
        Ok(wait())
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(filtered_ids(&app).len(), 50);
    let state = app.state.lock().unwrap();
    assert!(state.message_filter.early);
    assert_eq!(state.message_filter.trigger, "message_filter_failed");
    assert!(state.message_filter.reason.contains("总时限"));
    drop(state);
    assert!(!app.ai_busy.load(Ordering::SeqCst));
}

#[test]
fn busy_slot_does_not_consume_gate_but_started_rounds_keep_the_cooldown() {
    let (_root, app) = fixture(1);
    let other = app.begin_ai_for("classify").unwrap();
    app.filter_tick_with(&mut |_, _| panic!("busy filter cannot invoke"))
        .unwrap();
    assert_eq!(app.state.lock().unwrap().message_filter.last_run, 0);
    drop(other);
    app.filter_tick_with(&mut |_, _| Ok(wait())).unwrap();
    assert!(app.state.lock().unwrap().message_filter.last_run > 0);
    crate::message_progress::insert(&mut app.state.lock().unwrap(), message("late", "later"));
    app.filter_tick_with(&mut |_, _| panic!("new round must retain its frequency gate"))
        .unwrap();
    assert!(!filtered_ids(&app).contains("late"));
}

#[test]
fn disabled_unconfigured_and_work_gate_stops_do_not_trigger_full_analysis() {
    for stop in ["disabled", "unconfigured", "work_gate"] {
        let (_root, app) = fixture(105);
        let mut calls = 0;
        app.filter_tick_with(&mut |_, _| {
            calls += 1;
            if calls == 1 {
                return Ok(wait());
            }
            match stop {
                "disabled" => {
                    app.state.lock().unwrap().config.message_filter_enabled = false;
                    Ok(wait())
                }
                "unconfigured" => Err("AI_UNCONFIGURED:synthetic missing route".into()),
                _ => Err(crate::work_hours::DEFERRED.into()),
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(filtered_ids(&app).len(), 50);
        assert!(!app.state.lock().unwrap().message_filter.early);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
    }
}

#[test]
fn oversized_single_filter_row_falls_back_without_an_empty_model_call() {
    let (_root, app) = fixture(0);
    let mut oversized = message("oversized", "body");
    oversized.chat = "chat".repeat(app.state.lock().unwrap().config.max_input_chars);
    crate::message_progress::insert(&mut app.state.lock().unwrap(), oversized);
    app.filter_tick_with(&mut |_, _| panic!("oversized row must not issue an empty request"))
        .unwrap();
    assert!(filtered_ids(&app).is_empty());
    assert!(app.state.lock().unwrap().message_filter.early);
    assert!(!app.ai_busy.load(Ordering::SeqCst));
}

#[test]
fn large_filter_budget_uses_more_than_fifty_rows_and_ten_thousand_characters() {
    let (_root, app) = fixture(105);
    app.state.lock().unwrap().config.max_input_chars *= 3;
    let mut calls = 0;
    app.filter_tick_with(&mut |request, _| {
        calls += 1;
        assert_eq!(request.payload["changes"].as_array().unwrap().len(), 105);
        assert!(request.payload["changes"].to_string().chars().count() > 10_000);
        assert!(request.prepared.total <= request.config.max_input_chars);
        assert_eq!(request.payload["_batch_metrics"]["reduced_by"], json!([]));
        Ok(wait())
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(filtered_ids(&app).len(), 105);
}

#[test]
fn filter_fits_largest_prefix_including_fixed_overhead_and_honors_stop_checks() {
    let (_root, app) = fixture(0);
    let rows = (0..70)
        .map(|i| {
            crate::context_budget::message(&message(&format!("m{i:04}"), "含\"转义\\的正文"), 2_000)
        })
        .collect::<Vec<_>>();
    let mut state = app.state.lock().unwrap().clone();
    let mut context = app.capture_prepare_context_from(&state, "message_filter", true, json!([]));
    let whole = app
        .prepare_ai_candidate_with(
            "message_filter",
            &json!({"changes":rows,"_trigger":"scan_batch"}),
            AiMode::Automatic,
            &mut context,
        )
        .unwrap();
    let mut exact_context =
        app.capture_prepare_context_from(&state, "message_filter", true, json!([]));
    // Start with the same full candidate to freeze its runtime background.
    app.prepare_ai_candidate_with(
        "message_filter",
        &json!({"changes":rows,"_trigger":"scan_batch"}),
        AiMode::Automatic,
        &mut exact_context,
    )
    .unwrap();
    let exact = app
        .prepare_ai_candidate_with(
            "message_filter",
            &json!({"changes":&rows[..31],"_trigger":"scan_batch"}),
            AiMode::Automatic,
            &mut exact_context,
        )
        .unwrap();
    assert!(whole.prepared.total > exact.prepared.total);
    state.config.max_input_chars = exact.prepared.total;
    let mut context = app.capture_prepare_context_from(&state, "message_filter", true, json!([]));
    let call = app.begin_ai_for("message_filter").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let (selected, request) = app
        .fit_filter_batch(&rows, &mut context, &call, deadline)
        .unwrap();
    assert_eq!(selected, 31);
    assert_eq!(request.prepared.total, state.config.max_input_chars);
    let next = app
        .prepare_ai_candidate_with(
            "message_filter",
            &json!({"changes":&rows[..32],"_trigger":"scan_batch"}),
            AiMode::Automatic,
            &mut context,
        )
        .unwrap();
    assert!(next.prepared.total > next.config.max_input_chars);
    assert!(
        filtered_ids(&app).is_empty(),
        "packing alone must not acknowledge messages"
    );

    state.config.max_input_chars = 1;
    let mut tiny = app.capture_prepare_context_from(&state, "message_filter", true, json!([]));
    assert!(app
        .fit_filter_batch(&rows[..1], &mut tiny, &call, deadline)
        .err()
        .unwrap()
        .starts_with(crate::ai::INPUT_LIMIT_ERROR));
    assert!(app
        .fit_filter_batch(&rows, &mut context, &call, std::time::Instant::now())
        .err()
        .unwrap()
        .contains(FILTER_TIMEOUT));
    call.cancel.store(true, Ordering::SeqCst);
    assert_eq!(
        app.fit_filter_batch(&rows, &mut context, &call, deadline)
            .err()
            .unwrap(),
        crate::ai_control::CANCELLED
    );
    drop(call);
}

#[test]
fn changed_filter_configuration_or_source_rejects_stale_batch_without_consuming_tail() {
    for change_source in [false, true] {
        let (_root, app) = fixture(105);
        let mut calls = 0;
        app.filter_tick_with(&mut |_, _| {
            calls += 1;
            let mut state = app.state.lock().unwrap();
            if change_source {
                state.chat_sources.push(ChatSource {
                    id: "new-source".into(),
                    ..Default::default()
                });
            } else {
                state.config.max_input_chars += 1;
            }
            Ok(wait())
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(filtered_ids(&app).is_empty());
        assert!(!app.state.lock().unwrap().message_filter.early);
    }
}

#[test]
fn unrelated_prompts_models_and_trace_preferences_do_not_interrupt_filter_batches() {
    let (_root, app) = fixture(105);
    let mut calls = 0;
    app.filter_tick_with(&mut |_, _| {
        calls += 1;
        let mut state = app.state.lock().unwrap();
        state.config.classify_prompt.push_str("分析表达变化");
        state.config.archive_prompt.push_str("归档表达变化");
        state.config.retain_ai_traces = !state.config.retain_ai_traces;
        state
            .config
            .codex_models
            .push(json!({"model":format!("unused-{calls}")}));
        state.config.model_profiles = Some(ModelProfiles {
            filter: Some(ModelChoice::default()),
            analysis: Some(ModelChoice {
                model: format!("unused-{calls}"),
                ..Default::default()
            }),
            ..Default::default()
        });
        Ok(wait())
    })
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(filtered_ids(&app).len(), 105);
}
