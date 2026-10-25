use super::*;
use std::fs;

#[test]
fn text_previews_are_live_without_saves_and_terminal_commit_keeps_all_text() {
    let root = std::env::temp_dir().join(format!("shishi-preview-{}", id()));
    let app = App::load(root.clone()).unwrap();
    app.commit_state(|s| {
        s.handling.runs.push(Run {
            id: "run".into(),
            task_id: "task".into(),
            status: "inProgress".into(),
            updated: 10,
            rounds: vec![Round {
                id: "round".into(),
                status: "inProgress".into(),
                steps: vec![json!({"id":"step","type":"agentMessage","text":""})],
                ..Default::default()
            }],
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
    assert_eq!(
        app.state.lock().unwrap().handling.runs[0].rounds[0].steps[0]["text"],
        ""
    );
    let live = app.snapshot_page(Some("tasks"));
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
    drop(app);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn approval_and_usage_are_durable_and_stale_previews_cannot_revive_stopped_runs() {
    let root = std::env::temp_dir().join(format!("shishi-preview-state-{}", id()));
    let app = App::load(root.clone()).unwrap();
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
    assert!(app.live_handling(&state, None).is_empty());
    assert_eq!(app.preview_run(&state.handling.runs[0]).status, "ended");
    drop(state);
    drop(app);
    fs::remove_dir_all(root).unwrap();
}
