use super::*;

#[test]
fn text_previews_are_live_without_saves_and_terminal_commit_keeps_all_text() {
    let (root, app) = crate::test_support::scoped_app("shishi-preview");
    app.commit_state(|s| {
        s.handling.runs.push(Run {
            id: "run".into(),
            task_id: "task".into(),
            status: "inProgress".into(),
            updated: 10,
            rounds: vec![Round {
                id: "round".into(),
                status: "inProgress".into(),
                steps: vec![json!({"id":"step","type":"agentMessage","text":""})].into(),
                ..Default::default()
            }
            .into()]
            .into(),
            ..Default::default()
        });
        Ok(())
    })
    .unwrap();
    let saves = app.saves_revision();
    for text in ["First", "First and second"] {
        app.update_run_inner("run", true, |r| {
            r.progress = "Streaming".into();
            r.rounds[0].steps[0]["text"] = json!(text);
        })
        .unwrap();
    }
    assert_eq!(app.saves_revision(), saves);
    let revision = app
        .handling_preview_revision
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(!app.update_run_inner("run", true, |_| {}).unwrap());
    assert_eq!(
        app.handling_preview_revision
            .load(std::sync::atomic::Ordering::SeqCst),
        revision
    );
    // A durable call must still flush an existing preview even if its closure
    // changes nothing; only a fully unchanged durable run is a no-op.
    assert!(app.update_run_inner("run", false, |_| {}).unwrap());
    assert!(!app.update_run_inner("run", false, |_| {}).unwrap());
    let saves = app.saves_revision();
    assert_eq!(
        app.state.lock().unwrap().handling.runs[0].rounds[0].steps[0]["text"],
        "First and second"
    );
    let live = app.snapshot_cached(Some("tasks"), None, Some("task"));
    assert_eq!(
        live["handling"][0]["rounds"][0]["steps"][0]["text"],
        "First and second"
    );
    app.update_run_inner("run", true, |r| {
        r.status = "completed".into();
        r.rounds[0].status = "completed".into();
    })
    .unwrap();
    assert_eq!(app.saves_revision(), saves + 1);
    assert!(app.handling_previews.lock().unwrap().is_empty());
    let durable = serde_json::to_value(crate::managed_data::load_state(&root).unwrap()).unwrap();
    assert_eq!(
        durable["handling"]["runs"][0]["rounds"][0]["steps"][0]["text"],
        "First and second"
    );
    assert_eq!(durable["handling"]["runs"][0]["status"], "completed");
}

#[test]
fn long_history_stays_shared_and_runtime_sends_only_changed_rounds() {
    let (_root, app) = crate::test_support::scoped_app("shishi-preview-history");
    let history = (0..30).map(|i| Round {
        id: format!("round-{i}"), turn_id: format!("turn-{i}"),
        status: if i == 29 { "inProgress" } else { "completed" }.into(),
        steps: (0..100).map(|j| json!({"id":j,"kind":"agentMessage","text":format!("historical-{i}-{j}-{}", "x".repeat(1000))})).collect(),
        ..Default::default()
    }.into()).collect();
    app.commit_state(|state| {
        state.handling.runs.push(Run {
            id: "run".into(),
            task_id: "task".into(),
            updated: 10,
            status: "inProgress".into(),
            rounds: history,
            ..Default::default()
        });
        Ok(())
    })
    .unwrap();
    let frozen_runs = app.state.lock().unwrap().handling.runs.clone();
    let durable = &frozen_runs[0];
    let full = app.snapshot_cached(Some("tasks"), None, Some("task"));
    let saves = app.saves_revision();
    for text in ["first delta", "latest delta"] {
        app.update_run_inner("run", true, |run| {
            run.rounds[29].steps[99]["text"] = json!(text);
        })
        .unwrap();
    }
    let preview = app.preview_run(durable);
    assert!(!preview.rounds.same_snapshot(&durable.rounds));
    for i in 0..29 {
        assert!(preview.rounds[i].same_snapshot(&durable.rounds[i]));
        assert!(preview.rounds[i]
            .steps
            .same_snapshot(&durable.rounds[i].steps));
    }
    assert!(durable.rounds[29].steps[99]["text"]
        .as_str()
        .unwrap()
        .starts_with("historical-29-99-"));
    assert_eq!(app.saves_revision(), saves);
    let request = json!({"page":"tasks","task":"task","core_revision":full["core_revision"],"handling_revision":full["handling_revision"]});
    let delta = app.snapshot_request(&request);
    assert_eq!(delta["_unchanged"], true);
    let changes = delta["handling_live"][0]["_round_changes"]
        .as_array()
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0][0], 29);
    assert_eq!(changes[0][1]["steps"][99]["text"], "latest delta");
    assert!(!delta.to_string().contains("historical-0-0"));
    let mut unchanged = request;
    unchanged["handling_revision"] = delta["handling_revision"].clone();
    assert!(app.snapshot_request(&unchanged)["handling_live"]
        .as_array()
        .unwrap()
        .is_empty());
    // Tool lifecycle and final conclusions still flush the entire preview.
    app.update_run_inner("run", true, |run| {
        run.rounds[29].steps[99]["status"] = json!("completed");
        run.rounds[29].conclusion = "complete conclusion".into();
    })
    .unwrap();
    assert_eq!(app.saves_revision(), saves + 1);
    assert_eq!(
        app.state.lock().unwrap().handling.runs[0].rounds[29].steps[99]["text"],
        "latest delta"
    );
    assert!(durable.rounds[29].conclusion.is_empty());
}

#[test]
fn preview_delta_includes_text_restored_to_the_durable_version() {
    let (_root, app) = crate::test_support::scoped_app("shishi-preview-revert");
    app.commit_state(|state| {
        state.handling.runs.push(Run {
            id: "run".into(),
            task_id: "task".into(),
            updated: 10,
            rounds: vec![Round {
                steps: vec![json!({"id":"step","text":"durable"})].into(),
                ..Default::default()
            }
            .into()]
            .into(),
            ..Default::default()
        });
        Ok(())
    })
    .unwrap();
    let full = app.snapshot_cached(Some("tasks"), None, Some("task"));
    let mut request = json!({"page":"tasks","task":"task","core_revision":full["core_revision"],"handling_revision":full["handling_revision"]});
    for text in ["preview", "durable"] {
        app.update_run_inner("run", true, |run| {
            run.rounds[0].steps[0]["text"] = json!(text);
        })
        .unwrap();
        let live = app.snapshot_request(&request);
        assert_eq!(
            live["handling_live"][0]["_round_changes"][0][1]["steps"][0]["text"],
            text
        );
        request["handling_revision"] = live["handling_revision"].clone();
    }
}

#[test]
fn approval_and_usage_are_durable_and_stale_previews_cannot_revive_stopped_runs() {
    let (_root, app) = crate::test_support::scoped_app("shishi-preview-state");
    app.commit_state(|s| {
        s.handling.runs.push(Run {
            id: "run".into(),
            task_id: "task".into(),
            updated: 10,
            ..Default::default()
        });
        Ok(())
    })
    .unwrap();
    let saves = app.saves_revision();
    app.update_run_inner("run", true, |r| r.progress = "Transient".into())
        .unwrap();
    app.update_run_inner("run", true, |r| r.requests = vec![json!({"id":"approval"})])
        .unwrap();
    assert_eq!(app.saves_revision(), saves + 1);
    assert_eq!(
        app.state.lock().unwrap().handling.runs[0].progress,
        "Transient"
    );
    app.update_run_inner("run", true, |r| {
        r.usage = json!({"total":{"inputTokens":12,"outputTokens":3}})
    })
    .unwrap();
    assert_eq!(app.saves_revision(), saves + 2);
    app.update_run_inner("run", true, |r| r.progress = "Later preview".into())
        .unwrap();
    app.commit_state(|s| {
        let run = &mut s.handling.runs[0];
        run.stop_requested = true;
        run.status = "ended".into();
        run.updated += 1;
        Ok(())
    })
    .unwrap();
    let state = app.state.lock().unwrap();
    assert!(app.live_handling(&state, None, None).is_empty());
    assert_eq!(app.preview_run(&state.handling.runs[0]).status, "ended");
    drop(state);
}
