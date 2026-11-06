use super::*;

#[test]
fn runtime_routes_requests_and_reports_unknown_methods() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    let rt = Runtime::start(ctx, false).unwrap();
    assert_eq!(rt.call("nope", Value::Null).unwrap_err().0, "未知请求");
    let r = rt.call("ai_interrupt", serde_json::json!({})).unwrap();
    assert_eq!(r["skipped"], "该次 AI 分析已结束");
    let r = rt.call("ai_job_start", serde_json::json!({"method": "rm"})).unwrap_err();
    assert_eq!(r.0, "不支持的后台操作");
    let all = methods();
    assert_eq!(all.len(), all.iter().collect::<std::collections::BTreeSet<_>>().len(), "methods are unique");
}

#[test]
fn watch_reports_each_state_change_without_polling() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    let rt = Runtime::start(ctx.clone(), false).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    rt.watch(move |n| {
        let _ = tx.send(n);
    });
    // The watcher is registered once the executor has served a later call.
    rt.call("ui_ready", Value::Null).unwrap();
    let before = rx.try_recv().ok();
    ctx.db.poke();
    let n = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    assert!(before.is_none_or(|b| n > b));
    ctx.db.bump();
    assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap() > n);
}
