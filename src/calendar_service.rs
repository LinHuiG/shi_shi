use crate::{
    engine::App, holiday::*, model::*, work_hours::AiMode, work_scheduler::*, work_state::*,
};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::Path, sync::atomic::Ordering, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MorningJob {
    pub id: String,
    pub business_day: String,
    pub status: String,
    pub error: String,
    #[serde(default)]
    pub finished: i64,
}

fn cache_dir(root: &Path) -> Result<std::path::PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let dir = crate::storage::data_dir(&root).join("holidays");
    for path in [crate::storage::data_dir(&root), dir.clone()] {
        if let Ok(meta) = fs::symlink_metadata(&path) {
            use std::os::windows::fs::MetadataExt;
            if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
                return Err("日历缓存目录不能是链接或重解析点".into());
            }
        }
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if !dir
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(root)
    {
        return Err("日历缓存目录越界".into());
    }
    Ok(dir)
}

pub fn load_calendar(root: &Path, at: i64) -> CalendarData {
    let mut result = CalendarData::new();
    let Ok(dir) = cache_dir(root) else {
        return result;
    };
    let Ok(date) = shanghai_date(at) else {
        return result;
    };
    for year in date.year() - 1..=date.year() + 1 {
        let path = dir.join(format!("{year}.json"));
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        use std::os::windows::fs::MetadataExt;
        if !meta.is_file() || meta.file_attributes() & 0x400 != 0 || meta.len() > 512 * 1024 {
            continue;
        }
        if let Ok(bytes) = fs::read(path) {
            if let Ok(cache) = HolidayCache::from_bytes(&bytes) {
                if cache.year == year {
                    let _ = result.insert_cache(cache);
                }
            }
        }
    }
    result
}

impl App {
    pub(crate) fn calendar_for(&self, s: &State) -> CalendarData {
        let mut calendar = self.calendar.lock().unwrap().clone();
        for value in &s.calendar_overrides {
            let _ = calendar.set_manual_override(value.clone());
        }
        calendar
    }

    pub(crate) fn work_snapshot(&self, s: &State, at: i64) -> Result<WorkSnapshot, String> {
        let schedule = WorkSchedule::parse(&s.config.work_start, &s.config.work_end)
            .map_err(|e| e.to_string())?;
        s.work_scheduler
            .runtime
            .snapshot(at, &schedule, &self.calendar_for(s))
            .map_err(|e| e.to_string())
    }

    pub(crate) fn work_gate(
        &self,
        s: &State,
        kind: &str,
        mode: AiMode,
        attempt: Option<&str>,
        at: i64,
    ) -> bool {
        let Ok(snapshot) = self.work_snapshot(s, at) else {
            return false;
        };
        // Manual attempts are accounting links, not boundary-claim replays.
        if mode == AiMode::Manual {
            return true;
        }
        if let (AiMode::Nightly, "archive", Some(id)) = (mode, kind, attempt) {
            // Check committed authority even while working: a terminal attempt
            // must never dispatch again through an otherwise open time gate.
            return s
                .work_scheduler
                .runtime
                .find_end_claim(id)
                .is_some_and(|(_, claim)| {
                    claim.eligibility.is_eligible() && claim.claimed_at_ms <= at
                })
                && s.archive_attempts
                    .iter()
                    .find(|a| a.id == id)
                    .is_some_and(|a| matches!(a.status.as_str(), "queued" | "running"));
        }
        snapshot.status.ai_allowed()
    }

    pub fn set_work_state(&self, params: &Value) -> Result<Value, String> {
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        let schedule = WorkSchedule::parse(&next.config.work_start, &next.config.work_end)
            .map_err(|e| e.to_string())?;
        if let Some(enabled) = params["overtime"].as_bool() {
            if enabled {
                next.work_scheduler
                    .runtime
                    .begin_overtime(now(), &schedule)
                    .map_err(|e| e.to_string())?;
            } else {
                next.work_scheduler.runtime.end_overtime();
            }
        } else {
            let date =
                NaiveDate::parse_from_str(params["date"].as_str().ok_or("缺少日期")?, "%Y-%m-%d")
                    .map_err(|_| "日期格式应为 YYYY-MM-DD")?;
            let kind = match params["kind"].as_str() {
                Some("workday") => DayKind::Workday,
                Some("holiday") => DayKind::Holiday,
                _ => return Err("请选择工作日或假期".into()),
            };
            let value = ManualOverride::new(date, kind, now(), None).map_err(|e| e.to_string())?;
            next.calendar_overrides.retain(|v| v.date != date);
            next.calendar_overrides.push(value);
        }
        next.work_scheduler
            .runtime
            .observe(now(), &schedule, &self.calendar_for(&next))
            .map_err(|e| e.to_string())?;
        self.save(&next)?;
        *locked = next;
        Ok(json!(true))
    }

    /// No network/model work here. Called by an independent short-lived tick.
    pub(crate) fn work_boundary_tick(&self, previous: Option<i64>, at: i64) -> Result<(), String> {
        let mut locked = self.state.lock().unwrap();
        let schedule = WorkSchedule::parse(&locked.config.work_start, &locked.config.work_end)
            .map_err(|e| e.to_string())?;
        let calendar = self.calendar_for(&locked);
        let (scheduler, outcome) = locked
            .work_scheduler
            .tick_preview(TickInput {
                previous_tick_at_ms: previous,
                now_ms: at,
                schedule: &schedule,
                calendar: &calendar,
            })
            .map_err(|e| e.to_string())?;
        // Persist only meaningful observations/claims, not the 5-second clock.
        if outcome.actions.is_empty()
            && scheduler.runtime == locked.work_scheduler.runtime
            && scheduler.last_schedule == locked.work_scheduler.last_schedule
        {
            locked.work_scheduler = scheduler;
            return Ok(());
        }
        let mut next = locked.clone();
        next.work_scheduler = scheduler;
        for action in outcome.actions {
            match action {
                SchedulerAction::EndArchive {
                    business_day,
                    claim,
                    ..
                } => {
                    if !next
                        .archive_attempts
                        .iter()
                        .any(|a| a.id == claim.attempt_id)
                    {
                        next.archive_attempts.push(ArchiveAttempt {
                            id: claim.attempt_id,
                            trigger: "work_end".into(),
                            status: "queued".into(),
                            business_day: business_day.to_string(),
                            created: at,
                            ..Default::default()
                        });
                    }
                }
                SchedulerAction::MorningReport {
                    business_day,
                    claim,
                } => {
                    next.morning_jobs.push(MorningJob {
                        id: claim.claim_id,
                        business_day: business_day.to_string(),
                        status: "queued".into(),
                        error: String::new(),
                        finished: 0,
                    });
                }
            }
        }
        self.save(&next)?;
        *locked = next;
        Ok(())
    }

    pub(crate) fn morning_tick(&self) -> Result<(), String> {
        if self.ai_busy.load(Ordering::SeqCst) {
            return Ok(());
        }
        let job = {
            let mut locked = self.state.lock().unwrap();
            let Some(index) = locked
                .morning_jobs
                .iter()
                .position(|j| j.status == "queued")
            else {
                return Ok(());
            };
            let mut next = locked.clone();
            next.morning_jobs[index].status = "running".into();
            let job = next.morning_jobs[index].clone();
            self.save(&next)?;
            *locked = next;
            job
        };
        let result = self.report_result("早报", AiMode::Automatic);
        let (status, error) = match &result {
            Ok(report) => match report["summary_status"].as_str() {
                Some("done") | Some("reused") | Some("fallback") => {
                    ("succeeded".to_string(), String::new())
                }
                _ => (
                    "failed".to_string(),
                    report["summary_error"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("早报已生成，但汇总未完成；不会自动重试")
                        .to_string(),
                ),
            },
            Err(error) => ("failed".to_string(), error.clone()),
        };
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        let finished = result
            .as_ref()
            .ok()
            .and_then(|report| report["finished"].as_i64())
            .filter(|time| *time > 0)
            .unwrap_or_else(|| next.next_report_finished(now()));
        if let Some(row) = next.morning_jobs.iter_mut().find(|j| j.id == job.id) {
            row.status = status;
            row.error = error;
            row.finished = finished;
        }
        self.save(&next)?;
        *locked = next;
        Ok(())
    }

    /// Runs in one background thread, never on the boundary observer or under State lock.
    #[cfg(test)]
    pub(crate) fn test_end_overtime(&self) {
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        next.work_scheduler.runtime.end_overtime();
        self.save(&next).unwrap();
        *locked = next;
    }

    pub(crate) fn refresh_calendar(&self) -> Result<(), String> {
        let at = now();
        let date = shanghai_date(at).map_err(|e| e.to_string())?;
        let years = if date.month() == 12 {
            vec![date.year(), date.year() + 1]
        } else {
            vec![date.year()]
        };
        let dir = cache_dir(&self.root)?;
        for year in years {
            if self
                .calendar
                .lock()
                .unwrap()
                .cache(year)
                .is_some_and(|c| !c.is_stale(at))
            {
                continue;
            }
            let mut received = None;
            for base in [
                "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master",
                "https://cdn.jsdelivr.net/gh/NateScarlet/holiday-cn@master",
            ] {
                if crate::process::stopping() {
                    return Ok(());
                }
                let url = format!("{base}/{year}.json");
                let request = crate::http_client::HttpRequest::get(&url);
                let config = crate::http_client::HttpConfig {
                    total_timeout: Duration::from_secs(15),
                    max_response_bytes: 512 * 1024,
                    max_redirects: 2,
                };
                if let Ok(response) = crate::http_client::send_with_config(&request, &config) {
                    if response.status == 200 {
                        if let Ok(published) = parse_holiday_year(&response.body) {
                            if published.year == year {
                                received = Some(
                                    HolidayCache::from_published(
                                        &published,
                                        &url,
                                        now(),
                                        response.etag,
                                        response.last_modified,
                                    )
                                    .map_err(|e| e.to_string())?,
                                );
                                break;
                            }
                        }
                    }
                }
            }
            let Some(cache) = received else {
                return Err(format!(
                    "{year} 年日历获取失败；已有缓存继续使用，无缓存时请确认日期或开启加班"
                ));
            };
            let target = dir.join(format!("{year}.json"));
            let temp = dir.join(format!("{year}.next"));
            for path in [&target, &temp] {
                if let Ok(meta) = fs::symlink_metadata(path) {
                    use std::os::windows::fs::MetadataExt;
                    if !meta.is_file() || meta.file_attributes() & 0x400 != 0 {
                        return Err("日历缓存文件不能是链接或目录".into());
                    }
                }
            }
            fs::write(&temp, cache.to_bytes().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            fs::OpenOptions::new()
                .write(true)
                .open(&temp)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(temp, target).map_err(|e| e.to_string())?;
            self.calendar
                .lock()
                .unwrap()
                .insert_cache(cache)
                .map_err(|e| e.to_string())?;
            self.background.wake();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{calendar, date, ts_ms};

    #[test]
    fn every_ai_purpose_uses_the_same_work_state_gate() {
        let (_root, app) = crate::test_support::scoped_app("shishi-gate");
        *app.calendar.lock().unwrap() = calendar();
        let mut s = app.state.lock().unwrap();
        let schedule = WorkSchedule::parse(&s.config.work_start, &s.config.work_end).unwrap();
        for (at, automatic, overtime) in [
            (ts_ms(2025, 1, 1, 10, 0), false, false), // holiday
            (ts_ms(2025, 1, 2, 10, 0), true, false),
            (ts_ms(2025, 1, 2, 21, 0), false, false),
            (ts_ms(2025, 1, 4, 10, 0), false, false), // weekend
            (ts_ms(2026, 1, 5, 10, 0), false, false), // unknown calendar
            (ts_ms(2026, 1, 5, 10, 0), true, true),
        ] {
            if overtime {
                s.work_scheduler
                    .runtime
                    .begin_overtime(at, &schedule)
                    .unwrap();
            }
            for kind in PURPOSES.into_iter().chain(["tool_call", "unknown"]) {
                for mode in [AiMode::Manual, AiMode::Automatic, AiMode::Nightly] {
                    assert_eq!(
                        app.work_gate(&s, kind, mode, None, at),
                        mode == AiMode::Manual || automatic,
                        "{kind} at {at}"
                    );
                }
                assert!(app.work_gate(&s, kind, AiMode::Manual, Some("manual-attempt"), at));
            }
        }
        for at in [i64::MIN, i64::MAX, ts_ms(1800, 1, 1, 10, 0)] {
            assert!(!app.work_gate(&s, "archive", AiMode::Manual, None, at));
        }
        for start in ["25:00", "bad", "20:00"] {
            s.config.work_start = start.into();
            assert!(!app.work_gate(&s, "archive", AiMode::Manual, None, now()));
        }
        drop(s);
    }

    #[test]
    fn archive_gate_checks_committed_claim_and_attempt_even_while_working() {
        let (_root, app) = crate::test_support::scoped_app("archive-gate");
        *app.calendar.lock().unwrap() = calendar();
        let mut s = app.state.lock().unwrap();
        for (day, working) in [(1, false), (2, true)] {
            let at = ts_ms(2025, 1, day, 10, 0);
            s.work_scheduler.runtime.day_records.clear();
            for status in [
                None,
                Some("queued"),
                Some("running"),
                Some("succeeded"),
                Some("failed"),
                Some("unknown"),
            ] {
                s.archive_attempts.clear();
                if let Some(status) = status {
                    s.archive_attempts.push(ArchiveAttempt {
                        id: "end-test".into(),
                        status: status.into(),
                        ..Default::default()
                    });
                }
                for (claim_id, claimed_at_ms, eligibility) in [
                    (None, at, EndEligibility::WorkedBeforeEnd),
                    (Some("other"), at, EndEligibility::WorkedBeforeEnd),
                    (Some("end-test"), at + 1, EndEligibility::WorkedBeforeEnd),
                    (Some("end-test"), at, EndEligibility::NotEligible),
                    (Some("end-test"), at, EndEligibility::WorkedBeforeEnd),
                    (Some("end-test"), at, EndEligibility::OvertimeBeforeEnd),
                ] {
                    s.work_scheduler
                        .runtime
                        .day_record_mut(date(2025, 1, day))
                        .end_claim = claim_id.map(|id| EndArchiveClaim {
                        attempt_id: id.into(),
                        claimed_at_ms,
                        eligibility,
                    });
                    let expected = claim_id == Some("end-test")
                        && claimed_at_ms <= at
                        && eligibility.is_eligible()
                        && matches!(status, Some("queued" | "running"));
                    assert_eq!(app.work_gate(&s, "archive", AiMode::Nightly, Some("end-test"), at), expected,
                        "day {day}, status {status:?}, claim {claim_id:?}/{claimed_at_ms}/{eligibility:?}");
                    assert!(app.work_gate(&s, "archive", AiMode::Manual, Some("end-test"), at));
                    assert_eq!(
                        app.work_gate(&s, "archive", AiMode::Automatic, Some("end-test"), at),
                        working
                    );
                    assert_eq!(
                        app.work_gate(&s, "archive", AiMode::Nightly, None, at),
                        working
                    );
                    for kind in PURPOSES.into_iter().filter(|kind| *kind != "archive") {
                        assert_eq!(
                            app.work_gate(&s, kind, AiMode::Nightly, Some("end-test"), at),
                            working,
                            "{kind}"
                        );
                    }
                }
            }
        }
        drop(s);
    }
}
