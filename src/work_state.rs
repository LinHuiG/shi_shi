//! Persistent work runtime and the single AI gate.
//!
//! All timestamps in this module are explicit Unix **milliseconds**, matching
//! the application's `model::now()` and persisted state. There is no
//! seconds-based API here. Shanghai is fixed +08:00. Scanning is deliberately
//! not part of [`AiPurpose`]: message ingestion must continue in every state;
//! only model calls pass through [`gate`].

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::holiday::{CalendarData, CalendarDay, CalendarKind};

pub const WORK_RUNTIME_SCHEMA_VERSION: u32 = 1;
pub const WORK_MIN_YEAR: i32 = 1900;
pub const WORK_MAX_YEAR: i32 = 2100;
pub const SHANGHAI_OFFSET_MS: i64 = 8 * 60 * 60 * 1000;
pub const MAX_CLAIM_ID_CHARS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkError {
    InvalidTimestampMs(i64),
    InvalidTime { field: &'static str, value: String },
    EqualWorkTimes,
    DateOutOfRange(NaiveDate),
    MalformedState(String),
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    InvalidState(&'static str),
}

impl fmt::Display for WorkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimestampMs(at) => write!(f, "invalid Unix millisecond timestamp {at}"),
            Self::InvalidTime { field, value } => {
                write!(f, "{field} must be strict HH:MM, got {value:?}")
            }
            Self::EqualWorkTimes => write!(f, "work start and end must differ"),
            Self::DateOutOfRange(date) => write!(f, "date {date} is outside the supported range"),
            Self::MalformedState(detail) => write!(f, "work runtime is malformed: {detail}"),
            Self::UnsupportedSchemaVersion { found, expected } => write!(
                f,
                "unsupported work runtime schema version {found} (expected {expected})"
            ),
            Self::InvalidState(detail) => write!(f, "work runtime is invalid: {detail}"),
        }
    }
}

impl std::error::Error for WorkError {}

/// The only two schedule settings. Cross-midnight is represented by
/// `end_minute < start_minute`; equality is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkSchedule {
    start_minute: u16,
    end_minute: u16,
}

impl<'de> Deserialize<'de> for WorkSchedule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            start_minute: u16,
            end_minute: u16,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::from_minutes(wire.start_minute, wire.end_minute)
            .map_err(|e| <D::Error as serde::de::Error>::custom(e.to_string()))
    }
}

impl WorkSchedule {
    pub fn parse(start: &str, end: &str) -> Result<Self, WorkError> {
        let start_minute = parse_hhmm("work_start", start)?;
        let end_minute = parse_hhmm("work_end", end)?;
        Self::from_minutes(start_minute, end_minute)
    }

    pub fn from_minutes(start_minute: u16, end_minute: u16) -> Result<Self, WorkError> {
        if start_minute >= 24 * 60 {
            return Err(WorkError::InvalidTime {
                field: "work_start",
                value: format!("{start_minute} minutes"),
            });
        }
        if end_minute >= 24 * 60 {
            return Err(WorkError::InvalidTime {
                field: "work_end",
                value: format!("{end_minute} minutes"),
            });
        }
        if start_minute == end_minute {
            return Err(WorkError::EqualWorkTimes);
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }

    pub fn start_minute(self) -> u16 {
        self.start_minute
    }

    pub fn end_minute(self) -> u16 {
        self.end_minute
    }

    pub fn is_overnight(self) -> bool {
        self.end_minute < self.start_minute
    }

    pub fn start_timestamp_ms(self, business_day: NaiveDate) -> Result<i64, WorkError> {
        local_timestamp_ms(business_day, self.start_minute)
    }

    /// End of the shift that started on `business_day`, in Unix milliseconds.
    /// For an overnight schedule this is the following local date.
    pub fn end_timestamp_ms(self, business_day: NaiveDate) -> Result<i64, WorkError> {
        let day = if self.is_overnight() {
            business_day
                .succ_opt()
                .ok_or(WorkError::DateOutOfRange(business_day))?
        } else {
            business_day
        };
        local_timestamp_ms(day, self.end_minute)
    }

    /// `at_ms` belongs to the business day of the most recent work start at or
    /// before it. Exact work start belongs to the new business day.
    pub fn business_date(self, at_ms: i64) -> Result<NaiveDate, WorkError> {
        let (date, minute) = shanghai_date_and_minute(at_ms)?;
        let business_date = if minute >= self.start_minute {
            date
        } else {
            date.pred_opt().ok_or(WorkError::DateOutOfRange(date))?
        };
        validate_work_date(business_date)?;
        Ok(business_date)
    }

    /// True when `at_ms` is inside the shift that belongs to `business_day`.
    /// This is the only place the start/end interval is evaluated, so an
    /// overnight shift remains fully covered by its start business day.
    fn is_on_shift(self, business_day: NaiveDate, at_ms: i64) -> Result<bool, WorkError> {
        let start = self.start_timestamp_ms(business_day)?;
        let end = self.end_timestamp_ms(business_day)?;
        Ok(at_ms >= start && at_ms < end)
    }
}

fn parse_hhmm(field: &'static str, value: &str) -> Result<u16, WorkError> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[..2].iter().all(u8::is_ascii_digit)
        || !bytes[3..].iter().all(u8::is_ascii_digit)
    {
        return Err(WorkError::InvalidTime {
            field,
            value: value.to_owned(),
        });
    }
    let hour: u16 = value[..2].parse().map_err(|_| WorkError::InvalidTime {
        field,
        value: value.to_owned(),
    })?;
    let minute: u16 = value[3..].parse().map_err(|_| WorkError::InvalidTime {
        field,
        value: value.to_owned(),
    })?;
    if hour >= 24 || minute >= 60 {
        return Err(WorkError::InvalidTime {
            field,
            value: value.to_owned(),
        });
    }
    Ok(hour * 60 + minute)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Working,
    Overtime,
    OffDuty,
    Holiday,
    CalendarUnknown,
}

impl WorkStatus {
    pub fn ai_allowed(self) -> bool {
        matches!(self, Self::Working | Self::Overtime)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndEligibility {
    NotEligible,
    WorkedBeforeEnd,
    OvertimeBeforeEnd,
}

impl EndEligibility {
    pub fn is_eligible(self) -> bool {
        !matches!(self, Self::NotEligible)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveOvertime {
    pub started_at_ms: i64,
    pub business_day: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MorningClaim {
    pub claim_id: String,
    pub claimed_at_ms: i64,
}

/// A boundary claim. It intentionally has no status: archive status lives in
/// the archive attempt identified by `attempt_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndArchiveClaim {
    pub attempt_id: String,
    pub claimed_at_ms: i64,
    pub eligibility: EndEligibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDayRecord {
    pub first_worked_at_ms: Option<i64>,
    pub first_overtime_at_ms: Option<i64>,
    pub end_eligibility: Option<EndEligibility>,
    pub morning_claim: Option<MorningClaim>,
    pub end_claim: Option<EndArchiveClaim>,
}

impl WorkDayRecord {
    pub fn empty() -> Self {
        Self {
            first_worked_at_ms: None,
            first_overtime_at_ms: None,
            end_eligibility: None,
            morning_claim: None,
            end_claim: None,
        }
    }

    fn observe_first_worked(&mut self, at_ms: i64) {
        self.first_worked_at_ms = Some(self.first_worked_at_ms.map_or(at_ms, |old| old.min(at_ms)));
    }

    fn observe_first_overtime(&mut self, at_ms: i64) {
        self.first_overtime_at_ms = Some(
            self.first_overtime_at_ms
                .map_or(at_ms, |old| old.min(at_ms)),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkRuntime {
    pub schema_version: u32,
    pub active_overtime: Option<ActiveOvertime>,
    pub day_records: BTreeMap<NaiveDate, WorkDayRecord>,
}

impl Default for WorkRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkRuntime {
    pub fn new() -> Self {
        Self {
            schema_version: WORK_RUNTIME_SCHEMA_VERSION,
            active_overtime: None,
            day_records: BTreeMap::new(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WorkError> {
        let runtime: Self =
            serde_json::from_slice(bytes).map_err(|e| WorkError::MalformedState(e.to_string()))?;
        runtime.validate()?;
        Ok(runtime)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, WorkError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|e| WorkError::MalformedState(e.to_string()))
    }

    pub fn validate(&self) -> Result<(), WorkError> {
        if self.schema_version != WORK_RUNTIME_SCHEMA_VERSION {
            return Err(WorkError::UnsupportedSchemaVersion {
                found: self.schema_version,
                expected: WORK_RUNTIME_SCHEMA_VERSION,
            });
        }
        if let Some(active) = &self.active_overtime {
            validate_timestamp_ms(active.started_at_ms)?;
            validate_work_date(active.business_day)?;
        }
        for (day, record) in &self.day_records {
            validate_work_date(*day)?;
            for at in [record.first_worked_at_ms, record.first_overtime_at_ms]
                .into_iter()
                .flatten()
            {
                validate_timestamp_ms(at)?;
            }
            if let Some(claim) = &record.morning_claim {
                validate_claim_id(&claim.claim_id)?;
                validate_timestamp_ms(claim.claimed_at_ms)?;
            }
            if let Some(claim) = &record.end_claim {
                validate_claim_id(&claim.attempt_id)?;
                validate_timestamp_ms(claim.claimed_at_ms)?;
                if !claim.eligibility.is_eligible() {
                    return Err(WorkError::InvalidState(
                        "end archive claim has no end eligibility",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn day_record(&self, day: NaiveDate) -> Option<&WorkDayRecord> {
        self.day_records.get(&day)
    }

    pub fn day_record_mut(&mut self, day: NaiveDate) -> &mut WorkDayRecord {
        self.day_records
            .entry(day)
            .or_insert_with(WorkDayRecord::empty)
    }

    /// Clear manual overtime when the business day has rolled. The day record
    /// (including previous overtime start and end eligibility) is retained.
    pub fn expire_overtime(
        &mut self,
        at_ms: i64,
        schedule: &WorkSchedule,
    ) -> Result<bool, WorkError> {
        let business_day = schedule.business_date(at_ms)?;
        let expired = self
            .active_overtime
            .as_ref()
            .is_some_and(|active| active.business_day != business_day);
        if expired {
            self.active_overtime = None;
        }
        Ok(expired)
    }

    /// Enable overtime for the current business day. Repeated calls on the same
    /// business day are idempotent and preserve the first start time.
    pub fn begin_overtime(
        &mut self,
        at_ms: i64,
        schedule: &WorkSchedule,
    ) -> Result<NaiveDate, WorkError> {
        validate_timestamp_ms(at_ms)?;
        let business_day = schedule.business_date(at_ms)?;
        self.expire_overtime(at_ms, schedule)?;
        let record = self.day_record_mut(business_day);
        record.observe_first_overtime(at_ms);
        if self
            .active_overtime
            .as_ref()
            .is_none_or(|active| active.business_day != business_day)
        {
            self.active_overtime = Some(ActiveOvertime {
                started_at_ms: at_ms,
                business_day,
            });
        }
        Ok(business_day)
    }

    /// Leave overtime. The day record keeps `first_overtime_at_ms`, so an end
    /// boundary that was earned earlier is not cancelled.
    pub fn end_overtime(&mut self) {
        self.active_overtime = None;
    }

    /// Record a live observation and return the resulting snapshot.
    pub fn observe(
        &mut self,
        at_ms: i64,
        schedule: &WorkSchedule,
        calendar: &CalendarData,
    ) -> Result<WorkSnapshot, WorkError> {
        self.expire_overtime(at_ms, schedule)?;
        let before = self.snapshot(at_ms, schedule, calendar)?;
        let record = self.day_record_mut(before.business_date);
        match before.status {
            WorkStatus::Working => record.observe_first_worked(at_ms),
            WorkStatus::Overtime => record.observe_first_overtime(at_ms),
            WorkStatus::OffDuty | WorkStatus::Holiday | WorkStatus::CalendarUnknown => {}
        }
        self.snapshot(at_ms, schedule, calendar)
    }

    /// Pure status view; stale manual overtime is ignored without mutating.
    ///
    /// While inside a shift, the calendar of the shift's **business day** is
    /// authoritative, so an overnight shift is fully covered by the day it
    /// started. Outside the shift, the current Shanghai local day is shown.
    pub fn snapshot(
        &self,
        at_ms: i64,
        schedule: &WorkSchedule,
        calendar: &CalendarData,
    ) -> Result<WorkSnapshot, WorkError> {
        validate_timestamp_ms(at_ms)?;
        let (local_date, _) = shanghai_date_and_minute(at_ms)?;
        let business_date = schedule.business_date(at_ms)?;
        let on_shift = schedule.is_on_shift(business_date, at_ms)?;
        let calendar_date = if on_shift { business_date } else { local_date };
        let calendar_day = calendar.classify(calendar_date, at_ms);
        let active_overtime = self
            .active_overtime
            .as_ref()
            .filter(|active| active.business_day == business_date && active.started_at_ms <= at_ms);
        let status = if active_overtime.is_some() {
            WorkStatus::Overtime
        } else if on_shift {
            match calendar_day.kind {
                CalendarKind::Workday => WorkStatus::Working,
                CalendarKind::Holiday => WorkStatus::Holiday,
                CalendarKind::Unknown => WorkStatus::CalendarUnknown,
                CalendarKind::Weekend => WorkStatus::OffDuty,
            }
        } else {
            match calendar_day.kind {
                CalendarKind::Holiday => WorkStatus::Holiday,
                CalendarKind::Unknown => WorkStatus::CalendarUnknown,
                CalendarKind::Workday | CalendarKind::Weekend => WorkStatus::OffDuty,
            }
        };
        let record = self.day_records.get(&business_date);
        Ok(WorkSnapshot {
            at_ms,
            business_date,
            calendar_day,
            status,
            active_overtime_started_at_ms: active_overtime.map(|active| active.started_at_ms),
            first_worked_at_ms: record.and_then(|record| record.first_worked_at_ms),
            first_overtime_at_ms: record.and_then(|record| record.first_overtime_at_ms),
            end_eligibility: self.end_eligibility(business_date, schedule)?,
        })
    }

    /// A day is end-eligible only if working or overtime was observed strictly
    /// before its end boundary. An overtime start after the boundary never
    /// creates the claim retroactively. Exited overtime still counts.
    pub fn end_eligibility(
        &self,
        business_day: NaiveDate,
        schedule: &WorkSchedule,
    ) -> Result<EndEligibility, WorkError> {
        let end = schedule.end_timestamp_ms(business_day)?;
        let Some(record) = self.day_records.get(&business_day) else {
            return Ok(EndEligibility::NotEligible);
        };
        if record.first_worked_at_ms.is_some_and(|worked| worked < end) {
            return Ok(EndEligibility::WorkedBeforeEnd);
        }
        if record
            .first_overtime_at_ms
            .is_some_and(|started| started < end)
        {
            return Ok(EndEligibility::OvertimeBeforeEnd);
        }
        Ok(EndEligibility::NotEligible)
    }

    pub fn find_end_claim(&self, attempt_id: &str) -> Option<(NaiveDate, EndArchiveClaim)> {
        self.day_records.iter().find_map(|(day, record)| {
            record
                .end_claim
                .as_ref()
                .filter(|claim| claim.attempt_id == attempt_id)
                .map(|claim| (*day, claim.clone()))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSnapshot {
    pub at_ms: i64,
    pub business_date: NaiveDate,
    pub calendar_day: CalendarDay,
    pub status: WorkStatus,
    pub active_overtime_started_at_ms: Option<i64>,
    pub first_worked_at_ms: Option<i64>,
    pub first_overtime_at_ms: Option<i64>,
    pub end_eligibility: EndEligibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPurpose {
    Classify,
    Format,
    Edit,
    Schedule,
    Report,
    Learning,
    PromptReview,
    Archive,
    Handling,
    HandlingLearning,
    AdviceRevision,
    MessageFilter,
}

impl AiPurpose {
    pub const ALL: [Self; 12] = [
        Self::Classify,
        Self::Format,
        Self::Edit,
        Self::Schedule,
        Self::Report,
        Self::Learning,
        Self::PromptReview,
        Self::Archive,
        Self::Handling,
        Self::HandlingLearning,
        Self::AdviceRevision,
        Self::MessageFilter,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiTrigger {
    Manual,
    Scheduled,
    Boundary,
    QueuedArchive,
    Retry,
    Filter,
    ProviderCheck,
}

impl AiTrigger {
    pub const ALL: [Self; 7] = [
        Self::Manual,
        Self::Scheduled,
        Self::Boundary,
        Self::QueuedArchive,
        Self::Retry,
        Self::Filter,
        Self::ProviderCheck,
    ];
}

/// Read-only projection of the archive module's authoritative attempt state.
///
/// This value is deliberately **not** persisted in [`WorkRuntime`]: the archive
/// store remains the one source of truth. The integration maps its stored status
/// into this enum when it dispatches the already-claimed end archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveAttemptStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl ArchiveAttemptStatus {
    /// Only a queued/running attempt may dispatch AI. A succeeded or failed
    /// attempt is terminal for gate purposes.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// Authoritative read-only lookup for the narrow end-archive gate exception.
///
/// The integration must implement this over the archive module's committed
/// attempt state and read it in the same dispatch critical section that reserves
/// or runs the attempt. It must not be a gate-owned cache, UI hint, or a
/// caller-supplied allow flag. Missing attempts fail closed.
pub trait EndArchiveAttemptLookup {
    fn end_archive_attempt_status(&self, attempt_id: &str) -> Option<ArchiveAttemptStatus>;
}

/// Explicit fail-closed authority for call sites that cannot have an end-archive
/// exception (tests, dry runs). Production end-archive dispatch must pass the
/// archive module's real lookup instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoArchiveAttempts;

impl EndArchiveAttemptLookup for NoArchiveAttempts {
    fn end_archive_attempt_status(&self, _attempt_id: &str) -> Option<ArchiveAttemptStatus> {
        None
    }
}

/// The id is checked against a persisted end-archive claim **and** the
/// authoritative archive attempt status. This is not a caller-selected allow
/// flag; a missing/mismatched id or a terminal attempt cannot bypass the gate.
#[derive(Debug, Clone, Copy)]
pub struct AiGateRequest<'a> {
    pub purpose: AiPurpose,
    pub trigger: AiTrigger,
    pub at_ms: i64,
    pub end_attempt_id: Option<&'a str>,
}

impl<'a> AiGateRequest<'a> {
    pub fn new(purpose: AiPurpose, trigger: AiTrigger, at_ms: i64) -> Self {
        Self {
            purpose,
            trigger,
            at_ms,
            end_attempt_id: None,
        }
    }

    pub fn with_end_attempt_id(mut self, attempt_id: &'a str) -> Self {
        self.end_attempt_id = Some(attempt_id);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCode {
    AllowedManual,
    AllowedWorking,
    AllowedOvertime,
    AllowedEndArchiveClaim,
    BlockedOffDuty,
    BlockedHoliday,
    BlockedCalendarUnknown,
    EndArchiveClaimRequired,
    EndArchiveAttemptInactive,
}

impl GateCode {
    pub fn message(self) -> &'static str {
        match self {
            Self::AllowedManual => "allowed: explicit manual operation",
            Self::AllowedWorking => "allowed: working",
            Self::AllowedOvertime => "allowed: overtime",
            Self::AllowedEndArchiveClaim => "allowed: persisted end-boundary archive claim",
            Self::BlockedOffDuty => "blocked: off duty; enable overtime to allow AI",
            Self::BlockedHoliday => "blocked: holiday; enable overtime to allow AI",
            Self::BlockedCalendarUnknown => {
                "blocked: calendar unknown; confirm the day or enable overtime"
            }
            Self::EndArchiveClaimRequired => {
                "blocked: end archive requires its persisted boundary claim"
            }
            Self::EndArchiveAttemptInactive => {
                "blocked: end archive attempt is no longer queued or running"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateException {
    pub business_day: NaiveDate,
    pub attempt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDecision {
    pub allowed: bool,
    pub code: GateCode,
    pub business_date: NaiveDate,
    pub status: WorkStatus,
    pub can_enable_overtime: bool,
    pub exception: Option<GateException>,
}

/// The one work-time gate for all model purposes. Explicit manual operations
/// are allowed at any time; provider, quota and permission checks remain separate.
///
/// The sole exception is `Archive` + `Boundary`/`QueuedArchive` with an id that
/// exactly matches a stored end claim **and** whose authoritative archive
/// attempt is `Queued` or `Running`. `Succeeded`, `Failed`, and missing attempts
/// never authorize a new model call. The status is read through
/// [`EndArchiveAttemptLookup`], not duplicated inside the scheduler/work state.
/// A supplied claimed-attempt id is checked before the general state allowance,
/// so a terminal attempt cannot authorize the claimed archive even while the
/// state is otherwise `Working`/`Overtime`.
pub fn gate(
    runtime: &WorkRuntime,
    schedule: &WorkSchedule,
    calendar: &CalendarData,
    attempts: &dyn EndArchiveAttemptLookup,
    request: &AiGateRequest<'_>,
) -> Result<GateDecision, WorkError> {
    let snapshot = runtime.snapshot(request.at_ms, schedule, calendar)?;
    if request.trigger == AiTrigger::Manual && request.end_attempt_id.is_none() {
        return Ok(GateDecision {
            allowed: true,
            code: GateCode::AllowedManual,
            business_date: snapshot.business_date,
            status: snapshot.status,
            can_enable_overtime: false,
            exception: None,
        });
    }
    let exception_shape = is_end_archive_exception_shape(request);

    // A supplied claimed-attempt id is authoritative even while the work state
    // is otherwise AI-enabled. A succeeded/failed attempt must never dispatch
    // the claimed end archive; missing claims fall through so a plain working
    // archive is still governed by the normal state allowance below.
    if exception_shape && request.end_attempt_id.is_some() {
        match check_end_claim(runtime, attempts, request) {
            EndClaimCheck::Allowed(exception) => {
                return Ok(GateDecision {
                    allowed: true,
                    code: GateCode::AllowedEndArchiveClaim,
                    business_date: snapshot.business_date,
                    status: snapshot.status,
                    can_enable_overtime: false,
                    exception: Some(exception),
                });
            }
            EndClaimCheck::AttemptInactive => {
                return Ok(GateDecision {
                    allowed: false,
                    code: GateCode::EndArchiveAttemptInactive,
                    business_date: snapshot.business_date,
                    status: snapshot.status,
                    can_enable_overtime: !snapshot.status.ai_allowed(),
                    exception: None,
                });
            }
            EndClaimCheck::ClaimRequired => {
                return Ok(GateDecision {
                    allowed: false,
                    code: GateCode::EndArchiveClaimRequired,
                    business_date: snapshot.business_date,
                    status: snapshot.status,
                    can_enable_overtime: !snapshot.status.ai_allowed(),
                    exception: None,
                })
            }
        }
    }

    if snapshot.status.ai_allowed() {
        let code = if snapshot.status == WorkStatus::Overtime {
            GateCode::AllowedOvertime
        } else {
            GateCode::AllowedWorking
        };
        return Ok(GateDecision {
            allowed: true,
            code,
            business_date: snapshot.business_date,
            status: snapshot.status,
            can_enable_overtime: false,
            exception: None,
        });
    }

    let code = if exception_shape {
        GateCode::EndArchiveClaimRequired
    } else {
        match snapshot.status {
            WorkStatus::OffDuty => GateCode::BlockedOffDuty,
            WorkStatus::Holiday => GateCode::BlockedHoliday,
            WorkStatus::CalendarUnknown => GateCode::BlockedCalendarUnknown,
            WorkStatus::Working | WorkStatus::Overtime => unreachable!(),
        }
    };
    Ok(GateDecision {
        allowed: false,
        code,
        business_date: snapshot.business_date,
        status: snapshot.status,
        can_enable_overtime: true,
        exception: None,
    })
}

fn is_end_archive_exception_shape(request: &AiGateRequest<'_>) -> bool {
    request.purpose == AiPurpose::Archive
        && matches!(
            request.trigger,
            AiTrigger::Boundary | AiTrigger::QueuedArchive
        )
}

enum EndClaimCheck {
    Allowed(GateException),
    ClaimRequired,
    AttemptInactive,
}

fn check_end_claim(
    runtime: &WorkRuntime,
    attempts: &dyn EndArchiveAttemptLookup,
    request: &AiGateRequest<'_>,
) -> EndClaimCheck {
    let Some(attempt_id) = request.end_attempt_id else {
        return EndClaimCheck::ClaimRequired;
    };
    let Some((business_day, claim)) = runtime.find_end_claim(attempt_id) else {
        return EndClaimCheck::ClaimRequired;
    };
    let Some(status) = attempts.end_archive_attempt_status(attempt_id) else {
        return EndClaimCheck::ClaimRequired;
    };
    if !status.is_active() {
        return EndClaimCheck::AttemptInactive;
    }
    if !claim.eligibility.is_eligible() || claim.claimed_at_ms > request.at_ms {
        return EndClaimCheck::ClaimRequired;
    }
    EndClaimCheck::Allowed(GateException {
        business_day,
        attempt_id: claim.attempt_id,
    })
}

/// Convert a Unix-millisecond timestamp to the Shanghai local date.
pub fn shanghai_date(at_ms: i64) -> Result<NaiveDate, WorkError> {
    Ok(shanghai_local(at_ms)?.date_naive())
}

fn shanghai_date_and_minute(at_ms: i64) -> Result<(NaiveDate, u16), WorkError> {
    let local = shanghai_local(at_ms)?;
    Ok((
        local.date_naive(),
        (local.hour() as u16) * 60 + local.minute() as u16,
    ))
}

fn shanghai_local(at_ms: i64) -> Result<DateTime<FixedOffset>, WorkError> {
    let utc = DateTime::<Utc>::from_timestamp_millis(at_ms)
        .ok_or(WorkError::InvalidTimestampMs(at_ms))?;
    let offset = FixedOffset::east_opt((SHANGHAI_OFFSET_MS / 1000) as i32)
        .expect("fixed +08:00 offset is valid");
    let local = utc.with_timezone(&offset);
    validate_work_date(local.date_naive())?;
    Ok(local)
}

fn local_timestamp_ms(day: NaiveDate, minute_of_day: u16) -> Result<i64, WorkError> {
    validate_work_date(day)?;
    let hour = (minute_of_day / 60) as u32;
    let minute = (minute_of_day % 60) as u32;
    let naive = day
        .and_hms_opt(hour, minute, 0)
        .ok_or(WorkError::DateOutOfRange(day))?;
    Ok(naive.and_utc().timestamp_millis() - SHANGHAI_OFFSET_MS)
}

fn validate_timestamp_ms(at_ms: i64) -> Result<(), WorkError> {
    shanghai_local(at_ms).map(|_| ())
}

fn validate_work_date(day: NaiveDate) -> Result<(), WorkError> {
    if (WORK_MIN_YEAR..=WORK_MAX_YEAR).contains(&day.year()) {
        Ok(())
    } else {
        Err(WorkError::DateOutOfRange(day))
    }
}

fn validate_claim_id(id: &str) -> Result<(), WorkError> {
    if id.is_empty() || id.chars().count() > MAX_CLAIM_ID_CHARS || id.chars().any(char::is_control)
    {
        Err(WorkError::InvalidState("claim id is empty or invalid"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::holiday::{CalendarSource, ManualOverride};
    use crate::test_support::{calendar, date as d, ts_ms};
    use serde_json::json;

    fn schedule() -> WorkSchedule {
        WorkSchedule::parse("08:00", "20:00").unwrap()
    }

    struct OneAttempt {
        id: &'static str,
        status: ArchiveAttemptStatus,
    }

    impl EndArchiveAttemptLookup for OneAttempt {
        fn end_archive_attempt_status(&self, attempt_id: &str) -> Option<ArchiveAttemptStatus> {
            (attempt_id == self.id).then_some(self.status)
        }
    }

    #[test]
    fn schedule_parsing_rejects_invalid_and_equal_times() {
        let schedule = WorkSchedule::parse("08:00", "20:00").unwrap();
        assert_eq!(schedule.start_minute(), 8 * 60);
        assert_eq!(schedule.end_minute(), 20 * 60);
        assert!(!schedule.is_overnight());

        for invalid in ["8:00", "08:0", "0800", "24:00", "08:60", "08:00:00", ""] {
            assert!(
                WorkSchedule::parse(invalid, "20:00").is_err(),
                "start {invalid:?} must be rejected"
            );
        }
        assert!(matches!(
            WorkSchedule::parse("08:00", "08:00"),
            Err(WorkError::EqualWorkTimes)
        ));
        assert!(WorkSchedule::from_minutes(0, 1440).is_err());
        assert!(WorkSchedule::from_minutes(600, 600).is_err());

        let equal_json: Result<WorkSchedule, _> =
            serde_json::from_str(r#"{"start_minute":480,"end_minute":480}"#);
        assert!(equal_json.is_err());
        let unknown_json: Result<WorkSchedule, _> =
            serde_json::from_str(r#"{"start_minute":480,"end_minute":1200,"x":1}"#);
        assert!(unknown_json.is_err());
    }

    #[test]
    fn business_date_uses_most_recent_start_exactly() {
        let schedule = schedule();
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 2, 7, 59)).unwrap(),
            d(2025, 1, 1)
        );
        for (hour, minute) in [(8, 0), (12, 0), (19, 59), (20, 0), (23, 59)] {
            assert_eq!(
                schedule
                    .business_date(ts_ms(2025, 1, 2, hour, minute))
                    .unwrap(),
                d(2025, 1, 2),
                "time {hour:02}:{minute:02}"
            );
        }
    }

    #[test]
    fn business_date_covers_overnight_and_year_end() {
        let schedule = WorkSchedule::parse("22:00", "06:00").unwrap();
        assert!(schedule.is_overnight());
        assert_eq!(
            schedule.start_timestamp_ms(d(2025, 1, 2)).unwrap(),
            ts_ms(2025, 1, 2, 22, 0)
        );
        assert_eq!(
            schedule.end_timestamp_ms(d(2025, 1, 2)).unwrap(),
            ts_ms(2025, 1, 3, 6, 0)
        );
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 2, 21, 59)).unwrap(),
            d(2025, 1, 1)
        );
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 2, 22, 0)).unwrap(),
            d(2025, 1, 2)
        );
        for (hour, minute) in [(0, 0), (6, 0), (7, 0), (21, 59)] {
            assert_eq!(
                schedule
                    .business_date(ts_ms(2025, 1, 3, hour, minute))
                    .unwrap(),
                d(2025, 1, 2),
                "time {hour:02}:{minute:02}"
            );
        }
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 1, 0, 30)).unwrap(),
            d(2024, 12, 31)
        );
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 1, 6, 0)).unwrap(),
            d(2024, 12, 31)
        );
        assert_eq!(
            schedule.business_date(ts_ms(2025, 1, 1, 22, 0)).unwrap(),
            d(2025, 1, 1)
        );
    }

    #[test]
    fn statuses_cover_work_weekend_makeup_holiday_unknown_and_manual() {
        let calendar = calendar();
        let schedule = schedule();
        let runtime = WorkRuntime::new();

        let working = runtime
            .snapshot(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(working.status, WorkStatus::Working);
        assert_eq!(working.calendar_day.kind, CalendarKind::Workday);
        assert_eq!(working.business_date, d(2025, 1, 2));

        let at_start = runtime
            .snapshot(ts_ms(2025, 1, 2, 8, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(at_start.status, WorkStatus::Working);
        let at_end = runtime
            .snapshot(ts_ms(2025, 1, 2, 20, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(at_end.status, WorkStatus::OffDuty);

        let weekend = runtime
            .snapshot(ts_ms(2025, 1, 4, 10, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(weekend.status, WorkStatus::OffDuty);
        assert_eq!(weekend.calendar_day.kind, CalendarKind::Weekend);

        let makeup = runtime
            .snapshot(ts_ms(2025, 1, 11, 10, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(makeup.status, WorkStatus::Working);
        assert_eq!(makeup.calendar_day.source, Some(CalendarSource::Cache));

        let holiday = runtime
            .snapshot(ts_ms(2025, 1, 1, 10, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(holiday.status, WorkStatus::Holiday);
        assert_eq!(holiday.calendar_day.kind, CalendarKind::Holiday);

        let unknown = runtime
            .snapshot(ts_ms(2026, 1, 5, 10, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(unknown.status, WorkStatus::CalendarUnknown);
        assert_eq!(unknown.calendar_day.kind, CalendarKind::Unknown);

        let mut manual = calendar.clone();
        manual
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 2),
                    crate::holiday::DayKind::Holiday,
                    ts_ms(2025, 1, 2, 6, 0),
                    Some("synthetic correction".to_owned()),
                )
                .unwrap(),
            )
            .unwrap();
        let manual_holiday = runtime
            .snapshot(ts_ms(2025, 1, 2, 10, 0), &schedule, &manual)
            .unwrap();
        assert_eq!(manual_holiday.status, WorkStatus::Holiday);
        assert_eq!(
            manual_holiday.calendar_day.source,
            Some(CalendarSource::Manual)
        );

        manual
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 4),
                    crate::holiday::DayKind::Workday,
                    ts_ms(2025, 1, 4, 6, 0),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let manual_workday = runtime
            .snapshot(ts_ms(2025, 1, 4, 10, 0), &schedule, &manual)
            .unwrap();
        assert_eq!(manual_workday.status, WorkStatus::Working);
    }

    #[test]
    fn overnight_shift_uses_its_start_business_day_calendar_across_midnight() {
        let schedule = WorkSchedule::parse("22:00", "06:00").unwrap();
        let runtime = WorkRuntime::new();

        // Jan 2 is a workday; Jan 3 is explicitly a holiday. At 00:30 the
        // overnight shift must still be Working under the Jan 2 business day.
        let mut holiday_next_day = calendar();
        holiday_next_day
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 3),
                    crate::holiday::DayKind::Holiday,
                    ts_ms(2025, 1, 2, 20, 0),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let at_ms = ts_ms(2025, 1, 3, 0, 30);
        let snapshot = runtime
            .snapshot(at_ms, &schedule, &holiday_next_day)
            .unwrap();
        assert_eq!(snapshot.business_date, d(2025, 1, 2));
        assert_eq!(snapshot.calendar_day.date, d(2025, 1, 2));
        assert_eq!(snapshot.status, WorkStatus::Working);

        // Jan 2 is a holiday; Jan 3 is a workday. Without overtime the same
        // overnight time is a holiday, not an ordinary midnight workday.
        let mut holiday_start_day = calendar();
        holiday_start_day
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 2),
                    crate::holiday::DayKind::Holiday,
                    ts_ms(2025, 1, 1, 20, 0),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let snapshot = runtime
            .snapshot(at_ms, &schedule, &holiday_start_day)
            .unwrap();
        assert_eq!(snapshot.business_date, d(2025, 1, 2));
        assert_eq!(snapshot.calendar_day.date, d(2025, 1, 2));
        assert_eq!(snapshot.status, WorkStatus::Holiday);
    }

    #[test]
    fn overtime_persists_restart_same_day_and_expires_next_business_day() {
        let calendar = calendar();
        let schedule = schedule();
        let start = ts_ms(2025, 1, 1, 10, 0);
        let mut runtime = WorkRuntime::new();
        runtime.begin_overtime(start, &schedule).unwrap();
        assert_eq!(
            runtime
                .snapshot(start, &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Overtime
        );
        // A manual state enabled at 10:00 must not appear active before 10:00.
        assert_eq!(
            runtime
                .snapshot(ts_ms(2025, 1, 1, 9, 59), &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Holiday
        );

        let bytes = runtime.to_bytes().unwrap();
        let mut restored = WorkRuntime::from_bytes(&bytes).unwrap();
        assert_eq!(restored, runtime);
        assert_eq!(
            restored
                .snapshot(ts_ms(2025, 1, 1, 11, 0), &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Overtime
        );

        // It stays active until the next business day starts (08:00 here).
        assert_eq!(
            restored
                .snapshot(ts_ms(2025, 1, 2, 7, 59), &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Overtime
        );
        let expired = restored
            .observe(ts_ms(2025, 1, 2, 8, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(expired.status, WorkStatus::Working);
        assert!(restored.active_overtime.is_none());
        assert!(!restored
            .expire_overtime(ts_ms(2025, 1, 2, 8, 1), &schedule)
            .unwrap());
        assert_eq!(
            restored
                .snapshot(ts_ms(2025, 1, 2, 8, 1), &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Working
        );
        let prior = restored.day_record(d(2025, 1, 1)).unwrap();
        assert!(prior.first_overtime_at_ms.is_some());
        assert_eq!(
            restored.end_eligibility(d(2025, 1, 1), &schedule).unwrap(),
            EndEligibility::OvertimeBeforeEnd
        );
    }

    #[test]
    fn exited_overtime_still_makes_end_eligible_but_after_end_does_not() {
        let calendar = calendar();
        let schedule = schedule();

        let mut exited = WorkRuntime::new();
        exited
            .begin_overtime(ts_ms(2025, 1, 1, 9, 0), &schedule)
            .unwrap();
        exited.end_overtime();
        assert_eq!(
            exited
                .snapshot(ts_ms(2025, 1, 1, 12, 0), &schedule, &calendar)
                .unwrap()
                .status,
            WorkStatus::Holiday
        );
        assert_eq!(
            exited.end_eligibility(d(2025, 1, 1), &schedule).unwrap(),
            EndEligibility::OvertimeBeforeEnd
        );

        let mut worked = WorkRuntime::new();
        worked
            .observe(ts_ms(2025, 1, 2, 9, 0), &schedule, &calendar)
            .unwrap();
        assert_eq!(
            worked.end_eligibility(d(2025, 1, 2), &schedule).unwrap(),
            EndEligibility::WorkedBeforeEnd
        );

        let mut after_end = WorkRuntime::new();
        after_end
            .begin_overtime(ts_ms(2025, 1, 1, 20, 0), &schedule)
            .unwrap();
        assert_eq!(
            after_end.end_eligibility(d(2025, 1, 1), &schedule).unwrap(),
            EndEligibility::NotEligible
        );
        after_end.end_overtime();
        after_end
            .begin_overtime(ts_ms(2025, 1, 1, 20, 1), &schedule)
            .unwrap();
        assert_eq!(
            after_end.end_eligibility(d(2025, 1, 1), &schedule).unwrap(),
            EndEligibility::NotEligible
        );
    }

    #[test]
    fn runtime_serialization_is_versioned_and_strict() {
        let schedule = schedule();
        let calendar = calendar();
        let mut runtime = WorkRuntime::new();
        runtime
            .observe(ts_ms(2025, 1, 2, 9, 0), &schedule, &calendar)
            .unwrap();
        runtime.day_record_mut(d(2025, 1, 2)).end_claim = Some(EndArchiveClaim {
            attempt_id: "end-2025-01-02".to_owned(),
            claimed_at_ms: ts_ms(2025, 1, 2, 20, 0),
            eligibility: EndEligibility::WorkedBeforeEnd,
        });
        let bytes = runtime.to_bytes().unwrap();
        let restored = WorkRuntime::from_bytes(&bytes).unwrap();
        assert_eq!(restored, runtime);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = json!(99);
        assert!(matches!(
            WorkRuntime::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(WorkError::UnsupportedSchemaVersion { found: 99, .. })
        ));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value.as_object_mut().unwrap().remove("schema_version");
        assert!(matches!(
            WorkRuntime::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(WorkError::MalformedState(_))
        ));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["extra"] = json!(true);
        assert!(matches!(
            WorkRuntime::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(WorkError::MalformedState(_))
        ));
    }

    #[test]
    fn all_ai_purposes_are_blocked_off_duty_and_allowed_while_working() {
        let calendar = calendar();
        let schedule = schedule();
        let runtime = WorkRuntime::new();
        let off_duty = ts_ms(2025, 1, 2, 21, 0);

        for purpose in AiPurpose::ALL {
            for trigger in AiTrigger::ALL {
                let request = AiGateRequest::new(purpose, trigger, off_duty);
                let decision =
                    gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request).unwrap();
                assert_eq!(
                    decision.allowed,
                    trigger == AiTrigger::Manual,
                    "{purpose:?}/{trigger:?}"
                );
                assert_eq!(decision.can_enable_overtime, trigger != AiTrigger::Manual);
            }
        }

        let working = ts_ms(2025, 1, 2, 10, 0);
        for purpose in AiPurpose::ALL {
            for trigger in AiTrigger::ALL {
                let request = AiGateRequest::new(purpose, trigger, working);
                let decision =
                    gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request).unwrap();
                assert!(decision.allowed, "{purpose:?}/{trigger:?} must be allowed");
            }
        }

        let holiday = ts_ms(2025, 1, 1, 10, 0);
        for purpose in AiPurpose::ALL {
            let request = AiGateRequest::new(purpose, AiTrigger::Manual, holiday);
            assert!(
                gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request)
                    .unwrap()
                    .allowed
            );
        }
        let unknown = ts_ms(2026, 1, 5, 10, 0);
        for purpose in AiPurpose::ALL {
            let request = AiGateRequest::new(purpose, AiTrigger::Manual, unknown);
            assert_eq!(
                gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request)
                    .unwrap()
                    .code,
                GateCode::AllowedManual
            );
        }
        for purpose in AiPurpose::ALL {
            let request = AiGateRequest::new(purpose, AiTrigger::Scheduled, unknown);
            assert!(
                !gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request)
                    .unwrap()
                    .allowed
            );
        }
    }

    #[test]
    fn overtime_state_allows_all_purposes() {
        let calendar = calendar();
        let schedule = schedule();
        let mut runtime = WorkRuntime::new();
        runtime
            .begin_overtime(ts_ms(2025, 1, 2, 21, 0), &schedule)
            .unwrap();
        for purpose in AiPurpose::ALL {
            for trigger in AiTrigger::ALL {
                let request = AiGateRequest::new(purpose, trigger, ts_ms(2025, 1, 2, 21, 30));
                let decision =
                    gate(&runtime, &schedule, &calendar, &NoArchiveAttempts, &request).unwrap();
                assert!(decision.allowed);
                assert_eq!(decision.status, WorkStatus::Overtime);
            }
        }
    }

    #[test]
    fn only_a_stored_end_archive_claim_opens_the_narrow_exception() {
        let calendar = calendar();
        let schedule = schedule();
        let mut runtime = WorkRuntime::new();
        let claimed_at_ms = ts_ms(2025, 1, 1, 20, 0);
        runtime.day_record_mut(d(2025, 1, 1)).end_eligibility =
            Some(EndEligibility::OvertimeBeforeEnd);
        runtime.day_record_mut(d(2025, 1, 1)).end_claim = Some(EndArchiveClaim {
            attempt_id: "end-2025-01-01".to_owned(),
            claimed_at_ms,
            eligibility: EndEligibility::OvertimeBeforeEnd,
        });
        let at_ms = ts_ms(2025, 1, 1, 20, 5);
        let queued = OneAttempt {
            id: "end-2025-01-01",
            status: ArchiveAttemptStatus::Queued,
        };

        let claim_request = AiGateRequest::new(AiPurpose::Archive, AiTrigger::QueuedArchive, at_ms)
            .with_end_attempt_id("end-2025-01-01");
        let decision = gate(&runtime, &schedule, &calendar, &queued, &claim_request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.code, GateCode::AllowedEndArchiveClaim);
        assert_eq!(decision.status, WorkStatus::Holiday);
        let exception = decision.exception.unwrap();
        assert_eq!(exception.business_day, d(2025, 1, 1));
        assert_eq!(exception.attempt_id, "end-2025-01-01");

        let running = OneAttempt {
            id: "end-2025-01-01",
            status: ArchiveAttemptStatus::Running,
        };
        let boundary_claim = AiGateRequest::new(AiPurpose::Archive, AiTrigger::Boundary, at_ms)
            .with_end_attempt_id("end-2025-01-01");
        assert!(
            gate(&runtime, &schedule, &calendar, &running, &boundary_claim)
                .unwrap()
                .allowed
        );

        let no_id = AiGateRequest::new(AiPurpose::Archive, AiTrigger::QueuedArchive, at_ms);
        let no_id_decision = gate(&runtime, &schedule, &calendar, &queued, &no_id).unwrap();
        assert!(!no_id_decision.allowed);
        assert_eq!(no_id_decision.code, GateCode::EndArchiveClaimRequired);

        let wrong_id = AiGateRequest::new(AiPurpose::Archive, AiTrigger::QueuedArchive, at_ms)
            .with_end_attempt_id("end-other");
        assert!(
            !gate(&runtime, &schedule, &calendar, &queued, &wrong_id)
                .unwrap()
                .allowed
        );

        for terminal in [
            ArchiveAttemptStatus::Succeeded,
            ArchiveAttemptStatus::Failed,
        ] {
            let attempts = OneAttempt {
                id: "end-2025-01-01",
                status: terminal,
            };
            let decision = gate(&runtime, &schedule, &calendar, &attempts, &claim_request).unwrap();
            assert!(
                !decision.allowed,
                "a {terminal:?} attempt must not authorize AI"
            );
            assert_eq!(decision.code, GateCode::EndArchiveAttemptInactive);
            assert!(decision.exception.is_none());
        }

        let missing_attempt = NoArchiveAttempts;
        let missing_decision = gate(
            &runtime,
            &schedule,
            &calendar,
            &missing_attempt,
            &claim_request,
        )
        .unwrap();
        assert!(!missing_decision.allowed);
        assert_eq!(
            missing_decision.code,
            GateCode::EndArchiveClaimRequired,
            "a claim without a queued/running attempt must fail closed"
        );

        for purpose in AiPurpose::ALL {
            if purpose == AiPurpose::Archive {
                continue;
            }
            let request = AiGateRequest::new(purpose, AiTrigger::QueuedArchive, at_ms)
                .with_end_attempt_id("end-2025-01-01");
            let decision = gate(&runtime, &schedule, &calendar, &queued, &request).unwrap();
            assert!(!decision.allowed, "claim must not open {purpose:?}");
        }

        for trigger in [
            AiTrigger::Manual,
            AiTrigger::Scheduled,
            AiTrigger::Retry,
            AiTrigger::Filter,
            AiTrigger::ProviderCheck,
        ] {
            let request = AiGateRequest::new(AiPurpose::Archive, trigger, at_ms)
                .with_end_attempt_id("end-2025-01-01");
            let decision = gate(&runtime, &schedule, &calendar, &queued, &request).unwrap();
            assert!(!decision.allowed, "claim must not open trigger {trigger:?}");
        }

        // A claim is not valid before it was created.
        let future_claim = AiGateRequest::new(
            AiPurpose::Archive,
            AiTrigger::QueuedArchive,
            claimed_at_ms - 1,
        )
        .with_end_attempt_id("end-2025-01-01");
        assert!(
            !gate(&runtime, &schedule, &calendar, &queued, &future_claim)
                .unwrap()
                .allowed
        );
    }

    #[test]
    fn terminal_claimed_attempt_never_authorizes_even_while_working() {
        let calendar = calendar();
        let schedule = schedule();
        let mut runtime = WorkRuntime::new();
        runtime
            .observe(ts_ms(2025, 1, 2, 9, 0), &schedule, &calendar)
            .unwrap();
        runtime.day_record_mut(d(2025, 1, 2)).end_claim = Some(EndArchiveClaim {
            attempt_id: "end-2025-01-02".to_owned(),
            claimed_at_ms: ts_ms(2025, 1, 2, 9, 30),
            eligibility: EndEligibility::WorkedBeforeEnd,
        });
        let at_ms = ts_ms(2025, 1, 2, 10, 0);
        let request = AiGateRequest::new(AiPurpose::Archive, AiTrigger::QueuedArchive, at_ms)
            .with_end_attempt_id("end-2025-01-02");

        for terminal in [
            ArchiveAttemptStatus::Succeeded,
            ArchiveAttemptStatus::Failed,
        ] {
            let attempts = OneAttempt {
                id: "end-2025-01-02",
                status: terminal,
            };
            let decision = gate(&runtime, &schedule, &calendar, &attempts, &request).unwrap();
            assert_eq!(decision.status, WorkStatus::Working);
            assert!(!decision.allowed);
            assert_eq!(decision.code, GateCode::EndArchiveAttemptInactive);
            assert!(!decision.can_enable_overtime);
        }

        let queued = OneAttempt {
            id: "end-2025-01-02",
            status: ArchiveAttemptStatus::Queued,
        };
        let allowed = gate(&runtime, &schedule, &calendar, &queued, &request).unwrap();
        assert!(allowed.allowed);
        assert_eq!(allowed.code, GateCode::AllowedEndArchiveClaim);
    }

    #[test]
    fn scanning_is_not_part_of_the_ai_purpose_set() {
        // Scanning has no gate function in this crate; this length is a guard
        // that adding a Scan purpose to the AI enum is an explicit review event.
        assert_eq!(AiPurpose::ALL.len(), 12);
        assert_eq!(AiTrigger::ALL.len(), 7);
    }
}
