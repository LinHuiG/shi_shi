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
    gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
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
        if let Some(error) = self.fail_send.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(json!({"sent":true}))
    }
    fn read(&self, _: &str) -> Result<Value, String> {
        let snapshot = self.snapshot.lock().unwrap().clone();
        if let Some((entered, release)) = self.gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(snapshot)
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
        gate: Mutex::new(None),
    });
    *app.desktop_bridge.lock().unwrap() = Some(bridge.clone());
    (app, bridge, root)
}
fn start(app: &App) -> Result<Value, String> {
    let p = app.handling_prepare("task")?["projects"][0].clone();
    app.handling_start(&json!({"task_id":"task","project_id":p["id"],"cwd":p["cwd"]}))
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
