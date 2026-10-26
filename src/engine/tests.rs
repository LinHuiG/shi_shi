//! Existing regression groups, kept separate from application orchestration.
use super::*;

#[cfg(test)]
mod change_summary_tests {
    use super::*;

    fn fixture() -> State {
        let mut state = State::default();
        for (id, time) in [("old", 10), ("new", 20), ("newest", 30), ("older", 5)] {
            state.messages.insert(
                id.into(),
                Message {
                    id: id.into(),
                    time,
                    text: "合成进展".into(),
                    private: true,
                    ..Default::default()
                },
            );
        }
        let mut task = Task {
            id: "task".into(),
            title: "合成验证事项".into(),
            description: "核对合成结果".into(),
            status: "待处理".into(),
            waiting_for: "准备验证".into(),
            updated: 10,
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        normalize_task(&mut task);
        state.tasks.push(task);
        state
    }

    fn update() -> Value {
        json!({"action":"update","task_id":"task","sources":["new"],"tags":[],
            "fields":[{"field":"waiting_for","value":"核对验证结果"}],
            "reason":"新消息明确下一步","confidence":0.99,"change_summary":"已进入结果核对阶段"})
    }

    fn create() -> Value {
        json!({"action":"create","task_id":"","sources":["new"],"tags":[],
            "fields":[{"field":"title","value":"合成验证事项"},{"field":"description","value":"核对合成结果"}],
            "reason":"明确的新请求","confidence":0.99,"change_summary":"需要核对最新验证结果"})
    }

    #[test]
    fn tool_calls_follow_individual_accepted_actions_including_evidence_only_updates() {
        use crate::tool_calling::{CallRecord, ToolRun};
        let mut state = fixture();
        state.tool_runs.push(ToolRun {
            id: "synthetic-run".into(),
            calls: ["update-call", "create-call", "evidence-call"]
                .map(|call_id| CallRecord {
                    call_id: call_id.into(),
                    ..Default::default()
                })
                .into(),
            ..Default::default()
        });
        let mut update = update();
        update["_tool_run_id"] = json!("synthetic-run");
        update["tool_call_ids"] = json!(["update-call"]);
        let mut create = create();
        create["sources"] = json!(["newest"]);
        create["_tool_run_id"] = json!("synthetic-run");
        create["tool_call_ids"] = json!(["create-call"]);
        assert_eq!(apply(&mut state, &[update.clone(), create]).0, 2);
        let created = state
            .tasks
            .iter()
            .find(|t| t.id != "task")
            .unwrap()
            .id
            .clone();
        assert_eq!(state.tool_runs[0].calls[0].task_ids, ["task"]);
        assert_eq!(state.tool_runs[0].calls[1].task_ids, [created]);
        update["sources"] = json!(["older"]);
        update["tool_call_ids"] = json!(["evidence-call"]);
        assert_eq!(apply(&mut state, &[update]).0, 1);
        assert_eq!(state.tool_runs[0].calls[2].task_ids, ["task"]);
    }

    fn apply(state: &mut State, actions: &[Value]) -> (usize, BTreeSet<String>) {
        let allowed = state.messages.clone();
        let versions = state
            .tasks
            .iter()
            .map(|t| (t.id.clone(), t.updated))
            .collect();
        App::apply_actions_detailed(state, actions, &allowed, &versions, &[]).unwrap()
    }
    #[test]
    fn analysis_preserves_explicit_tool_bindings_and_new_tasks_have_none() {
        let mut state = fixture();
        state.tasks[0].enabled_tool_ids = vec!["synthetic-bound-tool".into()];
        apply(&mut state, &[update()]);
        assert_eq!(
            state.tasks[0].enabled_tool_ids,
            vec!["synthetic-bound-tool"]
        );
        state.tasks.clear();
        apply(&mut state, &[create()]);
        assert!(state.tasks[0].enabled_tool_ids.is_empty());
    }

    #[test]
    fn change_summary_is_saved_for_accepted_create_and_update() {
        let mut state = fixture();
        state.tasks.clear();
        let action = create();
        assert_eq!(apply(&mut state, &[action.clone()]).0, 1);
        assert_eq!(state.audit[0].kind, "ai_create");
        assert_eq!(
            state.audit[0].change_summary,
            action["change_summary"].as_str().unwrap()
        );
        assert_eq!(state.audit[0].after, json!(state.tasks[0]));
        let mut state = fixture();
        let action = update();
        assert_eq!(apply(&mut state, &[action.clone()]).0, 1);
        assert_eq!(state.tasks[0].waiting_for, "核对验证结果");
        assert_eq!(state.audit[0].kind, "ai_update");
        assert_eq!(
            state.audit[0].change_summary,
            action["change_summary"].as_str().unwrap()
        );
    }

    #[test]
    fn change_summary_invalid_or_missing_never_blocks_business_changes() {
        for invalid in [
            Value::Null,
            json!([]),
            json!(17),
            json!(""),
            json!("  "),
            json!("变".repeat(31)),
            json!("第一行\n第二行"),
            json!("一\u{2028}二"),
        ] {
            for creating in [false, true] {
                let mut state = fixture();
                let mut action = if creating {
                    state.tasks.clear();
                    create()
                } else {
                    update()
                };
                if invalid.is_null() {
                    action.as_object_mut().unwrap().remove("change_summary");
                } else {
                    action["change_summary"] = invalid.clone();
                }
                assert_eq!(apply(&mut state, &[action]).0, 1, "{invalid}");
                assert!(state.audit[0].change_summary.is_empty(), "{invalid}");
                assert_eq!(state.tasks.len(), 1);
            }
        }
        let mut state = fixture();
        let mut action = update();
        action["change_summary"] = json!("变".repeat(30));
        apply(&mut state, &[action]);
        assert_eq!(state.audit[0].change_summary.chars().count(), 30);
    }

    #[test]
    fn change_summary_is_empty_when_requested_fields_are_rejected_or_corrected() {
        for (field, value) in [
            ("status", "已完成"),
            ("status", "无效状态"),
            ("due", "2026-10-24T17:30:00+08:00"),
            ("due", "无效日期"),
        ] {
            let mut state = fixture();
            let mut action = update();
            action["fields"]
                .as_array_mut()
                .unwrap()
                .push(json!({"field":field,"value":value}));
            assert_eq!(apply(&mut state, &[action]).0, 1);
            assert_eq!(state.tasks[0].waiting_for, "核对验证结果");
            assert_eq!(state.tasks[0].status, "待处理");
            assert!(state.tasks[0].due.is_empty());
            assert!(state.audit[0].change_summary.is_empty(), "{field}/{value}");
        }
        let mut state = fixture();
        let mut action = update();
        action["tags"] = json!([{"kind":"目录中没有的组","value":"未知值"}]);
        assert_eq!(apply(&mut state, &[action]).0, 1);
        assert!(state.tasks[0].tags.is_empty());
        assert!(state.audit[0].change_summary.is_empty());

        // Even a redundant completion request needs accepted proof before its
        // accompanying summary can describe other changes in the same action.
        let mut state = fixture();
        state.tasks[0].status = "已完成".into();
        let mut action = update();
        action["fields"]
            .as_array_mut()
            .unwrap()
            .push(json!({"field":"status","value":"已完成"}));
        apply(&mut state, &[action]);
        assert!(state.audit[0].change_summary.is_empty());
    }

    #[test]
    fn change_summary_is_retained_for_proven_completion_and_deadline_changes() {
        for (field, value, proof) in [
            ("status", "已完成", "completion_evidence"),
            ("due", "2026-10-24T17:30:00+08:00", "deadline_evidence"),
        ] {
            let mut state = fixture();
            let mut action = update();
            action["fields"]
                .as_array_mut()
                .unwrap()
                .push(json!({"field":field,"value":value}));
            action[proof] = json!(["new"]);
            assert_eq!(apply(&mut state, &[action.clone()]).0, 1);
            assert_eq!(state.audit[0].after[field], value);
            assert_eq!(
                state.audit[0].change_summary,
                action["change_summary"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn change_summary_is_empty_for_evidence_only_or_unchanged_content() {
        for source in ["older", "old", "new"] {
            let mut state = fixture();
            let mut action = update();
            action["sources"] = json!([source]);
            if source == "new" {
                action["fields"][0]["value"] = json!("准备验证");
            }
            let applied = apply(&mut state, &[action]).0;
            assert_eq!(applied, usize::from(source != "old"));
            assert_eq!(state.tasks[0].waiting_for, "准备验证");
            assert!(state.audit.iter().all(|row| row.change_summary.is_empty()));
            if source == "older" {
                assert_eq!(state.audit[0].kind, "ai_evidence");
            }
        }
    }

    #[test]
    fn change_summary_never_leaks_from_rejected_or_deduplicated_actions() {
        let mut state = fixture();
        let allowed = state.messages.clone();
        assert_eq!(
            App::apply_actions_detailed(&mut state, &[update()], &allowed, &BTreeMap::new(), &[])
                .unwrap()
                .0,
            0
        );
        assert!(state.audit.is_empty());
        let mut action = update();
        action["sources"] = json!(["missing"]);
        assert_eq!(apply(&mut state, &[action]).0, 0);
        assert!(state.audit.is_empty());

        let mut action = create();
        action["sources"] = json!(["old", "new"]);
        action["fields"]
            .as_array_mut()
            .unwrap()
            .push(json!({"field":"waiting_for","value":"核对验证结果"}));
        assert_eq!(apply(&mut state, &[action]).0, 1);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.audit[0].kind, "ai_update");
        assert!(state.audit[0].change_summary.is_empty());
    }

    #[test]
    fn change_summary_stays_one_string_per_action_without_concatenation() {
        let mut state = fixture();
        let first = update();
        let mut second = update();
        second["sources"] = json!(["newest"]);
        second["fields"][0]["value"] = json!("等待最终确认");
        second["change_summary"] = json!("核对结束，等待最终确认");
        assert_eq!(apply(&mut state, &[first.clone(), second.clone()]).0, 2);
        assert_eq!(state.audit.len(), 2);
        assert_eq!(
            state.audit[0].change_summary,
            first["change_summary"].as_str().unwrap()
        );
        assert_eq!(
            state.audit[1].change_summary,
            second["change_summary"].as_str().unwrap()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_snapshot_roundtrips_through_manual_and_ai_edit_boundaries() {
        let root = std::env::temp_dir().join(format!("shishi-snapshot-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            s.config.daily_calls = 0;
            s.tasks.push(Task {
                id: "t".into(),
                title: "Synthetic".into(),
                status: "待处理".into(),
                updated: 10,
                ..Default::default()
            });
        }
        let mut task = app.snapshot()["tasks"][0].clone();
        assert_eq!(task["evidence_count"], 0);
        assert!(task_from_ui(&task).is_ok());
        task["unknown_business_field"] = json!(true);
        assert!(task_from_ui(&task).is_err());
        task.as_object_mut()
            .unwrap()
            .remove("unknown_business_field");
        task["title"] = json!("Saved synthetic title");
        let saved = app.task_edit(&json!({"task":task})).unwrap();
        assert_eq!(saved["title"], "Saved synthetic title");
        assert!(saved.get("evidence_count").is_none());
        let snapshot = app.snapshot()["tasks"][0].clone();
        // The zero quota proves the real AI-edit boundary accepted the snapshot
        // without permitting any provider invocation.
        let error = app
            .task_ai_edit(&json!({"task":snapshot,"text":"Update the synthetic description"}))
            .unwrap_err();
        assert!(error.contains("每日AI调用上限"), "{error}");
        assert_eq!(app.state.lock().unwrap().calls, 0);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_fingerprints_preserve_real_pending_edits_and_new_replies_after_reload() {
        let root = std::env::temp_dir().join(format!("shishi-fingerprint-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            for id in ["same", "edited", "invalid"] {
                let mut m = Message {
                    id: id.into(),
                    text: "Original".into(),
                    time: now() - 86_000_000,
                    ..Default::default()
                };
                let legacy = serde_json::to_string(&m).unwrap();
                s.analyzed.insert(
                    id.into(),
                    if id == "invalid" {
                        "invalid".into()
                    } else {
                        legacy.clone()
                    },
                );
                s.message_filter.checked.insert(id.into(), legacy);
                m.source_name = "Renamed source".into();
                m.group = "Renamed room".into();
                m.reply = "reply metadata".into();
                if id == "edited" {
                    m.text = "Actual edit".into();
                }
                s.messages.insert(id.into(), m);
            }
            s.messages.insert(
                "new-reply".into(),
                Message {
                    id: "new-reply".into(),
                    text: "Reply body".into(),
                    reply: "same".into(),
                    time: now(),
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        let pending = crate::context_budget::pending(&s)
            .into_iter()
            .map(|m| m.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            pending,
            BTreeSet::from(["edited".into(), "invalid".into(), "new-reply".into()])
        );
        assert!(!s.message_filter.checked.contains_key("same"));
        assert!(!fingerprint_matches(
            &s.messages["edited"],
            s.message_filter.checked.get("edited")
        ));
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_automation_chain_prevents_calls_reservations_and_progress_but_keeps_manual_operations(
    ) {
        let root = std::env::temp_dir().join(format!("shishi-automation-chain-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.test_force_working();
        for (scan, analysis, early) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            {
                let mut s = app.state.lock().unwrap();
                s.config.enabled = scan;
                s.config.auto_ai = analysis;
                s.config.message_filter_enabled = early;
                s.config.daily_calls = 100;
            }
            app.filter_tick().unwrap();
            assert_eq!(
                app.ask_ai_mode("message_filter", &json!({}), AiMode::Automatic)
                    .unwrap_err(),
                crate::message_filter::AUTOMATION_DISABLED
            );
            assert_eq!(
                app.reserve_ai_inner(AiMode::Automatic, None, "message_filter", None, None, None)
                    .err()
                    .unwrap(),
                crate::message_filter::AUTOMATION_DISABLED
            );
            if !scan || !analysis {
                assert_eq!(
                    app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
                    "自动 AI 已关闭，消息已保存"
                );
                assert_eq!(
                    app.reserve_ai_inner(AiMode::Automatic, None, "classify", None, None, None)
                        .err()
                        .unwrap(),
                    crate::message_filter::AUTOMATION_DISABLED
                );
            }
            let s = app.state.lock().unwrap();
            assert_eq!(s.calls, 0);
            assert_eq!(s.message_filter.calls, 0);
            assert_eq!(s.message_filter.last_run, 0);
            assert!(s.analyzed.is_empty());
            assert_eq!(
                (
                    s.config.enabled,
                    s.config.auto_ai,
                    s.config.message_filter_enabled
                ),
                (scan, analysis, early)
            );
        }
        app.state.lock().unwrap().config.enabled = false;
        assert_eq!(
            app.scan(AiMode::Automatic).unwrap()["skipped"],
            "增量消息扫描已关闭"
        );
        app.reserve_ai_inner(AiMode::Manual, None, "classify", None, None, None)
            .unwrap();
        assert_eq!(app.state.lock().unwrap().calls, 1);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn filter_decision_consumed_at_durable_reservation_not_success() {
        let root = std::env::temp_dir().join(format!("shishi-filter-once-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.enabled = false;
            s.config.auto_ai = true;
            s.config.message_filter_enabled = true;
            s.message_filter.early = true;
            s.message_filter.trigger = "message_filter".into();
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "Synthetic".into(),
                    time: now(),
                    ..Default::default()
                },
            );
            s.config.daily_calls = 0;
        }
        assert!(app
            .reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .is_err());
        assert!(app.state.lock().unwrap().message_filter.early);
        app.state.lock().unwrap().config.daily_calls = 100;
        // Failure to persist a start must neither consume the decision nor invoke AI.
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        assert!(app
            .reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .is_err());
        assert!(app.state.lock().unwrap().message_filter.early);
        crate::test_support::persistence_failure(&app.root, false).unwrap();
        app.reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .unwrap();
        {
            let s = app.state.lock().unwrap();
            assert!(!s.message_filter.early);
            assert!(s.message_filter.trigger.is_empty());
            assert!(s.analyzed.is_empty());
            assert_eq!(crate::context_budget::pending(&s).len(), 1);
        }
        // No success commit: covers provider failure, bad output and cancellation.
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        assert!(!app.state.lock().unwrap().message_filter.early);
        {
            let mut s = app.state.lock().unwrap();
            s.message_filter.early = true;
            s.message_filter.trigger = "message_filter".into();
            App::finish_message_batch(&mut s, BTreeMap::new(), &BTreeSet::new());
            assert!(
                s.message_filter.early,
                "a later decision survives the previous batch commit"
            );
        }
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn full_analysis_reservation_observes_the_whole_batch_without_marking_it_analyzed() {
        let root = std::env::temp_dir().join(format!("shishi-filter-batch-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let mut messages = BTreeMap::new();
        for n in 0..700 {
            let m = Message {
                id: format!("m{n}"),
                text: "Synthetic body".into(),
                time: now(),
                ..Default::default()
            };
            messages.insert(m.id.clone(), m);
        }
        {
            let mut s = app.state.lock().unwrap();
            s.messages = messages.clone().into();
            s.message_filter.early = true;
            s.message_filter.last_run = 1;
            s.config.daily_calls = 100;
        }
        app.reserve_ai_inner(
            AiMode::Manual,
            None,
            "classify",
            None,
            Some(1),
            Some(&messages),
        )
        .unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.message_filter.checked.len(), 700);
            assert!(s.analyzed.is_empty());
        }
        // A concurrently edited message must still be eligible for a new decision.
        {
            let mut s = app.state.lock().unwrap();
            s.messages.get_mut("m0").unwrap().text = "Real edit".into();
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        assert!(!fingerprint_matches(
            &s.messages["m0"],
            s.message_filter.checked.get("m0")
        ));
        assert!(crate::message_progress::filtered(&s, &s.messages["m1"]));
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn evidence_counts_only_live_unique_messages_across_roles() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-evidence-{}", id()))).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.messages.insert(
                "live".into(),
                Message {
                    id: "live".into(),
                    ..Default::default()
                },
            );
            s.messages.insert(
                "withdrawn".into(),
                Message {
                    id: "withdrawn".into(),
                    withdrawn: true,
                    ..Default::default()
                },
            );
            s.tasks.push(Task {
                id: "t".into(),
                evidence: TaskEvidence {
                    direct: vec!["live".into(), "missing".into()],
                    background: vec!["live".into()],
                    completion: vec!["withdrawn".into()],
                },
                ..Default::default()
            });
        }
        let snapshot = app.snapshot();
        assert_eq!(snapshot["tasks"][0]["evidence_count"], 1);
        assert_eq!(app.evidence("t").as_array().unwrap().len(), 1);
    }
    #[test]
    fn background_generation_splits_and_respects_manual_edits() {
        let root = std::env::temp_dir().join(format!("shishi-gen-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let draft = app
            .create_generation(&json!({"text":"新增客户查询需求，并调研缓存优化"}))
            .unwrap();
        assert_eq!(draft.generation, "pending");
        app.complete_generation(&draft,Ok(json!({"tasks":[{"title":"客户查询需求","category":"新需求","environment":"不适用","steps_changed":true,"steps":[{"id":"s1","title":"确认需求","status":"pending","depends_on":[],"waiting_for":""}]},{"title":"缓存优化调研","category":"调研","environment":"不适用"}]}))).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.tasks.len(), 2);
            assert!(s.tasks.iter().all(|t| t.generation.is_empty()));
            // Retired default categories fall back to the compact adopted catalog.
            assert!(s.tasks[0].category.is_empty());
            assert!(s.tasks[0].tags.is_empty());
            assert!(s.tasks[0].steps.is_empty());
        }
        let draft2 = app
            .create_generation(&json!({"text":"待修改事项"}))
            .unwrap();
        let mut edited = json!(draft2);
        edited["title"] = json!("人工修改的标题");
        app.task_edit(&json!({"task":edited})).unwrap();
        app.complete_generation(&draft2, Ok(json!({"tasks":[{"title":"不应覆盖"}]})))
            .unwrap();
        assert!(app
            .state
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|t| t.title == "人工修改的标题"));
        let draft3 = app
            .create_generation(&json!({"text":"会失败的请求"}))
            .unwrap();
        app.complete_generation(&draft3, Err("测试错误".into()))
            .unwrap();
        assert!(app
            .state
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|t| t.id == draft3.id && t.generation == "failed"));
        for name in ["state.json", "state.backup.json"] {
            let _ = fs::remove_file(root.join("data").join(name));
        }
        let _ = fs::remove_file(root.join(format!("data/logs/{}.jsonl", date())));
        let _ = fs::remove_dir(root.join("data/logs"));
        let _ = fs::remove_dir(root.join("data/tmp"));
        let _ = fs::remove_dir(root.join("data"));
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn scan_control_validates_and_persists_without_replacing_other_settings() {
        let root = std::env::temp_dir().join(format!("shishi-control-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        assert!(app.scan_control(&json!({"interval_minutes":0})).is_err());
        assert!(app.scan_control(&json!({"interval_minutes":1.5})).is_err());
        app.scan_control(&json!({"enabled":false,"interval_minutes":12}))
            .unwrap();
        let reloaded = App::load(root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        assert!(!s.config.enabled);
        assert_eq!(s.config.interval_minutes, 12);
        assert!(s.config.leaders.is_empty());
        drop(s);
        for name in ["state.json", "state.backup.json"] {
            let _ = fs::remove_file(root.join("data").join(name));
        }
        let _ = fs::remove_dir(root.join("data/logs"));
        let _ = fs::remove_dir(root.join("data/tmp"));
        let _ = fs::remove_dir(root.join("data"));
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn manual_edits_are_audited_not_locked_and_persisted() {
        let root = std::env::temp_dir().join(format!("shishi-test-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task=app.task_edit(&json!({"task":{"title":"测试延期","due":"2026-10-16T17:30:00+08:00","priority":"高"}})).unwrap();
        assert_eq!(task["manual"], true);
        assert_eq!(task["locked"], false);
        let s = app.state.lock().unwrap();
        assert_eq!(s.audit.last().unwrap().kind, "manual_edit");
        assert!(s.audit.last().unwrap().before.is_null());
        drop(s);
        let reloaded = App::load(root.clone()).unwrap();
        assert_eq!(
            reloaded.state.lock().unwrap().tasks[0].due,
            "2026-10-16T17:30:00+08:00"
        );
        for name in ["state.json", "state.backup.json"] {
            let _ = std::fs::remove_file(root.join("data").join(name));
        }
        let _ = std::fs::remove_dir(root.join("data/logs"));
        let _ = std::fs::remove_dir(root.join("data/tmp"));
        let _ = std::fs::remove_dir(root.join("data"));
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn stale_edit_cannot_overwrite_newer_task() {
        let root = std::env::temp_dir().join(format!("shishi-test-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let mut task = app.task_edit(&json!({"task":{"title":"旧版本"}})).unwrap();
        task["updated"] = json!(0);
        assert!(app.task_edit(&json!({"task":task})).is_err());
        for name in ["state.json", "state.backup.json"] {
            let _ = std::fs::remove_file(root.join("data").join(name));
        }
        let _ = std::fs::remove_dir(root.join("data/logs"));
        let _ = std::fs::remove_dir(root.join("data/tmp"));
        let _ = std::fs::remove_dir(root.join("data"));
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn source_withdrawal_cancels_auto_only() {
        let mut s = State::default();
        s.messages.insert(
            "m".into(),
            Message {
                id: "m".into(),
                withdrawn: true,
                ..Default::default()
            },
        );
        *s.tasks = vec![
            Task {
                id: "a".into(),
                evidence: TaskEvidence {
                    direct: vec!["m".into()],
                    ..Default::default()
                },
                status: "待处理".into(),
                ..Default::default()
            },
            Task {
                id: "b".into(),
                manual: true,
                evidence: TaskEvidence {
                    direct: vec!["m".into()],
                    ..Default::default()
                },
                status: "待处理".into(),
                ..Default::default()
            },
        ];
        reconcile_withdrawals(&mut s);
        assert_eq!(s.tasks[0].status, "已取消");
        assert_eq!(s.tasks[1].status, "待处理");
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn pending_work_survives_restart() {
        let root = std::env::temp_dir().join(format!("shishi-recovery-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = app
            .create_generation(&json!({"text":"准备方案并调研连接池"}))
            .unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.pending_scan = true;
            s.pending_analysis = true;
            s.pending_learning = true;
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let s = app.state.lock().unwrap();
        assert!(s.pending_scan && s.pending_analysis && s.pending_learning);
        assert_eq!(s.tasks[0].id, task.id);
        assert_eq!(s.tasks[0].generation, "pending");
        assert!(s.tasks[0].generation_error.is_empty());
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod work_hours_tests {
    use super::*;
    #[test]
    fn automatic_is_gated_off_hours_and_manual_still_uses_the_budget() {
        let root = std::env::temp_dir().join(format!("shishi-refresh-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let mut state = State::default();
        state.config.work_start = (local + chrono::Duration::hours(1))
            .format("%H:%M")
            .to_string();
        state.config.work_end = (local + chrono::Duration::hours(2))
            .format("%H:%M")
            .to_string();
        // Reaching the normal budget guard proves analysis ran without launching a real model.
        state.config.daily_calls = 0;
        state.messages.insert(
            "pending".into(),
            Message {
                id: "pending".into(),
                private: true,
                text: "请检查发布结果".into(),
                time: now(),
                ..Default::default()
            },
        );
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert!(app
            .analyze_after_scan(AiMode::Manual)
            .unwrap_err()
            .contains("每日"));
        assert!(app.reserve_ai(AiMode::Manual, None).is_err());
        assert_eq!(app.state.lock().unwrap().calls, 0);
        app.test_force_working();
        assert!(app.ai_allowed(AiMode::Automatic));
        assert!(app
            .analyze_after_scan(AiMode::Manual)
            .unwrap_err()
            .starts_with("已达到每日AI调用上限"));
        app.state.lock().unwrap().config.auto_ai = false;
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "自动 AI 已关闭，消息已保存"
        );
        app.state.lock().unwrap().config.auto_ai = true;
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "等待分析间隔或单批体积阈值"
        );
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.calls, 0);
            assert!(s.pending_analysis);
            assert!(s.analyzed.is_empty());
        }
        app.state.lock().unwrap().messages.clear();
        assert_eq!(
            app.analyze_after_scan(AiMode::Manual).unwrap()["skipped"],
            "没有相关变化，不调用AI"
        );
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn after_hours_defers_automatic_ai_and_preserves_manual_access() {
        let root = std::env::temp_dir().join(format!("shishi-hours-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let config = Config {
            codex: root.join("must-not-launch.exe").to_string_lossy().into(),
            work_start: (local + chrono::Duration::hours(1))
                .format("%H:%M")
                .to_string(),
            work_end: (local + chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string(),
            ..Default::default()
        };
        let mut state = State {
            config,
            pending_analysis: true,
            pending_learning: true,
            ..Default::default()
        };
        state.messages.insert(
            "waiting".into(),
            Message {
                id: "waiting".into(),
                private: true,
                text: "请排查连接异常".into(),
                time: now(),
                ..Default::default()
            },
        );
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let draft = app
            .create_generation(&json!({"text":"明天准备发布"}))
            .unwrap();
        assert!(!app.snapshot()["in_work_hours"].as_bool().unwrap());
        assert_eq!(
            app.reserve_ai(AiMode::Automatic, None).err().unwrap(),
            work_hours::DEFERRED
        );
        assert_eq!(
            app.analyze(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert_eq!(
            app.learn(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert_eq!(
            app.report("晚报", AiMode::Automatic).unwrap()["summary_status"],
            "pending"
        );
        assert_eq!(
            app.archive(AiMode::Automatic).unwrap()["skipped"],
            "归档失败或等待下次到点；失败仅手动重试"
        );
        app.generate_background(draft.clone(), AiMode::Automatic);
        app.resume_pending();
        app.tick();
        // A manual reservation is independent of the automatic work schedule.
        assert!(app.ai_allowed(AiMode::Manual));
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.calls, 0);
            assert_eq!(s.unreported_calls, 0);
            assert!(s.pending_analysis && s.pending_learning);
            assert!(s.analyzed.is_empty());
            assert_eq!(s.messages["waiting"].text, "请排查连接异常");
            assert_eq!(
                s.tasks
                    .iter()
                    .find(|t| t.id == draft.id)
                    .unwrap()
                    .generation,
                "pending"
            );
            assert!(s.reports.iter().all(|r| r["summary_status"] == "pending"));
        }
        assert!(!root.join("data/ai-traces").exists());
        // Automatic work still needs a working/overtime state.
        app.test_force_working();
        assert!(app.reserve_ai(AiMode::Manual, None).is_ok());
        assert_eq!(app.state.lock().unwrap().calls, 1);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    #[test]
    fn progress_must_be_new_and_not_older_than_current_stage() {
        let task = Task {
            evidence: TaskEvidence {
                direct: vec!["current".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut messages = BTreeMap::new();
        for (id, time, withdrawn) in [
            ("current", 200, false),
            ("older", 100, false),
            ("new", 300, false),
            ("same_time", 200, false),
            ("withdrawn", 400, true),
        ] {
            messages.insert(
                id.into(),
                Message {
                    id: id.into(),
                    time,
                    withdrawn,
                    ..Default::default()
                },
            );
        }
        assert!(has_new_progress(&task, &["new".into()], &messages));
        assert!(has_new_progress(&task, &["same_time".into()], &messages));
        for id in ["current", "older", "withdrawn", "missing"] {
            assert!(!has_new_progress(&task, &[id.into()], &messages));
        }
    }
}

#[cfg(test)]
mod today_tests {
    use super::*;
    #[test]
    fn tomorrow_survives_restart_and_returns_at_midnight_without_changing_deadline() {
        let root = std::env::temp_dir().join(format!("shishi-tomorrow-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let task = app
            .task_edit(&json!({"task":{"title":"发布","due":"2026-12-31T17:00:00+08:00"}}))
            .unwrap();
        app.task_action(&json!({"id":task["id"],"action":"tomorrow"}))
            .unwrap();
        let restored = App::load(root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        let t = &s.tasks[0];
        assert_eq!(t.due, task["due"]);
        assert_eq!(t.status, "待处理");
        assert!(!is_today_task(t, &s.audit, t.deferred_until - 1));
        assert!(is_today_task(t, &s.audit, t.deferred_until));
        let last = s.audit.len() - 1;
        drop(s);
        restored.undo(last).unwrap();
        assert_eq!(restored.state.lock().unwrap().tasks[0].deferred_until, 0);
        restored
            .task_action(&json!({"id":task["id"],"action":"tomorrow"}))
            .unwrap();
        restored
            .task_action(&json!({"id":task["id"],"action":"resume_today"}))
            .unwrap();
        assert!(restored.snapshot()["tasks"][0]["today"].as_bool().unwrap());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn today_keeps_completed_even_archived_but_not_yesterdays_edited_completion() {
        let at = 1_792_080_000_000;
        let yesterday = at - 86_400_000;
        let mut t = Task {
            id: "x".into(),
            status: "已完成".into(),
            created: yesterday,
            updated: at,
            archived: true,
            ..Default::default()
        };
        let mut audit = vec![Audit {
            time: yesterday,
            kind: "manual_status".into(),
            task_id: "x".into(),
            detail: String::new(),
            change_summary: String::new(),
            before: json!({"status":"待处理"}),
            after: json!({"status":"已完成"}),
        }];
        assert!(!is_today_task(&t, &audit, at));
        audit[0].time = at;
        assert!(is_today_task(&t, &audit, at));
        t.status = "待处理".into();
        t.archived = false;
        t.updated = yesterday;
        assert!(is_today_task(&t, &[], at));
        t.merged_into = "other".into();
        assert!(!is_today_task(&t, &audit, at));
    }
    #[test]
    fn status_index_picks_the_latest_entry_into_the_current_status() {
        let entry = |time, task: &str, from: &str, to: &str| Audit {
            time,
            kind: "manual_status".into(),
            task_id: task.into(),
            detail: String::new(),
            change_summary: String::new(),
            before: json!({"status":from}),
            after: json!({"status":to}),
        };
        let audit = vec![
            entry(10, "a", "待处理", "已完成"),
            entry(20, "a", "已完成", "待处理"),
            entry(30, "a", "待处理", "已完成"),
            entry(40, "a", "已完成", "已完成"),
            entry(50, "b", "待处理", "已完成"),
        ];
        let index = StatusChanges::new(&audit);
        let task = |id: &str, status: &str| Task {
            id: id.into(),
            status: status.into(),
            updated: 7,
            ..Default::default()
        };
        assert_eq!(
            index.changed_at(&task("a", "已完成")),
            30,
            "no-op entry ignored"
        );
        assert_eq!(index.changed_at(&task("a", "待处理")), 20);
        assert_eq!(index.changed_at(&task("b", "已完成")), 50);
        assert_eq!(
            index.changed_at(&task("c", "已完成")),
            7,
            "falls back to updated"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn page_sections_are_built_only_on_request_and_full_snapshot_keeps_all() {
        let root = std::env::temp_dir().join(format!("shishi-sections-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let heavy = ["handling", "audit", "evolution", "reports", "messages"];
        let full = app.snapshot();
        for key in heavy {
            assert!(full.get(key).is_some(), "full snapshot keeps {key}");
        }
        let core = app.snapshot_sections(Some(&json!([])));
        for key in heavy {
            assert!(core.get(key).is_none(), "core omits {key}");
        }
        assert!(core["tasks"].is_array() && core["config"].is_object());
        let reports = app.snapshot_sections(Some(&json!(["reports"])));
        assert!(reports["reports"].is_array());
        assert!(reports.get("audit").is_none());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn ui_pulse_is_stable_when_idle_and_changes_on_save_or_activity() {
        let root = std::env::temp_dir().join(format!("shishi-pulse-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let idle = app.ui_pulse().unwrap();
        assert_eq!(app.ui_pulse(), Some(idle));
        let state = app.state.lock().unwrap().clone();
        app.save(&state).unwrap();
        let saved = app.ui_pulse().unwrap();
        assert_ne!(saved, idle);
        app.log("scan", json!({"rows":0}));
        assert_ne!(app.ui_pulse().unwrap(), saved);
        let _held = app.state.lock().unwrap();
        assert_eq!(app.ui_pulse(), None, "a busy state skips the tick");
        drop(_held);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    fn fixture() -> (App, PathBuf, i64) {
        let root = std::env::temp_dir().join(format!("shishi-schedule-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let at = deadline_ms("2026-10-15T10:00:00+08:00").unwrap();
        app.state.lock().unwrap().schedules.push(Schedule {
            id: "inspection".into(),
            title: "巡检".into(),
            description: "核对运行状态".into(),
            priority: "普通".into(),
            environment: "未指定".into(),
            category: "日常事务".into(),
            frequency: "daily".into(),
            time: "09:00".into(),
            enabled: true,
            active_from: at - 86_400_000,
            ..Default::default()
        });
        (app, root, at)
    }
    #[test]
    fn generates_once_across_restarts_and_independent_daily_tasks() {
        let (app, root, at) = fixture();
        assert!(app.state.lock().unwrap().config.enabled);
        app.ai_busy.store(true, Ordering::SeqCst);
        assert_eq!(app.generate_scheduled(at).unwrap(), 1);
        let restored = App::load(root.clone()).unwrap();
        assert_eq!(restored.generate_scheduled(at + 1000).unwrap(), 0);
        let first = restored.snapshot()["tasks"][0].clone();
        restored
            .task_action(&json!({"id":first["id"],"action":"done"}))
            .unwrap();
        assert_eq!(restored.generate_scheduled(at + 86_400_000).unwrap(), 1);
        let s = restored.state.lock().unwrap();
        assert_eq!(s.tasks.len(), 2);
        assert_eq!(s.tasks[0].status, "待处理");
        assert_eq!(s.tasks[1].status, "已完成");
        assert_ne!(s.tasks[0].id, s.tasks[1].id);
        assert_eq!(s.tasks[0].scheduled_for, "2026-10-16");
        assert_eq!(s.calls, 0);
        drop(s);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn missed_days_are_not_backfilled_and_save_failure_is_retryable() {
        let (app, root, at) = fixture();
        crate::test_support::persistence_failure(&root, true).unwrap();
        assert!(app.generate_scheduled(at + 5 * 86_400_000).is_err());
        assert!(app.state.lock().unwrap().tasks.is_empty());
        assert!(app.state.lock().unwrap().schedules[0]
            .last_generated_on
            .is_empty());
        crate::test_support::persistence_failure(&root, false).unwrap();
        assert_eq!(app.generate_scheduled(at + 5 * 86_400_000).unwrap(), 1);
        assert_eq!(app.snapshot()["tasks"][0]["scheduled_for"], "2026-10-20");
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn pause_edit_delete_and_stale_edit_preserve_generated_tasks() {
        let (app, root, at) = fixture();
        app.generate_scheduled(at).unwrap();
        let mut rule = app.snapshot()["schedules"][0].clone();
        rule["enabled"] = json!(false);
        let paused = app.schedule_edit(&json!({"schedule":rule})).unwrap();
        assert!(app.schedule_edit(&json!({"schedule":rule})).is_err());
        assert_eq!(app.generate_scheduled(at + 86_400_000).unwrap(), 0);
        assert!(app
            .schedule_delete(&json!({"id":rule["id"],"updated":rule["updated"]}))
            .is_err());
        app.schedule_delete(&json!({"id":paused["id"],"updated":paused["updated"]}))
            .unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.schedules.is_empty());
        assert_eq!(s.tasks.len(), 1);
        drop(s);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn ai_draft_requires_explicit_valid_rule() {
        assert!(parse_schedule_draft(json!({"error":"请补充生成时间","schedule":{}})).is_err());
        assert!(parse_schedule_draft(
            json!({"error":"","schedule":{"title":"发布","frequency":"weekly","time":"15:00"}})
        )
        .is_err());
        let draft = parse_schedule_draft(json!({"error":"","schedule":{"title":"发布","frequency":"weekly","weekdays":[5],"time":"15:00"}})).unwrap();
        assert!(draft.enabled);
        assert!(draft.id.is_empty());
    }
}

#[cfg(test)]
mod ai_edit_tests {
    use super::*;
    fn fixture() -> (App, PathBuf, Task) {
        let root = std::env::temp_dir().join(format!("shishi-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let t = Task {
            id: id(),
            title: "确认会议安排".into(),
            description: "核对议程与参加方式".into(),
            category: "问题排查".into(),
            priority: "普通".into(),
            environment: "未指定".into(),
            status: "待处理".into(),
            evidence: TaskEvidence {
                direct: vec!["source-1".into()],
                ..Default::default()
            },
            due: "2026-10-16T17:00:00+08:00".into(),
            created: now() - 1000,
            updated: now(),
            deferred_until: schedule::tomorrow_start(now()),
            schedule_id: "rule".into(),
            scheduled_for: date(),
            ..Default::default()
        };
        app.state.lock().unwrap().tasks.push(t.clone());
        (app, root, t)
    }
    #[test]
    fn updates_only_requested_fields_and_can_undo() {
        let (app, root, mut old) = fixture();
        old.enabled_tool_ids = vec!["synthetic-bound-tool".into()];
        app.state.lock().unwrap().tasks[0].enabled_tool_ids = old.enabled_tool_ids.clone();
        let saved = app.complete_ai_edit(&old, old.clone(), "已经修复，现在等测试验证", json!({"reason":"根据人工进展调整阶段","changes":[{"field":"title","value":"确认更新后的会议安排"},{"field":"waiting_for","value":"测试同事"}]})).unwrap();
        let current: Task = serde_json::from_value(saved).unwrap();
        assert_eq!(current.id, old.id);
        assert_eq!(current.evidence, old.evidence);
        assert_eq!(current.enabled_tool_ids, old.enabled_tool_ids);
        assert_eq!(current.created, old.created);
        assert_eq!(current.due, old.due);
        assert_eq!(current.status, old.status);
        assert_eq!(current.deferred_until, old.deferred_until);
        assert_eq!(current.schedule_id, old.schedule_id);
        assert_eq!(current.title, "确认更新后的会议安排");
        let reopened = App::load(root.clone()).unwrap();
        assert_eq!(
            reopened.state.lock().unwrap().tasks[0].title,
            "确认更新后的会议安排"
        );
        reopened.undo(0).unwrap();
        assert_eq!(reopened.state.lock().unwrap().tasks[0].title, old.title);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn rejects_stale_invalid_and_immutable_changes() {
        let (app, root, old) = fixture();
        for result in [
            json!({"changes":[{"field":"id","value":"new"}]}),
            json!({"changes":[{"field":"due","value":"明天"}]}),
            json!({"changes":[{"field":"status","value":"进行中"}]}),
            json!({"changes":[{"field":"title","value":"甲"},{"field":"title","value":"乙"}]}),
        ] {
            assert!(app
                .complete_ai_edit(&old, old.clone(), "修改", result)
                .is_err());
            assert_eq!(json!(app.state.lock().unwrap().tasks[0]), json!(old));
        }
        app.task_action(&json!({"id":old.id,"action":"done"}))
            .unwrap();
        assert!(app
            .complete_ai_edit(
                &old,
                old.clone(),
                "改标题",
                json!({"changes":[{"field":"title","value":"过期结果"}]})
            )
            .is_err());
        assert_eq!(app.state.lock().unwrap().tasks[0].status, "已完成");
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn failed_save_does_not_apply_ai_edit() {
        let (app, root, old) = fixture();
        crate::test_support::persistence_failure(&root, true).unwrap();
        assert!(app
            .complete_ai_edit(
                &old,
                old.clone(),
                "改为高优先级",
                json!({"changes":[{"field":"priority","value":"高"}]})
            )
            .is_err());
        assert_eq!(json!(app.state.lock().unwrap().tasks[0]), json!(old));
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    #[test]
    fn notes_and_suggestions_survive_incremental_reports() {
        let root = std::env::temp_dir().join(format!("shishi-archive-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            s.config.daily_calls = 0;
            s.tasks.push(Task {
                id: "x".into(),
                title: "验证修复".into(),
                status: "待处理".into(),
                created: now(),
                updated: 42,
                ..Default::default()
            });
            s.suggestions.push(Suggestion {
                id: "keep".into(),
                title: "之前的建议".into(),
                rule: "保留未处理建议".into(),
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        assert!(app
            .archive_task(
                &json!({"id":"x","updated":41,"status":"已完成","note":"不要只凭收到关闭事项"})
            )
            .is_err());
        app.archive_task(
            &json!({"id":"x","updated":42,"status":"已完成","note":"不要只凭收到关闭事项"}),
        )
        .unwrap();
        let first = app.report("早报", AiMode::Manual).unwrap();
        let second = app.report("晚报", AiMode::Manual).unwrap();
        assert_eq!(first["suggestions"][0]["id"], "keep");
        assert_eq!(second["suggestions"][0]["id"], "keep");
        assert_eq!(second["previous_report"]["id"], first["id"]);
        assert_eq!(first["delta"].as_array().unwrap().len(), 1);
        assert!(second["delta"].as_array().unwrap().is_empty());
        assert_eq!(first["tasks"][0]["archive_note"], "不要只凭收到关闭事项");
        assert_eq!(app.state.lock().unwrap().learning_cursor, 0);
        drop(app);
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        assert_eq!(
            app.state.lock().unwrap().tasks[0].archive_note,
            "不要只凭收到关闭事项"
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn outgoing_messages_are_candidates_but_withdrawals_are_not() {
        let mut m = Message {
            mine: true,
            text: "请协助核对".into(),
            ..Default::default()
        };
        assert!(m.relevant());
        m.withdrawn = true;
        assert!(!m.relevant());
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn fixed_reduction_order_blocks_automatic_retries_without_cursor_or_analyzed_changes() {
        let root = std::env::temp_dir().join(format!("shishi-budget-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        let base = Config::default();
        let fixed = base.max_input_chars - ai::message_budget(&base, "classify");
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = fixed + 300;
            for i in 0..12 {
                let mid = format!("m{i}");
                s.messages.insert(
                    mid.clone(),
                    Message {
                        id: mid.clone(),
                        text: "消息内容".repeat(5),
                        time: now() - i * 1000,
                        private: true,
                        ..Default::default()
                    },
                );
                s.tasks.push(Task {
                    id: format!("t{i}"),
                    title: "需要核对的事项标题".repeat(6),
                    description: "需要核对的内容描述".repeat(12),
                    status: "待处理".into(),
                    environment: "未指定".into(),
                    evidence: TaskEvidence {
                        direct: vec![mid],
                        ..Default::default()
                    },
                    updated: now(),
                    ..Default::default()
                });
            }
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR), "{error}");
        {
            let s = app.state.lock().unwrap();
            assert!(s.auto_analysis_blocked);
            assert!(s
                .auto_analysis_block_reason
                .starts_with(ai::INPUT_LIMIT_ERROR));
            assert_eq!(s.calls, 0);
            assert!(s.analyzed.is_empty());
            assert_eq!(s.archive_ai_cursor, Some(0));
            assert_eq!(s.last_analysis_at, 0);
            assert_eq!(s.last_analysis_success_at, 0);
            assert!(s.pending_analysis);
            assert_eq!(s.messages.len(), 12);
        }
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "自动分析已暂停：本批仍超过输入上限，请手动精简后重试"
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn zero_message_budget_pauses_automatic_without_consuming_messages() {
        let root = std::env::temp_dir().join(format!("shishi-budget-zero-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = 1;
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "材料".into(),
                    time: now(),
                    private: true,
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR));
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.archive_ai_cursor, Some(0));
        assert!(s.pending_analysis);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn single_message_that_cannot_fit_pauses_automatic_instead_of_reporting_empty() {
        let root = std::env::temp_dir().join(format!("shishi-budget-first-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        let base = Config::default();
        let fixed = base.max_input_chars - ai::message_budget(&base, "classify");
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = fixed + 10;
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "很长".repeat(1_000),
                    time: now(),
                    private: true,
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.contains("单条消息"), "{error}");
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.archive_ai_cursor, Some(0));
        assert!(s.pending_analysis);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod evidence_identity_tests {
    use super::*;
    #[test]
    fn manual_feedback_keeps_the_frozen_versions_of_the_ai_decision() {
        let root = std::env::temp_dir().join(format!("shishi-decision-context-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let context = json!({"prompt_version":"p1","rule_version":2,"rule_ids":["r1"],"archived_memory_version":3});
        {
            let mut s = app.state.lock().unwrap();
            let task = Task {
                id: "t".into(),
                title: "待核对".into(),
                updated: 10,
                ..Default::default()
            };
            App::audit(
                &mut s,
                "ai_create",
                "t",
                "合成判断",
                Value::Null,
                json!(task),
            );
            s.tasks.push(task);
            App::stamp_decisions(&mut s, 0, &context);
            s.rule_version = 9; // Later adoption must not rewrite past provenance.
            app.save(&s).unwrap();
        }
        app.task_edit(&json!({"task":{"id":"t","updated":10,"title":"人工修正"}}))
            .unwrap();
        let s = app.state.lock().unwrap();
        let feedback = s.audit.last().unwrap();
        assert_eq!(feedback.before["decision_context"], context);
        assert_eq!(
            crate::context_budget::audit(feedback)["prior_decision"],
            context
        );
        assert_eq!(s.tasks[0].decision_context, context);
        drop(s);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejected_updates_stay_unanalyzed_and_manual_retry_survives_the_cap() {
        let at = now();
        let mut s = State::default();
        for (id, time) in [("a", at), ("b", at + 1), ("c", at + 2)] {
            s.messages
                .insert(id.into(), message(id, time, "请核对结果"));
        }
        s.tasks.push(Task {
            id: "t".into(),
            title: "核对".into(),
            updated: 10,
            ..Default::default()
        });
        s.tasks.push(Task {
            id: "conflict".into(),
            title: "人工已改".into(),
            updated: 30,
            ..Default::default()
        });
        let allowed = s.messages.clone();
        let action = |tid: &str, source: &str, title: &str| json!({"action":"update","task_id":tid,"sources":[source],"fields":[{"field":"title","value":title}],"reason":"新进展","confidence":0.99,"tags":[]});
        let actions = vec![
            action("t", "a", "第一步"),
            action("t", "b", "第二步"),
            action("conflict", "c", "过期修改"),
        ];
        let (applied, retry) = App::apply_actions_detailed(
            &mut s,
            &actions,
            &allowed,
            &BTreeMap::from([("t".into(), 10), ("conflict".into(), 20)]),
            &catalog(),
        )
        .unwrap();
        assert_eq!(applied, 2);
        assert_eq!(s.tasks[0].title, "第二步");
        assert_eq!(s.tasks[1].title, "人工已改");
        assert_eq!(retry, BTreeSet::from(["c".into()]));
        let fingerprints = allowed
            .iter()
            .map(|(id, m)| (id.clone(), fingerprint(m)))
            .collect::<BTreeMap<_, _>>();
        for _ in 0..3 {
            App::finish_message_batch(&mut s, fingerprints.clone(), &retry);
        }
        assert!(s.analyzed.contains_key("a") && s.analyzed.contains_key("b"));
        assert!(!s.analyzed.contains_key("c"));
        assert!(crate::context_budget::pending(&s).is_empty());
        assert_eq!(crate::context_budget::pending_for(&s, true).len(), 1);
        App::finish_message_batch(&mut s, fingerprints, &BTreeSet::new());
        assert!(s.analyzed.contains_key("c"));
        assert!(s.analysis_retry_counts.is_empty());
    }

    #[test]
    fn duplicate_create_cannot_bypass_manual_completion_and_step_guards() {
        let mut s = State::default();
        for (id, time) in [("old", 10), ("new", 200)] {
            s.messages.insert(id.into(), message(id, time, "核对发布"));
        }
        let confirmed = TaskStep {
            id: "confirmed".into(),
            title: "人工确认".into(),
            status: "completed".into(),
            confirmed: true,
            ..Default::default()
        };
        s.tasks.push(Task {
            id: "t".into(),
            title: "人工标题".into(),
            status: "待处理".into(),
            created: 20,
            updated: 100,
            manual: true,
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            steps: vec![confirmed.clone()],
            ..Default::default()
        });
        let allowed = s.messages.clone();
        let mut action = json!({"action":"create","sources":["old","new"],"completion_evidence":["new"],"fields":[{"field":"title","value":"模型标题"},{"field":"status","value":"已完成"}],"reason":"回复证据","confidence":0.8,"tags":[],"steps_changed":true,"steps":[{"id":"next","title":"下一步","status":"pending","depends_on":["confirmed"],"waiting_for":""}]});
        let versions = BTreeMap::from([("t".into(), 100)]);
        let (applied, retry) =
            App::apply_actions_detailed(&mut s, &[action.clone()], &allowed, &versions, &catalog())
                .unwrap();
        assert_eq!(applied, 0);
        assert!(retry.contains("new"));
        assert_eq!(s.tasks[0].title, "人工标题");
        action["confidence"] = json!(0.92);
        let (_, retry) =
            App::apply_actions_detailed(&mut s, &[action.clone()], &allowed, &versions, &catalog())
                .unwrap();
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.tasks[0].status, "待处理");
        assert!(retry.contains("new"));
        assert!(
            s.tasks[0]
                .steps
                .iter()
                .find(|x| x.id == "confirmed")
                .unwrap()
                == &confirmed
        );
        assert!(!s.tasks[0].steps.iter().any(|x| x.id == "next"));
        action["confidence"] = json!(0.99);
        let versions = BTreeMap::from([("t".into(), s.tasks[0].updated)]);
        let (_, retry) =
            App::apply_actions_detailed(&mut s, &[action], &allowed, &versions, &catalog())
                .unwrap();
        assert!(retry.is_empty());
        assert_eq!(s.tasks[0].status, "已完成");
    }
    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }
    fn catalog() -> Vec<crate::tags::Group> {
        vec![
            crate::tags::Group {
                display_priority: None,
                name: "事项类型".into(),
                values: vec!["日常事务".into(), "问题排查".into()],
            },
            crate::tags::Group {
                display_priority: None,
                name: "适用环境".into(),
                values: vec!["未指定".into(), "不适用".into()],
            },
        ]
    }
    #[test]
    fn unarchived_completed_task_is_matched_by_identity_and_receives_evidence() {
        let mut s = State::default();
        let old = Task {
            id: "old".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "已完成".into(),
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["old-msg".into()],
                ..Default::default()
            },
            updated: 100,
            created: 90,
            manual: true,
            ..Default::default()
        };
        s.messages
            .insert("old-msg".into(), message("old-msg", 50, "旧依据"));
        s.messages.insert(
            "new-msg".into(),
            message("new-msg", 200, "再次核对发布结果"),
        );
        s.messages.insert(
            "archived-msg".into(),
            message("archived-msg", 200, "归档依据"),
        );
        s.tasks.push(old);
        s.tasks.push(Task {
            id: "archived".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "已完成".into(),
            archived: true,
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["archived-msg".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let allowed = ["old-msg", "new-msg", "archived-msg"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let action = json!({
            "action":"create",
            "sources":["new-msg","old-msg"],
            "fields":[
                {"field":"title","value":"发布结果再次确认"},
                {"field":"description","value":"核对发布结果"}
            ],
            "reason":"延续发布验证",
            "confidence":0.9,
            "tags":[]
        });
        let applied = App::apply_actions(
            &mut s,
            &[action],
            &allowed,
            &BTreeMap::from([("old".into(), 100)]),
            &catalog(),
        )
        .unwrap();
        assert_eq!(applied, 1);
        let unarchived = s.tasks.iter().find(|t| t.id == "old").unwrap();
        assert_eq!(unarchived.status, "已完成");
        assert!(unarchived.evidence.direct.contains(&"new-msg".into()));
        assert_eq!(s.tasks.len(), 2);
        // The archived task is intentionally not used as a dedup identity: a
        // new create that matches the archived title would not be merged into it.
        let archived = s.tasks.iter().find(|t| t.id == "archived").unwrap();
        assert!(archived.archived);
    }
    #[test]
    fn direct_background_and_completion_evidence_stay_distinct_and_old_evidence_is_appended() {
        let mut s = State::default();
        s.messages
            .insert("old".into(), message("old", 300, "当前依据"));
        s.messages
            .insert("new".into(), message("new", 400, "新回复"));
        s.messages.insert("bg".into(), message("bg", 100, "背景"));
        s.messages
            .insert("done".into(), message("done", 500, "已完成"));
        s.messages
            .insert("older".into(), message("older", 50, "更早补充"));
        s.tasks.push(Task {
            id: "t".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "待处理".into(),
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            updated: 200,
            ..Default::default()
        });
        let allowed = ["old", "new", "bg", "done", "older"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let versions = BTreeMap::from([("t".to_string(), 200i64)]);
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["new"],"background_evidence":["bg"],"completion_evidence":["done"],
            "fields":[{"field":"status","value":"已完成"}],
            "reason":"明确完成","confidence":0.99,"tags":[]
        });
        App::apply_actions(&mut s, &[action], &allowed, &versions, &catalog()).unwrap();
        let task = s.tasks.iter().find(|t| t.id == "t").unwrap();
        assert_eq!(task.status, "已完成");
        assert_eq!(task.evidence.direct, vec!["new", "old"]);
        assert_eq!(task.evidence.background, vec!["bg"]);
        assert_eq!(task.evidence.completion, vec!["done"]);
        assert!(task.evidence.all().contains("done"));
        // An older message is appended as evidence without rolling the state back.
        let versions = BTreeMap::from([("t".to_string(), task.updated)]);
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["older"],
            "fields":[{"field":"title","value":"不应回退的标题"}],
            "reason":"补充更早证据","confidence":0.99,"tags":[]
        });
        App::apply_actions(&mut s, &[action], &allowed, &versions, &catalog()).unwrap();
        let task = s.tasks.iter().find(|t| t.id == "t").unwrap();
        assert_eq!(task.title, "发布验证");
        assert!(task.evidence.all().contains("older"));
        assert!(task.evidence.direct.contains(&"older".into()));
        assert_eq!(task.status, "已完成");
    }
}

#[cfg(test)]
mod correction_tests {
    use super::*;

    #[test]
    fn temporary_memory_is_writable_only_in_message_analysis_requests() {
        let (app, root) = app();
        for kind in ["classify", "report", "archive", "learning", "prompt_review"] {
            let state = app.state.lock().unwrap().clone();
            let mut context = app
                .capture_prepare_context_from(&state, kind, false, json!([]))
                .unwrap();
            let request = app
                .prepare_ai_request_with(kind, &json!({}), AiMode::Manual, &mut context)
                .unwrap();
            assert_eq!(request.payload["memory"]["writable"], kind == "classify");
            assert_eq!(
                request.payload["_local_context"]["memory"]["writable"],
                kind == "classify"
            );
        }
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_memory_write_and_inline_continuation_survive_preparation_and_reload() {
        let (mut app, root) = app();
        for (index, text) in ["合成记录：等待核对", "合成记录：已核对，等待确认"]
            .iter()
            .enumerate()
        {
            let state = app.state.lock().unwrap().clone();
            let mut context = app
                .capture_prepare_context_from(&state, "classify", false, json!([]))
                .unwrap();
            let request = app
                .prepare_ai_request_with(
                    "classify",
                    &json!({"changes":[]}),
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            let snapshot = &request.payload["_local_context"];
            assert_eq!(
                snapshot["memory"]["before"],
                state.analysis_memory.temporary
            );
            let meter = crate::analysis_context::Meter::default();
            if index > 0 {
                assert_eq!(
                    snapshot["memory"]["inline_text"],
                    state.analysis_memory.temporary
                );
                assert!(request
                    .prepared
                    .prompt
                    .contains(&state.analysis_memory.temporary));
            }
            let update = crate::analysis_context::memory_update(
                snapshot,
                &meter,
                &json!({"memory_update":[text]}),
            )
            .unwrap()
            .unwrap();
            app.save_temporary_memory(&update).unwrap();
            assert!(
                app.save_temporary_memory(&update).is_err(),
                "stale baseline cannot overwrite newer memory"
            );
            drop(app);
            app = App::load(root.clone()).unwrap();
            let state = app.state.lock().unwrap();
            assert_eq!(state.analysis_memory.temporary, *text);
            assert_eq!(state.analysis_memory.temporary_version, (index + 1) as u64);
        }
        assert!(app
            .save_temporary_memory(&json!({"text":"invalid"}))
            .is_err());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handling_learning_prepares_only_scoped_evidence_without_unavailable_retrieval() {
        let (app, root) = app();
        let state = app.state.lock().unwrap().clone();
        let mut context = app
            .capture_prepare_context_from(&state, "handling_learning", false, json!([]))
            .unwrap();
        let request = app
            .prepare_ai_request_with(
                "handling_learning",
                &json!({"current":{"prompt":"合成策略","routes":[]},"evidence":[]}),
                AiMode::Manual,
                &mut context,
            )
            .unwrap();
        assert!(request.payload.get("_local_context").is_none());
        assert!(request.prepared.tools.is_none());
        assert!(request.prepared.prompt.contains("只读补查：不可用"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn app() -> (App, PathBuf) {
        let root = std::env::temp_dir().join(format!("shishi-correction-{}", id()));
        {
            let app = App::load(root.clone()).unwrap();
            app.state.lock().unwrap().config.model_profiles = None;
            (app, root)
        }
    }

    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            source_id: "source".into(),
            chat: "chat".into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }

    #[test]
    fn partial_manual_edit_preserves_evidence_summary_schedule_and_archive_fields() {
        let (app, root) = app();
        {
            let mut s = app.state.lock().unwrap();
            s.tasks.push(Task {
                id: "t".into(),
                title: "原标题".into(),
                description: "原描述".into(),
                status: "待处理".into(),
                priority: "普通".into(),
                environment: "未指定".into(),
                summary: "模型生成的摘要".into(),
                summary_updated_at: 120,
                content_updated_at: 100,
                evidence: TaskEvidence {
                    direct: vec!["m1".into()],
                    background: vec!["m2".into()],
                    completion: vec!["m3".into()],
                },
                created: 50,
                updated: 100,
                schedule_id: "schedule".into(),
                scheduled_for: "2026-10-18".into(),
                deferred_until: 999,
                archive_note: "保留的归档备注".into(),
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        // An old/partial UI payload that omits new fields must not reset them.
        let saved = app
            .task_edit(&json!({"task":{
                "id":"t","updated":100,"status":"已完成","priority":"高"
            }}))
            .unwrap();
        let task: Task = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(task.title, "原标题");
        assert_eq!(task.description, "原描述");
        assert_eq!(task.evidence.direct, vec!["m1"]);
        assert_eq!(task.evidence.background, vec!["m2"]);
        assert_eq!(task.evidence.completion, vec!["m3"]);
        assert_eq!(task.summary, "模型生成的摘要");
        assert_eq!(task.summary_updated_at, 120);
        assert_eq!(task.content_updated_at, 100);
        assert_eq!(task.schedule_id, "schedule");
        assert_eq!(task.scheduled_for, "2026-10-18");
        assert_eq!(task.deferred_until, 999);
        assert_eq!(task.archive_note, "保留的归档备注");

        // Only a content edit invalidates the summary timestamp.
        let saved = app
            .task_edit(&json!({"task":{
                "id":"t","updated":saved["updated"],"title":"新的标题"
            }}))
            .unwrap();
        let task: Task = serde_json::from_value(saved).unwrap();
        assert_eq!(task.evidence.direct, vec!["m1"]);
        assert!(task.content_updated_at > 100);
        assert_eq!(task.summary, "原描述");
        assert_eq!(task.summary_updated_at, task.content_updated_at);
        assert_eq!(task.archive_note, "保留的归档备注");
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn generic_lexical_overlap_never_merges_independent_tasks_without_shared_evidence() {
        let mut s = State::default();
        s.messages
            .insert("old".into(), message("old", 100, "旧事项"));
        s.messages
            .insert("new".into(), message("new", 200, "另一个对象"));
        s.tasks.push(Task {
            id: "old-task".into(),
            title: "处理问题".into(),
            description: "处理问题".into(),
            environment: "生产".into(),
            status: "待处理".into(),
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            updated: 100,
            ..Default::default()
        });
        let allowed = ["old", "new"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let action = json!({
            "action":"create","sources":["new"],
            "fields":[
                {"field":"title","value":"处理问题"},
                {"field":"description","value":"处理问题"}
            ],
            "reason":"不同对象","confidence":0.9,"tags":[]
        });
        let catalog = crate::tags::groups(&s);
        App::apply_actions(&mut s, &[action], &allowed, &BTreeMap::new(), &catalog).unwrap();
        assert_eq!(s.tasks.len(), 2);
        assert!(s.tasks[1].evidence.direct.contains(&"new".into()));
        assert!(s.tasks[0].evidence.direct == vec!["old"]);
    }

    #[test]
    fn frozen_retrieval_survives_snapshot_rebuild_and_cross_chat_omitted_task_update() {
        let (app, root) = app();
        {
            let mut s = app.state.lock().unwrap();
            s.messages
                .insert("old".into(), message("old", 100, "最初问题"));
            let mut done = message("done", 300, "原始完成回复");
            done.chat = "other-chat".into();
            s.messages.insert("done".into(), done);
            s.tasks.push(Task {
                id: "t".into(),
                title: "核对".into(),
                description: "核对".into(),
                status: "待处理".into(),
                evidence: TaskEvidence {
                    direct: vec!["old".into()],
                    ..Default::default()
                },
                updated: 100,
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        let (batch, context, request) = {
            let s = app.state.lock().unwrap();
            let batch = crate::context_budget::classify_with_task_limit(
                &s,
                &[s.messages["old"].clone()],
                0,
            )
            .unwrap();
            let mut context = app
                .capture_prepare_context_from(&s, "classify", false, json!([]))
                .unwrap();
            let mut request = app
                .prepare_ai_request_with("classify", &batch.payload, AiMode::Manual, &mut context)
                .unwrap();
            request.frozen = Some(FrozenView::new(
                batch.allowed.clone(),
                batch.versions.clone(),
                &request.records,
            ));
            (batch, context, request)
        };
        assert!(!request.initial_tasks.contains("t"));
        // A cross-chat completion message and an omitted task are both present
        // in the frozen view, so the request may retrieve and apply them.
        let frozen = request.frozen.as_ref().unwrap();
        let mut allowed = frozen.initial_messages.clone();
        admit_retrieved(&mut allowed, &[json!("done"), json!("t")], frozen);
        assert_eq!(allowed["done"].text, "原始完成回复");
        let mut versions = frozen.task_versions.clone();
        versions.retain(|id, _| request.initial_tasks.contains(id) || id == "t");
        assert!(allowed.contains_key("done"));
        assert_eq!(versions.get("t"), Some(&100));
        let mut state = app.state.lock().unwrap();
        retain_live_frozen(&mut allowed, &mut versions, frozen, &state);
        assert!(allowed.contains_key("done"));
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["done"],"completion_evidence":["done"],
            "fields":[{"field":"status","value":"已完成"}],
            "reason":"跨会话完成证据","confidence":0.99,"tags":[]
        });
        let catalog = crate::tags::groups(&state);
        App::apply_actions(&mut state, &[action], &allowed, &versions, &catalog).unwrap();
        let task = state.tasks.iter().find(|task| task.id == "t").unwrap();
        assert_eq!(task.status, "已完成");
        assert_eq!(task.evidence.completion, vec!["done"]);
        drop(state);

        // Rebuild the shared snapshot while the request is conceptually in
        // flight, then mutate live state and add a new message. The old request
        // still sees its frozen body and can never admit the new message.
        {
            let mut s = app.state.lock().unwrap();
            s.messages.get_mut("done").unwrap().text = "并发改写后的正文".into();
            s.messages
                .insert("late".into(), message("late", 400, "请求开始后才出现"));
            app.save(&s).unwrap();
            crate::snapshots::ensure(&root, &s, now()).unwrap();
        }
        let shared = crate::snapshots::records(&root).unwrap();
        assert!(shared.iter().any(|row| row["id"] == "late"));
        assert!(shared
            .iter()
            .any(|row| row["id"] == "done" && row["data"]["text"] == "并发改写后的正文"));
        let frozen = request.frozen.as_ref().unwrap();
        let mut allowed = frozen.initial_messages.clone();
        admit_retrieved(&mut allowed, &[json!("done"), json!("late")], frozen);
        assert_eq!(allowed["done"].text, "原始完成回复");
        assert!(!allowed.contains_key("late"));
        let live = app.state.lock().unwrap();
        let mut versions = frozen.task_versions.clone();
        retain_live_frozen(&mut allowed, &mut versions, frozen, &live);
        assert!(!allowed.contains_key("done"));
        drop(live);
        drop(batch);
        drop(context);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn accepted_prepared_request_is_sent_with_frozen_runtime_and_measured_prompt() {
        let (app, root) = app();
        app.test_force_working();
        let mut messages = (0..12)
            .map(|i| Message {
                id: format!("m{}", i + 1),
                text: "需要核对的消息内容".repeat(20),
                time: now() + i,
                private: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        messages[0].images = vec![ImageRef {
            id: "synthetic-image".into(),
            path: root
                .join("data/media/synthetic.png")
                .to_string_lossy()
                .into(),
        }];
        let (mut context, limit, full_total, full_runtime, reduced_total) = {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = "00:00".into();
            s.config.work_end = "23:59".into();
            s.config.codex = "nonexistent-must-not-launch.exe".into();
            s.config.retain_ai_traces = true;
            s.config.max_input_chars = 60_000;
            for message in &messages {
                s.messages.insert(message.id.clone(), message.clone());
            }
            // Hidden snapshot ids participate in alias allocation but are not
            // part of this batch. Some collide with the m1.. alias space so
            // visible aliases are longer than their original ids.
            for i in 0..200 {
                let mut hidden = message(
                    &format!("hidden-{i}"),
                    now() - 1_000 - i,
                    "隐藏快照材料，不进入本批提示词",
                );
                hidden.private = true;
                s.analyzed.insert(hidden.id.clone(), fingerprint(&hidden));
                s.messages.insert(hidden.id.clone(), hidden);
            }
            for i in 13..40 {
                let mut hidden = message(&format!("m{i}"), now() - 2_000 - i, "别名冲突材料");
                hidden.private = true;
                s.analyzed.insert(hidden.id.clone(), fingerprint(&hidden));
                s.messages.insert(hidden.id.clone(), hidden);
            }
            for i in 0..12 {
                s.tasks.push(Task {
                    id: format!("t{i}"),
                    title: format!("事项{i}"),
                    description: "事项摘要内容".repeat(10),
                    status: "待处理".into(),
                    updated: 100 + i,
                    ..Default::default()
                });
            }
            s.tag_state.versions.push(crate::tags::Version {
                priority_kind: String::new(),
                assignments: vec![],
                id: "v1".into(),
                groups: vec![crate::tags::Group {
                    display_priority: None,
                    name: "事项类型".into(),
                    values: (0..40).map(|i| format!("类型{i}")).collect(),
                }],
                ..Default::default()
            });
            app.save(&s).unwrap();
            let mut context = app
                .capture_prepare_context_from(&s, "classify", false, json!([]))
                .unwrap();
            let full_batch =
                crate::context_budget::classify_with_task_limit(&s, &messages, usize::MAX).unwrap();
            let full = app
                .prepare_ai_request_with(
                    "classify",
                    &full_batch.payload,
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            let full_total = full.prepared.total;
            let full_runtime = full.runtime.clone();
            assert!(full.payload["_local_context"]["images"]
                .as_array()
                .is_some_and(|images| !images.is_empty()));
            assert!(full.payload["tag_catalog"]
                .as_array()
                .is_some_and(|g| !g.is_empty()));
            assert!(full
                .records
                .iter()
                .any(|row| row["kind"] == "message" && row["id"] == "hidden-77"));
            assert!(!full.payload["_local_context"]["records"]
                .to_string()
                .contains("hidden-77"));
            let encoded_alias_lengths = full.payload["_local_context"]["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["kind"] == "message")
                .filter_map(|row| row["id"].as_str())
                .filter(|id| id.starts_with('m') && id.chars().skip(1).all(|c| c.is_ascii_digit()))
                .map(|id| id.chars().count())
                .collect::<Vec<_>>();
            assert!(
                encoded_alias_lengths.iter().any(|length| *length >= 3),
                "expected hidden ids to lengthen aliases: {encoded_alias_lengths:?}"
            );
            let reduced_batch =
                crate::context_budget::classify_with_task_limit(&s, &messages[..10], 10).unwrap();
            let reduced = app
                .prepare_ai_request_with(
                    "classify",
                    &reduced_batch.payload,
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            assert_eq!(reduced.runtime, full_runtime);
            let reduced_total = reduced.prepared.total;
            let limit = reduced_total + 20;
            assert!(full_total > limit);
            assert!(reduced_total <= limit);
            context.config.max_input_chars = limit;
            s.config.max_input_chars = limit;
            app.save(&s).unwrap();
            (context, limit, full_total, full_runtime, reduced_total)
        };
        let (accepted_batch, accepted) = {
            let s = app.state.lock().unwrap();
            let mut changes = messages.clone();
            let mut task_limit = usize::MAX;
            loop {
                let batch =
                    crate::context_budget::classify_with_task_limit(&s, &changes, task_limit)
                        .unwrap();
                let mut request = app
                    .prepare_ai_request_with(
                        "classify",
                        &batch.payload,
                        AiMode::Manual,
                        &mut context,
                    )
                    .unwrap();
                assert_eq!(request.runtime, full_runtime);
                let total = request.prepared.total;
                if total <= limit {
                    request.frozen = Some(FrozenView::new(
                        batch.allowed.clone(),
                        batch.versions.clone(),
                        &request.records,
                    ));
                    break (batch, request);
                }
                if changes.len() > 10 {
                    changes.truncate(10);
                    continue;
                }
                if task_limit > 10 {
                    task_limit = 10;
                    continue;
                }
                panic!("reduction did not fit: {total} > {limit}");
            }
        };
        assert_eq!(accepted_batch.allowed.len(), 10);
        assert_eq!(
            accepted_batch.fingerprints.len(),
            accepted_batch.allowed.len()
        );
        assert_eq!(accepted.prepared.total, reduced_total);
        assert_eq!(
            accepted.prepared.prompt.chars().count(),
            accepted.prepared.prompt_chars
        );
        assert_eq!(
            accepted.prepared.prompt_chars
                + accepted.prepared.schema_chars
                + accepted.prepared.tool_chars
                + accepted.prepared.wire_chars,
            accepted.prepared.total
        );
        assert!(accepted.prepared.tools.is_some());
        drop(context);
        // The adapter is deliberately unreachable (fixture path), but the
        // exact-send trace is written before the CLI is validated.
        let result = app.invoke_ai_request(&accepted, AiMode::Manual);
        assert!(result.is_err());
        let traces = std::fs::read_dir(root.join("data/ai-traces"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let trace = traces
            .into_iter()
            .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
            .expect("trace dir");
        let sent = std::fs::read(trace.join("input.txt")).unwrap();
        assert_eq!(sent, accepted.prepared.prompt.as_bytes());
        assert_eq!(
            String::from_utf8(sent.clone()).unwrap().chars().count(),
            accepted.prepared.prompt_chars
        );
        assert_eq!(
            std::fs::read_to_string(trace.join("schema.json")).unwrap(),
            accepted.prepared.schema.to_string()
        );
        assert_eq!(
            std::fs::read_to_string(trace.join("tools.json")).unwrap(),
            accepted.prepared.tools.as_ref().unwrap().to_string()
        );
        assert!(full_total > limit);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn final_enriched_budget_failure_pauses_automatic_analysis_without_consuming_messages() {
        let (app, root) = app();
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = "00:00".into();
            s.config.work_end = "23:59".into();
            let fixed = s.config.max_input_chars - ai::message_budget(&s.config, "classify");
            s.config.max_input_chars = fixed + 3_000;
            s.analysis_memory.temporary = "临".repeat(4_000);
            s.analysis_memory.archived_entries.push(MemoryEntry {
                id: "mem".into(),
                key: "长期".into(),
                text: "记".repeat(4_000),
                version: 1,
                ..Default::default()
            });
            s.messages
                .insert("m".into(), message("m", now(), "请核对这个事项"));
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR), "{error}");
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s
            .auto_analysis_block_reason
            .starts_with(ai::INPUT_LIMIT_ERROR));
        assert_eq!(s.calls, 0);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.last_analysis_at, 0);
        assert_eq!(s.last_analysis_success_at, 0);
        assert!(s.pending_analysis);
        assert!(s.analysis_memory.temporary.chars().count() == 4_000);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod editable_memory_concurrency_tests {
    use super::*;
    #[test]
    fn manual_save_blocks_an_older_ai_result_even_when_body_is_unchanged() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-memory-race-{}", id()))).unwrap();
        let stale = json!({"before":"","text":"旧 AI 结果","expected":0});
        app.memory_save(&json!({"kind":"temporary","expected":0,"text":""}))
            .unwrap();
        assert!(app.save_temporary_memory(&stale).is_err());
        assert_eq!(app.memory_read()["temporary"]["text"], "");
    }
}

#[cfg(test)]
mod response_finalization_tests {
    use super::*;
    #[test]
    fn received_response_with_invalid_reference_has_a_correlated_terminal_failure() {
        let root = std::env::temp_dir().join(format!("shishi-finalization-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        let mut state = app.state.lock().unwrap().clone();
        state.messages.insert(
            "message-real".into(),
            Message {
                id: "message-real".into(),
                text: "Synthetic evidence".into(),
                time: now(),
                ..Default::default()
            },
        );
        state.tasks.push(Task {
            id: "task-real".into(),
            title: "Synthetic task".into(),
            status: "pending".into(),
            ..Default::default()
        });
        let mut context = app
            .capture_prepare_context_from(&state, "classify", false, json!([]))
            .unwrap();
        let request = app.prepare_ai_request_with("classify", &json!({"changes":[{"id":"message-real","text":"Synthetic evidence"}],"tasks":[{"id":"task-real"}]}), AiMode::Manual, &mut context).unwrap();
        let request_id = "synthetic-finalization";
        app.log(
            "ai_start",
            json!({"request_id":request_id,"kind":"classify"}),
        );
        let result = json!({"actions":[{"action":"update","task_id":"t999","sources":["m1"]}]});
        let detail = json!({"request_id":request_id,"kind":"classify","response_received":true});
        let error = app
            .finalize_ai_response(&request, request_id, detail, Ok(result))
            .unwrap_err();
        assert!(error.contains("未知证据短编号"), "{error}");
        let rows = app.activity.lock().unwrap();
        let failed = rows.iter().find(|r| r["kind"] == "ai_failed").unwrap();
        assert_eq!(failed["detail"]["response_received"], true);
        assert_eq!(failed["detail"]["failure_stage"], "response_validation");
        assert!(rows.iter().any(|r| r["kind"] == "ai_start"
            && r["detail"]["request_id"] == failed["detail"]["request_id"]));
        assert!(!rows.iter().any(|r| r["kind"] == "ai_success"));
        drop(rows);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
