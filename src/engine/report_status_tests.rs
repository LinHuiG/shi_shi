use super::*;
use crate::calendar_service::MorningJob;

fn job(id: &str, status: &str, finished: i64) -> MorningJob {
    MorningJob {
        id: id.into(),
        business_day: "2026-10-29".into(),
        status: status.into(),
        error: if status == "failed" {
            "Synthetic failure".into()
        } else {
            String::new()
        },
        finished,
    }
}

fn outcome(state: &State) -> Value {
    json!(state.report_outcome())
}

#[test]
fn latest_completed_result_unifies_reports_and_archive_without_changing_history() {
    let mut state = State::default();
    state.morning_jobs.push(job("morning", "failed", 10));
    assert_eq!(outcome(&state)["label"], "早报");
    state.archive_attempts.push(ArchiveAttempt {
        id: "archive".into(),
        status: "succeeded".into(),
        finished: 20,
        ..Default::default()
    });
    assert_eq!(outcome(&state)["status"], "succeeded");
    state.reports.push(
        json!({"id":"evening","label":"晚报","summary_status":"failed","time":5,"finished":30}),
    );
    assert_eq!(outcome(&state)["id"], "evening");
    for status in ["done", "reused", "fallback"] {
        let finished = state.next_report_finished(30);
        state.reports.push(json!({"id":status,"label":"早报","summary_status":status,"time":1,"finished":finished}));
        assert_eq!(outcome(&state)["id"], status);
        assert_eq!(outcome(&state)["status"], "succeeded");
    }
    // A delayed scheduler write retains the report's completion time.
    state.morning_jobs.push(job("late-write", "failed", 30));
    let before = json!(state);
    for _ in 0..3 {
        assert_eq!(outcome(&state)["status"], "succeeded");
    }
    assert_eq!(json!(state), before);
    assert_eq!(state.morning_jobs[0].status, "failed");
}

#[test]
fn unfinished_partial_and_interrupted_results_never_clear_a_failure() {
    let mut state = State::default();
    state.morning_jobs.push(job("failed", "failed", 10));
    for status in ["queued", "running"] {
        state.morning_jobs.push(job(status, status, 30));
        state.archive_attempts.push(ArchiveAttempt {
            id: status.into(),
            status: status.into(),
            created: 30,
            ..Default::default()
        });
    }
    for report in [
        json!({"summary_status":"pending","time":40}),
        json!({"summary_status":"done","attempt_id":"running","time":40}),
        json!({"summary_status":"done","remaining_events":1,"time":40}),
        json!({"summary_status":"done","archive_complete":false,"time":40}),
    ] {
        state.reports.push(report);
    }
    assert_eq!(outcome(&state)["id"], "failed");
    state
        .reports
        .push(json!({"id":"cancelled","summary_status":"interrupted","finished":50}));
    assert_eq!(outcome(&state)["status"], "failed");
}

#[test]
fn legacy_job_times_use_claim_or_business_day_not_reload_time() {
    use crate::work_state::{MorningClaim, WorkDayRecord};
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 29).unwrap();
    let midnight = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
        - 8 * 3_600_000;
    let mut state = State::default();
    let legacy: MorningJob = serde_json::from_value(
        json!({"id":"legacy","business_day":"2026-10-29","status":"failed","error":"Synthetic"}),
    )
    .unwrap();
    state.morning_jobs.push(legacy);
    assert_eq!(outcome(&state)["finished"], midnight);
    let mut record = WorkDayRecord::default();
    record.morning_claim = Some(MorningClaim {
        claim_id: "legacy".into(),
        claimed_at_ms: midnight + 8 * 3_600_000,
    });
    state.work_scheduler.runtime.day_records.insert(day, record);
    assert_eq!(outcome(&state)["finished"], midnight + 8 * 3_600_000);
    state.archive_attempts.push(ArchiveAttempt {
        id: "manual".into(),
        status: "succeeded".into(),
        finished: midnight + 9 * 3_600_000,
        ..Default::default()
    });
    assert_eq!(outcome(&state)["status"], "succeeded");
    let reloaded: State = serde_json::from_value(json!(state)).unwrap();
    assert_eq!(outcome(&reloaded), outcome(&state));
}

#[test]
fn completion_clock_preserves_serial_commit_order_for_equal_or_rewound_time() {
    let state = Mutex::new(State::default());
    std::thread::scope(|scope| {
        for i in 0..8 {
            let state = &state;
            scope.spawn(move || {
                let mut locked = state.lock().unwrap();
                let finished = locked.next_report_finished(100);
                locked
                    .morning_jobs
                    .push(job(&i.to_string(), "failed", finished));
            });
        }
    });
    let mut state = state.into_inner().unwrap();
    assert_eq!(
        state
            .morning_jobs
            .iter()
            .map(|j| j.finished)
            .collect::<Vec<_>>(),
        (100..108).collect::<Vec<_>>()
    );
    let finished = state.next_report_finished(1);
    assert_eq!(finished, 108);
    state.archive_attempts.push(ArchiveAttempt {
        id: "recovered".into(),
        status: "succeeded".into(),
        finished,
        ..Default::default()
    });
    assert_eq!(outcome(&state)["id"], "recovered");
    state
        .morning_jobs
        .push(job("old-late-write", "failed", 101));
    assert_eq!(outcome(&state)["id"], "recovered");
}

#[test]
fn manual_archive_success_clears_legacy_failure_across_pages_and_restart() {
    let (root, app) = crate::test_support::scoped_app("shishi-report-status");
    {
        let mut state = app.state.lock().unwrap();
        state.config.model_profiles = None;
        state.config.daily_calls = 0;
        let mut old = job("legacy", "failed", 0);
        old.business_day = date_from(now() - 86_400_000);
        state.morning_jobs.push(old);
        app.save(&state).unwrap();
    }
    let report = app.archive(AiMode::Manual).unwrap();
    assert_eq!(report["summary_status"], "fallback");
    for page in ["tasks", "reports", "settings"] {
        assert_eq!(
            app.snapshot_page(Some(page))["report_outcome"]["status"],
            "succeeded"
        );
    }
    assert_eq!(app.state.lock().unwrap().calls, 0);
    drop(app);
    let app = App::load(root.clone()).unwrap();
    assert_eq!(
        app.snapshot_page(Some("tasks"))["report_outcome"]["status"],
        "succeeded"
    );
    assert_eq!(app.state.lock().unwrap().morning_jobs[0].status, "failed");
}

#[test]
fn report_reuse_publishes_success_only_after_save_and_new_failure_replaces_it() {
    let (root, app) = crate::test_support::scoped_app("shishi-report-save");
    {
        let mut state = app.state.lock().unwrap();
        state.config.model_profiles = None;
        state.config.daily_calls = 0;
        state
            .morning_jobs
            .push(job("failed", "failed", now() - 1000));
        state.reports.push(json!({"id":"prior","time":now()-2000,"tasks":[],"suggestions":[],"ai_summary":{"summary":"Synthetic","completed":[],"attention":[],"next_steps":[]},"summary_status":"done"}));
        app.save(&state).unwrap();
    }
    crate::test_support::persistence_failure(&root, true).unwrap();
    assert!(app.report("早报", AiMode::Manual).is_err());
    assert_eq!(outcome(&app.state.lock().unwrap())["status"], "failed");
    crate::test_support::persistence_failure(&root, false).unwrap();
    fs::write(root.join("data/reports"), "synthetic file obstruction").unwrap();
    let report = app.report("早报", AiMode::Manual).unwrap();
    assert_eq!(report["summary_status"], "reused");
    assert!(report["finished"].as_i64().unwrap() > 0);
    assert_eq!(outcome(&app.state.lock().unwrap())["status"], "succeeded");
    assert!(app.activity.lock().unwrap().iter().any(|row| {
        row["kind"] == "report_file_failed" && row["detail"]["report"] == report["id"]
    }));
    app.state.lock().unwrap().tasks.push(Task {
        id: "new".into(),
        title: "Synthetic".into(),
        status: "待处理".into(),
        ..Default::default()
    });
    let report = app.report("晚报", AiMode::Manual).unwrap();
    assert_eq!(report["summary_status"], "failed");
    assert_eq!(outcome(&app.state.lock().unwrap())["label"], "晚报");
    assert_eq!(app.state.lock().unwrap().calls, 0);
    drop(app);
    let app = App::load(root.clone()).unwrap();
    assert_eq!(outcome(&app.state.lock().unwrap())["label"], "晚报");
}

#[test]
fn cancelled_report_keeps_public_error_and_scheduler_uses_its_committed_time() {
    let (_root, app) = crate::test_support::scoped_app("shishi-report-cancel");
    {
        let mut state = app.state.lock().unwrap();
        state.config.model_profiles = None;
        state.config.codex = "nonexistent-must-not-launch.exe".into();
        state.config.daily_calls = 0;
    }
    app.test_force_working();
    for scheduled in [false, true] {
        if scheduled {
            app.state
                .lock()
                .unwrap()
                .morning_jobs
                .push(job("queued", "queued", 0));
        }
        let result = std::thread::scope(|scope| {
            // Block before provider preparation; cancellation must prevent any call.
            let files = app.prepare_gate.lock().unwrap();
            let worker = scope.spawn(|| {
                if scheduled {
                    app.morning_tick().map(|_| Value::Null)
                } else {
                    app.report("早报", AiMode::Manual)
                }
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let request = loop {
                if let Some(request) = app
                    .active_ai
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|call| call.id.clone())
                {
                    break Some(request);
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let interrupted = request.map(|request| app.interrupt_ai(&request));
            drop(files);
            let result = worker.join().unwrap();
            assert!(interrupted.is_some_and(|value| value.is_ok()));
            result
        });
        if scheduled {
            assert!(result.is_ok());
            let state = app.state.lock().unwrap();
            assert_eq!(state.morning_jobs.last().unwrap().status, "failed");
            assert_eq!(
                json!(state.morning_jobs.last().unwrap().finished),
                state.reports[0]["finished"]
            );
        } else {
            assert_eq!(result.unwrap_err(), crate::ai_control::CANCELLED);
        }
        assert_eq!(
            app.state.lock().unwrap().reports[0]["summary_status"],
            "interrupted"
        );
    }
    assert_eq!(app.state.lock().unwrap().calls, 0);
    app.archive(AiMode::Manual).unwrap();
    assert_eq!(outcome(&app.state.lock().unwrap())["status"], "succeeded");
}

#[test]
fn report_limit_retains_the_latest_completion_among_pending_rows() {
    let mut state = State::default();
    state.morning_jobs.push(job("old-failure", "failed", 10));
    state
        .reports
        .push(json!({"id":"success","summary_status":"done","finished":20}));
    for index in 0..200 {
        state.reports.insert(
            0,
            json!({"id":format!("pending-{index}"),"summary_status":"pending","time":100+index}),
        );
        state.truncate_reports();
        assert!(state.reports.len() <= 60);
        assert_eq!(outcome(&state)["id"], "success");
    }
    assert_eq!(state.reports[0]["id"], "pending-199");
    assert_eq!(state.reports[59]["id"], "success");
    state.reports.insert(
        0,
        json!({"id":"new-failure","summary_status":"failed","finished":300}),
    );
    state.truncate_reports();
    assert_eq!(outcome(&state)["id"], "new-failure");
    assert!(!state.reports.iter().any(|r| r["id"] == "success"));
}

#[test]
fn retention_keeps_latest_completed_attempt_and_legacy_morning_claim() {
    use crate::work_state::{MorningClaim, WorkDayRecord};
    let at = now();
    let old = at - 100 * 86_400_000;
    let mut state = State::default();
    state.archive_attempts.push(ArchiveAttempt {
        id: "long-running-success".into(),
        status: "succeeded".into(),
        created: old,
        finished: at - 100,
        ..Default::default()
    });
    state.archive_attempts.push(ArchiveAttempt {
        id: "queued".into(),
        status: "queued".into(),
        created: at,
        ..Default::default()
    });
    state.morning_jobs.push(job("old-failure", "failed", old));
    crate::retention::prune(&mut state, at);
    assert_eq!(outcome(&state)["id"], "long-running-success");

    let mut state = State::default();
    let day = chrono::NaiveDate::parse_from_str(&date_from(old), "%Y-%m-%d").unwrap();
    let midnight = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
        - 8 * 3_600_000;
    let mut record = WorkDayRecord::default();
    record.morning_claim = Some(MorningClaim {
        claim_id: "legacy-success".into(),
        claimed_at_ms: midnight + 8 * 3_600_000,
    });
    state.work_scheduler.runtime.day_records.insert(day, record);
    let mut legacy = job("legacy-success", "succeeded", 0);
    legacy.business_day = day.to_string();
    state.morning_jobs.push(legacy);
    state.archive_attempts.push(ArchiveAttempt {
        id: "earlier-failure".into(),
        status: "failed".into(),
        created: midnight,
        finished: midnight + 7 * 3_600_000,
        ..Default::default()
    });
    crate::retention::prune(&mut state, at);
    assert_eq!(outcome(&state)["id"], "legacy-success");
    assert!(state.work_scheduler.runtime.day_records.contains_key(&day));
}
