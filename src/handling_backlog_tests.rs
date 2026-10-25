use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};

#[path = "handling_followup_tests.rs"]
mod followup_tests;

struct Agent {
    alive: AtomicBool,
    fail_close: AtomicBool,
    fail_save_on_close: Mutex<Option<std::path::PathBuf>>,
    answered: AtomicUsize,
    interrupted: AtomicUsize,
    observed: AtomicUsize,
    submitted: AtomicUsize,
    steered: AtomicUsize,
    fail_read: AtomicBool,
    fail_send: AtomicBool,
    read_gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    history: Mutex<Value>,
    pending: Mutex<Vec<Value>>,
}
impl Agent {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
            fail_close: AtomicBool::new(false),
            fail_save_on_close: Mutex::new(None),
            answered: AtomicUsize::new(0),
            interrupted: AtomicUsize::new(0),
            observed: AtomicUsize::new(0),
            submitted: AtomicUsize::new(0),
            steered: AtomicUsize::new(0),
            fail_read: AtomicBool::new(false),
            fail_send: AtomicBool::new(false),
            read_gate: Mutex::new(None),
            history: Mutex::new(json!({"data":[]})),
            pending: Mutex::new(vec![
                json!({"id":"approval","method":"item/commandExecution/requestApproval","params":{"command":"synthetic-command"}}),
            ]),
        }
    }
}
impl AgentSession for Agent {
    fn close(&self) -> Result<(), String> {
        if self.fail_close.load(Ordering::SeqCst) {
            return Err("synthetic close failure".into());
        }
        self.alive.store(false, Ordering::SeqCst);
        if let Some(root) = self.fail_save_on_close.lock().unwrap().take() {
            crate::test_support::persistence_failure(&root, true).unwrap();
        }
        Ok(())
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn start(&self, _: &str, _: &str, _: &ContextTool, _: &Config) -> Result<Value, String> {
        Ok(json!({"thread":{"id":id()},"model":"synthetic"}))
    }
    fn rename(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn submit(&self, _: &str, _: &str, _: &Config, _: &str) -> Result<Value, String> {
        self.submitted.fetch_add(1, Ordering::SeqCst);
        if self.fail_send.load(Ordering::SeqCst) {
            return Err("synthetic uncertain send".into());
        }
        Ok(json!({"turn":{"id":id()}}))
    }
    fn steer(&self, _: &str, _: &str, _: &str, _: &str) -> Result<(), String> {
        self.steered.fetch_add(1, Ordering::SeqCst);
        if self.fail_send.load(Ordering::SeqCst) {
            return Err("synthetic uncertain steer".into());
        }
        Ok(())
    }
    fn read(&self, _: &str) -> Result<Value, String> {
        self.observed.fetch_add(1, Ordering::SeqCst);
        let gate = self.read_gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        if self.fail_read.load(Ordering::SeqCst) {
            return Err("synthetic read failure".into());
        }
        Ok(json!({"thread":{}}))
    }
    fn history(&self, _: &str) -> Result<Value, String> {
        self.observed.fetch_add(1, Ordering::SeqCst);
        Ok(self.history.lock().unwrap().clone())
    }
    fn resume(&self, _: &str, _: &ContextTool) -> Result<Value, String> {
        Ok(json!({"thread":{}}))
    }
    fn unsubscribe(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn interrupt(&self, _: &str, _: &str) -> Result<(), String> {
        self.interrupted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn events(&self) -> Vec<ai_backend::UsageEvent> {
        vec![]
    }
    fn requests(&self, _: &str) -> Vec<Value> {
        self.pending.lock().unwrap().clone()
    }
    fn respond(&self, _: &str, _: &str, _: &Value, _: &Value) -> Result<(), String> {
        self.answered.fetch_add(1, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
        Ok(())
    }
}
fn fixture(driver: &str) -> (Arc<App>, Arc<Agent>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("shishi-stop-{}", id()));
    let app = Arc::new(App::load(root.clone()).unwrap());
    {
        let mut s = app.state.lock().unwrap();
        s.config.model_profiles = None;
        if driver == "dsh" {
            let connection = Connection {
                id: "synthetic-dsh".into(),
                driver: "dsh".into(),
                executable: "synthetic".into(),
                ..Default::default()
            };
            s.config.selected_connection = connection.id.clone();
            s.config.ai_connections.push(connection);
        }
        s.tasks.push(Task {
            id: "task".into(),
            title: "Keep this task".into(),
            status: "待处理".into(),
            ..Default::default()
        });
    }
    let agent = Arc::new(Agent::new());
    let connection = app.handling_connection().unwrap();
    app.state.lock().unwrap().handling.runs.push(Run {
        id: "run".into(),
        task_id: "task".into(),
        thread_id: "old-thread".into(),
        connection: Some(connection),
        status: "waiting".into(),
        watching: true,
        requests: agent.requests("old-thread"),
        rounds: vec![Round {
            id: "old-round".into(),
            turn_id: "old-turn".into(),
            status: "inProgress".into(),
            question: "preserve history".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    app.handling_client
        .lock()
        .unwrap()
        .insert("run:run".into(), agent.clone());
    app.persist().unwrap();
    (app, agent, root)
}
fn stop() -> Value {
    json!({"task_id":"task","run_id":"run"})
}

#[test]
fn terminate_is_independent_of_submission_lock_and_ignores_late_events_on_both_drivers() {
    for driver in ["dsh"] {
        let (app, agent, root) = fixture(driver);
        let other = Arc::new(Agent::new());
        app.handling_client
            .lock()
            .unwrap()
            .insert("run:other".into(), other.clone());
        let submission = app.handling_action.lock().unwrap();
        app.handling_terminate(&stop()).unwrap();
        drop(submission);
        assert!(!agent.is_alive());
        assert!(other.is_alive());
        assert!(app.desktop_approvals().is_empty());
        let mut run = app.run("task").unwrap();
        assert!(can_restart(&run));
        assert_eq!(run.rounds[0].question, "preserve history");
        let before = json!(run);
        apply_notification(
            &mut run,
            &json!({"method":"turn/started","params":{"turn":{"id":"late","status":"inProgress"}}}),
        );
        apply_history(
            &mut run,
            &json!([{"id":"old-turn","status":"completed"}]),
            &Value::Null,
        );
        assert_eq!(json!(run), before);
        assert_eq!(
            app.update_run("run", |r| r.status = "inProgress".into())
                .unwrap_err(),
            STOPPED
        );
        assert_eq!(app.state.lock().unwrap().tasks[0].title, "Keep this task");
        assert!(app.handling_request(&json!({"task_id":"task","run_id":"run","thread_id":"old-thread","request_id":"\"approval\"","action":"accept"})).is_err());
        assert_eq!(agent.answered.load(Ordering::SeqCst), 0);
        drop(app);
        let loaded = App::load(root.clone()).unwrap();
        assert_eq!(
            loaded
                .state
                .lock()
                .unwrap()
                .handling
                .runs
                .iter()
                .find(|r| r.id == "run")
                .unwrap()
                .status,
            "terminated"
        );
        drop(loaded);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn stop_marker_is_saved_before_waiting_for_a_client_and_blocks_other_updates() {
    let (app, agent, root) = fixture("dsh");
    let cache = app.handling_client.lock().unwrap();
    let worker = app.clone();
    let stop_thread = std::thread::spawn(move || worker.handling_terminate(&stop()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !app.state.lock().unwrap().handling.runs[0].stop_requested {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        app.update_run("run", |r| r.status = "completed".into())
            .unwrap_err(),
        STOPPED
    );
    assert!(agent.is_alive());
    drop(cache);
    stop_thread.join().unwrap().unwrap();
    assert!(!agent.is_alive());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn failed_close_or_final_save_keeps_stop_marker_and_can_be_retried() {
    let (app, agent, root) = fixture("dsh");
    agent.fail_close.store(true, Ordering::SeqCst);
    assert!(app.handling_terminate(&stop()).is_err());
    let r = app.run("task").unwrap();
    assert!(r.stop_requested);
    assert!(!can_restart(&r));
    assert!(app.ensure_run_open("run").is_err());
    agent.fail_close.store(false, Ordering::SeqCst);
    *agent.fail_save_on_close.lock().unwrap() = Some(root.clone());
    assert!(app.handling_terminate(&stop()).is_err());
    assert!(app.run("task").unwrap().stop_requested);
    crate::test_support::persistence_failure(&root, false).unwrap();
    app.handling_terminate(&stop()).unwrap();
    assert!(can_restart(&app.run("task").unwrap()));
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn failed_initial_save_never_closes_a_backend_and_desktop_control_is_preserved() {
    let (app, agent, root) = fixture("dsh");
    crate::test_support::persistence_failure(&root, true).unwrap();
    assert!(app.handling_terminate(&stop()).is_err());
    assert!(agent.is_alive());
    assert!(!app.run("task").unwrap().stop_requested);
    crate::test_support::persistence_failure(&root, false).unwrap();
    app.state.lock().unwrap().handling.runs[0].desktop_control = true;
    assert!(app.handling_terminate(&stop()).is_err());
    assert!(agent.is_alive());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn approval_identifies_its_original_run_and_cannot_follow_a_reused_request_id() {
    let (app, agent, root) = fixture("dsh");
    let mut request = json!({"task_id":"task","run_id":"run","thread_id":"wrong-thread","request_id":"\"approval\"","action":"accept"});
    assert!(app.handling_request(&request).is_err());
    assert_eq!(agent.answered.load(Ordering::SeqCst), 0);
    request["thread_id"] = json!("old-thread");
    app.handling_request(&request).unwrap();
    assert!(app.handling_request(&request).is_err());
    assert_eq!(agent.answered.load(Ordering::SeqCst), 1);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn a_terminated_run_allows_a_new_formal_session_and_preserves_the_old_one() {
    let (app, _, root) = fixture("dsh");
    app.handling_terminate(&stop()).unwrap();
    let fresh = Arc::new(Agent::new());
    let key = app.handling_connection().unwrap().cache_key();
    app.handling_client.lock().unwrap().insert(key, fresh);
    let project = app.handling_prepare("task").unwrap()["projects"][0].clone();
    let reply = app
        .handling_start(&json!({"task_id":"task","project_id":project["id"],"cwd":project["cwd"]}))
        .unwrap();
    assert_ne!(reply["thread_id"], "old-thread");
    let state = app.state.lock().unwrap();
    assert_eq!(state.handling.runs.len(), 2);
    assert_eq!(state.handling.runs[0].status, "terminated");
    assert_eq!(state.handling.runs[1].status, "inProgress");
    drop(state);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn command_unicode_and_already_damaged_output_are_preserved_through_events_and_snapshots() {
    let (app, _, root) = fixture("dsh");
    let healthy = "D:/合成资料/路径🚀/文件.txt";
    let damaged = "D:/\u{fffd}\u{fffd}/file.txt";
    for text in [healthy, damaged] {
        let event = json!({"method":"item/completed","params":{"turnId":"old-turn","item":{"type":"commandExecution","id":"command","command":"synthetic","aggregatedOutput":text}}});
        let decoded: Value = serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
        app.update_run("run", |r| apply_notification(r, &decoded))
            .unwrap();
        let snapshot = app.snapshot();
        assert_eq!(
            snapshot["handling"][0]["rounds"][0]["steps"][0]["text"],
            format!("synthetic\n{text}")
        );
    }
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_completed_local_run_can_be_closed_for_a_new_session() {
    let (app, agent, root) = fixture("dsh");
    {
        let mut s = app.state.lock().unwrap();
        s.handling.runs[0].status = "completed".into();
        s.handling.runs[0].rounds[0].status = "completed".into();
    }
    app.handling_terminate(&stop()).unwrap();
    let run = app.run("task").unwrap();
    assert!(can_restart(&run));
    assert_eq!(run.rounds[0].status, "completed");
    assert!(!agent.is_alive());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
