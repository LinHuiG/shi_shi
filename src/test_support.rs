//! Synthetic fixtures shared by unit tests. Nothing here is compiled into the
//! release binary, and every value is invented rather than copied from a user
//! profile, calendar download or real conversation.
use crate::engine::App;
use crate::holiday::{CalendarData, DayKind, HolidayCache, PublishedHolidayYear};
use crate::work_state::WorkSchedule;
use chrono::{NaiveDate, TimeZone, Utc};
use std::collections::BTreeMap;

/// Existing commit-failure regressions now fail the SQLite commit instead of
/// obstructing the retired JSON temporary file.
pub fn persistence_failure(root: &std::path::Path, blocked: bool) -> Result<(), String> {
    let db =
        rusqlite::Connection::open(crate::storage::data_dir(root).join(crate::state_store::FILE))
            .map_err(|e| e.to_string())?;
    let sql = if blocked {
        "CREATE TRIGGER reject_state_insert BEFORE INSERT ON metadata BEGIN SELECT RAISE(ABORT,'synthetic commit rejection'); END; CREATE TRIGGER reject_state_update BEFORE UPDATE ON metadata BEGIN SELECT RAISE(ABORT,'synthetic commit rejection'); END;"
    } else {
        "DROP TRIGGER reject_state_insert; DROP TRIGGER reject_state_update;"
    };
    db.execute_batch(sql).map_err(|e| e.to_string())
}

pub fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("synthetic date")
}

fn local_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
        .expect("synthetic timestamp")
        .timestamp_millis()
        - 8 * 60 * 60 * 1000
}

/// Shanghai wall-clock time at minute precision.
pub fn ts_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    local_ms(year, month, day, hour, minute, 0)
}

/// Shanghai wall-clock time with an explicit second, for boundary ticks.
pub fn ts_ms_prec(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    local_ms(year, month, day, hour, minute, second)
}

/// Calendar with one published year: Jan 1 is an explicit holiday and Jan 11
/// (a Saturday) is a made-up workday. Everything else follows the weekend rule.
pub fn calendar() -> CalendarData {
    let mut days = BTreeMap::new();
    days.insert(date(2025, 1, 1), DayKind::Holiday);
    days.insert(date(2025, 1, 11), DayKind::Workday);
    let published = PublishedHolidayYear { year: 2025, days };
    let cache = HolidayCache::from_published(
        &published,
        "synthetic",
        ts_ms(2024, 12, 1, 0, 0),
        None,
        None,
    )
    .expect("synthetic holiday cache");
    let mut data = CalendarData::new();
    data.insert_cache(cache).expect("insert synthetic cache");
    data
}

#[cfg(test)]
impl App {
    /// Put a test app into an explicit, persisted overtime state so tests that
    /// exercise unrelated purposes are not coupled to the real clock or to a
    /// published holiday calendar. The gate itself is tested separately.
    pub(crate) fn test_force_working(&self) {
        let mut locked = self.state.lock().unwrap();
        let mut next = locked.clone();
        next.config.work_start = "00:00".into();
        next.config.work_end = "23:59".into();
        let schedule = WorkSchedule::parse("00:00", "23:59").expect("synthetic schedule");
        next.work_scheduler
            .runtime
            .begin_overtime(crate::model::now(), &schedule)
            .expect("synthetic overtime");
        next.work_scheduler.last_schedule = Some(schedule);
        self.save(&next).expect("persist synthetic work state");
        *locked = next;
    }
}
