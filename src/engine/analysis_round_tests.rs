//! One frozen backlog, serial requests, and durable per-batch commits. No provider calls.
use super::*;
use std::sync::Arc;

fn fixture() -> (crate::test_support::TestRoot, App) {
    let (root, app) = crate::test_support::scoped_app("ss-analysis-round");
    app.commit_state(|s| {
        s.config.model_profiles = None;
        s.config.codex = root.join("must-not-launch.exe").to_string_lossy().into();
        s.config.analysis_interval_minutes = 120;
        let at = now() - 60_000;
        for i in 0..23 {
            let id = format!("m{i:02}");
            s.messages.insert(
                id.clone(),
                Message {
                    id,
                    source_id: "synthetic-source".into(),
                    chat: "synthetic-room".into(),
                    text: format!("合成材料 {i}：{}", "核对资料".repeat(440)),
                    private: true,
                    time: at + i,
                    updated_at: at + i,
                    ..Default::default()
                },
            );
        }
        Ok(())
    })
    .unwrap();
    // Fit ten full messages with room for runtime metadata. The remaining
    // backlog is deliberately larger than that exact prepared request.
    let limit = {
        let s = app.state.lock().unwrap();
        let changes = s.messages.values().take(10).cloned().collect::<Vec<_>>();
        let batch = crate::context_budget::classify(&s, &changes);
        let mut context = app.capture_prepare_context_from(&s, "classify", false, json!([]));
        app.prepare_ai_request_with("classify", &batch.payload, AiMode::Manual, &mut context)
            .unwrap()
            .prepared
            .total
            + 1000
    };
    app.state.lock().unwrap().config.max_input_chars = limit;
    (root, app)
}

fn ids(request: &PreparedAiRequest) -> Vec<String> {
    request.tag_payload["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

fn reserve(app: &App, request: &PreparedAiRequest, mode: AiMode) -> Result<(), String> {
    ai::validate_prepared(&request.config, &request.payload, &request.prepared)?;
    let frozen = request.frozen.as_ref().unwrap();
    let messages = request.tag_payload["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &frozen.initial_messages[row["id"].as_str().unwrap()])
        .collect::<Vec<_>>();
    app.reserve_ai_inner(
        mode,
        "classify",
        None,
        request.payload["_filter_decision_at"].as_i64(),
        Some(&messages),
    )?;
    Ok(())
}

fn create(source: &str, title: &str) -> Value {
    json!({"action":"create","sources":[source],"fields":[
        {"field":"title","value":title},{"field":"description","value":"合成进展"}],
        "tag_changes":[],"confidence":0.99,"reason":"合成材料中的新事项"})
}

#[test]
fn validated_analysis_conclusions_and_actions_commit_and_log_without_extra_calls() {
    for (reason, action_count) in [
        ("本批只有普通确认，无需新增或修改事项".to_owned(), 0),
        (ai::DEGRADED_ANALYSIS_REASON.to_owned(), 0),
        ("🙂".repeat(ai::NO_ACTION_REASON_LIMIT), 0),
        (String::new(), 1),
    ] {
        let (root, app) = fixture();
        app.commit_state(|s| {
            s.messages.retain(|id, _| id == "m00");
            Ok(())
        })
        .unwrap();
        let actions = if action_count == 0 {
            vec![]
        } else {
            vec![create("m00", "按合成证据创建")]
        };
        let mut calls = 0;
        let outcome = app
            .run_analysis_with(AiMode::Manual, false, &mut |request, _| {
                calls += 1;
                reserve(&app, request, AiMode::Manual)?;
                Ok(json!({"actions":actions,"no_action_reason":reason,"_analysis_degraded":reason == ai::DEGRADED_ANALYSIS_REASON}))
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(outcome["processed"], 1);
        assert_eq!(outcome["applied"], action_count);
        {
            let state = app.state.lock().unwrap();
            assert_eq!(state.tasks.len(), action_count);
            assert!(crate::context_budget::pending(&state).is_empty());
            assert!(crate::message_progress::analyzed(
                &state,
                &state.messages["m00"]
            ));
        }
        app.log_writer.flush().unwrap();
        drop(app);
        let reopened = App::load(root.clone()).unwrap();
        let activity = reopened.activity.lock().unwrap();
        let logged = activity
            .iter()
            .find(|row| row["kind"] == "analysis_result")
            .unwrap();
        assert_eq!(logged["detail"]["no_action_reason"], reason);
        assert_eq!(
            logged["detail"]["analysis_degraded"],
            reason == ai::DEGRADED_ANALYSIS_REASON
        );
        assert!(!logged["detail"]["request_id"].as_str().unwrap().is_empty());
        drop(activity);
        drop(reopened);
    }
}

#[test]
fn empty_analysis_explanations_do_not_commit_on_cancel_or_save_failure() {
    for stop in ["cancel", "save"] {
        let (root, app) = fixture();
        app.commit_state(|s| {
            s.messages.retain(|id, _| id == "m00");
            Ok(())
        })
        .unwrap();
        let mut calls = 0;
        let error = app
            .run_analysis_with(AiMode::Manual, false, &mut |request, call| {
                calls += 1;
                reserve(&app, request, AiMode::Manual)?;
                if stop == "cancel" {
                    app.interrupt_ai(&call.id)?;
                } else {
                    crate::test_support::persistence_failure(&root, true)?;
                }
                Ok(json!({"actions":[],"no_action_reason":"合成确认消息，无需事项操作"}))
            })
            .unwrap_err();
        if stop == "save" {
            crate::test_support::persistence_failure(&root, false).unwrap();
            assert!(error.contains("synthetic commit rejection"), "{error}");
        } else {
            assert_eq!(error, crate::ai_control::CANCELLED);
        }
        assert_eq!(calls, 1);
        assert!(app.state.lock().unwrap().analyzed.is_empty());
        assert!(!app
            .activity
            .lock()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "analysis_result"));
        drop(app);
        let reopened = App::load(root.clone()).unwrap();
        assert!(reopened.state.lock().unwrap().analyzed.is_empty());
        drop(reopened);
    }
}

#[test]
fn candidate_reductions_share_records_and_aliases_until_materialization() {
    let (_root, app) = fixture();
    let state = app.state.lock().unwrap().clone();
    let changes = state
        .messages
        .values()
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    let first = crate::context_budget::classify(&state, &changes);
    let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
    let first = app
        .prepare_ai_candidate_with("classify", &first.payload, AiMode::Manual, &mut context)
        .unwrap();
    let reduced = crate::context_budget::classify(&state, &changes[..3]);
    let mut reduced = app
        .prepare_ai_candidate_with("classify", &reduced.payload, AiMode::Manual, &mut context)
        .unwrap();
    assert!(Arc::ptr_eq(&first.records, &reduced.records));
    assert!(Arc::ptr_eq(&first.wire_ids, &reduced.wire_ids));
    assert_eq!(reduced.payload["_local_context"]["records"], json!([]));
    let before = reduced.prepared.total;
    reduced.materialize_records();
    assert_eq!(
        ai::prepare(&reduced.config, "classify", &reduced.payload).total,
        before
    );
    let mut decoded = json!({"_context_seen":reduced.payload["_local_context"]["records"].as_array().unwrap().iter().filter(|r| r["kind"] == "message").map(|r| r["id"].clone()).collect::<Vec<_>>()});
    reduced.wire_ids.decode(&mut decoded).unwrap();
    assert_eq!(
        decoded["_context_seen"].as_array().unwrap().len(),
        state.messages.len()
    );
    assert!(decoded["_context_seen"]
        .as_array()
        .unwrap()
        .contains(&json!("m00")));
}

#[test]
fn shared_prepare_context_uses_each_candidates_delivered_room_aliases() {
    let (_root, app) = fixture();
    let mut state = app.state.lock().unwrap().clone();
    state.messages.retain(|id, _| id == "m00" || id == "m01");
    for (id, account) in [
        ("m00", "synthetic-account-a"),
        ("m01", "synthetic-account-b"),
    ] {
        let message = state.messages.get_mut(id).unwrap();
        message.source_id = account.into();
        message.chat = "same-synthetic-chat-id".into();
        message.text = format!("Synthetic material from {account}");
    }
    let changes = state.messages.values().cloned().collect::<Vec<_>>();
    let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
    let first = crate::context_budget::classify(&state, &changes);
    let mut first = app
        .prepare_ai_candidate_with("classify", &first.payload, AiMode::Manual, &mut context)
        .unwrap();
    // The next candidate reuses full records and message aliases, but its r1
    // belongs to the other account. Room aliases belong to the delivered pack.
    let second = crate::context_budget::classify(&state, &changes[1..]);
    let mut second = app
        .prepare_ai_candidate_with("classify", &second.payload, AiMode::Manual, &mut context)
        .unwrap();
    assert!(Arc::ptr_eq(&first.records, &second.records));
    assert!(Arc::ptr_eq(&first.wire_ids, &second.wire_ids));
    for (request, account) in [
        (&mut first, "synthetic-account-a"),
        (&mut second, "synthetic-account-b"),
    ] {
        let before = request.prepared.total;
        request.materialize_records();
        let result: Value = serde_json::from_str(&crate::analysis_context::query(
            &request.payload["_local_context"],
            &mut crate::analysis_context::Meter::default(),
            &json!({"chat":"r1","kind":"message"}),
        ))
        .unwrap();
        let rows = result["records"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{result}");
        assert_eq!(rows[0]["source"], account);
        assert_eq!(rows[0]["chat"], "same-synthetic-chat-id");
        assert_eq!(
            ai::prepare(&request.config, "classify", &request.payload).total,
            before
        );
        assert!(json!(ai::public_payload(&request.payload))
            .get("_local_context")
            .is_none());
    }
    assert!(second.payload["_local_context"]["room_aliases"]
        .get("r2")
        .is_none());
    // A candidate with no room metadata must not inherit the earlier map.
    let mut empty = app
        .prepare_ai_candidate_with(
            "classify",
            &json!({"changes":[],"rooms":[]}),
            AiMode::Manual,
            &mut context,
        )
        .unwrap();
    empty.materialize_records();
    let result: Value = serde_json::from_str(&crate::analysis_context::query(
        &empty.payload["_local_context"],
        &mut crate::analysis_context::Meter::default(),
        &json!({"chat":"r1"}),
    ))
    .unwrap();
    assert!(result["error"].as_str().unwrap().contains("未知会话"));
}

#[test]
fn config_or_source_changes_reject_a_frozen_result_but_cursor_progress_does_not() {
    for change in ["config", "source", "cursor"] {
        let (_root, app) = fixture();
        app.state.lock().unwrap().chat_sources.push(ChatSource {
            id: "source".into(),
            enabled: true,
            ..Default::default()
        });
        let mut calls = 0;
        let result = app.run_analysis_with(AiMode::Manual, false, &mut |request, _| {
            reserve(&app, request, AiMode::Manual)?;
            calls += 1;
            app.commit_state(|s| {
                match change {
                    "config" => s.config.classify_prompt.push_str("\nChanged policy"),
                    "source" => s.chat_sources[0].config = json!({"account":"changed"}),
                    _ => s.chat_sources[0].cursor = json!(calls),
                }
                Ok(())
            })?;
            Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
        });
        if change == "cursor" {
            assert!(result.is_ok(), "{result:?}");
            assert!(calls > 1);
        } else {
            assert!(result.unwrap_err().contains("配置或来源已变化"));
            assert_eq!(calls, 1);
            assert_eq!(
                crate::context_budget::pending(&app.state.lock().unwrap()).len(),
                23
            );
        }
    }
}

#[test]
fn one_round_drains_manual_and_early_automatic_batches_including_the_small_tail() {
    for mode in [AiMode::Manual, AiMode::Automatic] {
        let (_root, app) = fixture();
        app.test_force_working();
        if mode == AiMode::Automatic {
            let mut s = app.state.lock().unwrap();
            s.message_filter.early = true;
            s.message_filter.last_run = now();
            s.message_filter.trigger = "message_filter".into();
            s.last_analysis_at = now();
            app.analysis_last_attempt.store(now(), Ordering::SeqCst);
        }
        let mut requests = BTreeSet::new();
        let mut messages = BTreeSet::new();
        let mut sizes = Vec::new();
        let mut token: Option<Arc<AtomicBool>> = None;
        let mut previous_timeout = 600;
        let result = app
            .run_analysis_with(mode, mode == AiMode::Automatic, &mut |request, call| {
                assert!(requests.insert(call.id.clone()));
                assert!(app.ai_busy.load(Ordering::SeqCst));
                if let Some(token) = &token {
                    assert!(Arc::ptr_eq(token, &call.cancel));
                    let s = app.state.lock().unwrap();
                    assert!(!crate::context_budget::automatic_due(
                        &s,
                        &crate::context_budget::pending(&s),
                        now(),
                        app.analysis_last_attempt.load(Ordering::SeqCst)
                    ));
                } else {
                    token = Some(call.cancel.clone());
                }
                assert!(request.config.ai_timeout_seconds <= previous_timeout);
                previous_timeout = request.config.ai_timeout_seconds;
                reserve(&app, request, mode)?;
                let selected = ids(request);
                sizes.push(selected.len());
                for id in selected {
                    assert!(messages.insert(id));
                }
                Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
            })
            .unwrap();
        assert!(sizes.len() > 1, "{sizes:?}");
        assert!(sizes.last().unwrap() < &sizes[0], "{sizes:?}");
        assert_eq!(messages.len(), 23);
        assert_eq!(result["processed"], 23);
        assert_eq!(result["batches"], sizes.len());
        assert_eq!(result["remaining"], 0);
        let s = app.state.lock().unwrap();
        assert!(s
            .messages
            .values()
            .all(|m| crate::message_progress::analyzed(&s, m)));
        assert_eq!(s.calls as usize, sizes.len());
        assert!(!s.pending_analysis);
        drop(s);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        assert!(app.analyzing_messages.lock().unwrap().is_empty());
    }
}

#[test]
fn retry_priorities_and_action_counts_do_not_split_a_request_that_fits() {
    for (mode, count, create_tasks) in [
        (AiMode::Manual, 30, false),
        (AiMode::Automatic, 30, false),
        (AiMode::Manual, 1025, false),
        (AiMode::Automatic, 513, true),
    ] {
        let (_root, app) = fixture();
        app.test_force_working();
        app.commit_state(|state| {
            state.messages.clear();
            state.config.max_input_chars = if count == 30 { 80_000 } else { 200_000 };
            let at = now() - 60_000;
            for i in 0..count {
                let id = format!("short-{i:04}");
                let retries = if i < 8 {
                    0
                } else if i < 12 {
                    1
                } else {
                    8
                };
                if retries > 0 {
                    state.analysis_retry_counts.insert(id.clone(), retries);
                    state
                        .analysis_retries
                        .insert(id.clone(), "合成普通失败，等待后续自动分析".into());
                }
                state.messages.insert(
                    id.clone(),
                    Message {
                        id,
                        text: "合成确认".into(),
                        time: at + i,
                        updated_at: at + i,
                        private: true,
                        ..Default::default()
                    },
                );
            }
            Ok(())
        })
        .unwrap();
        let mut batches = Vec::new();
        let mut seen = BTreeSet::new();
        let result = app
            .run_analysis_with(mode, false, &mut |request, _| {
                reserve(&app, request, mode)?;
                let selected = ids(request);
                batches.push(selected.len());
                assert!(selected.iter().cloned().all(|id| seen.insert(id)));
                assert!(request.prepared.total <= request.config.max_input_chars);
                let actions = selected.iter().filter(|_| create_tasks).map(|source| {
                    let mut action = create(source, &format!("Synthetic task {source}"));
                    for key in ["task_id", "suggested_project_id", "suggested_project_reason"] {
                        action[key] = json!("");
                    }
                    for key in ["background_evidence", "completion_evidence", "deadline_evidence", "deferral_evidence", "tool_call_ids"] {
                        action[key] = json!([]);
                    }
                    action
                }).collect::<Vec<_>>();
                shishi_api::schema_validate::validate_output(&request.prepared.schema,
                    json!({"actions":actions,"no_action_reason":"合成确认处理完毕","tag_suggestions":[],"image_notes":[],"memory_update":[]})
                ).map_err(|error| error.to_string())
            })
            .unwrap();
        assert_eq!(batches, vec![count as usize]);
        assert_eq!(result["processed"], count);
        assert_eq!(result["remaining"], 0);
        let state = app.state.lock().unwrap();
        assert!(state
            .messages
            .values()
            .all(|m| crate::message_progress::analyzed(&state, m)));
        assert!(crate::context_budget::pending_for(&state, true).is_empty());
        assert!(state.analysis_retry_counts.is_empty());
        assert_eq!(state.calls, 1);
        assert_eq!(
            state.tasks.len(),
            if create_tasks { count as usize } else { 0 }
        );
    }
}

#[test]
fn oversized_head_is_deferred_without_blocking_tail_and_recovers_after_budget_change() {
    let (_root, app) = fixture();
    app.test_force_working();
    let mut batch = {
        let mut state = app.state.lock().unwrap();
        state.messages.retain(|id, _| id == "m00" || id == "m01");
        state.messages.get_mut("m00").unwrap().text = "大".repeat(2000);
        state.messages.get_mut("m01").unwrap().text = "短确认".into();
        state.message_filter.early = true;
        state.message_filter.last_run = now();
        state.message_filter.trigger = "message_filter".into();
        crate::context_budget::classify(&state, &[state.messages["m01"].clone()])
    };
    batch.payload["_trigger"] = json!("message_filter");
    batch.payload["_filter_decision_at"] = json!(app.state.lock().unwrap().message_filter.last_run);
    // Use the production capture, including the local project catalog. An
    // empty catalog plus arbitrary slack would accidentally reject both rows.
    let mut context = app
        .capture_prepare_context("classify", false, None)
        .unwrap();
    let limit = app
        .prepare_ai_candidate_with("classify", &batch.payload, AiMode::Automatic, &mut context)
        .unwrap()
        .prepared
        .total;
    app.state.lock().unwrap().config.max_input_chars = limit;
    let result = app
        .run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
            assert_eq!(ids(request), ["m01"]);
            reserve(&app, request, AiMode::Automatic)?;
            Ok(json!({"actions":[],"no_action_reason":"合成确认无需建立事项"}))
        })
        .unwrap();
    assert_eq!(result["processed"], 1, "{:?}", app.activity.lock().unwrap());
    assert_eq!(result["remaining"], 1);
    {
        let mut state = app.state.lock().unwrap();
        assert!(!state.auto_analysis_blocked);
        assert_eq!(state.analysis_retry_counts["m00"], 3);
        assert!(!crate::message_progress::analyzed(
            &state,
            &state.messages["m00"]
        ));
        assert!(crate::context_budget::pending(&state).is_empty());
        state.config.report_prompt = "修改无关用途".into();
        assert!(crate::context_budget::pending(&state).is_empty());
        state.config.max_input_chars = 100_000;
        assert_eq!(crate::context_budget::pending(&state).len(), 1);
    }
    let result = app
        .run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
            assert_eq!(ids(request), ["m00"]);
            reserve(&app, request, AiMode::Automatic)?;
            Ok(json!({"actions":[],"no_action_reason":"扩大预算后完成合成核对"}))
        })
        .unwrap();
    assert_eq!(result["processed"], 1);
    assert!(app.state.lock().unwrap().analysis_retries.is_empty());
}

#[test]
fn fixed_input_pause_rechecks_relevant_changes_and_legacy_empty_fingerprint() {
    let (_root, app) = fixture();
    app.test_force_working();
    {
        let mut state = app.state.lock().unwrap();
        state.config.classify_prompt = "合成固定规则".repeat(5000);
        state.config.max_input_chars = 4_000;
    }
    let error = app
        .run_analysis_with(AiMode::Automatic, false, &mut |_, _| {
            panic!("fixed input overflow must not call a provider")
        })
        .unwrap_err();
    assert!(error.starts_with(ai::INPUT_LIMIT_ERROR));
    {
        let mut state = app.state.lock().unwrap();
        assert!(analysis_budget_blocked(&state));
        state.config.archive_prompt = "无关归档配置".into();
        state.config.retain_ai_traces = !state.config.retain_ai_traces;
        assert!(analysis_budget_blocked(&state));
        let saved = state.auto_analysis_block_fingerprint.clone();
        state.auto_analysis_block_fingerprint.clear();
        assert!(
            !analysis_budget_blocked(&state),
            "older persisted flags must be reevaluated"
        );
        state.auto_analysis_block_fingerprint = saved;
        state.config.classify_prompt.clear();
        state.config.max_input_chars = 100_000;
        assert!(!analysis_budget_blocked(&state));
    }
    app.run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
        reserve(&app, request, AiMode::Automatic)?;
        Ok(json!({"actions":[],"no_action_reason":"固定输入恢复后完成核对"}))
    })
    .unwrap();
    assert!(!app.state.lock().unwrap().auto_analysis_blocked);
}

#[test]
fn unrelated_purpose_and_catalog_settings_do_not_discard_analysis_results() {
    let (_root, app) = fixture();
    let mut calls = 0;
    let result = app
        .run_analysis_with(AiMode::Manual, false, &mut |request, _| {
            reserve(&app, request, AiMode::Manual)?;
            calls += 1;
            app.commit_state(|state| {
                state.config.archive_prompt.push_str("归档表达变化");
                state.config.report_prompt.push_str("报告表达变化");
                state.config.handling_prompt.push_str("处置表达变化");
                state.config.retain_ai_traces = !state.config.retain_ai_traces;
                state
                    .config
                    .codex_models
                    .push(json!({"model":format!("unused-{calls}")}));
                state
                    .config
                    .ai_connections
                    .push(crate::ai_backend::Connection {
                        id: format!("unused-{calls}"),
                        ..Default::default()
                    });
                state.config.model_profiles = Some(ModelProfiles {
                    analysis: Some(ModelChoice::default()),
                    handling: Some(ModelChoice {
                        model: format!("unused-{calls}"),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                Ok(())
            })?;
            Ok(json!({"actions":[],"no_action_reason":"合成确认无需新增事项"}))
        })
        .unwrap();
    assert!(calls > 1);
    assert_eq!(result["processed"], 23);
}

#[test]
fn invalid_final_results_stop_the_round_but_do_not_starve_untried_messages() {
    for invalid_wire in [false, true] {
        let (_root, app) = fixture();
        app.test_force_working();
        let invalid = |request: &PreparedAiRequest, call: &crate::ai_control::AiCall<'_>| {
            if invalid_wire {
                app.finalize_ai_response(
                    request,
                    &call.id,
                    json!({"request_id":call.id,"kind":"classify","response_received":true}),
                    Ok(json!({"actions":[create("m999", "未知短编号")],"no_action_reason":""})),
                    true,
                )
            } else {
                Ok(json!({"actions":[{"action":"invalid"}],"no_action_reason":""}))
            }
        };
        let mut failed = BTreeSet::new();
        assert!(app
            .run_analysis_with(AiMode::Automatic, false, &mut |request, call| {
                reserve(&app, request, AiMode::Automatic)?;
                failed.extend(ids(request));
                invalid(request, call)
            })
            .is_err());
        let mut successful = BTreeSet::new();
        for _ in 0..2 {
            assert!(app
                .run_analysis_with(AiMode::Automatic, false, &mut |request, call| {
                    reserve(&app, request, AiMode::Automatic)?;
                    let selected = ids(request);
                    if selected.iter().any(|id| failed.contains(id)) {
                        failed.extend(selected);
                        invalid(request, call)
                    } else {
                        successful.extend(selected);
                        Ok(json!({"actions":[],"no_action_reason":"正常尾部已完成核对"}))
                    }
                })
                .is_err());
        }
        assert_eq!(successful.len() + failed.len(), 23);
        let state = app.state.lock().unwrap();
        assert_eq!(crate::context_budget::pending(&state).len(), failed.len());
        for id in &failed {
            assert!(state.analysis_retry_counts[id] >= 2);
            assert!(!crate::message_progress::analyzed(
                &state,
                &state.messages[id]
            ));
        }
        assert!(state
            .analysis_retry_counts
            .values()
            .any(|count| *count == 3));
        assert!(state.tasks.is_empty());
        drop(state);
        let recovered = app
            .run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
                reserve(&app, request, AiMode::Automatic)?;
                assert!(ids(request).iter().all(|id| failed.contains(id)));
                Ok(json!({"actions":[],"no_action_reason":"后续自动分析已核对合成资料"}))
            })
            .unwrap();
        assert_eq!(recovered["processed"], failed.len());
        let state = app.state.lock().unwrap();
        assert!(crate::context_budget::pending(&state).is_empty());
        assert!(state.analysis_retries.is_empty());
    }
}

#[test]
fn failed_rounds_with_possible_command_execution_require_manual_retry_for_that_batch() {
    for (status, save_failure) in [
        (None, false),
        (Some("succeeded"), false),
        (Some("rejected"), false),
        (None, true),
    ] {
        let (root, app) = fixture();
        app.test_force_working();
        let mut affected = Vec::new();
        assert!(app
            .run_analysis_with(AiMode::Automatic, false, &mut |request, call| {
                reserve(&app, request, AiMode::Automatic)?;
                affected = ids(request);
                app.commit_state(|state| {
                    state.tool_runs.push(crate::tool_calling::ToolRun {
                        request_id: call.id.clone(),
                        id: call.id.clone(),
                        calls: vec![crate::tool_calling::CallRecord {
                            call_id: "synthetic-call".into(),
                            result: status.map(|status| crate::tool_runner::CommandResult {
                                status: status.into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        ..Default::default()
                    });
                    Ok(())
                })?;
                if save_failure {
                    crate::test_support::persistence_failure(&root, true)?;
                }
                Err("synthetic connection failure after command response".into())
            })
            .is_err());
        if save_failure {
            crate::test_support::persistence_failure(&root, false).unwrap();
        }
        let guarded = status != Some("rejected");
        {
            let state = app.state.lock().unwrap();
            for id in &affected {
                assert_eq!(state.analysis_retry_counts.contains_key(id), guarded);
                if guarded {
                    assert!(state.analysis_retries[id].contains("核对命令记录"));
                }
                assert!(!crate::message_progress::analyzed(
                    &state,
                    &state.messages[id]
                ));
            }
        }
        app.run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
            if guarded {
                assert!(ids(request).iter().all(|id| !affected.contains(id)));
            }
            reserve(&app, request, AiMode::Automatic)?;
            Ok(json!({"actions":[],"no_action_reason":"其他消息已完成核对"}))
        })
        .unwrap();
        if guarded {
            assert_eq!(
                crate::context_budget::pending_for(&app.state.lock().unwrap(), true).len(),
                affected.len()
            );
            app.run_analysis_with(AiMode::Manual, false, &mut |request, _| {
                assert!(ids(request).iter().all(|id| affected.contains(id)));
                reserve(&app, request, AiMode::Manual)?;
                Ok(json!({"actions":[],"no_action_reason":"人工核对后重新分析，未重放命令"}))
            })
            .unwrap();
        }
        assert!(crate::context_budget::pending_for(&app.state.lock().unwrap(), true).is_empty());
        assert_eq!(app.state.lock().unwrap().tool_runs.len(), 1);
    }
}

#[test]
fn concurrent_new_and_edited_bodies_wait_for_the_next_round() {
    let (_root, app) = fixture();
    let mut calls = 0;
    let mut delivered = BTreeSet::new();
    let mut changed_current = String::new();
    let result = app
        .run_analysis_with(AiMode::Manual, false, &mut |request, _| {
            reserve(&app, request, AiMode::Manual)?;
            calls += 1;
            let selected = ids(request);
            assert!(!selected.contains(&"new".to_owned()));
            if calls > 1 {
                assert!(!selected.contains(&"m22".to_owned()));
            }
            for id in &selected {
                assert!(delivered.insert(id.clone()));
            }
            if calls == 1 {
                assert!(!selected.contains(&"m22".to_owned()));
                changed_current = selected[0].clone();
                app.commit_state(|s| {
                    for id in [changed_current.as_str(), "m22"] {
                        s.messages.get_mut(id).unwrap().text = "并发修改后的合成正文".into();
                    }
                    s.messages.insert(
                        "new".into(),
                        Message {
                            id: "new".into(),
                            text: "本轮开始后的新消息".into(),
                            private: true,
                            time: now(),
                            updated_at: now(),
                            ..Default::default()
                        },
                    );
                    Ok(())
                })?;
                let attempted = app.analysis_last_attempt.load(Ordering::SeqCst);
                assert_eq!(
                    app.analyze(AiMode::Manual).unwrap_err(),
                    crate::ai_control::BUSY
                );
                assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), attempted);
            }
            Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
        })
        .unwrap();
    assert!(calls > 1);
    assert_eq!(result["remaining"], 3);
    let s = app.state.lock().unwrap();
    let pending = crate::context_budget::pending(&s)
        .into_iter()
        .map(|m| m.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        pending,
        BTreeSet::from([changed_current, "m22".into(), "new".into()])
    );
    drop(s);
}

#[test]
fn a_task_version_conflict_is_attempted_once_without_stalling_other_batches() {
    for command_ran in [false, true] {
        let (_root, app) = fixture();
        app.state.lock().unwrap().tasks.push(Task {
            id: "task".into(),
            title: "合成事项".into(),
            updated: 10,
            evidence: TaskEvidence {
                direct: vec!["m00".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let mut calls = 0;
        app.run_analysis_with(AiMode::Manual, false, &mut |request, call| {
            reserve(&app, request, AiMode::Manual)?;
            calls += 1;
            if calls == 1 {
                assert!(ids(request).contains(&"m00".to_owned()));
                app.commit_state(|s| {
                    s.tasks[0].title = "人工已修改".into();
                    s.tasks[0].updated = 20;
                    if command_ran {
                        s.tool_runs.push(crate::tool_calling::ToolRun {
                            id: call.id.clone(),
                            request_id: call.id.clone(),
                            calls: vec![crate::tool_calling::CallRecord {
                                call_id: "executed".into(),
                                result: Some(crate::tool_runner::CommandResult {
                                    status: "succeeded".into(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }],
                            ..Default::default()
                        });
                    }
                    Ok(())
                })?;
                let mut actions = vec![json!({"action":"update","task_id":"task",
                "sources":["m00"],"fields":[{"field":"title","value":"旧版本结果"}],
                "tag_changes":[],"confidence":0.99,"reason":"合成进展"})];
                if command_ran {
                    actions.push(create("m01", "同批有效动作保留"));
                }
                return Ok(json!({"no_action_reason":"","actions":actions}));
            }
            assert!(!ids(request).contains(&"m00".to_owned()));
            Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
        })
        .unwrap();
        let s = app.state.lock().unwrap();
        assert!(calls > 1);
        assert_eq!(s.tasks[0].title, "人工已修改");
        assert_eq!(
            s.analysis_retry_counts.get("m00"),
            Some(&if command_ran { 3 } else { 1 })
        );
        assert_eq!(
            crate::context_budget::pending(&s).len(),
            usize::from(!command_ran)
        );
        assert_eq!(crate::context_budget::pending_for(&s, true).len(), 1);
        if command_ran {
            assert!(s.analysis_retries["m00"].contains("核对命令记录"));
            assert!(s.tasks.iter().any(|task| task.title == "同批有效动作保留"));
        }
        drop(s);
    }
}

#[test]
fn later_failure_or_cancellation_keeps_only_the_successful_batches() {
    for stop in ["provider", "invalid", "cancel"] {
        let (root, app) = fixture();
        let mut calls = 0;
        let mut committed = Vec::new();
        let mut rejected = Vec::new();
        let mut committed_at = 0;
        let error = app
            .run_analysis_with(AiMode::Manual, false, &mut |request, call| {
                reserve(&app, request, AiMode::Manual)?;
                calls += 1;
                let selected = ids(request);
                if calls == 1 {
                    committed = selected.clone();
                    return Ok(json!({"no_action_reason":"","actions":[create(&selected[0], "已提交的合成事项")]}));
                }
                rejected = selected.clone();
                committed_at = app.state.lock().unwrap().last_analysis_at;
                match stop {
                    "provider" => Err("合成供应商失败".into()),
                    "invalid" => Ok(json!({"no_action_reason":"","actions":[create(&selected[0], "不得提交"),
                    {"action":"invalid"}]})),
                    _ => {
                        app.interrupt_ai(&call.id)?;
                        Ok(json!({"no_action_reason":"","actions":[create(&selected[0], "不得提交")]}))
                    }
                }
            })
            .unwrap_err();
        assert_eq!(calls, 2);
        if stop == "cancel" {
            assert_eq!(error, crate::ai_control::CANCELLED);
        } else {
            assert!(error.contains("已提交 1 批"), "{error}");
        }
        let s = app.state.lock().unwrap();
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.tasks[0].title, "已提交的合成事项");
        assert!(committed
            .iter()
            .all(|id| crate::message_progress::analyzed(&s, &s.messages[id])));
        assert!(rejected
            .iter()
            .all(|id| !crate::message_progress::analyzed(&s, &s.messages[id])));
        assert!(s.pending_analysis);
        assert_eq!(s.analysis_interrupted_at > 0, stop == "cancel");
        if stop == "cancel" {
            assert_eq!(s.last_analysis_at, committed_at);
            assert_eq!(s.last_analysis_success_at, committed_at);
        }
        drop(s);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        assert!(app.analyzing_messages.lock().unwrap().is_empty());
        assert!(app
            .activity
            .lock()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "analysis_partial"));
        let saved = app.state.lock().unwrap().analyzed.clone();
        drop(app);
        let reopened = App::load(root.clone()).unwrap();
        assert_eq!(reopened.state.lock().unwrap().analyzed, saved);
        assert_eq!(reopened.state.lock().unwrap().tasks.len(), 1);
        drop(reopened);
    }
}

#[test]
fn quotas_and_automatic_switches_are_checked_between_batches() {
    for stop in ["quota", "disabled"] {
        let (_root, app) = fixture();
        app.test_force_working();
        if stop == "quota" {
            app.state.lock().unwrap().config.daily_calls = 1;
        }
        let mut accepted = 0;
        let error = app
            .run_analysis_with(AiMode::Automatic, false, &mut |request, _| {
                reserve(&app, request, AiMode::Automatic)?;
                accepted += 1;
                if stop == "disabled" {
                    app.state.lock().unwrap().config.auto_ai = false;
                }
                Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
            })
            .unwrap_err();
        assert!(error.contains("已提交 1 批"), "{error}");
        assert_eq!(accepted, 1);
        let s = app.state.lock().unwrap();
        assert_eq!(s.calls, 1);
        assert!(s.pending_analysis);
        assert!(s
            .messages
            .values()
            .any(|m| crate::message_progress::analyzed(&s, m)));
        assert!(!crate::context_budget::pending(&s).is_empty());
        drop(s);
    }
}

#[test]
fn cancellation_before_the_first_reservation_consumes_only_its_own_early_decision() {
    for newer_decision in [false, true] {
        let (_root, app) = fixture();
        app.test_force_working();
        let decision_at = now();
        let interval = app.state.lock().unwrap().config.analysis_interval_minutes as i64 * 60_000;
        let original_anchor = decision_at - interval + 30_000;
        let previous_attempt = decision_at - 60_000;
        {
            let mut s = app.state.lock().unwrap();
            s.last_analysis_at = original_anchor;
            s.analysis_pending_since = original_anchor;
            s.message_filter.early = true;
            s.message_filter.last_run = decision_at;
            s.message_filter.trigger = "message_filter".into();
        }
        app.analysis_last_attempt
            .store(previous_attempt, Ordering::SeqCst);
        let error = app
            .run_analysis_with(AiMode::Automatic, true, &mut |_, call| {
                // Simulate cancellation at the invocation boundary, before the
                // provider adapter reserves quota or consumes a filter decision.
                if newer_decision {
                    let mut s = app.state.lock().unwrap();
                    s.message_filter.last_run = decision_at + 1;
                }
                app.interrupt_ai(&call.id)?;
                Err(crate::ai_control::CANCELLED.into())
            })
            .unwrap_err();
        assert_eq!(error, crate::ai_control::CANCELLED);
        let s = app.state.lock().unwrap();
        assert_eq!(s.calls, 0);
        assert_eq!(s.message_filter.early, newer_decision);
        assert!(s.analysis_interrupted_at > 0);
        assert_eq!(s.last_analysis_at, original_anchor);
        assert_eq!(
            app.analysis_last_attempt.load(Ordering::SeqCst),
            previous_attempt
        );
        let pending = crate::context_budget::pending(&s);
        assert_eq!(pending.len(), 23);
        let scheduled_at = original_anchor + interval;
        assert!(!crate::context_budget::due(&s, &pending, scheduled_at - 1));
        assert!(crate::context_budget::due(&s, &pending, scheduled_at));
        drop(s);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
    }
}

#[test]
fn a_total_deadline_is_shared_across_requests_and_rejects_late_results() {
    let (_root, app) = fixture();
    app.state.lock().unwrap().config.ai_timeout_seconds = 3;
    let mut calls = 0;
    let mut first_timeout = 0;
    let mut first_ids = Vec::new();
    let error = app
        .run_analysis_with(AiMode::Manual, false, &mut |request, _| {
            reserve(&app, request, AiMode::Manual)?;
            calls += 1;
            if calls == 1 {
                first_timeout = request.config.ai_timeout_seconds;
                first_ids = ids(request);
                std::thread::sleep(std::time::Duration::from_millis(1100));
            } else {
                assert!(request.config.ai_timeout_seconds < first_timeout);
                std::thread::sleep(std::time::Duration::from_millis(2100));
            }
            Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
        })
        .unwrap_err();
    assert_eq!(calls, 2);
    assert!(
        error.contains("总时限") && error.contains("已提交 1 批"),
        "{error}"
    );
    let s = app.state.lock().unwrap();
    assert_eq!(
        s.messages
            .values()
            .filter(|m| crate::message_progress::analyzed(&s, m))
            .count(),
        first_ids.len()
    );
    assert!(s.pending_analysis);
    drop(s);
    assert!(!app.ai_busy.load(Ordering::SeqCst));
}

#[test]
fn an_explicit_interrupt_keeps_its_meaning_when_the_deadline_expires_afterward() {
    let (_root, app) = fixture();
    app.test_force_working();
    {
        let mut s = app.state.lock().unwrap();
        s.config.ai_timeout_seconds = 1;
        s.message_filter.early = true;
        s.message_filter.last_run = now();
    }
    let error = app
        .run_analysis_with(AiMode::Automatic, true, &mut |_, call| {
            app.interrupt_ai(&call.id)?;
            std::thread::sleep(std::time::Duration::from_millis(1100));
            Err(crate::ai_control::CANCELLED.into())
        })
        .unwrap_err();
    assert_eq!(error, crate::ai_control::CANCELLED);
    let s = app.state.lock().unwrap();
    assert!(s.analysis_interrupted_at > 0);
    assert!(!s.message_filter.early);
    assert_eq!(s.calls, 0);
    assert_eq!(crate::context_budget::pending(&s).len(), 23);
    drop(s);
}

#[test]
fn memory_tags_tasks_and_message_progress_share_one_acceptance_transaction() {
    for outcome in ["success", "invalid", "cancel", "save_failure"] {
        let (root, app) = fixture();
        // A single batch makes the transaction's complete accepted state easy
        // to distinguish from an earlier, independently committed batch.
        app.state
            .lock()
            .unwrap()
            .messages
            .retain(|id, _| id == "m00");
        let result = app.run_analysis_with(AiMode::Manual, false, &mut |request, call| {
            reserve(&app, request, AiMode::Manual)?;
            let source = &ids(request)[0];
            let mut output = json!({
                "actions":[create(source, "同批提交的合成事项")],
                "no_action_reason":"",
                "_memory_update":{"before":"","text":"同批提交的临时记忆","expected":0},
                "tag_suggestions":[{"kind":"合成分类","value":"合成标签",
                    "reason":"材料提供分类依据","evidence_ids":[source]}]
            });
            if outcome == "invalid" {
                output["actions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"action":"invalid"}));
            }
            request.wire_ids.encode(&mut output);
            let finalized = app.finalize_ai_response(
                request,
                &call.id,
                json!({"request_id":call.id,"kind":"classify","response_received":true}),
                Ok(output),
                true,
            )?;
            {
                let s = app.state.lock().unwrap();
                assert!(s.analysis_memory.temporary.is_empty());
                assert!(s.tag_state.proposals.is_empty());
                assert!(s.tasks.is_empty());
            }
            if outcome == "cancel" {
                app.interrupt_ai(&call.id)?;
            } else if outcome == "save_failure" {
                crate::test_support::persistence_failure(&root, true)?;
            }
            Ok(finalized)
        });
        if outcome == "save_failure" {
            crate::test_support::persistence_failure(&root, false).unwrap();
        }
        assert_eq!(
            result.is_ok(),
            outcome == "success",
            "{outcome}: {result:?}"
        );
        let s = app.state.lock().unwrap();
        let committed = outcome == "success";
        assert_eq!(s.tasks.len(), usize::from(committed));
        assert_eq!(s.tag_state.proposals.len(), usize::from(committed));
        assert_eq!(!s.analysis_memory.temporary.is_empty(), committed);
        assert_eq!(s.analysis_memory.temporary_version, u64::from(committed));
        assert_eq!(
            crate::message_progress::analyzed(&s, &s.messages["m00"]),
            committed
        );
        drop(s);
        drop(app);
        let reopened = App::load(root.clone()).unwrap();
        let s = reopened.state.lock().unwrap();
        assert_eq!(s.tasks.len(), usize::from(committed));
        assert_eq!(s.tag_state.proposals.len(), usize::from(committed));
        assert_eq!(!s.analysis_memory.temporary.is_empty(), committed);
        assert_eq!(
            crate::message_progress::analyzed(&s, &s.messages["m00"]),
            committed
        );
        drop(s);
        drop(reopened);
    }
}

#[test]
fn scheduler_rechecks_due_without_noop_saves_or_busy_cooldown_changes() {
    let (_root, app) = fixture();
    app.test_force_working();
    {
        let mut s = app.state.lock().unwrap();
        s.config.max_input_chars = 1_000_000;
        s.pending_analysis = true;
        s.analysis_pending_since = now();
        s.last_analysis_at = now();
        s.message_filter.early = false;
    }
    let before = app.saves_revision();
    assert_eq!(
        app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
        "等待分析间隔或单批体积阈值"
    );
    assert_eq!(app.saves_revision(), before);
    let result = app
        .run_analysis_with(AiMode::Automatic, true, &mut |_, _| panic!("not due"))
        .unwrap();
    assert_eq!(result["skipped"], "等待分析间隔或单批体积阈值");
    assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
    assert_eq!(app.saves_revision(), before);
    let held = app.begin_ai().unwrap();
    assert!(app.analyze(AiMode::Manual).is_err());
    assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
    drop(held);
}

#[test]
fn manual_scan_followup_waits_for_ai_and_rechecks_committed_progress() {
    for consumed in [false, true] {
        let (_root, app) = fixture();
        app.state
            .lock()
            .unwrap()
            .messages
            .retain(|id, _| id == "m00");
        // Manual scan follow-ups remain explicit even with automation disabled.
        app.state.lock().unwrap().config.auto_ai = false;
        let held = app.begin_ai_for("message_filter").unwrap();
        let previous = held.id.clone();
        let revision = app.saves_revision();
        let (started, entered) = std::sync::mpsc::channel();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                started.send(()).unwrap();
                let mut calls = 0;
                let result = app.run_analysis_with(AiMode::Manual, true, &mut |request, _| {
                    calls += 1;
                    reserve(&app, request, AiMode::Manual)?;
                    Ok(json!({"actions":[],"no_action_reason":"本批合成确认消息无需新增或修改事项"}))
                });
                sent.send((calls, result)).unwrap();
            });
            entered.recv().unwrap();
            assert!(received
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err());
            assert_eq!(app.active_ai.lock().unwrap().as_ref().unwrap().id, previous);
            assert!(!held.cancel.load(Ordering::SeqCst));
            assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
            assert_eq!(app.saves_revision(), revision);
            if consumed {
                // Another analysis may finish these exact messages while the
                // manual follow-up waits. Do not submit them a second time.
                app.commit_state(|s| {
                    s.analyzed
                        .insert("m00".into(), fingerprint(&s.messages["m00"]));
                    Ok(())
                })
                .unwrap();
            }
            drop(held);
            let (calls, result) = received
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let result = result.unwrap();
            assert_eq!(calls, usize::from(!consumed));
            if consumed {
                assert_eq!(result["skipped"], "没有相关变化，不调用AI");
            } else {
                assert_eq!(result["processed"], 1);
            }
        });
        assert!(!app.ai_busy.load(Ordering::SeqCst));
    }
}

#[test]
fn manual_scan_followup_keeps_archive_priority_and_does_not_retry_provider_errors() {
    let (_root, app) = fixture();
    app.state
        .lock()
        .unwrap()
        .messages
        .retain(|id, _| id == "m00");
    app.commit_state(|s| {
        s.archive_attempts.push(ArchiveAttempt {
            id: "queued-fixture".into(),
            status: "queued".into(),
            ..Default::default()
        });
        Ok(())
    })
    .unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut calls = 0;
            let result = app.run_analysis_with(AiMode::Manual, true, &mut |request, _| {
                calls += 1;
                reserve(&app, request, AiMode::Manual)?;
                // Even a provider returning a busy-looking error is terminal.
                Err(crate::ai_control::BUSY.into())
            });
            sent.send((calls, result)).unwrap();
        });
        assert!(received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        let archive = app.begin_ai_for("archive").unwrap();
        app.commit_state(|s| {
            s.archive_attempts[0].status = "succeeded".into();
            Ok(())
        })
        .unwrap();
        assert!(received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        drop(archive);
        let (calls, result) = received
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(result.unwrap_err(), crate::ai_control::BUSY);
    });
    assert_eq!(
        crate::context_budget::pending(&app.state.lock().unwrap()).len(),
        1
    );
    assert!(!app.ai_busy.load(Ordering::SeqCst));
}

#[test]
fn manual_scan_waiting_uses_the_total_deadline_without_touching_the_active_call() {
    let (_root, app) = fixture();
    app.state.lock().unwrap().config.ai_timeout_seconds = 1;
    let held = app.begin_ai().unwrap();
    let revision = app.saves_revision();
    let started = std::time::Instant::now();
    let error = app
        .run_analysis_with(AiMode::Manual, true, &mut |_, _| {
            panic!("a waiting follow-up must not invoke a provider")
        })
        .unwrap_err();
    assert!(error.contains("等待 AI 空闲达到分析总时限"), "{error}");
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(app.active_ai.lock().unwrap().as_ref().unwrap().id, held.id);
    assert!(!held.cancel.load(Ordering::SeqCst));
    assert_eq!(app.saves_revision(), revision);
    assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
    let s = app.state.lock().unwrap();
    assert_eq!(s.calls, 0);
    assert_eq!(crate::context_budget::pending(&s).len(), 23);
    drop(s);
    drop(held);
}

#[test]
fn interrupting_the_call_a_manual_scan_waits_for_does_not_restart_analysis() {
    for release_immediately in [false, true] {
        let (_root, app) = fixture();
        app.state.lock().unwrap().config.ai_timeout_seconds = 2;
        let held = app.begin_ai_for("message_filter").unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let result = app.run_analysis_with(AiMode::Manual, true, &mut |_, _| {
                    panic!("an interrupted scan follow-up must not call a provider")
                });
                sent.send(result).unwrap();
            });
            // Wait until the follow-up has captured the active cancellation
            // token, then exercise cancellation racing with slot release.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while Arc::strong_count(&held.cancel) < 3 && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(Arc::strong_count(&held.cancel) >= 3);
            app.interrupt_ai(&held.id).unwrap();
            if release_immediately {
                held.finish();
            }
            let result = received
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            assert_eq!(result.unwrap_err(), crate::ai_control::CANCELLED);
        });
        let s = app.state.lock().unwrap();
        assert!(s.ai_interrupted_at > 0);
        assert_eq!(s.calls, 0);
        assert_eq!(crate::context_budget::pending(&s).len(), 23);
        drop(s);
        assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
        drop(held);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        // A later scheduled operation gets a fresh token. The queueing path
        // must leave both analysis clocks untouched, not create a new cooldown.
        let next = app.begin_ai_for("report").unwrap();
        assert!(!next.cancel.load(Ordering::SeqCst));
        drop(next);
        let s = app.state.lock().unwrap();
        assert_eq!(s.ai_interrupted_at, 0);
        assert_eq!(s.analysis_interrupted_at, 0);
        assert_eq!(s.last_analysis_at, 0);
        assert_eq!(app.analysis_last_attempt.load(Ordering::SeqCst), 0);
        assert!(s.config.automatic_analysis_enabled());
        drop(s);
    }
}
