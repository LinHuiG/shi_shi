use serde_json::json;

use super::*;
use crate::{data, Config, Db};

#[test]
fn failure_class_follows_reference_order() {
    for (error, class) in [
        (CANCELLED, "cancelled"),
        ("用户中断了请求", "cancelled"),
        ("AI 输入超限：1 字符", "resource_limit"),
        ("HTTP 429 rate limit", "resource_limit"),
        ("供应商拒答，未应用本次结果", "provider_refusal"),
        ("API 分析失败：HTTP 504", "upstream_timeout"),
        ("invalid tool arguments", "tool_arguments"),
        ("请求超时", "temporary_failure"),
        ("connection reset by peer", "temporary_failure"),
        ("connection refused", "unknown"),
        ("插件读取失败", "source_failure"),
        ("模型未返回有效的结构化调用或最终结果", "unknown"),
        // Earlier categories win: a cancelled timeout is still cancelled.
        ("timeout after cancel", "cancelled"),
    ] {
        assert_eq!(failure_class(error), class, "{error}");
    }
    assert_eq!(purpose_label("prompt_review"), "归档分析");
    assert_eq!(purpose_label("schedule"), "事项更新");
}

fn db() -> Db {
    Db::memory().unwrap()
}

#[test]
fn reserve_checks_switch_gate_and_daily_limit_in_order() {
    let db = db();
    let mut config = Config::default();
    config.daily_calls = 2;
    db.test_write_blocking(move |tx| {
        let mut off = config.clone();
        off.auto_ai = false;
        let e = data::ai::reserve_call(tx, &off, "classify", Mode::Automatic, false).unwrap_err();
        assert_eq!(e.0, AUTOMATION_DISABLED);
        // Manual work is not stopped by the switch, only by the gate result.
        assert_eq!(data::ai::reserve_call(tx, &off, "classify", Mode::Manual, false).unwrap_err().0, DEFERRED);
        data::ai::reserve_call(tx, &config, "classify", Mode::Manual, true)?;
        data::ai::reserve_call(tx, &config, "edit", Mode::Manual, true)?;
        assert_eq!(data::ai::reserve_call(tx, &config, "archive", Mode::Manual, true).unwrap_err().0, DAILY_LIMIT);
        // The message filter has its own counter and no limit.
        for _ in 0..3 {
            data::ai::reserve_call(tx, &config, "message_filter", Mode::Automatic, true)?;
        }
        let main = data::ai::usage_today(tx, "main")?;
        assert_eq!((main.calls, main.unreported_calls), (2, 2));
        assert_eq!(data::ai::usage_today(tx, "message_filter")?.calls, 3);
        Ok(())
    })
    .unwrap();
}

#[test]
fn reserve_resets_an_earlier_day_and_usage_settles_reported_calls() {
    let db = db();
    let config = Config::default();
    db.test_write_blocking(move |tx| {
        tx.execute(
            "INSERT INTO usage_daily(day, purpose, calls, input_tokens, unreported_calls) VALUES ('2000-01-01', 'main', 150, 9, 4)",
            [],
        )?;
        assert_eq!(data::ai::usage_today(tx, "main")?.calls, 0);
        data::ai::reserve_call(tx, &config, "classify", Mode::Manual, true)?;
        data::ai::reserve_call(tx, &config, "classify", Mode::Manual, true)?;
        let n: i64 = tx.query_row("SELECT COUNT(*) FROM usage_daily", [], |r| r.get(0))?;
        assert_eq!(n, 1);
        data::ai::record_usage(tx, "classify", &json!({"input_tokens": 10, "output_tokens": 2, "cached_input_tokens": 4}))?;
        data::ai::record_usage(tx, "classify", &json!({"input_tokens": 1, "output_tokens": 1, "partial": true}))?;
        // Unknown usage changes nothing.
        data::ai::record_usage(tx, "classify", &json!({"input_tokens": 5}))?;
        let u = data::ai::usage_today(tx, "main")?;
        assert_eq!(
            (u.calls, u.input_tokens, u.output_tokens, u.cached_input_tokens, u.cached_reports, u.unreported_calls),
            (2, 11, 3, 4, 1, 1)
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        usage::status(&json!({"input_tokens": 1, "output_tokens": 1, "partial": true}), true),
        "reported_partial"
    );
    assert_eq!(usage::status(&json!({"input_tokens": 1, "output_tokens": 1}), true), "reported");
    assert_eq!(usage::status(&json!({}), true), "request_failed_without_complete_usage");
    assert_eq!(usage::status(&json!({}), false), "provider_did_not_report_complete_usage");
    assert_eq!(usage::cache_state(&json!({"cached_input_tokens": 0})), ("zero", Some(0)));
    assert_eq!(usage::cache_state(&json!({})), ("missing", None));
}

#[test]
fn errors_are_recorded_once_and_cancellation_is_not_an_error() {
    let db = db();
    db.test_write_blocking(|tx| {
        assert!(data::ai::ai_error(tx, "网络错误")?);
        assert!(!data::ai::ai_error(tx, "网络错误")?);
        assert!(!data::ai::ai_error(tx, CANCELLED)?);
        assert!(!data::ai::ai_error(tx, "AI_UNCONFIGURED:消息过滤未配置 AI 接入或模型，本次任务未执行")?);
        assert_eq!(data::ai::analysis_state(tx)?.last_error, "网络错误");
        let logged: i64 = tx.query_row("SELECT COUNT(*) FROM activity WHERE kind = 'error'", [], |r| r.get(0))?;
        assert_eq!(logged, 1);
        Ok(())
    })
    .unwrap();
}

mod work_calendar {
    use chrono::NaiveDate;
    use serde_json::json;

    use crate::ai::calendar::{self, Calendar, DayKind, Kind, Source, Year};
    use crate::ai::work::{self, Action, Eligibility, Runtime, Status, WorkSchedule};
    use crate::ai::Mode;
    use crate::{data, Config, Db};

    const FETCHED: i64 = 1_790_000_000_000;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Shanghai local `YYYY-MM-DD HH:MM[:SS]` as Unix ms.
    fn at(s: &str) -> i64 {
        let s = if s.len() == 16 { format!("{s}:00") } else { s.to_string() };
        let t = chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").unwrap();
        t.and_utc().timestamp_millis() - work::SHANGHAI_OFFSET_MS
    }

    fn year(y: i32, days: &[(&str, DayKind)]) -> Year {
        Year { year: y, fetched_at: FETCHED, days: days.iter().map(|(d, k)| (date(d), *k)).collect() }
    }

    fn calendar_2026() -> Calendar {
        let mut c = Calendar::default();
        c.insert_year(year(2026, &[("2026-10-01", DayKind::Holiday), ("2026-10-10", DayKind::Workday)]));
        c
    }

    fn day_schedule() -> WorkSchedule {
        WorkSchedule::parse("08:00", "20:00").unwrap()
    }

    #[test]
    fn classify_follows_reference_precedence() {
        let mut c = calendar_2026();
        let now = FETCHED + 1;
        let thu = c.classify(date("2026-11-05"), now);
        assert_eq!(
            (thu.kind, thu.source, thu.publication_year, thu.stale_age),
            (Kind::Workday, Some(Source::Cache), Some(2026), None)
        );
        assert_eq!(c.classify(date("2026-11-07"), now).kind, Kind::Weekend);
        assert_eq!(c.classify(date("2026-10-10"), now).kind, Kind::Workday);
        assert_eq!(c.classify(date("2026-10-01"), now).kind, Kind::Holiday);
        let unknown = c.classify(date("2025-05-06"), now);
        assert_eq!(unknown.kind, Kind::Unknown);
        assert_eq!(unknown.json()["diagnostics"], json!([{"kind": "no_coverage"}]));
        // The next year's file publishes the New Year arrangement for December.
        assert_eq!(c.classify(date("2026-12-31"), now).kind, Kind::Workday);
        c.insert_year(year(2027, &[("2026-12-31", DayKind::Holiday)]));
        let eve = c.classify(date("2026-12-31"), now);
        assert_eq!((eve.kind, eve.publication_year), (Kind::Holiday, Some(2027)));
        // An absent record in the later file never erases the older one.
        assert_eq!(c.classify(date("2026-10-01"), now).kind, Kind::Holiday);
        c.set_override(date("2026-11-05"), DayKind::Holiday);
        let manual = c.classify(date("2026-11-05"), now);
        assert_eq!((manual.kind, manual.source), (Kind::Holiday, Some(Source::Manual)));
        let stale = c.classify(date("2026-11-06"), FETCHED + calendar::CACHE_TTL_MS);
        assert_eq!(stale.stale_age, Some(calendar::CACHE_TTL_MS));
        assert_eq!(stale.json()["diagnostics"][0]["kind"], "stale_cache");
        assert_eq!(stale.json()["stale"], true);
    }

    #[test]
    fn parse_year_is_strict() {
        let ok = br#"{"$schema":"https://example.invalid/schema.json","year":2026,"papers":["p"],
            "days":[{"name":"New Year","date":"2025-12-31","isOffDay":true},{"date":"2026-01-04","isOffDay":false}]}"#;
        let parsed = calendar::parse_year(ok, 5).unwrap();
        assert_eq!(parsed.year, 2026);
        assert_eq!(parsed.days[&date("2025-12-31")], DayKind::Holiday);
        assert_eq!(parsed.days[&date("2026-01-04")], DayKind::Workday);
        for (body, reason) in [
            (&b""[..], "empty"),
            (br#"{"year":2026,"days":[],"extra":1}"#, "invalid"),
            (br#"{"year":2026,"days":[]}"#, "no days"),
            (br#"{"year":2026,"days":[{"date":"2025-11-30","isOffDay":true}]}"#, "outside publication year"),
            (br#"{"year":2026,"days":[{"date":"2026-1-4","isOffDay":true}]}"#, "invalid holiday date"),
            (br#"{"year":2026,"days":[{"date":"2026-01-04","isOffDay":true},{"date":"2026-01-04","isOffDay":false}]}"#, "duplicate"),
            (br#"{"year":1800,"days":[{"date":"1800-01-04","isOffDay":true}]}"#, "outside 1900"),
            (br#"{"$id":" x","year":2026,"days":[{"date":"2026-01-04","isOffDay":true}]}"#, "metadata $id"),
        ] {
            let err = calendar::parse_year(body, 0).unwrap_err();
            assert!(err.contains(reason), "{err}");
        }
        assert_eq!(calendar::wanted_years(date("2026-11-05")), vec![2026]);
        assert_eq!(calendar::wanted_years(date("2026-12-01")), vec![2026, 2027]);
    }

    #[tokio::test]
    async fn fetch_year_tries_mirrors_and_reports_failure() {
        let body = br#"{"year":2026,"days":[{"date":"2026-01-01","isOffDay":true}]}"#.to_vec();
        let calls = std::sync::Mutex::new(Vec::new());
        let got = calendar::fetch_year(2026, 7, |url| {
            calls.lock().unwrap().push(url.clone());
            let body = body.clone();
            async move {
                if url.contains("jsdelivr") {
                    Ok(body)
                } else {
                    Err("HTTP 503".into())
                }
            }
        })
        .await
        .unwrap();
        assert_eq!((got.year, got.fetched_at), (2026, 7));
        assert_eq!(calls.lock().unwrap().len(), 2);
        let err = calendar::fetch_year(2027, 7, |_| async { Err::<Vec<u8>, _>("down".to_string()) }).await.unwrap_err();
        assert_eq!(err.0, "2027 年日历获取失败；已有缓存继续使用，无缓存时请确认日期或开启加班");
    }

    #[test]
    fn schedule_parse_errors_match_reference() {
        assert_eq!(WorkSchedule::parse("8:00", "20:00").unwrap_err(), r#"work_start must be strict HH:MM, got "8:00""#);
        assert_eq!(WorkSchedule::parse("08:00", "24:00").unwrap_err(), r#"work_end must be strict HH:MM, got "24:00""#);
        assert_eq!(WorkSchedule::parse("08:00", "08:00").unwrap_err(), "work start and end must differ");
    }

    #[test]
    fn snapshot_status_day_and_overnight() {
        let c = calendar_2026();
        let r = Runtime::default();
        let s = day_schedule();
        let status = |t: &str, s: WorkSchedule| r.snapshot(at(t), s, &c).unwrap().status;
        assert_eq!(status("2026-11-05 09:00", s), Status::Working);
        assert_eq!(status("2026-11-05 21:00", s), Status::OffDuty);
        assert_eq!(status("2026-11-07 09:00", s), Status::OffDuty);
        assert_eq!(status("2026-10-01 09:00", s), Status::Holiday);
        assert_eq!(status("2025-05-06 09:00", s), Status::CalendarUnknown);
        let night = WorkSchedule::parse("22:00", "06:00").unwrap();
        let snap = r.snapshot(at("2026-11-06 03:00"), night, &c).unwrap();
        assert_eq!((snap.status, snap.business_date), (Status::Working, date("2026-11-05")));
        // Friday's shift runs into Saturday and still follows Friday's calendar.
        assert_eq!(status("2026-11-07 02:00", night), Status::Working);
        assert_eq!(status("2026-11-08 02:00", night), Status::OffDuty);
        assert_eq!(status("2026-11-06 12:00", night), Status::OffDuty);
    }

    #[test]
    fn overtime_lasts_its_business_day() {
        let c = calendar_2026();
        let s = day_schedule();
        let mut r = Runtime::default();
        r.begin_overtime(at("2026-11-07 10:00"), s).unwrap();
        r.begin_overtime(at("2026-11-07 11:00"), s).unwrap();
        let snap = r.observe(at("2026-11-07 12:00"), s, &c).unwrap();
        assert_eq!(snap.status, Status::Overtime);
        assert_eq!(snap.overtime_started, Some(at("2026-11-07 10:00")));
        assert_eq!(snap.end_eligibility, Eligibility::OvertimeBeforeEnd);
        let next = r.observe(at("2026-11-08 09:00"), s, &c).unwrap();
        assert_eq!(next.status, Status::OffDuty);
        assert_eq!(r.overtime, None);
        // Overtime started after the end boundary never earns the end claim.
        let mut late = Runtime::default();
        late.begin_overtime(at("2026-11-07 21:00"), s).unwrap();
        assert_eq!(late.end_eligibility(date("2026-11-07"), s).unwrap(), Eligibility::NotEligible);
    }

    #[test]
    fn tick_claims_once_and_never_catches_up() {
        let c = calendar_2026();
        let s = day_schedule();
        let mut r = Runtime::default();
        let (_, a) = r.tick(None, at("2026-11-05 07:59:30"), s, &c).unwrap();
        assert!(a.is_empty());
        let (_, a) = r.tick(Some(at("2026-11-05 07:59:30")), at("2026-11-05 08:00:30"), s, &c).unwrap();
        assert!(matches!(&a[..], [Action::MorningReport { claim, .. }] if claim.id == "morning-2026-11-05"));
        let (_, a) = r.tick(Some(at("2026-11-05 07:59:30")), at("2026-11-05 08:00:40"), s, &c).unwrap();
        assert!(a.is_empty(), "a boundary below the clock floor is not claimed again");
        let (_, a) = r.tick(Some(at("2026-11-05 19:59:30")), at("2026-11-05 20:00:30"), s, &c).unwrap();
        assert!(matches!(&a[..], [Action::EndArchive { claim, .. }] if claim.id == "end-2026-11-05"));
        assert_eq!(r.days[&date("2026-11-05")].end_eligibility, Some(Eligibility::WorkedBeforeEnd));

        // A gap over two minutes misses the boundary.
        let mut gap = Runtime::default();
        let (_, a) = gap.tick(Some(at("2026-11-05 07:57:00")), at("2026-11-05 08:00:30"), s, &c).unwrap();
        assert!(a.is_empty());
        // So does a schedule change.
        let mut changed = Runtime::default();
        changed.last_schedule = Some(WorkSchedule::parse("09:00", "18:00").unwrap());
        let (_, a) = changed.tick(Some(at("2026-11-05 07:59:30")), at("2026-11-05 08:00:30"), s, &c).unwrap();
        assert!(a.is_empty());
        // A weekend has neither a morning report nor an end archive.
        let mut weekend = Runtime::default();
        let (_, a) = weekend.tick(Some(at("2026-11-07 07:59:30")), at("2026-11-07 08:00:30"), s, &c).unwrap();
        assert!(a.is_empty());
        let (_, a) = weekend.tick(Some(at("2026-11-07 19:59:30")), at("2026-11-07 20:00:30"), s, &c).unwrap();
        assert!(a.is_empty());
        assert_eq!(weekend.days[&date("2026-11-07")].end_eligibility, Some(Eligibility::NotEligible));
    }

    fn seeded() -> Db {
        let db = Db::memory().unwrap();
        db.test_write_blocking(|tx| data::ai::save_holiday_year(tx, &year(2026, &[("2026-10-01", DayKind::Holiday)])))
            .unwrap();
        db
    }

    #[test]
    fn boundary_tick_queues_jobs_once_and_skips_no_op_writes() {
        let db = seeded();
        let config = Config::default();
        let tick = |previous: &str, now: &str| {
            let (config, p, n) = (config.clone(), at(previous), at(now));
            db.test_write_blocking(move |tx| data::ai::boundary_tick(tx, &config, Some(p), n)).unwrap()
        };
        assert_eq!(tick("2026-11-05 07:59:30", "2026-11-05 08:00:30"), 1);
        assert_eq!(tick("2026-11-05 07:59:30", "2026-11-05 08:00:40"), 0);
        let pending = |previous: &str, now: &str| {
            let (config, p, n) = (config.clone(), at(previous), at(now));
            db.test_read_blocking(move |c| data::ai::tick_pending(c, &config, Some(p), n)).unwrap()
        };
        assert!(!pending("2026-11-05 10:00:00", "2026-11-05 10:01:00"));
        assert!(pending("2026-11-05 19:59:30", "2026-11-05 20:00:30"));
        assert_eq!(tick("2026-11-05 19:59:30", "2026-11-05 20:00:30"), 1);
        assert_eq!(tick("2026-11-05 19:59:30", "2026-11-05 20:00:31"), 0);
        let jobs = db
            .test_read_blocking(|c| {
                let a: i64 =
                    c.query_row("SELECT count(*) FROM archive_attempts WHERE id = 'end-2026-11-05'", [], |r| r.get(0))?;
                let m: i64 =
                    c.query_row("SELECT count(*) FROM morning_jobs WHERE id = 'morning-2026-11-05'", [], |r| r.get(0))?;
                Ok((a, m))
            })
            .unwrap();
        assert_eq!(jobs, (1, 1));
    }

    #[test]
    fn gate_per_mode_and_claim_check() {
        let db = seeded();
        let config = Config::default();
        let c = config.clone();
        db.test_write_blocking(move |tx| {
            data::ai::boundary_tick(tx, &c, Some(at("2026-11-05 07:59:30")), at("2026-11-05 08:00:30"))?;
            data::ai::boundary_tick(tx, &c, Some(at("2026-11-05 19:59:30")), at("2026-11-05 20:00:30"))
        })
        .unwrap();
        let c = config.clone();
        db.test_read_blocking(move |conn| {
            let gate =
                |kind: &str, mode, attempt, t: &str| data::ai::ai_gate(conn, &c, kind, mode, attempt, at(t)).unwrap();
            assert!(gate("analysis", Mode::Automatic, None, "2026-11-05 09:00"));
            assert!(!gate("analysis", Mode::Automatic, None, "2026-11-05 21:00"));
            assert!(gate("analysis", Mode::Manual, None, "2026-11-05 21:00"));
            assert!(gate("analysis", Mode::Manual, None, "2025-05-06 21:00"));
            assert!(gate("archive", Mode::Nightly, Some("end-2026-11-05"), "2026-11-05 21:00"));
            assert!(!gate("archive", Mode::Nightly, Some("end-2026-11-04"), "2026-11-05 21:00"));
            assert!(
                !gate("archive", Mode::Nightly, Some("end-2026-11-05"), "2026-11-05 19:00"),
                "claim is later than now"
            );
            let mut attempt = crate::data::archive::attempt(conn, "end-2026-11-05")?.unwrap();
            assert_eq!(data::ai::claim_check(conn, &c, &attempt, at("2026-11-05 21:00")), Ok(()));
            attempt.business_day = "2026-11-04".into();
            assert_eq!(
                data::ai::claim_check(conn, &c, &attempt, at("2026-11-05 21:00")).unwrap_err(),
                "归档排队缺少匹配的持久化下班 claim；已停止自动执行，等待手动归档"
            );
            Ok(())
        })
        .unwrap();
        db.test_write_blocking(|tx| Ok(tx.execute("UPDATE archive_attempts SET status = 'failed'", [])?)).unwrap();
        let c = config.clone();
        let err = db
            .test_read_blocking(move |conn| {
                let attempt = crate::data::archive::attempt(conn, "end-2026-11-05")?.unwrap();
                Ok(data::ai::claim_check(conn, &c, &attempt, at("2026-11-05 21:00")).unwrap_err())
            })
            .unwrap();
        assert_eq!(err, "下班归档授权已失效；已停止自动执行，等待手动归档");
    }

    #[test]
    fn set_state_validates_and_applies() {
        let db = seeded();
        let config = Config::default();
        let set = |p: serde_json::Value, t: &str| {
            let (c, n) = (config.clone(), at(t));
            db.test_write_blocking(move |tx| data::ai::set_work_state(tx, &c, &p, n))
        };
        let status = |t: &str| {
            let (c, n) = (config.clone(), at(t));
            db.test_read_blocking(move |conn| Ok(data::ai::work_snapshot(conn, &c, n)?.unwrap().status)).unwrap()
        };
        for (p, err) in [
            (json!({}), "缺少日期"),
            (json!({"date": "2026/11/05", "kind": "holiday"}), "日期格式应为 YYYY-MM-DD"),
            (json!({"date": "2026-11-05", "kind": "weekend"}), "请选择工作日或假期"),
        ] {
            assert_eq!(set(p, "2026-11-05 09:00").unwrap_err().0, err);
        }
        assert_eq!(status("2026-11-05 09:00"), Status::Working);
        set(json!({"date": "2026-11-05", "kind": "holiday"}), "2026-11-05 09:00").unwrap();
        assert_eq!(status("2026-11-05 09:00"), Status::Holiday);
        set(json!({"overtime": true}), "2026-11-07 10:00").unwrap();
        assert_eq!(status("2026-11-07 10:30"), Status::Overtime);
        set(json!({"overtime": false}), "2026-11-07 11:00").unwrap();
        assert_eq!(status("2026-11-07 11:30"), Status::OffDuty);
        let mut bad = config.clone();
        bad.work_start = "8点".into();
        let err = db
            .test_write_blocking(move |tx| data::ai::set_work_state(tx, &bad, &json!({"overtime": true}), 0))
            .unwrap_err();
        assert!(err.0.contains("work_start must be strict HH:MM"));
    }
}

mod traces {
    use super::super::prompt::{Prepared, Route};
    use super::super::trace::{self, Trace};
    use crate::config::Connection as AiConnection;
    use crate::Db;
    use serde_json::json;

    fn route(model: &str) -> Route {
        Route {
            kind: "classify".into(),
            connection: AiConnection::default(),
            model: model.into(),
            effort: "low".into(),
        }
    }

    fn prepared(prompt: &str) -> Prepared {
        Prepared {
            prompt: prompt.into(),
            schema: json!({"type": "object"}),
            tools: None,
            definitions: vec![],
            prompt_chars: 0,
            schema_chars: 0,
            tool_chars: 0,
            wire_chars: 0,
            total: 0,
        }
    }

    fn id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn saved(db: &Db, at: i64, model: &str, prompt: &str) -> (String, Trace) {
        let (request_id, trace) = (id(), Trace::new(at));
        trace::request(&trace, &route(model), &prepared(prompt));
        trace::save(db, &request_id, &trace).await.unwrap();
        (request_id, trace)
    }

    #[tokio::test]
    async fn read_reports_reference_errors() {
        let db = Db::memory().unwrap();
        let e = |p: serde_json::Value| {
            let db = db.clone();
            async move { trace::read(&db, true, &p).await.unwrap_err().0 }
        };
        assert_eq!(e(json!({})).await, "缺少调用编号");
        assert_eq!(e(json!({"id": "x"})).await, "无效的调用编号");
        assert_eq!(e(json!({"id": id()})).await, "这次调用没有保存详细输入输出（旧版记录）");
        assert_eq!(
            trace::read(&db, false, &json!({"id": id()})).await.unwrap_err().0,
            "完整跟踪已关闭，本次未保存输入输出"
        );
    }

    #[tokio::test]
    async fn saved_trace_reads_back_and_compares_with_previous_same_identity() {
        let db = Db::memory().unwrap();
        let (old, _) = saved(&db, 100, "m", "old").await;
        let (other, _) = saved(&db, 150, "x", "other").await;
        let (current, trace) = saved(&db, 200, "m", "new").await;
        // A later request of the same identity is never "previous".
        saved(&db, 300, "m", "later").await;
        trace.put("output.txt", "{\"ok\":true}");
        trace.event(&json!({"type": "phase", "text": "a"}));
        trace.tool_failure(&json!({"bad": 1}));
        trace.round(1).put("input.txt", "round one");
        trace.put("error.txt", "boom");
        trace::save(&db, &current, &trace).await.unwrap();
        let full = trace::read(&db, true, &json!({"id": current})).await.unwrap();
        assert_eq!(full["input"], "new");
        assert_eq!(full["output"], "{\"ok\":true}");
        assert_eq!(full["schema"], "{\"type\":\"object\"}");
        assert_eq!(full["error"], "boom");
        assert_eq!(full["events"], "{\"bad\":1}\n\n{\"text\":\"a\",\"type\":\"phase\"}\n");
        let rows = db.ai_trace(current.clone()).await.unwrap();
        assert!(rows.contains(&("round-0001/input.txt".into(), "round one".into())));
        let prev = trace::read(&db, true, &json!({"id": current, "previous": true})).await.unwrap();
        assert_eq!(prev["previous_input"], "old");
        assert_eq!(prev["previous_request_id"], old);
        let none = trace::read(&db, true, &json!({"id": other, "previous": true})).await.unwrap_err().0;
        assert_eq!(none, "没有更早的已保留请求可供比较");
        let gone = current.clone();
        db.test_write(move |tx| {
            Ok(tx.execute("DELETE FROM ai_traces WHERE request_id = ?1 AND name = 'request.json'", [gone])?)
        })
        .await
        .unwrap();
        let e = trace::read(&db, true, &json!({"id": current, "previous": true})).await.unwrap_err().0;
        assert_eq!(e, "当前记录缺少同类请求标识，请使用更新后的调用记录比较");
    }

    #[tokio::test]
    async fn saves_write_only_changes_and_events_are_bounded() {
        let db = Db::memory().unwrap();
        let (request_id, trace) = saved(&db, 1, "m", "p").await;
        trace::save(&db, &request_id, &trace).await.unwrap();
        trace.append("events.jsonl", &"x".repeat(trace::EVENTS_LIMIT), Some(trace::EVENTS_LIMIT));
        trace.event(&json!(1));
        assert_eq!(trace.get("events.jsonl").unwrap().len(), trace::EVENTS_LIMIT + 1);
        trace::save(&db, &request_id, &trace).await.unwrap();
        assert_eq!(db.ai_trace(request_id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn prune_removes_requests_started_before_the_cutoff() {
        let db = Db::memory().unwrap();
        let (old, _) = saved(&db, 100, "m", "old").await;
        let (new, _) = saved(&db, 200, "m", "new").await;
        assert_eq!(db.prune_ai_traces(150).await.unwrap(), 3);
        assert!(db.ai_trace(old).await.unwrap().is_empty());
        assert_eq!(db.ai_trace(new).await.unwrap().len(), 3);
    }
}

mod invoke_pipeline {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use crate::ai::invoke::{self, Stage};
    use crate::ai::provider::fake::{Fake, Step};
    use crate::ai::trace;
    use crate::ai::{Mode, CANCELLED};
    use crate::config::Connection as AiConnection;
    use crate::{data, Config, Ctx};

    fn ctx(max_input_chars: usize) -> Arc<Ctx> {
        let mut config = Config::default();
        config.ai_connections =
            vec![AiConnection { id: "c1".into(), name: "测试".into(), driver: "api".into(), ..Default::default() }];
        config.max_input_chars = max_input_chars;
        let ctx = Ctx::memory(config).unwrap();
        // The API key is resolved before the (fake) driver is called.
        let db = ctx.db.clone();
        std::thread::spawn(move || db.set_credential_blocking("c1".into(), "synthetic-key".into()))
            .join()
            .unwrap()
            .unwrap();
        ctx
    }

    async fn logs(ctx: &Ctx) -> Vec<Value> {
        let mut rows = ctx.db.test_read(|c| crate::data::activity::recent(c, 50)).await.unwrap();
        rows.reverse();
        rows
    }

    async fn kinds(ctx: &Ctx) -> Vec<String> {
        logs(ctx).await.iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect()
    }

    async fn calls(ctx: &Ctx) -> u64 {
        ctx.db.test_read(|c| data::ai::usage_today(c, "main")).await.unwrap().calls
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_reservation_without_changing_analysis_state() {
        let ctx = ctx(10);
        let fake = Fake::reply(json!({}));
        ctx.ai.set_driver(Some(fake.clone()));
        let before = ctx.db.test_read(data::ai::analysis_state).await.unwrap();
        let call = ctx.exec.try_call("test").unwrap();
        let request = invoke::build(&ctx, "classify", Mode::Automatic, "测试", json!({"changes": []})).await.unwrap();
        let error = invoke::invoke(&ctx, request, &call.cancel).await.err().unwrap();
        assert!(error.0.starts_with(crate::ai::INPUT_LIMIT_ERROR), "{}", error.0);
        assert_eq!(fake.calls(), 0);
        assert_eq!(calls(&ctx).await, 0);
        assert_eq!(kinds(&ctx).await, ["ai_rejected"]);
        assert_eq!(ctx.db.test_read(data::ai::analysis_state).await.unwrap(), before);
    }

    #[tokio::test]
    async fn tool_declarations_are_parsed_once_and_rejected_explicitly() {
        let ctx = ctx(100_000);
        let tool = |name: &str| json!({"name": name, "description": "合成工具", "parameters": {"type": "object", "properties": {}}});
        let ok = invoke::build(&ctx, "classify", Mode::Manual, "", json!({"_registered_tools": [tool("lookup")]}))
            .await
            .unwrap();
        assert_eq!(ok.prepared.definitions.len(), 1);
        assert_eq!(ok.prepared.definitions[0].name, "lookup");
        let bad = json!({"_registered_tools": [{"name": "lookup", "description": "x", "parameters": 1}]});
        let e = invoke::build(&ctx, "classify", Mode::Manual, "", bad).await.err().unwrap();
        assert!(e.0.starts_with("工具声明无效："), "{}", e.0);
        let many: Vec<Value> = (0..=shishi_api::openai::MAX_TOOLS).map(|i| tool(&format!("t{i}"))).collect();
        let e =
            invoke::build(&ctx, "classify", Mode::Manual, "", json!({"_registered_tools": many})).await.err().unwrap();
        assert_eq!(e.0, "本次可用工具过多，请关闭部分工具后重试");
    }

    #[tokio::test]
    async fn unconfigured_purpose_is_logged_and_never_sent() {
        let mut config = Config::default();
        config.model_profiles.as_mut().unwrap().review = None;
        let ctx = Ctx::memory(config).unwrap();
        let error = invoke::build(&ctx, "archive", Mode::Manual, "", json!({})).await.err().unwrap();
        assert_eq!(error.0, "AI_UNCONFIGURED:归档分析未配置 AI 接入或模型，本次任务未执行");
        let row = &logs(&ctx).await[0];
        assert_eq!(row["kind"], "error");
        assert_eq!(row["detail"]["failure_class"], "unconfigured");
        assert_eq!(row["detail"]["message"], "归档分析未配置 AI 接入或模型，本次任务未执行");
    }

    #[tokio::test]
    async fn accepted_reply_counts_usage_and_logs_start_and_success() {
        let ctx = ctx(400_000);
        let fake = Fake::reply(json!({"report": {"summary": "完成"}}));
        ctx.ai.set_driver(Some(fake.clone()));
        let call = ctx.exec.try_call("test").unwrap();
        let request = invoke::build(&ctx, "archive", Mode::Manual, "手动归档", json!({"changes": []})).await.unwrap();
        let value = invoke::call(&ctx, request, &call.cancel).await.unwrap();
        assert_eq!(value["report"]["summary"], "完成");
        assert_eq!(fake.calls(), 1);
        let main = ctx.db.test_read(|c| data::ai::usage_today(c, "main")).await.unwrap();
        assert_eq!((main.calls, main.unreported_calls, main.input_tokens, main.output_tokens), (1, 0, 10, 5));
        assert_eq!(kinds(&ctx).await, ["ai_start", "ai_success"]);
        let success = &logs(&ctx).await[1]["detail"];
        assert_eq!(success["request_id"], call.id());
        assert_eq!(success["thread_id"], "fake-thread");
        assert_eq!(success["usage_status"], "reported");
        assert_eq!(ctx.exec.ai_status().unwrap().live.phase, "分析完成");
    }

    #[tokio::test]
    async fn traces_are_saved_only_when_retained() {
        let ctx = ctx(400_000);
        ctx.ai.set_driver(Some(Fake::reply(json!({"report": {"summary": "完成"}}))));
        let call = ctx.exec.try_call("test").unwrap();
        let request = invoke::build(&ctx, "archive", Mode::Manual, "手动归档", json!({"changes": []})).await.unwrap();
        invoke::call(&ctx, request, &call.cancel).await.unwrap();
        assert!(ctx.db.ai_trace(call.id().into()).await.unwrap().is_empty());
        drop(call);

        ctx.config
            .update(&ctx.db, |c| {
                c.retain_ai_traces = true;
                Ok(())
            })
            .await
            .unwrap();
        ctx.ai.set_driver(Some(Fake::new(vec![
            Step::Reply(json!({"report": {"summary": "完成"}}), json!({})),
            Step::Fail("API 分析失败：HTTP 504".into()),
        ])));
        for failed in [false, true] {
            let call = ctx.exec.try_call("test").unwrap();
            let request =
                invoke::build(&ctx, "archive", Mode::Manual, "手动归档", json!({"changes": []})).await.unwrap();
            assert_eq!(invoke::call(&ctx, request, &call.cancel).await.is_err(), failed);
            let saved = trace::read(&ctx.db, true, &json!({"id": call.id()})).await.unwrap();
            assert!(!saved["input"].as_str().unwrap().is_empty());
            assert_eq!(saved["output"].as_str().unwrap().contains("完成"), !failed);
            assert_eq!(saved["error"], if failed { "API 分析失败：HTTP 504" } else { "" });
            assert!(saved["events"].as_str().unwrap().contains("phase"));
        }
    }

    #[tokio::test]
    async fn a_missing_api_key_fails_as_a_provider_error_without_sending() {
        let ctx = ctx(400_000);
        assert!(ctx.db.delete_credential("c1".into()).await.unwrap());
        let fake = Fake::reply(json!({"report": {"summary": "完成"}}));
        ctx.ai.set_driver(Some(fake.clone()));
        let call = ctx.exec.try_call("test").unwrap();
        let request = invoke::build(&ctx, "archive", Mode::Manual, "手动归档", json!({"changes": []})).await.unwrap();
        assert_eq!(invoke::call(&ctx, request, &call.cancel).await.unwrap_err().0, "未配置此连接的 API Key");
        assert_eq!(fake.calls(), 0);
        let rows = logs(&ctx).await;
        let failed = rows.iter().find(|r| r["kind"] == "ai_failed").unwrap();
        assert_eq!(failed["detail"]["failure_stage"], "provider");
    }

    #[tokio::test]
    async fn provider_and_finalization_failures_carry_their_stage() {
        let ctx = ctx(400_000);
        ctx.ai.set_driver(Some(Fake::new(vec![
            Step::Fail("API 分析失败：HTTP 504".into()),
            Step::Invalid("模型未按格式返回".into()),
            Step::Reply(json!({"x": 1}), json!({})),
        ])));
        let call = ctx.exec.try_call("test").unwrap();
        for _ in 0..2 {
            let request = invoke::build(&ctx, "edit", Mode::Manual, "", json!({})).await.unwrap();
            assert!(invoke::invoke(&ctx, request, &call.cancel).await.is_err());
        }
        let request = invoke::build(&ctx, "edit", Mode::Manual, "", json!({})).await.unwrap();
        let outcome = invoke::invoke(&ctx, request, &call.cancel).await.ok().unwrap();
        let error =
            outcome.reject(&ctx, crate::Error::new("AI 返回未知证据短编号，本次结果未应用"), Stage::Validation).await;
        assert_eq!(error.0, crate::ai::UNKNOWN_EVIDENCE);
        let rows = logs(&ctx).await;
        let failed: Vec<_> = rows.iter().filter(|r| r["kind"] == "ai_failed").map(|r| &r["detail"]).collect();
        assert_eq!(failed.len(), 3);
        assert_eq!(
            (failed[0]["failure_stage"].as_str(), failed[0]["failure_class"].as_str()),
            (Some("provider"), Some("upstream_timeout"))
        );
        assert_eq!(failed[1]["failure_class"], "response_validation");
        assert_eq!(failed[2]["failure_stage"], "response_validation");
        assert_eq!(failed[2]["usage_status"], "provider_did_not_report_complete_usage");
        assert_eq!(calls(&ctx).await, 3);
        // One-sentence edits report in their own dialog, not as the analysis error.
        assert_eq!(ctx.db.test_read(data::ai::analysis_state).await.unwrap().last_error, "");
    }

    #[tokio::test]
    async fn interrupt_ends_a_hanging_request_as_cancelled() {
        let ctx = ctx(400_000);
        ctx.ai.set_driver(Some(Fake::new(vec![Step::Hang])));
        let call = ctx.exec.try_call("test").unwrap();
        let request = invoke::build(&ctx, "edit", Mode::Manual, "", json!({})).await.unwrap();
        let interrupt = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                ctx.exec.interrupt(None)
            })
        };
        let error = invoke::invoke(&ctx, request, &call.cancel).await.err().unwrap();
        assert!(interrupt.await.unwrap());
        assert_eq!(error.0, CANCELLED);
        assert_eq!(kinds(&ctx).await, ["ai_start", "ai_cancelled"]);
        assert_eq!(ctx.db.test_read(data::ai::analysis_state).await.unwrap().last_error, "");
    }

    #[tokio::test]
    async fn archive_layer_picks_the_mode_and_sends_the_measured_render() {
        use crate::archive::layer as model;
        use crate::archive::Prepared;
        assert_eq!(model::mode("report", &json!({"_trigger": "manual_report"})), Mode::Manual);
        assert_eq!(model::mode("report", &json!({"_trigger": "scheduled_report"})), Mode::Automatic);
        assert_eq!(model::mode("archive", &json!({"_trigger": "work_end"})), Mode::Nightly);
        assert_eq!(model::mode("archive", &json!({"_trigger": "manual_archive"})), Mode::Manual);

        let ctx = ctx(400_000);
        let layer = model::model(&ctx).await.unwrap();
        let request = Prepared { purpose: "report", payload: json!({"_trigger": "manual_report", "tasks": []}) };
        let measured = layer.measure(request).unwrap();
        // The rule text alone exceeds the payload size.
        assert!(measured.total > measured.request.payload.to_string().chars().count());
        assert!(measured.total <= 400_000);
        // The measured render is what is sent: a later edit does not re-render it.
        let before = ctx.config.get().model_choice("report").model;
        ctx.config.update(&ctx.db, |c| Ok(c.model = "改后的模型".into())).await.unwrap();

        let fake = Fake::reply(json!({"report": {"summary": "完成"}}));
        ctx.ai.set_driver(Some(fake.clone()));
        let value = ctx
            .exec
            .run_ai("report", std::time::Duration::from_secs(5), |cancel| layer.invoke(measured, cancel))
            .await
            .unwrap();
        assert_eq!(value["report"]["summary"], "完成");
        let start = &logs(&ctx).await[0];
        assert_eq!(
            (start["kind"].as_str(), start["detail"]["trigger"].as_str()),
            (Some("ai_start"), Some("manual_report"))
        );
        assert_eq!(start["detail"]["model"].as_str(), Some(before.as_str()));
        assert_ne!(before, "改后的模型");
    }
}

/// One-sentence edits run beside the internal AI slot.
mod concurrent_edits {
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::sync::Notify;

    use crate::ai::provider::{Context, Done, Driver, Event, Job, Reply, Sending};
    use crate::ai::{BUSY, CANCELLED};
    use crate::{data, Config, Ctx, Runtime};

    /// An async driver: each request waits, holding no thread, until the
    /// test opens the gate whose needle its prompt contains, or until it is
    /// cancelled.
    #[derive(Default)]
    struct Gated {
        gates: Mutex<Vec<(String, Arc<Notify>, Result<Value, String>)>>,
        entered: Mutex<Option<Sender<String>>>,
    }

    impl Gated {
        fn gate(&self, needle: &str, outcome: Result<Value, String>) -> Arc<Notify> {
            let open = Arc::new(Notify::new());
            self.gates.lock().unwrap().push((needle.into(), open.clone(), outcome));
            open
        }
    }

    impl Driver for Gated {
        fn send(self: Arc<Self>, mut job: Job) -> Sending {
            Box::pin(async move {
                let found = {
                    let mut gates = self.gates.lock().unwrap();
                    let at = gates.iter().position(|(needle, ..)| job.prepared.prompt.contains(needle.as_str()));
                    at.map(|at| gates.remove(at))
                };
                let (needle, open, outcome) = found.expect("no gate for the request");
                (job.events)(Event::SessionStarted(format!("gated-{needle}")));
                (job.events)(Event::Stream { kind: "phase".into(), text: format!("等待{needle}") });
                self.entered.lock().unwrap().as_ref().unwrap().send(needle).unwrap();
                let reply: Reply = tokio::select! {
                    _ = open.notified() => outcome.map(|v| (v, json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}))),
                    _ = job.cancel.cancelled() => Err(CANCELLED.into()),
                };
                Done { reply, session: job.session }
            })
        }

        fn analyze(&self, _: &Job, _: Option<&mut dyn Context>, _: &mut dyn FnMut(Event)) -> Reply {
            unreachable!("Gated answers by send")
        }
    }

    fn start(rt: &Runtime, method: &str, params: Value) -> Receiver<crate::Result<Value>> {
        let (tx, rx) = channel();
        rt.request(method.into(), params, move |r| tx.send(r).unwrap());
        rx
    }

    fn wait(rx: &Receiver<crate::Result<Value>>) -> crate::Result<Value> {
        rx.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    fn edit(title: &str) -> Value {
        json!({"changes": [{"field": "title", "value": title}], "tag_changes": [], "tag_suggestions": []})
    }

    /// `(threads, rss_kb)` of this process, from `ps`; Windows has no `ps`
    /// and the figures are only printed, so it samples nothing there.
    #[cfg(windows)]
    fn measure() -> Option<(usize, u64)> {
        None
    }

    #[cfg(not(windows))]
    fn measure() -> Option<(usize, u64)> {
        let pid = std::process::id().to_string();
        let flag = if cfg!(target_os = "macos") { "-M" } else { "-L" };
        let threads = std::process::Command::new("ps").args([flag, "-p", &pid]).output().unwrap().stdout;
        let rss = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().unwrap().stdout;
        let threads = String::from_utf8_lossy(&threads).lines().count() - 1;
        Some((threads, String::from_utf8_lossy(&rss).trim().parse().unwrap()))
    }

    #[test]
    fn edits_overlap_the_internal_call_cancel_alone_and_skip_stale_results() {
        let ctx = Ctx::memory(Config::default()).unwrap();
        let rt = Runtime::start(ctx.clone(), false).unwrap();
        let driver = Arc::new(Gated::default());
        let (entered, arrivals) = channel();
        *driver.entered.lock().unwrap() = Some(entered);
        ctx.ai.set_driver(Some(driver.clone()));
        let task = |title: &str| rt.call("edit", json!({"task": {"title": title, "status": "待处理"}})).unwrap();
        let tasks: Vec<Value> = ["甲事项", "乙事项", "丙事项"].into_iter().map(task).collect();
        let analysis = || ctx.db.test_read_blocking(data::ai::analysis_state).unwrap();
        ctx.db
            .test_write_blocking(|tx| {
                Ok(tx
                    .execute("UPDATE analysis_state SET pending = 1, pending_since = 100, last_analysis_at = 90", [])?)
            })
            .unwrap();
        let before = analysis();
        let baseline = measure();

        let schedule = driver
            .gate("每天九点写日报", Ok(json!({"schedule": {"title": "日报", "frequency": "daily", "time": "09:00"}})));
        let open: Vec<Arc<Notify>> = vec![
            driver.gate("甲事项", Ok(edit("甲改"))),
            driver.gate("乙事项", Ok(edit("乙改"))),
            driver.gate("丙事项", Err("API 分析失败：HTTP 504".into())),
        ];
        let internal = start(&rt, "schedule_format", json!({"text": "每天九点写日报"}));
        let edits: Vec<_> = tasks.iter().map(|t| start(&rt, "edit_ai", json!({"task": t, "text": "改标题"}))).collect();
        for _ in 0..4 {
            arrivals.recv_timeout(Duration::from_secs(10)).unwrap();
        }

        // One internal call and three edits are in flight together; the
        // slot still admits only one internal call.
        let overlap = measure();
        eprintln!("T02 overlap (threads, rss_kb): baseline {baseline:?}; 1 internal + 3 edits {overlap:?}");
        assert_eq!(ctx.exec.ai_status().unwrap().source, "schedule");
        let running: HashMap<String, String> =
            ctx.exec.manual_status().into_iter().map(|s| (s.subject, s.request_id)).collect();
        let ids: Vec<String> = tasks.iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(running.len(), 3);
        assert!(ids.iter().all(|id| running.contains_key(id)));
        assert_eq!(rt.call("schedule_format", json!({"text": "每天十点写周报"})).unwrap_err().0, BUSY);
        let state = rt.call("state", json!({})).unwrap();
        let shown = state["ai_edits"].as_array().unwrap();
        assert_eq!(shown.len(), 3, "{state}");
        assert!(shown.iter().all(|e| e["progress"].as_str().unwrap().starts_with("等待")), "{state}");

        // Stopping one edit leaves the others and the internal call running.
        let stopped = rt.call("ai_interrupt", json!({"request_id": running[&ids[1]]})).unwrap();
        assert_eq!(stopped["interrupted"], true);
        assert_eq!(wait(&edits[1]).unwrap_err().0, CANCELLED);
        assert_eq!(ctx.exec.manual_status().len(), 2);
        assert!(ctx.exec.ai_busy());
        assert_eq!(ctx.exec.last_interrupt(), None);

        // A failed edit releases only itself.
        open[2].notify_one();
        assert!(wait(&edits[2]).unwrap_err().0.contains("HTTP 504"));
        assert_eq!(ctx.exec.manual_status().len(), 1);

        open[0].notify_one();
        assert_eq!(wait(&edits[0]).unwrap()["title"], "甲改");
        schedule.notify_one();
        assert_eq!(wait(&internal).unwrap()["title"], "日报");
        assert!(ctx.exec.manual_status().is_empty() && !ctx.exec.ai_busy());
        let titles: Vec<Value> =
            ids.iter().map(|id| rt.call("task_get", json!({"id": id})).unwrap()["title"].clone()).collect();
        assert_eq!(titles, [json!("甲改"), json!("乙事项"), json!("丙事项")]);

        // A manual change while the edit waits wins; the result is skipped.
        let current = rt.call("task_get", json!({"id": ids[0]})).unwrap();
        let late = driver.gate("甲改", Ok(edit("迟到的结果")));
        let pending = start(&rt, "edit_ai", json!({"task": current, "text": "再改标题"}));
        arrivals.recv_timeout(Duration::from_secs(10)).unwrap();
        rt.call("edit", json!({"task": {"id": ids[0], "updated": current["updated"], "title": "手动改"}})).unwrap();
        late.notify_one();
        assert_eq!(wait(&pending).unwrap_err().0, "分析期间事项已有新修改，本次结果未覆盖。请重新打开编辑后再试");
        assert_eq!(rt.call("task_get", json!({"id": ids[0]})).unwrap()["title"], "手动改");
        assert!(ctx.exec.manual_status().is_empty());

        // Edits never move the analysis trigger, and a failed edit is
        // answered in its dialog, not as the analysis error.
        let after = analysis();
        assert_eq!((after.pending, after.pending_since, after.last_analysis_at), (true, 100, 90));
        assert_eq!((before.last_error.as_str(), after.last_error.as_str()), ("", ""));
    }
}
