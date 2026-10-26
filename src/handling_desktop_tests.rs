use super::*;
use crate::desktop_bridge::DesktopApi;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Mutex,
};

struct Bridge {
    created: AtomicUsize,
    sent: AtomicUsize,
    snapshot: Mutex<Value>,
    calls: Mutex<Vec<Value>>,
    fail_create: Mutex<Option<String>>,
    fail_send: Mutex<Option<String>>,
    fail_read: Mutex<Option<String>>,
    gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    send_gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}
impl DesktopApi for Bridge {
    fn projects(&self) -> Result<Vec<Value>, String> {
        Ok(vec![])
    }
    fn create(
        &self,
        cwd: &str,
        _: &str,
        prompt: &str,
        model: &str,
        effort: &str,
        _: &ContextTool,
    ) -> Result<Value, String> {
        self.created.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push(json!({"cwd":cwd,"prompt":prompt,"model":model,"effort":effort}));
        if let Some(error) = self.fail_create.lock().unwrap().clone() {
            return Err(error);
        }
        *self.snapshot.lock().unwrap() = json!({"thread":{"id":"00000000-0000-4000-8000-000000000002","cwd":cwd,"status":{"type":"active"}},"turns":[{"id":"turn","status":"inProgress","items":[{"id":"delegation","type":"functionCallOutput","output":{"text":format!("<codex_delegation><input>{prompt}</input></codex_delegation>")}}]}]});
        Ok(json!({"threadId":"00000000-0000-4000-8000-000000000002","hostId":"local"}))
    }
    fn send(
        &self,
        _: &str,
        text: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Value, String> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push(json!({"text":text,"model":model,"effort":effort}));
        let gate = self.send_gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if let Some(error) = self.fail_send.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(json!({"sent":true}))
    }
    fn read(&self, _: &str) -> Result<Value, String> {
        let result = self
            .fail_read
            .lock()
            .unwrap()
            .take()
            .map_or_else(|| Ok(self.snapshot.lock().unwrap().clone()), Err);
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        result
    }
}
fn fixture() -> (Arc<App>, Arc<Bridge>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("shishi-desktop-{}", id()));
    let app = Arc::new(App::load(root.clone()).unwrap());
    {
        let mut s = app.state.lock().unwrap();
        s.config.model_profiles = None;
        s.config.model = "synthetic".into();
        s.config.reasoning_effort = "high".into();
        s.tasks.push(Task {
            id: "task".into(),
            title: "Synthetic investigation".into(),
            status: "待处理".into(),
            ..Default::default()
        });
    }
    let bridge = Arc::new(Bridge {
        created: AtomicUsize::new(0),
        sent: AtomicUsize::new(0),
        snapshot: Mutex::new(json!({"thread":{},"turns":[]})),
        calls: Mutex::new(vec![]),
        fail_create: Mutex::new(None),
        fail_send: Mutex::new(None),
        fail_read: Mutex::new(None),
        gate: Mutex::new(None),
        send_gate: Mutex::new(None),
    });
    *app.desktop_bridge.lock().unwrap() = Some(bridge.clone());
    (app, bridge, root)
}
fn start(app: &App) -> Result<Value, String> {
    let p = app.handling_prepare("task")?["projects"][0].clone();
    app.handling_start(&json!({"task_id":"task","project_id":p["id"],"cwd":p["cwd"]}))
}
#[test]
fn partial_desktop_history_keeps_completed_output_and_clears_notice_on_recovery() {
    let mut run = Run {
        transport: "desktop".into(),
        ..Default::default()
    };
    let full = json!({"thread":{"id":"thread","status":{"type":"idle"}},"turns":[{"id":"turn","status":"completed","items":[
        {"id":"input","type":"userMessage","content":[{"type":"text","text":"Synthetic question"}]},
        {"id":"tool","type":"commandExecution","command":"synthetic check","aggregatedOutput":"Keep full output"},
        {"id":"answer","type":"agentMessage","text":"Complete synthetic conclusion"}
    ]}]});
    apply_desktop_snapshot(&mut run, &full);
    let steps = run.rounds[0].steps.clone();
    let partial = json!({"thread":{"id":"thread","status":{"type":"idle"}},"read_notice":"Synthetic limited read","turns":[{"id":"turn","status":"completed","_shishi_partial":true,"items":[
        {"id":"tool","type":"commandExecution","command":"synthetic check"},
        {"id":"answer","type":"agentMessage","text":"Complete"}
    ]}]});
    apply_desktop_snapshot(&mut run, &partial);
    assert_eq!(run.rounds[0].steps, steps);
    assert_eq!(run.rounds[0].question, "Synthetic question");
    assert_eq!(run.rounds[0].conclusion, "Complete synthetic conclusion");
    assert_eq!(run.sync_notice, "Synthetic limited read");
    assert!(run.error.is_empty());
    apply_desktop_snapshot(&mut run, &full);
    assert!(run.sync_notice.is_empty());
}
#[test]
fn missing_metadata_preserves_verified_model_without_claiming_current_model() {
    let mut run = Run {
        model: "configured-model".into(),
        ..Default::default()
    };
    let empty = json!({"thread":{"id":"thread"},"turns":[]});
    apply_desktop_snapshot(&mut run, &empty);
    assert_eq!(run.model_reported, Some(false));
    assert!(run.last_reported_model.is_empty());
    apply_desktop_snapshot(
        &mut run,
        &json!({"thread":{"id":"thread","model":"verified-model","reasoningEffort":"high"},"turns":[]}),
    );
    apply_desktop_snapshot(&mut run, &empty);
    assert_eq!(run.model_reported, Some(false));
    assert_eq!(run.last_reported_model, "verified-model");
    assert_eq!(run.last_reported_effort, "high");
    let mut restored: Run = serde_json::from_value(json!(run)).unwrap();
    apply_desktop_snapshot(
        &mut restored,
        &json!({"thread":{"id":"thread","model":"new-model","reasoningEffort":"medium"},"turns":[]}),
    );
    assert_eq!(restored.model_reported, Some(true));
    assert_eq!(restored.last_reported_model, "new-model");
    assert_eq!(restored.last_reported_effort, "medium");
}
fn finish(app: &App) {
    let run = app.run("task").unwrap();
    app.handling_terminate(&json!({"task_id":"task","run_id":run.id}))
        .unwrap();
}
fn question(app: &App, text: &str) -> Value {
    let run = app.run("task").unwrap();
    json!({"task_id":"task","run_id":run.id,"thread_id":run.thread_id,"text":text})
}

#[test]
fn deleting_codex_does_not_interrupt_or_override_the_existing_desktop_session() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    app.delete_connection(&json!({"id":""})).unwrap();
    assert_eq!(app.snapshot()["handling"][0]["current_source"], true);
    app.handling_followup(&question(&app, "Synthetic follow-up"))
        .unwrap();
    assert_eq!(bridge.sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        bridge.calls.lock().unwrap().last().unwrap()["model"],
        Value::Null
    );
    assert_eq!(
        app.state
            .lock()
            .unwrap()
            .config
            .for_kind("handling")
            .selected_connection,
        crate::model::UNCONFIGURED_CONNECTION
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn desktop_create_followup_and_completion_never_start_an_execution_client() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    assert_eq!(bridge.created.load(Ordering::SeqCst), 1);
    assert!(app.handling_client.lock().unwrap().is_empty());
    assert_eq!(bridge.calls.lock().unwrap()[0]["model"], "synthetic");
    assert_eq!(bridge.calls.lock().unwrap()[0]["effort"], "high");
    app.handling_sync_desktop(None);
    let run = app.run("task").unwrap();
    assert_eq!(run.rounds.len(), 1);
    assert_eq!(run.rounds[0].turn_id, "turn");
    let lock = app.handling_action.lock().unwrap();
    app.handling_followup(&question(&app, "继续核对")).unwrap();
    drop(lock);
    assert_eq!(bridge.sent.load(Ordering::SeqCst), 1);
    assert_eq!(bridge.calls.lock().unwrap()[1]["model"], Value::Null);
    finish(&app);
    assert_eq!(app.run("task").unwrap().status, "ended");
    assert!(app.handling_client.lock().unwrap().is_empty());
    assert!(app
        .handling_followup(&question(&app, "结束后不发送"))
        .is_err());
    drop(app);
    let loaded = App::load(root.clone()).unwrap();
    assert_eq!(
        loaded.state.lock().unwrap().handling.runs[0].status,
        "ended"
    );
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn both_native_inputs_and_actual_text_updates_are_deduplicated_and_waiting_is_visible() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    {
        let mut snapshot = bridge.snapshot.lock().unwrap();
        snapshot["thread"]["status"] =
            json!({"type":"active","activeFlags":["waitingOnUserInput"]});
        snapshot["turns"][0]["items"].as_array_mut().unwrap().extend([json!({"id":"native","type":"userMessage","content":[{"type":"text","text":"Codex 端插话"}]}),json!({"id":"message","type":"agentMessage","phase":"commentary","text":"第一段"})]);
    }
    app.handling_sync_desktop(None);
    app.handling_sync_desktop(None);
    let run = app.run("task").unwrap();
    assert_eq!(run.waiting_on, "input");
    assert_eq!(run.rounds[0].supplements.len(), 1);
    assert_eq!(run.rounds[0].supplements[0].text, "Codex 端插话");
    assert_eq!(run.progress, "第一段");
    bridge.snapshot.lock().unwrap()["turns"][0]["items"][2]["text"] = json!("第一段第二段");
    app.handling_sync_desktop(None);
    assert_eq!(app.run("task").unwrap().progress, "第一段第二段");
    assert_eq!(
        app.run("task").unwrap().rounds[0]
            .steps
            .iter()
            .filter(|s| s["id"] == "message")
            .count(),
        1
    );
    assert_eq!(app.state.lock().unwrap().tasks[0].status, "待处理");
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn ending_while_a_read_is_in_flight_blocks_late_updates_and_allows_a_new_run() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    let (entered, rx) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    *bridge.gate.lock().unwrap() = Some((entered, gate));
    let worker = app.clone();
    let read = std::thread::spawn(move || worker.handling_sync_desktop(None));
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    finish(&app);
    let old = app.run("task").unwrap();
    release.send(()).unwrap();
    read.join().unwrap();
    assert_eq!(json!(app.run("task").unwrap()), json!(old));
    app.handling_sync(Some("task")).unwrap();
    assert_eq!(json!(app.run("task").unwrap()), json!(old));
    start(&app).unwrap();
    assert_ne!(app.run("task").unwrap().id, old.id);
    assert_eq!(app.state.lock().unwrap().handling.runs.len(), 2);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn uncertain_creation_and_followup_never_auto_retry_and_preserve_the_input() {
    let (app, bridge, root) = fixture();
    *bridge.fail_create.lock().unwrap() = Some("提交结果待核实：断开".into());
    assert!(start(&app).is_err());
    assert_eq!(app.run("task").unwrap().status, "uncertain");
    assert!(!app.run("task").unwrap().rounds[0].question.is_empty());
    assert!(start(&app).is_err());
    assert_eq!(bridge.created.load(Ordering::SeqCst), 1);
    finish(&app);
    *bridge.fail_create.lock().unwrap() = None;
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    *bridge.fail_send.lock().unwrap() = Some("提交结果待核实：断开".into());
    assert!(app.handling_followup(&question(&app, "保留草稿")).is_err());
    assert!(app.handling_followup(&question(&app, "不重发")).is_err());
    assert_eq!(bridge.sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.run("task").unwrap().rounds[0].supplements[0].text,
        "保留草稿"
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn a_desktop_model_change_is_not_overwritten_by_an_unchanged_profile() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    bridge.snapshot.lock().unwrap()["thread"]["model"] = json!("desktop-choice");
    app.handling_sync_desktop(None);
    assert_eq!(app.run("task").unwrap().model, "desktop-choice");
    app.handling_followup(&question(&app, "先继续")).unwrap();
    assert_eq!(
        bridge.calls.lock().unwrap().last().unwrap()["model"],
        Value::Null
    );
    // The mock did not publish the accepted input yet; model change coverage
    // uses a separate fresh state so there is no pending remote ambiguity.
    finish(&app);
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    app.state.lock().unwrap().config.model = "explicit-new-model".into();
    app.handling_followup(&question(&app, "明确切换")).unwrap();
    assert_eq!(
        bridge.calls.lock().unwrap().last().unwrap()["model"],
        "explicit-new-model"
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_known_pre_dispatch_failure_does_not_permanently_lock_followups() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    *bridge.fail_send.lock().unwrap() = Some("桌面连接尚未建立".into());
    assert!(app
        .handling_followup(&question(&app, "未发出的输入"))
        .is_err());
    assert!(app.run("task").unwrap().rounds[0].supplements[0].not_submitted);
    *bridge.fail_send.lock().unwrap() = None;
    app.handling_followup(&question(&app, "重试输入")).unwrap();
    assert_eq!(bridge.sent.load(Ordering::SeqCst), 2);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn late_success_and_failure_cannot_overwrite_a_followup_or_trigger_a_save() {
    for failure in [false, true] {
        let (app, bridge, root) = fixture();
        start(&app).unwrap();
        app.handling_sync_desktop(None);
        if failure {
            *bridge.fail_read.lock().unwrap() = Some("Synthetic old read failure".into());
        } else {
            bridge.snapshot.lock().unwrap()["thread"]["status"]["activeFlags"] =
                json!(["waitingOnUserInput"]);
        }
        let (entered, rx) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        *bridge.gate.lock().unwrap() = Some((entered, gate));
        let worker = app.clone();
        let read = std::thread::spawn(move || worker.handling_sync_desktop(None));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        bridge.snapshot.lock().unwrap()["thread"]["status"]["activeFlags"] = json!([]);
        app.handling_followup(&question(&app, "Synthetic newer input"))
            .unwrap();
        let current = app.run("task").unwrap();
        let saves = app.saves_revision();
        release.send(()).unwrap();
        assert!(!read.join().unwrap());
        assert!(app.run("task").unwrap() == current);
        assert_eq!(app.saves_revision(), saves);
        assert!(app.desktop_reading.lock().unwrap().is_empty());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn sync_during_submission_keeps_the_pending_input_until_acknowledged() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    app.handling_sync_desktop(None);
    let (entered, rx) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    *bridge.send_gate.lock().unwrap() = Some((entered, gate));
    let worker = app.clone();
    let send = std::thread::spawn(move || {
        worker.handling_followup(&question(&worker, "Synthetic pending input"))
    });
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let pending = app.run("task").unwrap();
    assert_eq!(pending.status, "submitting");
    let saves = app.saves_revision();
    assert!(!app.handling_sync_desktop(None));
    assert!(app.run("task").unwrap() == pending);
    assert_eq!(app.saves_revision(), saves);
    release.send(()).unwrap();
    send.join().unwrap().unwrap();
    assert!(app.run("task").unwrap().rounds[0].supplements[0].accepted);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unchanged_sync_is_a_noop_and_failed_commits_leave_the_run_retryable() {
    let (app, bridge, root) = fixture();
    start(&app).unwrap();
    assert!(app.handling_sync_desktop(None));
    let before = app.run("task").unwrap();
    let saves = app.saves_revision();
    assert!(!app.handling_sync_desktop(None));
    assert_eq!(app.saves_revision(), saves);
    bridge.snapshot.lock().unwrap()["thread"]["model"] = json!("verified-new-model");
    crate::test_support::persistence_failure(&root, true).unwrap();
    assert!(!app.handling_sync_desktop(None));
    assert!(app.run("task").unwrap() == before);
    assert_eq!(app.saves_revision(), saves);
    assert!(app.desktop_reading.lock().unwrap().is_empty());
    crate::test_support::persistence_failure(&root, false).unwrap();
    assert!(app.handling_sync_desktop(None));
    assert_eq!(app.run("task").unwrap().model, "verified-new-model");
    *bridge.fail_read.lock().unwrap() = Some("Synthetic current read failure".into());
    assert!(app.handling_sync_desktop(None));
    assert_eq!(app.run("task").unwrap().waiting_on, "sync");
    let saves = app.saves_revision();
    *bridge.fail_read.lock().unwrap() = Some("Synthetic current read failure".into());
    assert!(!app.handling_sync_desktop(None));
    assert_eq!(app.saves_revision(), saves);
    assert!(app.handling_sync_desktop(None));
    assert!(app.run("task").unwrap().error.is_empty());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_reported_model_without_effort_does_not_inherit_the_requested_effort() {
    let mut run = Run {
        effort: "high".into(),
        ..Default::default()
    };
    apply_desktop_snapshot(
        &mut run,
        &json!({"thread":{"model":"verified-model"},"turns":[]}),
    );
    assert_eq!(run.model_reported, Some(true));
    assert!(run.effort.is_empty() && run.last_reported_effort.is_empty());
}
