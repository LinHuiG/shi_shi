use super::*;
use std::sync::mpsc;

#[test]
fn owners_are_idempotent_and_default_exit_releases_only_self() {
    let r = Arc::new(Owners::default());
    r.acquire(true).unwrap();
    r.acquire(false).unwrap();
    r.acquire(false).unwrap();
    assert!(!r.release(true, false));
    assert!(!r.gui() && r.cli());
    assert!(r.release(false, false));
    assert!(r.acquire(true).is_err());
    assert!(r.begin().is_err());
}

#[test]
fn force_releases_both_and_drain_waits_for_inflight_request() {
    let r = Arc::new(Owners::default());
    r.acquire(true).unwrap();
    r.acquire(false).unwrap();
    let guard = r.begin().unwrap();
    assert!(r.release(false, true));
    assert_eq!(r.snapshot()["active_requests"], 1);
    let worker = r.clone();
    let (sent, received) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        worker.wait_idle(Duration::from_secs(2));
        sent.send(()).unwrap();
    });
    assert!(received.recv_timeout(Duration::from_millis(40)).is_err());
    drop(guard);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    waiter.join().unwrap();
}

#[test]
fn channel_authenticates_routes_and_signals_after_reply() {
    let root = wire::test_dir("service");
    let data = root.join("data");
    fs::create_dir(&data).unwrap();
    let owners = Arc::new(Owners::default());
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let handler: Arc<Handler> = Arc::new(|method, params| match method {
        "echo" => Ok(params),
        "update_install" => Ok(json!(true)),
        _ => Err("未知请求".into()),
    });
    let service =
        start(data.clone(), owners.clone(), handler, Arc::new(move |s| tx.lock().unwrap().send(s).unwrap())).unwrap();
    let route = wire::probe(&root).unwrap().expect("route answers");
    let forged = wire::Route { token: "x".repeat(32), ..route.clone() };
    assert!(wire::call(&forged, "backend.status", json!({})).and_then(wire::success).is_err());
    assert_eq!(wire::success(wire::call(&route, "echo", json!({"a": 1})).unwrap()).unwrap()["a"], 1);
    assert!(owners.cli(), "business calls register the CLI owner");
    let error = wire::success(wire::call(&route, "pick", json!({})).unwrap()).unwrap_err();
    assert!(error.contains("图形界面"));
    assert_eq!(wire::success(wire::call(&route, "nope", json!({})).unwrap()).unwrap_err(), "未知请求");
    wire::success(wire::call(&route, "ui.open", json!({})).unwrap()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Signal::Show);
    wire::success(wire::call(&route, "update_install", json!({})).unwrap()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Signal::Handoff);
    let r = wire::success(wire::call(&route, "ui.close", json!({})).unwrap()).unwrap();
    assert_eq!(r["closing"], false);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Signal::Hide);
    let r = wire::success(wire::call(&route, "backend.stop", json!({})).unwrap()).unwrap();
    assert_eq!(r["closing"], true);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Signal::Shutdown);
    drop(service);
    let file = data.join(wire::ROUTE_FILE);
    for _ in 0..100 {
        if !file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!file.exists(), "the owner removes its route on exit");
    fs::remove_dir_all(root).unwrap();
}
