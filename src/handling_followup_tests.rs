//! Deterministic synchronization collisions; all sessions are offline fixtures.
use super::*;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

fn question() -> Value {
    json!({"task_id":"task","run_id":"run","thread_id":"old-thread","text":"Check the synthetic evidence"})
}
fn pending(app: &App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.handling_followups.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "follow-up never entered the queue"
        );
        std::thread::yield_now();
    }
}
fn active_history() -> Value {
    json!({"data":[{"id":"old-turn","status":"inProgress","items":[]}]})
}

#[test]
fn deleting_the_configured_source_keeps_the_existing_dsh_session_usable() {
    let (app, agent, root) = fixture("dsh");
    app.state.lock().unwrap().handling.runs[0].model = "synthetic-original".into();
    app.delete_connection(&json!({"id":"synthetic-dsh"}))
        .unwrap();
    assert_eq!(
        app.state
            .lock()
            .unwrap()
            .config
            .for_kind("handling")
            .selected_connection,
        crate::model::UNCONFIGURED_CONNECTION
    );
    app.handling_followup(&question()).unwrap();
    assert_eq!(agent.submitted.load(Ordering::SeqCst), 1);
    assert!(app.run("task").unwrap().connection.is_some());
    assert!(agent.is_alive());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_single_followup_survives_same_or_other_run_sync_and_sync_failure() {
    for driver in ["dsh"] {
        for other in [false, true] {
            for fail_read in [false, true] {
                let (app, agent, root) = fixture(driver);
                *agent.history.lock().unwrap() = active_history();
                agent.fail_read.store(fail_read, Ordering::SeqCst);
                if other {
                    let mut state = app.state.lock().unwrap();
                    let mut run = state.handling.runs[0].clone();
                    state.handling.runs[0].watching = false;
                    run.id = "other-run".into();
                    run.task_id = "other-task".into();
                    run.thread_id = "other-thread".into();
                    state.handling.runs.push(run);
                    drop(state);
                    app.handling_client
                        .lock()
                        .unwrap()
                        .insert("run:other-run".into(), agent.clone());
                }
                let (entered_tx, entered_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                *agent.read_gate.lock().unwrap() = Some((entered_tx, release_rx));
                let sync_app = app.clone();
                let sync = std::thread::spawn(move || sync_app.handling_sync(None));
                entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                let worker_app = app.clone();
                let (done_tx, done_rx) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    done_tx
                        .send(worker_app.handling_followup(&question()))
                        .unwrap()
                });
                pending(&app);
                assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                assert_eq!(agent.steered.load(Ordering::SeqCst), 0);
                assert_eq!(agent.submitted.load(Ordering::SeqCst), 0);
                assert!(app
                    .handling_followup(&question())
                    .unwrap_err()
                    .contains("请勿重复提交"));
                // Reconciliation yields to queued manual input, without draining events.
                assert_eq!(app.handling_sync(None).unwrap(), json!(false));
                assert!(!app.handling_stream().unwrap());
                release_tx.send(()).unwrap();
                sync.join().unwrap().unwrap();
                assert_eq!(
                    done_rx
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap()["steered"],
                    true
                );
                worker.join().unwrap();
                assert_eq!(agent.steered.load(Ordering::SeqCst), 1);
                assert_eq!(agent.submitted.load(Ordering::SeqCst), 0);
                assert_eq!(app.run("task").unwrap().rounds[0].supplements.len(), 1);
                assert!(app.handling_followups.lock().unwrap().is_empty());
                drop(app);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }
}

#[test]
fn a_turn_finishing_during_the_wait_starts_one_followup_turn() {
    for driver in ["dsh"] {
        let (app, agent, root) = fixture(driver);
        let guard = app.handling_action.lock().unwrap();
        let worker_app = app.clone();
        let worker = std::thread::spawn(move || worker_app.handling_followup(&question()));
        pending(&app);
        *agent.history.lock().unwrap() =
            json!({"data":[{"id":"old-turn","status":"completed","items":[]}]});
        drop(guard);
        assert_eq!(worker.join().unwrap().unwrap()["steered"], false);
        assert_eq!(agent.steered.load(Ordering::SeqCst), 0);
        assert_eq!(agent.submitted.load(Ordering::SeqCst), 1);
        let run = app.run("task").unwrap();
        assert_eq!(run.rounds.len(), 2);
        assert_eq!(run.rounds[1].question, question()["text"]);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn queued_input_cannot_outlive_termination_handoff_or_a_changed_target() {
    for change in ["terminate", "handoff", "replace", "thread", "source"] {
        let (app, agent, root) = fixture("dsh");
        let guard = app.handling_action.lock().unwrap();
        let worker_app = app.clone();
        let worker = std::thread::spawn(move || worker_app.handling_followup(&question()));
        pending(&app);
        if change == "terminate" {
            app.handling_terminate(&question()).unwrap();
        } else {
            let mut state = app.state.lock().unwrap();
            match change {
                "handoff" => state.handling.runs[0].desktop_control = true,
                "replace" => {
                    let mut next = state.handling.runs[0].clone();
                    next.id = "fresh-run".into();
                    next.thread_id = "fresh-thread".into();
                    state.handling.runs.push(next);
                }
                "thread" => state.handling.runs[0].thread_id = "rebound-thread".into(),
                "source" => {
                    state.config.ai_connections.push(Connection {
                        id: "other-source".into(),
                        executable: "synthetic".into(),
                        ..Default::default()
                    });
                    state.config.selected_connection = "other-source".into();
                }
                _ => unreachable!(),
            }
        }
        // Cancellation is noticed even while the original synchronization still holds its lock.
        assert!(worker.join().unwrap().is_err());
        assert!(app.handling_followups.lock().unwrap().is_empty());
        assert_eq!(agent.steered.load(Ordering::SeqCst), 0);
        assert_eq!(agent.submitted.load(Ordering::SeqCst), 0);
        drop(guard);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn wait_timeout_and_stale_ui_identity_never_send_or_leave_a_reservation() {
    let (app, agent, root) = fixture("dsh");
    let guard = app.handling_action.lock().unwrap();
    assert!(app
        .handling_followup_wait(&question(), Duration::from_millis(25))
        .unwrap_err()
        .contains("尚未发送"));
    assert!(app.handling_followups.lock().unwrap().is_empty());
    drop(guard);
    for field in ["run_id", "thread_id"] {
        let mut stale = question();
        stale[field] = json!("stale");
        assert!(app
            .handling_followup(&stale)
            .unwrap_err()
            .contains("会话已变化"));
    }
    assert_eq!(agent.observed.load(Ordering::SeqCst), 0);
    assert_eq!(agent.steered.load(Ordering::SeqCst), 0);
    assert_eq!(agent.submitted.load(Ordering::SeqCst), 0);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn uncertain_steer_or_submit_is_never_retried() {
    for driver in ["dsh"] {
        for active in [true, false] {
            let (app, agent, root) = fixture(driver);
            if active {
                *agent.history.lock().unwrap() = active_history();
            }
            agent.fail_send.store(true, Ordering::SeqCst);
            assert!(app
                .handling_followup(&question())
                .unwrap_err()
                .contains("synthetic uncertain"));
            assert_eq!(agent.steered.load(Ordering::SeqCst), usize::from(active));
            assert_eq!(agent.submitted.load(Ordering::SeqCst), usize::from(!active));
            assert!(app.handling_followups.lock().unwrap().is_empty());
            let run = app.run("task").unwrap();
            if active {
                assert!(!run.rounds[0].supplements[0].accepted);
            } else {
                assert_eq!(run.status, "uncertain");
            }
            drop(app);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
