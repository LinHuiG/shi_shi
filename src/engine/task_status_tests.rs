use super::*;

#[test]
fn task_query_focus_matches_snapshot_and_preserves_raw_status_filter() {
    let (_root, app) = crate::test_support::scoped_app("shishi-focus");
    let at = now();
    {
        let mut state = app.state.lock().unwrap();
        for (name, status, blocker, waiting, until, archived, merged) in [
            ("pending", "待处理", "", "", 0, false, ""),
            ("processing", "处理中", "", "", 0, false, ""),
            ("waiting", "待处理", "", "Reply", 0, false, ""),
            ("blocked", "处理中", "Blocked", "", 0, false, ""),
            ("both", "待处理", "Blocked", "Reply", 0, false, ""),
            (
                "deferred",
                "待处理",
                "Blocked",
                "Reply",
                at + 60000,
                false,
                "",
            ),
            ("expired", "处理中", "", "", at - 1, false, ""),
            ("blank", "待处理", " \t", " \n", 0, false, ""),
            ("done", "已完成", "Blocked", "Reply", at + 60000, false, ""),
            ("cancelled", "已取消", "", "", 0, false, ""),
            ("dismissed", "无需处理", "", "", 0, false, ""),
            ("archived", "待处理", "", "", 0, true, ""),
            ("merged", "待处理", "", "", 0, false, "pending"),
        ] {
            state.tasks.push(Task {
                id: name.into(),
                title: name.into(),
                status: status.into(),
                blocker: blocker.into(),
                waiting_for: waiting.into(),
                deferred_until: until,
                archived,
                merged_into: merged.into(),
                ..Default::default()
            });
        }
    }
    let snapshot = app.snapshot();
    let mut all = std::collections::BTreeSet::new();
    for (focus, count) in [("待处理", 2), ("处理中", 2), ("被阻塞", 3), ("延后处理", 1)]
    {
        let result = crate::command_api::tasks_query(&app, &json!({"focus":focus})).unwrap();
        assert_eq!(result["total"], count);
        for row in result["items"].as_array().unwrap() {
            assert_eq!(row["focus"], focus);
            assert!(all.insert(row["id"].as_str().unwrap().to_owned()));
            let ui = snapshot["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == row["id"])
                .unwrap();
            assert_eq!(ui["deferred"], focus == "延后处理");
        }
    }
    assert_eq!(all.len(), 8);
    let raw = crate::command_api::tasks_query(&app, &json!({"status":"待处理"})).unwrap();
    assert_eq!(raw["total"], 7);
    let page =
        crate::command_api::tasks_query(&app, &json!({"focus":"被阻塞","limit":1,"offset":1}))
            .unwrap();
    assert_eq!(page["total"], 3);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    for invalid in [json!("unknown"), json!(null), json!(3)] {
        assert!(crate::command_api::tasks_query(&app, &json!({"focus":invalid})).is_err());
    }
}

#[test]
fn task_query_searches_display_numbers_and_text_without_changing_scope_or_pagination() {
    let (_root, app) = crate::test_support::scoped_app("shishi-number-search");
    {
        let mut state = app.state.lock().unwrap();
        for (name, number, description, updated, until, archived) in [
            (
                "visible-number",
                141,
                "Neutral context",
                10,
                now() + 3_600_000,
                false,
            ),
            ("number-in-text", 179, "Reference 141055", 30, 0, false),
            ("hash-in-text", 180, "Related #141 entry", 20, 0, false),
            ("other", 200, "Neutral context", 0, 0, false),
            ("archived-number", 1410, "Neutral context", 40, 0, true),
        ] {
            state.tasks.push(Task {
                id: name.into(),
                title: "Synthetic task".into(),
                description: description.into(),
                status: "待处理".into(),
                updated,
                deferred_until: until,
                archived,
                ..Default::default()
            });
            state.task_numbers.insert(name.into(), number);
        }
    }
    let ids = |params: Value| {
        crate::command_api::tasks_query(&app, &params).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(json!({"query":"141","archived":false})),
        ["number-in-text", "hash-in-text", "visible-number"]
    );
    for query in ["#141", "#14"] {
        assert_eq!(
            ids(json!({"query":query,"archived":false})),
            ["hash-in-text", "visible-number"]
        );
    }
    for query in ["141055", "number-in-text"] {
        assert_eq!(
            ids(json!({"query":query,"archived":false})),
            ["number-in-text"]
        );
    }
    assert_eq!(
        ids(json!({"query":"141","focus":"延后处理"})),
        ["visible-number"]
    );
    assert_eq!(
        ids(json!({"query":"141","focus":"待处理"})),
        ["number-in-text", "hash-in-text"]
    );
    assert!(ids(json!({"query":"141","status":"已完成"})).is_empty());
    assert!(ids(json!({"query":"no-match"})).is_empty());
    assert_eq!(
        ids(json!({"query":"#14","archived":true})),
        ["archived-number"]
    );
    let page = crate::command_api::tasks_query(
        &app,
        &json!({"query":"141","archived":false,"offset":1,"limit":1}),
    )
    .unwrap();
    assert_eq!(page["total"], 3);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], "hash-in-text");
    assert_eq!(page["items"][0]["number"], 180);
    let snapshot = app.snapshot();
    let ui_number = snapshot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "visible-number")
        .unwrap()["number"]
        .clone();
    let numbered =
        crate::command_api::tasks_query(&app, &json!({"query":"#141","focus":"延后处理"})).unwrap();
    assert_eq!(numbered["items"][0]["number"], ui_number);
    assert_eq!(ui_number, 141);
}

#[test]
fn task_query_orders_priority_before_latest_update() {
    let (_root, app) = crate::test_support::scoped_app("shishi-sort");
    {
        let mut state = app.state.lock().unwrap();
        state.tag_state.versions.push(crate::tags::Version {
            priority_kind: "Level".into(),
            groups: vec![crate::tags::Group {
                name: "Level".into(),
                values: vec!["High".into(), "Low".into()],
                ..Default::default()
            }],
            ..Default::default()
        });
        for (id, level, updated) in [
            ("low", "Low", 30),
            ("older", "High", 10),
            ("newer", "High", 20),
            ("none", "", 99),
        ] {
            state.tasks.push(Task {
                id: id.into(),
                title: id.into(),
                status: "待处理".into(),
                updated,
                tags: if level.is_empty() {
                    vec![]
                } else {
                    vec![crate::tags::Tag {
                        kind: "Level".into(),
                        value: level.into(),
                    }]
                },
                ..Default::default()
            });
        }
    }
    let rows = crate::command_api::tasks_query(&app, &json!({})).unwrap();
    let ids = rows["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["newer", "older", "low", "none"]);

    let at = now();
    {
        let mut state = app.state.lock().unwrap();
        for (id, level, updated, until) in [
            ("far", "High", 40, at + 2 * 86_400_000),
            ("early", "Low", 25, at + 3_600_000),
            ("tie-old", "High", 15, at + 3_600_000),
            ("tie-new", "High", 19, at + 3_600_000),
        ] {
            state.tasks.push(Task {
                id: id.into(),
                title: id.into(),
                status: "待处理".into(),
                updated,
                deferred_until: until,
                tags: vec![crate::tags::Tag {
                    kind: "Level".into(),
                    value: level.into(),
                }],
                ..Default::default()
            });
        }
    }
    let ids = |params: Value| {
        crate::command_api::tasks_query(&app, &params).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(json!({"focus":"延后处理"})),
        ["tie-new", "tie-old", "early", "far"]
    );
    assert_eq!(
        ids(json!({})),
        ["tie-new", "newer", "tie-old", "early", "older", "low", "far", "none"]
    ); // Non-deferred rows keep their original slots in the mixed ordering.
    assert_eq!(
        ids(json!({"focus":"待处理"})),
        ["newer", "older", "low", "none"]
    );
    assert_eq!(
        ids(json!({"focus":"延后处理","limit":2,"offset":1})),
        ["tie-old", "early"]
    );
    assert_eq!(
        ids(json!({"limit":3,"offset":2})),
        ["tie-old", "early", "older"]
    );
    {
        let mut state = app.state.lock().unwrap();
        state
            .tasks
            .iter_mut()
            .find(|t| t.id == "far")
            .unwrap()
            .deferred_until = at + 1_800_000;
    }
    assert_eq!(
        ids(json!({"focus":"延后处理"})),
        ["far", "tie-new", "tie-old", "early"]
    );
    {
        let mut state = app.state.lock().unwrap();
        state
            .tasks
            .iter_mut()
            .find(|t| t.id == "far")
            .unwrap()
            .deferred_until = at - 1;
        state
            .tasks
            .iter_mut()
            .find(|t| t.id == "early")
            .unwrap()
            .deferred_until = 0;
    }
    assert_eq!(
        ids(json!({"focus":"待处理"})),
        ["far", "newer", "older", "low", "early", "none"]
    );
    assert_eq!(ids(json!({"focus":"延后处理"})), ["tie-new", "tie-old"]);
}

#[test]
fn legacy_deferral_requires_clear_history_to_resume() {
    let (root, app) = crate::test_support::scoped_app("shishi-legacy-resume");
    {
        let mut state = app.state.lock().unwrap();
        for (id, before) in [("active", "处理中"), ("pending", "待处理")] {
            let task = Task {
                id: id.into(),
                title: id.into(),
                status: "待处理".into(),
                deferred_until: now() - 1,
                ..Default::default()
            };
            let mut after = json!(task);
            after.as_object_mut().unwrap().remove("resume_processing");
            state.audit.push(Audit {
                time: now() - 2,
                kind: "manual_status".into(),
                task_id: id.into(),
                detail: "延后处理".into(),
                before: json!({"status":before}),
                after,
                ..Default::default()
            });
            state.tasks.push(task);
        }
    }
    app.persist().unwrap();
    drop(app);
    let app = App::load(root.clone()).unwrap();
    assert_eq!(app.state.lock().unwrap().tasks[0].status, "处理中");
    assert_eq!(app.state.lock().unwrap().tasks[1].status, "待处理");
}

#[test]
fn deferred_processing_resumes_transactionally_and_after_restart() {
    let (root, app) = crate::test_support::scoped_app("shishi-resume");
    for (name, status) in [
        ("active", "处理中"),
        ("pending", "待处理"),
        ("done", "处理中"),
    ] {
        let task = app
            .task_edit(&json!({"task":{"title":name,"status":status}}))
            .unwrap();
        app.task_action(&json!({"id":task["id"],"action":"defer","choice":"hour"}))
            .unwrap();
        app.task_action(&json!({"id":task["id"],"action":"defer","choice":"tomorrow"}))
            .unwrap();
        if name == "done" {
            app.task_action(&json!({"id":task["id"],"action":"done"}))
                .unwrap();
        }
    }
    let at = app.state.lock().unwrap().tasks[0].deferred_until;
    app.resume_deferred(at - 1).unwrap();
    assert_eq!(app.state.lock().unwrap().tasks[0].status, "待处理");
    assert_eq!(
        app.state.lock().unwrap().tasks[0].focus(at - 1),
        Some("延后处理")
    );
    crate::test_support::persistence_failure(&root, true).unwrap();
    assert!(app.resume_deferred(at).is_err());
    assert_eq!(app.state.lock().unwrap().tasks[0].status, "待处理");
    crate::test_support::persistence_failure(&root, false).unwrap();
    app.resume_deferred(at).unwrap();
    let state = app.state.lock().unwrap();
    assert_eq!(state.tasks[0].status, "处理中");
    assert_eq!(state.tasks[0].deferred_until, 0);
    assert_eq!(state.tasks[0].focus(at), Some("处理中"));
    assert_eq!(state.tasks[1].status, "待处理");
    assert_eq!(state.tasks[1].focus(at), Some("待处理"));
    assert_eq!(state.tasks[2].status, "已完成");
    assert_eq!(state.tasks[2].focus(at), None);
    assert!(state.handling.runs.is_empty());
    let audit_count = state.audit.len();
    drop(state);
    app.resume_deferred(at + 1).unwrap();
    assert_eq!(app.state.lock().unwrap().audit.len(), audit_count);
    {
        let mut state = app.state.lock().unwrap();
        state.tasks[0].status = "待处理".into();
        state.tasks[0].resume_processing = true;
        state.tasks[0].deferred_until = now() - 1;
    }
    app.persist().unwrap();
    drop(app);
    let restored = App::load(root.clone()).unwrap();
    assert_eq!(restored.state.lock().unwrap().tasks[0].status, "处理中");
    assert_eq!(
        restored.state.lock().unwrap().tasks[0].focus(now()),
        Some("处理中")
    );
    drop(restored);
}

#[test]
fn start_defer_persist_without_handling_and_reject_stale_actions() {
    let (root, app) = crate::test_support::scoped_app("shishi-status");
    let task = app.task_edit(&json!({"task":{"title":"Synthetic","status":"待处理","due":"2030-01-02T12:00:00+08:00"}})).unwrap();
    let tid = &task["id"];
    app.task_action(&json!({"id":tid,"action":"start","updated":task["updated"]}))
        .unwrap();
    assert!(app
        .task_action(&json!({"id":tid,"action":"defer","updated":task["updated"]}))
        .is_err());
    for choice in ["hour", "tomorrow", "next_week"] {
        let at = now();
        app.task_action(&json!({"id":tid,"action":"defer","choice":choice}))
            .unwrap();
        let state = app.state.lock().unwrap();
        let current = &state.tasks[0];
        assert_eq!(current.status, "待处理");
        assert!(
            (schedule::defer_until(choice, &state.config.work_start, at).unwrap()
                ..=schedule::defer_until(choice, &state.config.work_start, now()).unwrap())
                .contains(&current.deferred_until)
        );
        assert_eq!(current.due, task["due"]);
        assert!(state.handling.runs.is_empty());
    }
    for choice in [json!(0), json!("bad"), json!(-1), json!(1.5), json!("24")] {
        assert!(app
            .task_action(&json!({"id":tid,"action":"defer","choice":choice}))
            .is_err());
    }
    let saved = crate::state_store::load(&crate::storage::data_dir(&root)).unwrap();
    assert!(saved.tasks[0].deferred_until > now());
    app.task_action(&json!({"id":tid,"action":"start"}))
        .unwrap();
    let state = app.state.lock().unwrap();
    assert_eq!(state.tasks[0].status, "处理中");
    assert_eq!(state.tasks[0].deferred_until, 0);
    assert!(state.handling.runs.is_empty());
    drop(state);
    app.task_action(&json!({"id":tid,"action":"done"})).unwrap();
    assert!(app
        .task_action(&json!({"id":tid,"action":"defer"}))
        .is_err());
}

#[test]
fn urgent_evidence_is_required_to_override_deferral() {
    let mut original = State::default();
    original.tasks.push(Task {
        id: "task".into(),
        title: "Synthetic".into(),
        status: "待处理".into(),
        updated: 10,
        created: 1,
        manual: true,
        deferred_until: now() + 86_400_000,
        ..Default::default()
    });
    original.messages.insert(
        "new".into(),
        Message {
            private: true,
            ..crate::test_support::message("new", "Synthetic urgent evidence", 20)
        },
    );
    let base = json!({"action":"update","task_id":"task","sources":["new"],"tag_changes":[],
        "fields":[{"field":"status","value":"处理中"}],"confidence":0.99,"reason":"New evidence"});
    for mode in ["absent", "empty", "no-proof", "old-proof", "urgent"] {
        let mut state = original.clone();
        let mut action = base.clone();
        if mode != "absent" {
            action["fields"]
                .as_array_mut()
                .unwrap()
                .push(json!({"field":"deferred_until","value":if mode=="empty" {""} else {"0"}}));
        }
        if mode == "urgent" || mode == "old-proof" {
            action["deferral_reason"] = json!("Explicit urgent change");
            action["deferral_evidence"] = json!(["new"]);
        }
        if mode == "old-proof" {
            state.tasks[0].created = 30;
        }
        let allowed = state.messages.clone();
        App::apply_actions(
            &mut state,
            &[action],
            &allowed,
            &BTreeMap::from([("task".into(), 10)]),
            &[],
        )
        .unwrap();
        if mode == "urgent" {
            assert_eq!(state.tasks[0].deferred_until, 0);
            assert_eq!(state.tasks[0].status, "处理中");
            assert_eq!(
                state.audit.last().unwrap().before["deferred_until"],
                original.tasks[0].deferred_until
            );
            assert!(state.tasks[0].reason.contains("Explicit urgent change"));
        } else {
            assert_eq!(
                state.tasks[0].deferred_until, original.tasks[0].deferred_until,
                "{mode}"
            );
            assert_eq!(state.tasks[0].status, "待处理", "{mode}");
        }
    }
}

#[test]
fn processing_status_survives_normalization_and_context() {
    let mut task = Task {
        status: "处理中".into(),
        title: "Synthetic".into(),
        blocker: "Blocked".into(),
        waiting_for: "Reply".into(),
        ..Default::default()
    };
    normalize_task(&mut task);
    assert_eq!(task.status, "处理中");
    assert_eq!(crate::context_budget::task(&task)["status"], "处理中");
    assert!(validate_edit(&task).is_ok());
    assert!(is_today_task(&task, &[], now()));
}
