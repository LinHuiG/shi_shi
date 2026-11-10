//! Work runtime and the single AI time gate.
//!
//! Timestamps are Unix milliseconds; Shanghai is fixed UTC+8. A business
//! day starts at the work start time, so an overnight shift belongs to the
//! day it started. Scanning never waits on this gate; only AI dispatch does.
//!
//! The boundary tick is deterministic: given the previous live tick and
//! now, it observes work, and on a crossed start boundary claims the
//! morning report, on a crossed end boundary the work-end archive. A claim
//! and its queued job commit in one transaction, so a day is claimed once.
//! Startup, a schedule change, a backward clock or a gap over two minutes
//! never catch up a passed boundary.

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate};
use serde_json::{json, Value};

use super::calendar::{self, Calendar, Kind};
use crate::config::Config;
use crate::{Error, Result};

pub const SHANGHAI_OFFSET_MS: i64 = 8 * 60 * 60 * 1000;
/// Two live ticks further apart than this are a missed boundary.
pub const MAX_TICK_GAP_MS: i64 = 120_000;
pub(crate) const MIN_YEAR: i32 = 1900;
pub(crate) const MAX_YEAR: i32 = 2100;
/// Day records older than this are dropped unless a live job cites them.
pub(crate) const KEEP_DAYS: i64 = 90;

type WorkResult<T> = std::result::Result<T, String>;

/// Work start and end; `end < start` is an overnight shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkSchedule {
    start: u16,
    end: u16,
}

impl WorkSchedule {
    /// Strict `HH:MM` for both ends; equal times are rejected. Settings wraps
    /// the reason as `上下班时间无效：{e}`.
    pub fn parse(start: &str, end: &str) -> WorkResult<Self> {
        let start = parse_hhmm("work_start", start)?;
        let end = parse_hhmm("work_end", end)?;
        if start == end {
            return Err("work start and end must differ".into());
        }
        Ok(Self { start, end })
    }

    pub fn of(config: &Config) -> WorkResult<Self> {
        Self::parse(&config.work_start, &config.work_end)
    }

    fn overnight(self) -> bool {
        self.end < self.start
    }

    pub fn start_at(self, day: NaiveDate) -> WorkResult<i64> {
        local_ms(day, self.start)
    }

    /// End of the shift that started on `day`.
    pub fn end_at(self, day: NaiveDate) -> WorkResult<i64> {
        let day = if self.overnight() { day.succ_opt().ok_or_else(|| out_of_range(day))? } else { day };
        local_ms(day, self.end)
    }

    /// The day of the latest work start at or before `at`.
    pub fn business_date(self, at: i64) -> WorkResult<NaiveDate> {
        let (date, minute) = local(at)?;
        let day = if minute >= self.start { date } else { date.pred_opt().ok_or_else(|| out_of_range(date))? };
        check_date(day)?;
        Ok(day)
    }

    fn on_shift(self, day: NaiveDate, at: i64) -> WorkResult<bool> {
        Ok(at >= self.start_at(day)? && at < self.end_at(day)?)
    }

    pub(crate) fn text(self) -> String {
        format!("{}-{}", self.start, self.end)
    }

    pub(crate) fn from_text(text: &str) -> Option<Self> {
        let (a, b) = text.split_once('-')?;
        let (start, end) = (a.parse().ok()?, b.parse().ok()?);
        (start < 1440 && end < 1440 && start != end).then_some(Self { start, end })
    }
}

fn parse_hhmm(field: &str, value: &str) -> WorkResult<u16> {
    let b = value.as_bytes();
    if b.len() == 5 && b[2] == b':' && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit) {
        let hour = u16::from((b[0] - b'0') * 10 + b[1] - b'0');
        let minute = u16::from((b[3] - b'0') * 10 + b[4] - b'0');
        if hour < 24 && minute < 60 {
            return Ok(hour * 60 + minute);
        }
    }
    Err(format!("{field} must be strict HH:MM, got {value:?}"))
}

fn out_of_range(day: NaiveDate) -> String {
    format!("date {day} is outside the supported range")
}

fn check_date(day: NaiveDate) -> WorkResult<()> {
    (MIN_YEAR..=MAX_YEAR).contains(&day.year()).then_some(()).ok_or_else(|| out_of_range(day))
}

/// Shanghai date and minute of day.
fn local(at: i64) -> WorkResult<(NaiveDate, u16)> {
    let t = chrono::DateTime::from_timestamp_millis(at.saturating_add(SHANGHAI_OFFSET_MS))
        .ok_or_else(|| format!("invalid Unix millisecond timestamp {at}"))?
        .naive_utc();
    check_date(t.date())?;
    use chrono::Timelike;
    Ok((t.date(), (t.hour() * 60 + t.minute()) as u16))
}

pub fn shanghai_date(at: i64) -> WorkResult<NaiveDate> {
    local(at).map(|(d, _)| d)
}

fn local_ms(day: NaiveDate, minute: u16) -> WorkResult<i64> {
    check_date(day)?;
    let t = day.and_hms_opt(u32::from(minute / 60), u32::from(minute % 60), 0).ok_or_else(|| out_of_range(day))?;
    Ok(t.and_utc().timestamp_millis() - SHANGHAI_OFFSET_MS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Working,
    Overtime,
    OffDuty,
    Holiday,
    CalendarUnknown,
}

impl Status {
    pub fn ai_allowed(self) -> bool {
        matches!(self, Self::Working | Self::Overtime)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Overtime => "overtime",
            Self::OffDuty => "off_duty",
            Self::Holiday => "holiday",
            Self::CalendarUnknown => "calendar_unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    NotEligible,
    WorkedBeforeEnd,
    OvertimeBeforeEnd,
}

impl Eligibility {
    pub fn is_eligible(self) -> bool {
        self != Self::NotEligible
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotEligible => "not_eligible",
            Self::WorkedBeforeEnd => "worked_before_end",
            Self::OvertimeBeforeEnd => "overtime_before_end",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Self::NotEligible, Self::WorkedBeforeEnd, Self::OvertimeBeforeEnd].into_iter().find(|e| e.as_str() == text)
    }
}

/// A boundary claim: the id of the job it authorized and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub id: String,
    pub at: i64,
}

/// One business day's observations and claims.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayRecord {
    pub first_worked: Option<i64>,
    pub first_overtime: Option<i64>,
    /// Set at the end boundary; an end claim implies an eligible value.
    pub end_eligibility: Option<Eligibility>,
    pub morning_claim: Option<Claim>,
    pub end_claim: Option<Claim>,
}

fn earliest(slot: &mut Option<i64>, at: i64) {
    *slot = Some(slot.map_or(at, |old| old.min(at)));
}

/// The persisted runtime, holding only the day records of the loaded range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Runtime {
    /// Manual overtime: start time and the business day it belongs to.
    pub overtime: Option<(i64, NaiveDate)>,
    pub days: BTreeMap<NaiveDate, DayRecord>,
    pub last_schedule: Option<WorkSchedule>,
    pub(crate) clock_floor: Option<i64>,
}

/// The `work_state` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub at: i64,
    pub business_date: NaiveDate,
    pub calendar_day: calendar::Day,
    pub status: Status,
    pub overtime_started: Option<i64>,
    pub first_worked: Option<i64>,
    pub first_overtime: Option<i64>,
    pub end_eligibility: Eligibility,
}

impl Snapshot {
    pub fn json(&self) -> Value {
        json!({
            "at_ms": self.at,
            "business_date": self.business_date.to_string(),
            "calendar_day": self.calendar_day.json(),
            "status": self.status.as_str(),
            "active_overtime_started_at_ms": self.overtime_started,
            "first_worked_at_ms": self.first_worked,
            "first_overtime_at_ms": self.first_overtime,
            "end_eligibility": self.end_eligibility.as_str(),
        })
    }
}

impl Runtime {
    fn record(&mut self, day: NaiveDate) -> &mut DayRecord {
        self.days.entry(day).or_default()
    }

    /// Clear manual overtime once its business day has rolled; the day
    /// record keeps its observations.
    fn expire_overtime(&mut self, at: i64, schedule: WorkSchedule) -> WorkResult<()> {
        let day = schedule.business_date(at)?;
        if self.overtime.is_some_and(|(_, d)| d != day) {
            self.overtime = None;
        }
        Ok(())
    }

    /// Start overtime for the current business day; repeating it keeps the
    /// first start.
    pub fn begin_overtime(&mut self, at: i64, schedule: WorkSchedule) -> WorkResult<()> {
        let day = schedule.business_date(at)?;
        self.expire_overtime(at, schedule)?;
        earliest(&mut self.record(day).first_overtime, at);
        if self.overtime.is_none_or(|(_, d)| d != day) {
            self.overtime = Some((at, day));
        }
        Ok(())
    }

    /// Leave overtime; an end eligibility earned earlier stays.
    pub fn end_overtime(&mut self) {
        self.overtime = None;
    }

    /// Record a live observation and return the resulting snapshot.
    pub fn observe(&mut self, at: i64, schedule: WorkSchedule, calendar: &Calendar) -> WorkResult<Snapshot> {
        self.expire_overtime(at, schedule)?;
        let before = self.snapshot(at, schedule, calendar)?;
        let record = self.record(before.business_date);
        match before.status {
            Status::Working => earliest(&mut record.first_worked, at),
            Status::Overtime => earliest(&mut record.first_overtime, at),
            _ => {}
        }
        self.snapshot(at, schedule, calendar)
    }

    /// Pure status view. Inside a shift the business day's calendar decides;
    /// outside it, the local day's. Stale overtime is ignored.
    pub fn snapshot(&self, at: i64, schedule: WorkSchedule, calendar: &Calendar) -> WorkResult<Snapshot> {
        let (local_date, _) = local(at)?;
        let business_date = schedule.business_date(at)?;
        let on_shift = schedule.on_shift(business_date, at)?;
        let calendar_day = calendar.classify(if on_shift { business_date } else { local_date }, at);
        let overtime = self.overtime.filter(|&(started, day)| day == business_date && started <= at);
        let status = match (overtime.is_some(), on_shift, calendar_day.kind) {
            (true, _, _) => Status::Overtime,
            (_, _, Kind::Holiday) => Status::Holiday,
            (_, _, Kind::Unknown) => Status::CalendarUnknown,
            (_, true, Kind::Workday) => Status::Working,
            _ => Status::OffDuty,
        };
        let record = self.days.get(&business_date);
        Ok(Snapshot {
            at,
            business_date,
            calendar_day,
            status,
            overtime_started: overtime.map(|o| o.0),
            first_worked: record.and_then(|r| r.first_worked),
            first_overtime: record.and_then(|r| r.first_overtime),
            end_eligibility: self.end_eligibility(business_date, schedule)?,
        })
    }

    /// Eligible only if work or overtime was seen strictly before the end
    /// boundary; overtime started afterwards never claims it back.
    pub fn end_eligibility(&self, day: NaiveDate, schedule: WorkSchedule) -> WorkResult<Eligibility> {
        let end = schedule.end_at(day)?;
        let before = |t: Option<i64>| t.is_some_and(|t| t < end);
        Ok(match self.days.get(&day) {
            Some(r) if before(r.first_worked) => Eligibility::WorkedBeforeEnd,
            Some(r) if before(r.first_overtime) => Eligibility::OvertimeBeforeEnd,
            _ => Eligibility::NotEligible,
        })
    }

    /// One boundary tick. Returns the snapshot at `now` and the claims made.
    pub fn tick(
        &mut self,
        previous: Option<i64>,
        now: i64,
        schedule: WorkSchedule,
        calendar: &Calendar,
    ) -> WorkResult<(Snapshot, Vec<Action>)> {
        local(now)?;
        let changed = self.last_schedule.is_some_and(|old| old != schedule);
        // `None < Some`, so this is the later of the two known times.
        let floor = self.clock_floor.max(previous);
        let mut actions = Vec::new();
        if let Some(previous) = previous.filter(|p| now > *p && now - p <= MAX_TICK_GAP_MS && !changed) {
            let mut crossed = boundaries(schedule, previous, now)?;
            crossed.retain(|b| floor.is_none_or(|floor| b.at > floor));
            crossed.sort_by_key(|b| (b.at, b.end));
            for b in crossed {
                let claim = |slot: &mut Option<Claim>, kind: &str| {
                    slot.get_or_insert_with(|| Claim { id: format!("{kind}-{}", b.day), at: b.at }).clone()
                };
                if b.end {
                    let eligibility = self.end_eligibility(b.day, schedule)?;
                    let record = self.record(b.day);
                    record.end_eligibility = Some(eligibility);
                    if eligibility.is_eligible() && record.end_claim.is_none() {
                        actions.push(Action::EndArchive { day: b.day, claim: claim(&mut record.end_claim, "end") });
                    }
                } else {
                    let snapshot = self.observe(b.at, schedule, calendar)?;
                    let record = self.record(b.day);
                    if snapshot.status.ai_allowed()
                        && snapshot.calendar_day.is_workday()
                        && record.morning_claim.is_none()
                    {
                        actions.push(Action::MorningReport {
                            day: b.day,
                            claim: claim(&mut record.morning_claim, "morning"),
                        });
                    }
                }
            }
        }
        let snapshot = self.observe(now, schedule, calendar)?;
        self.last_schedule = Some(schedule);
        self.clock_floor = Some(floor.map_or(now, |f| f.max(now)));
        Ok((snapshot, actions))
    }
}

/// What a tick asks the archive domain to queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    MorningReport { day: NaiveDate, claim: Claim },
    EndArchive { day: NaiveDate, claim: Claim },
}

struct Boundary {
    at: i64,
    day: NaiveDate,
    end: bool,
}

fn boundaries(schedule: WorkSchedule, previous: i64, now: i64) -> WorkResult<Vec<Boundary>> {
    let mut days = std::collections::BTreeSet::new();
    for date in [shanghai_date(previous)?, shanghai_date(now)?] {
        for d in [date.pred_opt(), Some(date), date.succ_opt()].into_iter().flatten() {
            days.extend(check_date(d).ok().map(|_| d));
        }
    }
    let mut out = Vec::new();
    for day in days {
        for (end, at) in [(false, schedule.start_at(day)?), (true, schedule.end_at(day)?)] {
            if at > previous && at <= now {
                out.push(Boundary { at, day, end });
            }
        }
    }
    Ok(out)
}

/// The next start or end boundary strictly after `at`.
pub fn next_boundary(schedule: WorkSchedule, at: i64) -> WorkResult<i64> {
    let today = shanghai_date(at)?;
    let mut next = i64::MAX;
    for offset in -1..=2 {
        let day = today + Duration::days(offset);
        for t in [schedule.start_at(day)?, schedule.end_at(day)?] {
            if t > at {
                next = next.min(t);
            }
        }
    }
    Ok(next)
}

/// The days whose rows a classification around `at` (and `previous` when
/// it is a live tick) needs.
pub(crate) fn window(previous: Option<i64>, at: i64) -> WorkResult<(NaiveDate, NaiveDate)> {
    let now = shanghai_date(at)?;
    let earliest =
        previous.filter(|p| *p <= at && at - p <= MAX_TICK_GAP_MS).and_then(|p| shanghai_date(p).ok()).unwrap_or(now);
    Ok((earliest - Duration::days(1), now + Duration::days(1)))
}

pub(crate) const NO_CLAIM: &str = "归档排队缺少匹配的持久化下班 claim；已停止自动执行，等待手动归档";
pub(crate) const CLAIM_EXPIRED: &str = "下班归档授权已失效；已停止自动执行，等待手动归档";

const SAVE_FAILED: &str = "上下班边界保存失败超过120秒，已跳过本次边界，请手动生成早报或归档";
const TICK_FAILED: &str = "上下班边界检查连续失败超过 120 秒，期间若跨越上班或下班时间，早报或下班归档可能未生成；请检查后手动生成早报或手动归档";

/// The boundary loop and the calendar refresh. The boundary loop wakes at
/// the next start or end boundary and at least once a minute (the gap
/// stays under [`MAX_TICK_GAP_MS`], so a crossing is seen live), and on
/// every commit; it writes only when something changed.
pub fn spawn(ctx: &std::sync::Arc<crate::Ctx>) {
    let boundary = ctx.clone();
    tokio::spawn(async move {
        let ctx = boundary;
        let mut changes = ctx.db.changes();
        let mut previous: Option<i64> = None;
        let mut failed_since: Option<i64> = None;
        loop {
            let at = crate::time::now();
            let config = ctx.config.get();
            if failed_since.is_some_and(|since| at.saturating_sub(since) > MAX_TICK_GAP_MS) {
                log_error(&ctx, SAVE_FAILED).await;
                previous = Some(at);
                failed_since = None;
            }
            // Advance only after a committed tick, so a failed save retries
            // the crossed boundary within the gap.
            let result = ctx.db.tick_work_boundary(config.clone(), previous, at).await;
            match result {
                Ok(()) => {
                    previous = Some(at);
                    failed_since = None;
                }
                Err(e) => {
                    failed_since.get_or_insert(previous.unwrap_or(at));
                    log_error(&ctx, &e.0).await;
                    if previous.is_some_and(|p| at - p > MAX_TICK_GAP_MS) {
                        log_error(&ctx, TICK_FAILED).await;
                        previous = Some(at);
                    }
                }
            }
            let next = WorkSchedule::of(&config).and_then(|s| next_boundary(s, at)).unwrap_or(i64::MAX);
            let wait = next.saturating_sub(crate::time::now()).saturating_add(200).clamp(200, crate::time::MINUTE);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(wait as u64), changes.changed()).await;
        }
    });
    let refresh = ctx.clone();
    tokio::spawn(async move {
        loop {
            // Traces follow the message retention.
            let days = i64::from(refresh.config.get().message_retention_days.max(1));
            let _ = refresh.db.prune_ai_traces(crate::time::now() - days * crate::time::DAY).await;
            let delay = match refresh_calendar(&refresh).await {
                Ok(()) => crate::time::HOUR,
                Err(e) => {
                    log_error(&refresh, &e.0).await;
                    15 * crate::time::MINUTE
                }
            };
            tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
        }
    });
}

async fn log_error(ctx: &crate::Ctx, message: &str) {
    ctx.db.record_activity("error", json!({"message": message})).await;
}

/// Fetch this year (and next year in December) when missing or older than
/// the TTL; a failed year keeps its old copy.
pub async fn refresh_calendar(ctx: &crate::Ctx) -> Result<()> {
    let at = crate::time::now();
    let today = shanghai_date(at).map_err(Error)?;
    for year in calendar::wanted_years(today) {
        let fetched = ctx.db.holiday_fetched_at(year).await?;
        if fetched.is_some_and(|f| at.saturating_sub(f) < calendar::CACHE_TTL_MS) {
            continue;
        }
        let fresh = calendar::fetch_year(year, crate::time::now(), http_get).await?;
        ctx.db.save_holiday_year(fresh).await?;
    }
    Ok(())
}

/// Bounded HTTPS GET of a calendar file: 15 s, 512 KiB.
async fn http_get(url: String) -> std::result::Result<Vec<u8>, String> {
    use shishi_api::http_client::{send, HttpRequest};
    let request = move || {
        send(&HttpRequest::get(&url, std::time::Duration::from_secs(15), calendar::MAX_BODY_BYTES), None)
            .map_err(|e| Error(e.to_string()))
    };
    let response = crate::exec::blocking(request).await.map_err(|e| e.0)?;
    match response.status {
        200 => Ok(response.body),
        status => Err(format!("HTTP {status}")),
    }
}
