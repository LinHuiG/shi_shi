use super::*;

fn exec() -> Arc<Executor> {
    Arc::new(Executor::new(Db::memory().unwrap()))
}

#[tokio::test]
async fn slot_is_serial_and_released_after_failure_or_drop() {
    let e = exec();
    let call = e.try_call("analysis").unwrap();
    assert!(e.ai_busy());
    assert_eq!(e.try_call("edit").err().unwrap().0, BUSY);
    let manual = e.manual_call("edit", "t1");
    assert_eq!(e.manual_status().len(), 1);
    assert!(!call.finish(), "not cancelled");
    assert!(!e.ai_busy());
    drop(manual);
    assert!(e.manual_status().is_empty());
    // A failure path that only drops the call releases it too.
    let result: Result<()> = (|| {
        let _call = e.try_call("report")?;
        Err(Error::new("网络错误"))
    })();
    assert!(result.is_err());
    assert!(!e.ai_busy());
    e.try_call("edit").unwrap();
}

#[tokio::test]
async fn waiting_reservation_wakes_on_release_and_stops_on_interrupt_or_deadline() {
    let e = exec();
    let first = e.try_call("").unwrap();
    let waiter = e.clone();
    let wait = tokio::spawn(async move {
        waiter.wait_call("", Instant::now() + Duration::from_secs(5)).await.map(|c| c.id().to_string())
    });
    tokio::task::yield_now().await;
    let first_id = first.id().to_string();
    drop(first);
    let second = wait.await.unwrap().unwrap();
    assert_ne!(second, first_id);
    assert!(!e.ai_busy(), "the waiter's call was dropped with its task");

    // Interrupting the call it waits behind ends the wait.
    let running = e.try_call("").unwrap();
    let waiter = e.clone();
    let wait =
        tokio::spawn(async move { waiter.wait_call("", Instant::now() + Duration::from_secs(5)).await.map(|_| ()) });
    tokio::task::yield_now().await;
    assert!(e.interrupt(Some(running.id())));
    assert_eq!(wait.await.unwrap().unwrap_err().0, CANCELLED);
    assert!(running.finish());

    // A call that times out or fails cancels itself only: the waiter
    // (a morning report behind an analysis) still gets the slot.
    let timing_out = e.try_call("").unwrap();
    let waiter = e.clone();
    let wait =
        tokio::spawn(async move { waiter.wait_call("", Instant::now() + Duration::from_secs(5)).await.map(|_| ()) });
    tokio::task::yield_now().await;
    timing_out.cancel.cancel();
    tokio::task::yield_now().await;
    assert!(!wait.is_finished(), "the slot is still held");
    drop(timing_out);
    wait.await.unwrap().unwrap();

    let _held = e.try_call("").unwrap();
    let err = e.wait_call("", Instant::now() + Duration::from_millis(20)).await;
    assert_eq!(err.err().unwrap().0, WAIT_TIMEOUT);
}

#[tokio::test]
async fn interrupt_only_stops_the_named_request_and_next_request_renews_id() {
    let e = exec();
    let mut call = e.try_call("").unwrap();
    let manual = e.manual_call("edit", "t");
    assert!(!e.interrupt(Some("old-request")));
    assert!(!call.cancel.is_cancelled());
    let first = call.id().to_string();
    call.next_request().unwrap();
    assert_ne!(call.id(), first);
    assert_eq!(e.ai_status().unwrap().request_id, call.id());
    assert!(!e.interrupt(Some(&first)), "an earlier request of the call is over");
    assert!(e.interrupt(Some(manual.id())));
    assert!(e.last_interrupt().is_none(), "a manual stop is not the slot's interrupt");
    assert!(!call.cancel.is_cancelled());
    assert!(e.interrupt(Some(&call.id().to_string())));
    assert!(e.last_interrupt().is_some());
    assert_eq!(call.next_request().unwrap_err().0, CANCELLED);
    assert!(call.finish());
    // A new call clears the interrupt marker and is not cancelled.
    let next = e.try_call("").unwrap();
    assert!(e.last_interrupt().is_none());
    drop(call);
    assert!(e.ai_busy(), "a finished call never releases its successor");
    assert!(!next.finish());
}

#[tokio::test]
async fn job_results_are_stable_and_unknown_after_restart() {
    let e = exec();
    let id = e.job_start(async { Ok(json!({"x": 1})) }).unwrap();
    let failed = e.job_start(async { Err(Error::new("网络错误")) }).unwrap();
    let panicked = e.job_start(async { panic!("boom") }).unwrap();
    while [&id, &failed, &panicked].iter().any(|j| e.job_result(j).unwrap()["done"] == false) {
        tokio::task::yield_now().await;
    }
    assert_eq!(e.job_result(&id).unwrap()["data"]["x"], 1);
    assert_eq!(e.job_result(&id).unwrap()["data"]["x"], 1);
    assert_eq!(e.job_result(&failed).unwrap()["error"], "网络错误");
    assert_eq!(e.job_result(&panicked).unwrap()["error"], "后台操作异常终止，请查看日志");
    assert!(exec().job_result(&id).unwrap_err().0.contains("已重启"));
    assert_eq!(e.job_result("").unwrap_err().0, "缺少操作编号");
}

#[test]
fn live_output_merges_by_kind_and_keeps_limits() {
    let mut live = Live::default();
    live.push("text", "ab", 0);
    live.push("text", "cd", 0);
    live.push("reasoning", "x", 0);
    assert_eq!(live.entries.len(), 2);
    assert_eq!(live.entries[0].text, "abcd");
    live.push("reasoning", &"长".repeat(13_000), 0);
    assert_eq!(live.entries[1].text.chars().count(), 12_000);
    for i in 0..80 {
        live.push(if i % 2 == 0 { "tool" } else { "text" }, "y", 0);
    }
    assert_eq!(live.entries.len(), 64);
    for i in 0..6 {
        live.push(if i % 2 == 0 { "tool" } else { "text" }, &"z".repeat(11_000), 0);
    }
    let total: usize = live.entries.iter().map(|e| e.text.chars().count()).sum();
    assert!(total <= 48_000);
    assert_eq!(live.revision, 90);
}
