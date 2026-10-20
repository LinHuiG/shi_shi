//! Deterministic boundary scheduler. It owns no clock, network or model call:
//! the live previous tick and current time are supplied in Unix **milliseconds**
//! (matching the application's `model::now()`), and it only mutates persisted
//! claims/observations in [`SchedulerState`].

use std::collections::BTreeSet;
use std::fmt;

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::holiday::CalendarData;
use crate::work_state::{
    shanghai_date, EndArchiveClaim, MorningClaim, WorkError, WorkRuntime, WorkSchedule,
    WorkSnapshot, WorkStatus, WORK_MAX_YEAR, WORK_MIN_YEAR,
};

pub const SCHEDULER_STATE_SCHEMA_VERSION: u32 = 1;
/// A crossing is "live" only when the two live ticks are this close, in Unix
/// milliseconds. A larger gap is conservatively a missed boundary, not a
/// catch-up.
pub const MAX_TICK_GAP_MS: i64 = 120_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerState {
    pub schema_version: u32,
    pub runtime: WorkRuntime,
    pub last_schedule: Option<WorkSchedule>,
    /// Monotonic anti-retro guard, in Unix milliseconds. It is not a catch-up
    /// cursor: startup still supplies `previous_tick_at_ms = None`, and a
    /// backward clock cannot make a boundary that was already passed eligible
    /// again.
    clock_floor_ms: Option<i64>,
}

impl Default for SchedulerState {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerState {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEDULER_STATE_SCHEMA_VERSION,
            runtime: WorkRuntime::new(),
            last_schedule: None,
            clock_floor_ms: None,
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WorkError> {
        let state: Self =
            serde_json::from_slice(bytes).map_err(|e| WorkError::MalformedState(e.to_string()))?;
        state.validate()?;
        Ok(state)
    }

    /// Validate the scheduler schema and its nested work-runtime version even
    /// when the value was deserialized as part of a larger application state.
    pub fn validate(&self) -> Result<(), WorkError> {
        if self.schema_version != SCHEDULER_STATE_SCHEMA_VERSION {
            return Err(WorkError::UnsupportedSchemaVersion {
                found: self.schema_version,
                expected: SCHEDULER_STATE_SCHEMA_VERSION,
            });
        }
        self.runtime.validate()?;
        if let Some(floor_ms) = self.clock_floor_ms {
            shanghai_date(floor_ms)?;
        }
        if let Some(schedule) = &self.last_schedule {
            if schedule.is_overnight() != (schedule.start_minute() > schedule.end_minute()) {
                return Err(WorkError::InvalidState("invalid stored work schedule"));
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, WorkError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|e| WorkError::MalformedState(e.to_string()))
    }

    /// Compute a tick against a clone without mutating `self` or persisting.
    ///
    /// This is suitable for a dry run/UI preview only. Integration dispatch
    /// must use [`Self::tick_and_commit`].
    pub fn tick_preview(&self, input: TickInput<'_>) -> Result<(Self, TickOutcome), WorkError> {
        let mut next = self.clone();
        let outcome = next.tick(input)?;
        Ok((next, outcome))
    }

    /// Atomic integration tick: clone, tick, serialize, persist, swap, dispatch.
    ///
    /// `save_atomic` receives the complete serialized next state and must
    /// durably replace the scheduler state file atomically (for example
    /// temporary file + flush + rename). It must return `Err` without leaving a
    /// partial state. On any error `self` is unchanged and no actions are
    /// returned, so callers must dispatch `outcome.actions` only after this
    /// method returns `Ok`.
    ///
    /// This commits scheduler claims before returning actions. The app must then
    /// create/reserve the archive attempt and persist that attempt (and its
    /// queued/running status) before invoking a model; the archive module owns
    /// attempt success/failure. A failed reservation must not be treated as
    /// success or auto-requeued.
    pub fn tick_and_commit<E>(
        &mut self,
        input: TickInput<'_>,
        save_atomic: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<TickOutcome, TickCommitError<E>> {
        let (next, outcome) = self.tick_preview(input).map_err(TickCommitError::Tick)?;
        let bytes = next.to_bytes().map_err(TickCommitError::Encode)?;
        save_atomic(&bytes).map_err(TickCommitError::Save)?;
        *self = next;
        Ok(outcome)
    }

    /// Private mutating implementation. Public integration code must use the
    /// preview or atomic-commit wrappers above.
    ///
    /// Startup passes `previous_tick_at_ms = None`; the scheduler then records
    /// an observation at `now_ms` and does not inspect any boundary that has
    /// already passed. Time/schedule changes, backward clocks and gaps greater
    /// than 120 s likewise suppress boundary detection for that tick.
    fn tick(&mut self, input: TickInput<'_>) -> Result<TickOutcome, WorkError> {
        // Validate `now_ms` as a real Shanghai date in the supported range.
        // It may legitimately be negative: 1900..1969 are before the Unix
        // epoch but inside the supported calendar range.
        shanghai_date(input.now_ms)?;
        let now_ms = input.now_ms;
        let previous_ms = input.previous_tick_at_ms;
        let gap_ms = previous_ms.map(|prev| now_ms.saturating_sub(prev));
        let schedule_changed = self.last_schedule.is_some_and(|old| old != *input.schedule);
        let backward = previous_ms.is_some_and(|prev| now_ms < prev);
        let stalled = previous_ms.is_some_and(|prev| now_ms == prev);
        let too_large = gap_ms.is_some_and(|gap| gap > MAX_TICK_GAP_MS);
        let live_crossing =
            previous_ms.is_some() && !backward && !stalled && !too_large && !schedule_changed;
        let guard_before = match (self.clock_floor_ms, previous_ms) {
            (Some(floor_ms), Some(previous_ms)) => Some(floor_ms.max(previous_ms)),
            (Some(floor_ms), None) => Some(floor_ms),
            (None, Some(previous_ms)) => Some(previous_ms),
            (None, None) => None,
        };

        let mut notices = Vec::new();
        if previous_ms.is_none() {
            notices.push(TickNotice::StartupNoCatchup);
        }
        if backward {
            notices.push(TickNotice::BackwardClock {
                previous_tick_at_ms: previous_ms.unwrap_or_default(),
                now_ms,
            });
        } else if stalled {
            notices.push(TickNotice::ClockDidNotAdvance);
        }
        if let Some(gap_ms) = gap_ms {
            if too_large {
                notices.push(TickNotice::GapTooLarge { gap_ms });
            }
        }
        if schedule_changed {
            notices.push(TickNotice::ScheduleChanged);
        }
        if !input.enabled {
            notices.push(TickNotice::SchedulingDisabled);
        }

        let mut actions = Vec::new();
        if input.enabled && live_crossing {
            let previous_ms = previous_ms.expect("live crossing has a previous tick");
            // Do not back-date an observation to `previous_ms`: every live
            // tick's final `observe(now_ms)` already recorded it, while
            // observing the past could expire overtime that belongs to the new
            // business day.
            let mut boundaries = boundaries_between(input.schedule, previous_ms, now_ms)?;
            if let Some(floor_ms) = guard_before {
                boundaries.retain(|boundary| boundary.at_ms > floor_ms);
            }
            boundaries.sort_by_key(|boundary| (boundary.at_ms, boundary.kind.order()));
            for boundary in boundaries {
                match boundary.kind {
                    BoundaryKind::Morning => {
                        let snapshot =
                            self.runtime
                                .observe(boundary.at_ms, input.schedule, input.calendar)?;
                        if matches!(snapshot.status, WorkStatus::Working | WorkStatus::Overtime)
                            && snapshot.calendar_day.is_workday()
                        {
                            let record = self.runtime.day_record_mut(boundary.business_day);
                            if record.morning_claim.is_none() {
                                let claim = MorningClaim {
                                    claim_id: morning_claim_id(boundary.business_day),
                                    claimed_at_ms: boundary.at_ms,
                                };
                                record.morning_claim = Some(claim.clone());
                                actions.push(SchedulerAction::MorningReport {
                                    business_day: boundary.business_day,
                                    claim,
                                });
                            }
                        }
                    }
                    BoundaryKind::End => {
                        let eligibility = self
                            .runtime
                            .end_eligibility(boundary.business_day, input.schedule)?;
                        let claim = {
                            let record = self.runtime.day_record_mut(boundary.business_day);
                            record.end_eligibility = Some(eligibility.clone());
                            if eligibility.is_eligible() && record.end_claim.is_none() {
                                let claim = EndArchiveClaim {
                                    attempt_id: end_attempt_id(boundary.business_day),
                                    claimed_at_ms: boundary.at_ms,
                                    eligibility: eligibility.clone(),
                                };
                                record.end_claim = Some(claim.clone());
                                Some(claim)
                            } else {
                                None
                            }
                        };
                        if let Some(claim) = claim {
                            actions.push(SchedulerAction::EndArchive {
                                business_day: boundary.business_day,
                                claim,
                                dispatch: if input.ai_busy {
                                    EndDispatch::QueueBusy
                                } else {
                                    EndDispatch::RunNow
                                },
                            });
                        }
                    }
                }
            }
        }

        let snapshot = self
            .runtime
            .observe(now_ms, input.schedule, input.calendar)?;
        self.last_schedule = Some(*input.schedule);
        let previous_high_water = previous_ms.unwrap_or(now_ms);
        self.clock_floor_ms = Some(
            self.clock_floor_ms
                .unwrap_or(now_ms)
                .max(previous_high_water)
                .max(now_ms),
        );
        Ok(TickOutcome {
            snapshot,
            actions,
            notices,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    pub previous_tick_at_ms: Option<i64>,
    pub now_ms: i64,
    pub schedule: &'a WorkSchedule,
    pub calendar: &'a CalendarData,
    pub ai_busy: bool,
    pub enabled: bool,
}

impl<'a> TickInput<'a> {
    pub fn new(
        previous_tick_at_ms: Option<i64>,
        now_ms: i64,
        schedule: &'a WorkSchedule,
        calendar: &'a CalendarData,
    ) -> Self {
        Self {
            previous_tick_at_ms,
            now_ms,
            schedule,
            calendar,
            ai_busy: false,
            enabled: true,
        }
    }
}

/// Failure of an atomic scheduler tick. The current state is unchanged for
/// every variant, and no actions are returned on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickCommitError<E> {
    Tick(WorkError),
    Encode(WorkError),
    Save(E),
}

impl<E: fmt::Display> fmt::Display for TickCommitError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tick(error) => write!(f, "scheduler tick failed: {error}"),
            Self::Encode(error) => write!(f, "scheduler state encoding failed: {error}"),
            Self::Save(error) => write!(f, "scheduler state save failed: {error}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display + 'static> std::error::Error for TickCommitError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tick(error) | Self::Encode(error) => Some(error),
            Self::Save(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    pub snapshot: WorkSnapshot,
    pub actions: Vec<SchedulerAction>,
    pub notices: Vec<TickNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndDispatch {
    RunNow,
    QueueBusy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickNotice {
    StartupNoCatchup,
    ClockDidNotAdvance,
    BackwardClock {
        previous_tick_at_ms: i64,
        now_ms: i64,
    },
    GapTooLarge {
        gap_ms: i64,
    },
    ScheduleChanged,
    SchedulingDisabled,
}

/// Requests only. The scheduler never reports a report/archive as successful;
/// the app reserves/executes and the archive module owns attempt status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerAction {
    MorningReport {
        business_day: NaiveDate,
        claim: MorningClaim,
    },
    EndArchive {
        business_day: NaiveDate,
        claim: EndArchiveClaim,
        dispatch: EndDispatch,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryKind {
    Morning,
    End,
}

impl BoundaryKind {
    fn order(self) -> u8 {
        match self {
            Self::Morning => 0,
            Self::End => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Boundary {
    at_ms: i64,
    business_day: NaiveDate,
    kind: BoundaryKind,
}

fn boundaries_between(
    schedule: &WorkSchedule,
    previous_ms: i64,
    now_ms: i64,
) -> Result<Vec<Boundary>, WorkError> {
    if now_ms <= previous_ms {
        return Ok(Vec::new());
    }
    let previous_date = shanghai_date(previous_ms)?;
    let now_date = shanghai_date(now_ms)?;
    let mut candidate_dates = BTreeSet::new();
    for date in [previous_date, now_date] {
        for candidate in [date.pred_opt(), Some(date), date.succ_opt()]
            .into_iter()
            .flatten()
        {
            if (WORK_MIN_YEAR..=WORK_MAX_YEAR).contains(&candidate.year()) {
                candidate_dates.insert(candidate);
            }
        }
    }

    let mut result = Vec::new();
    for business_day in candidate_dates {
        let start_ms = schedule.start_timestamp_ms(business_day)?;
        if start_ms > previous_ms && start_ms <= now_ms {
            result.push(Boundary {
                at_ms: start_ms,
                business_day,
                kind: BoundaryKind::Morning,
            });
        }
        let end_ms = schedule.end_timestamp_ms(business_day)?;
        if end_ms > previous_ms && end_ms <= now_ms {
            result.push(Boundary {
                at_ms: end_ms,
                business_day,
                kind: BoundaryKind::End,
            });
        }
    }
    Ok(result)
}

pub fn morning_claim_id(business_day: NaiveDate) -> String {
    format!("morning-{business_day}")
}

/// Stable id the integration must use when it creates the archive attempt.
pub fn end_attempt_id(business_day: NaiveDate) -> String {
    format!("end-{business_day}")
}

impl fmt::Display for SchedulerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MorningReport {
                business_day,
                claim,
            } => write!(f, "morning report for {business_day} ({})", claim.claim_id),
            Self::EndArchive {
                business_day,
                claim,
                dispatch,
            } => write!(
                f,
                "end archive for {business_day} attempt {} ({dispatch:?})",
                claim.attempt_id
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::holiday::{DayKind, ManualOverride};
    use crate::test_support::{calendar, date as d, ts_ms, ts_ms_prec};
    use crate::work_state::AiGateRequest;
    use crate::work_state::{
        AiPurpose, AiTrigger, ArchiveAttemptStatus, EndArchiveAttemptLookup, GateCode,
    };
    use chrono::Datelike;
    use serde_json::json;

    fn schedule() -> WorkSchedule {
        WorkSchedule::parse("08:00", "20:00").unwrap()
    }

    struct QueuedAttempt<'a>(&'a str);

    impl EndArchiveAttemptLookup for QueuedAttempt<'_> {
        fn end_archive_attempt_status(&self, attempt_id: &str) -> Option<ArchiveAttemptStatus> {
            (attempt_id == self.0).then_some(ArchiveAttemptStatus::Queued)
        }
    }

    #[test]
    fn startup_after_end_never_catches_up_even_with_persisted_work() {
        let calendar = calendar();
        let schedule = schedule();
        let mut runtime = WorkRuntime::new();
        runtime
            .observe(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        let mut state = SchedulerState::new();
        state.runtime = runtime;
        state.last_schedule = Some(schedule);

        let outcome = state
            .tick(TickInput::new(
                None,
                ts_ms(2025, 1, 2, 20, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(outcome.actions.is_empty());
        assert!(outcome.notices.contains(&TickNotice::StartupNoCatchup));
        assert_eq!(outcome.snapshot.status, WorkStatus::OffDuty);
        assert!(state
            .runtime
            .day_record(d(2025, 1, 2))
            .unwrap()
            .end_claim
            .is_none());
    }

    #[test]
    fn tick_and_commit_saves_before_swap_and_discards_on_save_failure() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        let input = TickInput::new(
            Some(ts_ms_prec(2025, 1, 2, 7, 59, 50)),
            ts_ms_prec(2025, 1, 2, 8, 0, 5),
            &schedule,
            &calendar,
        );
        let before = state.clone();

        let failure = state
            .tick_and_commit(input, |bytes| {
                assert!(!bytes.is_empty(), "save must receive the encoded state");
                Err("synthetic disk full")
            })
            .unwrap_err();
        assert!(matches!(
            failure,
            TickCommitError::Save("synthetic disk full")
        ));
        assert_eq!(state, before, "failed save must not mutate live state");

        let (preview, preview_outcome) = state.tick_preview(input).unwrap();
        assert_eq!(state, before, "preview must not mutate live state");
        assert_eq!(preview_outcome.actions.len(), 1);
        assert_eq!(
            preview
                .runtime
                .day_record(d(2025, 1, 2))
                .unwrap()
                .morning_claim
                .as_ref()
                .unwrap()
                .claim_id,
            "morning-2025-01-02"
        );

        let mut saved: Option<Vec<u8>> = None;
        let outcome = state
            .tick_and_commit(input, |bytes| {
                saved = Some(bytes.to_vec());
                Ok::<(), &'static str>(())
            })
            .unwrap();
        assert_eq!(outcome.actions.len(), 1);
        assert_eq!(
            state
                .runtime
                .day_record(d(2025, 1, 2))
                .unwrap()
                .morning_claim
                .as_ref()
                .unwrap()
                .claim_id,
            "morning-2025-01-02"
        );
        let persisted = SchedulerState::from_bytes(saved.as_deref().unwrap()).unwrap();
        assert_eq!(persisted, state);
    }

    #[test]
    fn supported_year_edges_do_not_probe_out_of_range_candidates() {
        let calendar = calendar();
        let schedule = schedule();
        for now_ms in [
            ts_ms_prec(1900, 1, 1, 8, 0, 5),
            ts_ms_prec(2100, 12, 31, 8, 0, 5),
        ] {
            let mut state = SchedulerState::new();
            let outcome = state
                .tick(TickInput::new(
                    Some(now_ms - 15_000),
                    now_ms,
                    &schedule,
                    &calendar,
                ))
                .unwrap();
            assert!(
                outcome.actions.is_empty(),
                "edge calendar is unknown, so no automatic action"
            );
        }
    }

    #[test]
    fn live_morning_claim_is_exact_and_idempotent() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        let mut input = TickInput::new(
            Some(ts_ms_prec(2025, 1, 2, 7, 59, 50)),
            ts_ms_prec(2025, 1, 2, 8, 0, 5),
            &schedule,
            &calendar,
        );
        input.ai_busy = true;

        let first = state.tick(input).unwrap();
        assert_eq!(first.actions.len(), 1);
        let claim = match &first.actions[0] {
            SchedulerAction::MorningReport {
                business_day,
                claim,
            } => {
                assert_eq!(*business_day, d(2025, 1, 2));
                claim
            }
            other => panic!("unexpected action {other:?}"),
        };
        assert_eq!(claim.claim_id, "morning-2025-01-02");
        assert_eq!(claim.claimed_at_ms, ts_ms(2025, 1, 2, 8, 0));

        let second = state.tick(input).unwrap();
        assert!(second.actions.is_empty());
        assert_eq!(
            state
                .runtime
                .day_record(d(2025, 1, 2))
                .unwrap()
                .morning_claim,
            Some(claim.clone())
        );
    }

    #[test]
    fn morning_is_limited_to_workdays_including_makeup_and_manual() {
        let schedule = schedule();
        let mut calendar = calendar();

        for (day, hour) in [
            (d(2025, 1, 4), 8), // ordinary Saturday
            (d(2025, 1, 1), 8), // holiday
            (d(2026, 1, 5), 8), // no cache => unknown
        ] {
            let mut state = SchedulerState::new();
            let start = ts_ms(day.year(), day.month(), day.day(), hour, 0);
            let outcome = state
                .tick(TickInput::new(
                    Some(start - 10),
                    start + 5,
                    &schedule,
                    &calendar,
                ))
                .unwrap();
            assert!(
                outcome.actions.is_empty(),
                "no morning report for {day} ({:?})",
                outcome.snapshot.status
            );
        }

        let mut holiday_overtime = SchedulerState::new();
        holiday_overtime
            .runtime
            .begin_overtime(ts_ms(2025, 1, 1, 8, 0), &schedule)
            .unwrap();
        let start = ts_ms(2025, 1, 1, 8, 0);
        let outcome = holiday_overtime
            .tick(TickInput::new(
                Some(start - 10),
                start + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(outcome.actions.is_empty());
        assert_eq!(outcome.snapshot.status, WorkStatus::Overtime);
        assert!(!outcome.snapshot.calendar_day.is_workday());

        let makeup = d(2025, 1, 11);
        let mut state = SchedulerState::new();
        let start = ts_ms(2025, 1, 11, 8, 0);
        let outcome = state
            .tick(TickInput::new(
                Some(start - 10),
                start + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            outcome.actions.as_slice(),
            [SchedulerAction::MorningReport { business_day, .. }] if *business_day == makeup
        ));

        calendar
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 4),
                    DayKind::Workday,
                    ts_ms(2025, 1, 4, 6, 0),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let mut state = SchedulerState::new();
        let start = ts_ms(2025, 1, 4, 8, 0);
        let outcome = state
            .tick(TickInput::new(
                Some(start - 10),
                start + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            outcome.actions.as_slice(),
            [SchedulerAction::MorningReport { business_day, .. }] if *business_day == d(2025, 1, 4)
        ));
    }

    #[test]
    fn regular_end_and_holiday_overtime_end_are_claimed_once() {
        let calendar = calendar();
        let schedule = schedule();

        let mut regular = SchedulerState::new();
        let startup = regular
            .tick(TickInput::new(
                None,
                ts_ms_prec(2025, 1, 2, 19, 59, 50),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(startup.actions.is_empty());
        let outcome = regular
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 19, 59, 50)),
                ts_ms_prec(2025, 1, 2, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert_eq!(outcome.actions.len(), 1);
        let SchedulerAction::EndArchive {
            business_day,
            claim,
            dispatch,
        } = &outcome.actions[0]
        else {
            panic!("expected end archive");
        };
        assert_eq!(*business_day, d(2025, 1, 2));
        assert_eq!(claim.attempt_id, "end-2025-01-02");
        assert_eq!(
            claim.eligibility,
            crate::work_state::EndEligibility::WorkedBeforeEnd
        );
        assert_eq!(*dispatch, EndDispatch::RunNow);
        assert_eq!(claim.claimed_at_ms, ts_ms(2025, 1, 2, 20, 0));
        let again = regular
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 19, 59, 55)),
                ts_ms_prec(2025, 1, 2, 20, 0, 10),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(again.actions.is_empty());

        let mut holiday = SchedulerState::new();
        holiday
            .runtime
            .begin_overtime(ts_ms(2025, 1, 1, 9, 0), &schedule)
            .unwrap();
        let outcome = holiday
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 1, 19, 59, 50)),
                ts_ms_prec(2025, 1, 1, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            outcome.actions.as_slice(),
            [SchedulerAction::EndArchive { business_day, claim, .. }]
                if *business_day == d(2025, 1, 1)
                    && claim.eligibility == crate::work_state::EndEligibility::OvertimeBeforeEnd
        ));
        assert_eq!(outcome.snapshot.status, WorkStatus::Overtime);
        assert_eq!(outcome.snapshot.calendar_day.date, d(2025, 1, 1));

        let mut no_work_holiday = SchedulerState::new();
        let outcome = no_work_holiday
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 1, 19, 59, 50)),
                ts_ms_prec(2025, 1, 1, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(outcome.actions.is_empty());
        assert_eq!(
            no_work_holiday
                .runtime
                .day_record(d(2025, 1, 1))
                .unwrap()
                .end_eligibility,
            Some(crate::work_state::EndEligibility::NotEligible)
        );
    }

    #[test]
    fn overnight_start_and_end_belong_to_start_business_day() {
        let calendar = calendar();
        let schedule = WorkSchedule::parse("22:00", "06:00").unwrap();
        let mut state = SchedulerState::new();

        let start = ts_ms(2025, 1, 2, 22, 0);
        let morning = state
            .tick(TickInput::new(
                Some(start - 10),
                start + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            morning.actions.as_slice(),
            [SchedulerAction::MorningReport { business_day, claim }]
                if *business_day == d(2025, 1, 2)
                    && claim.claimed_at_ms == start
        ));
        assert_eq!(morning.snapshot.business_date, d(2025, 1, 2));

        let end = ts_ms(2025, 1, 3, 6, 0);
        let evening = state
            .tick(TickInput::new(
                Some(end - 10),
                end + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            evening.actions.as_slice(),
            [SchedulerAction::EndArchive { business_day, claim, .. }]
                if *business_day == d(2025, 1, 2)
                    && claim.attempt_id == "end-2025-01-02"
                    && claim.claimed_at_ms == end
        ));
        assert_eq!(evening.snapshot.business_date, d(2025, 1, 2));
        assert_eq!(evening.snapshot.status, WorkStatus::OffDuty);
    }

    #[test]
    fn year_end_overnight_claims_span_into_the_new_year() {
        let calendar = calendar();
        let schedule = WorkSchedule::parse("22:00", "06:00").unwrap();
        let mut state = SchedulerState::new();

        let start = ts_ms(2024, 12, 31, 22, 0);
        let first = state
            .tick(TickInput::new(
                Some(start - 10),
                start + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            first.actions.as_slice(),
            [SchedulerAction::MorningReport { business_day, .. }]
                if *business_day == d(2024, 12, 31)
        ));

        let end = ts_ms(2025, 1, 1, 6, 0);
        let second = state
            .tick(TickInput::new(
                Some(end - 10),
                end + 5,
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(matches!(
            second.actions.as_slice(),
            [SchedulerAction::EndArchive { business_day, claim, .. }]
                if *business_day == d(2024, 12, 31)
                    && claim.attempt_id == "end-2024-12-31"
        ));
        assert_eq!(second.snapshot.business_date, d(2024, 12, 31));
        assert_eq!(second.snapshot.status, WorkStatus::Holiday);
    }

    #[test]
    fn busy_end_queues_once_and_restart_does_not_requeue() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        state
            .runtime
            .observe(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        let previous = ts_ms_prec(2025, 1, 2, 19, 59, 50);
        let now = ts_ms_prec(2025, 1, 2, 20, 0, 5);
        let mut input = TickInput::new(Some(previous), now, &schedule, &calendar);
        input.ai_busy = true;
        let outcome = state.tick(input).unwrap();
        assert!(matches!(
            outcome.actions.as_slice(),
            [SchedulerAction::EndArchive { dispatch: EndDispatch::QueueBusy, claim, .. }]
                if claim.attempt_id == "end-2025-01-02"
        ));
        let first_actions = outcome.actions.clone();

        // Repeating the same boundary must not create a second queue entry.
        let repeat = state.tick(input).unwrap();
        assert!(repeat.actions.is_empty());

        let bytes = state.to_bytes().unwrap();
        let mut restored = SchedulerState::from_bytes(&bytes).unwrap();
        assert_eq!(restored.runtime, state.runtime);
        let after_restart = restored
            .tick(TickInput::new(
                None,
                ts_ms(2025, 1, 2, 20, 30),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(after_restart.actions.is_empty());
        assert!(after_restart
            .notices
            .contains(&TickNotice::StartupNoCatchup));
        assert_eq!(
            restored.runtime.find_end_claim("end-2025-01-02").is_some(),
            true
        );
        assert_eq!(first_actions.len(), 1);
    }

    #[test]
    fn exited_holiday_overtime_still_gets_end_claim_and_gate_exception() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        state
            .runtime
            .begin_overtime(ts_ms(2025, 1, 1, 9, 0), &schedule)
            .unwrap();
        state.runtime.end_overtime();
        let outcome = state
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 1, 19, 59, 50)),
                ts_ms_prec(2025, 1, 1, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        let SchedulerAction::EndArchive { claim, .. } = &outcome.actions[0] else {
            panic!("expected end archive");
        };
        assert_eq!(
            claim.eligibility,
            crate::work_state::EndEligibility::OvertimeBeforeEnd
        );
        assert_eq!(outcome.snapshot.status, WorkStatus::Holiday);

        let request = AiGateRequest::new(
            AiPurpose::Archive,
            AiTrigger::QueuedArchive,
            ts_ms(2025, 1, 1, 20, 5),
        )
        .with_end_attempt_id(&claim.attempt_id);
        let attempts = QueuedAttempt(&claim.attempt_id);
        let decision =
            crate::work_state::gate(&state.runtime, &schedule, &calendar, &attempts, &request)
                .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.code, GateCode::AllowedEndArchiveClaim);
    }

    #[test]
    fn suspend_backward_clock_and_time_config_change_do_not_backfill() {
        let schedule = schedule();
        let calendar = calendar();

        let mut suspended = SchedulerState::new();
        suspended
            .runtime
            .observe(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        suspended.last_schedule = Some(schedule);
        let missed = suspended
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 19, 58, 0)),
                ts_ms_prec(2025, 1, 2, 20, 1, 0),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(missed.actions.is_empty());
        assert!(missed
            .notices
            .iter()
            .any(|notice| matches!(notice, TickNotice::GapTooLarge { gap_ms: 180_000 })));
        assert!(suspended
            .runtime
            .day_record(d(2025, 1, 2))
            .unwrap()
            .end_claim
            .is_none());

        let backwards = suspended
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 20, 1, 0)),
                ts_ms_prec(2025, 1, 2, 19, 59, 0),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(backwards.actions.is_empty());
        assert!(backwards
            .notices
            .iter()
            .any(|notice| matches!(notice, TickNotice::BackwardClock { .. })));

        let mut changed = SchedulerState::new();
        changed
            .tick(TickInput::new(
                None,
                ts_ms(2025, 1, 2, 8, 30),
                &schedule,
                &calendar,
            ))
            .unwrap();
        let new_schedule = WorkSchedule::parse("09:00", "21:00").unwrap();
        let switch = changed
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 8, 59, 50)),
                ts_ms_prec(2025, 1, 2, 9, 0, 5),
                &new_schedule,
                &calendar,
            ))
            .unwrap();
        assert!(switch.actions.is_empty());
        assert!(switch.notices.contains(&TickNotice::ScheduleChanged));
        let after_switch = changed
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 9, 0, 5)),
                ts_ms_prec(2025, 1, 2, 9, 0, 20),
                &new_schedule,
                &calendar,
            ))
            .unwrap();
        assert!(after_switch.actions.is_empty());
        assert!(changed
            .runtime
            .day_record(d(2025, 1, 2))
            .and_then(|record| record.morning_claim.as_ref())
            .is_none());
    }

    #[test]
    fn after_end_overtime_never_backdates_end_eligibility() {
        let calendar = calendar();
        let schedule = schedule();

        let mut at_boundary = SchedulerState::new();
        at_boundary
            .runtime
            .begin_overtime(ts_ms_prec(2025, 1, 1, 20, 0, 2), &schedule)
            .unwrap();
        let outcome = at_boundary
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 1, 19, 59, 50)),
                ts_ms_prec(2025, 1, 1, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(outcome.actions.is_empty());
        let record = at_boundary.runtime.day_record(d(2025, 1, 1)).unwrap();
        assert_eq!(
            record.end_eligibility,
            Some(crate::work_state::EndEligibility::NotEligible)
        );
        assert!(record.end_claim.is_none());

        let mut later = SchedulerState::new();
        later
            .runtime
            .begin_overtime(ts_ms_prec(2025, 1, 1, 20, 1, 0), &schedule)
            .unwrap();
        let outcome = later
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 1, 20, 0, 50)),
                ts_ms_prec(2025, 1, 1, 20, 1, 10),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(outcome.actions.is_empty());
        assert!(later.runtime.find_end_claim("end-2025-01-01").is_none());
    }

    #[test]
    fn backward_clock_does_not_rearm_a_passed_boundary() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        let startup = state
            .tick(TickInput::new(
                None,
                ts_ms_prec(2025, 1, 2, 20, 0, 5),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(startup.actions.is_empty());

        let backward = state
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 20, 0, 5)),
                ts_ms_prec(2025, 1, 2, 19, 59, 55),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(backward.actions.is_empty());

        // Crossing 20:00 again is still the already-passed startup boundary,
        // not a new live one.
        let forward = state
            .tick(TickInput::new(
                Some(ts_ms_prec(2025, 1, 2, 19, 59, 55)),
                ts_ms_prec(2025, 1, 2, 20, 0, 10),
                &schedule,
                &calendar,
            ))
            .unwrap();
        assert!(forward.actions.is_empty());
        assert!(state.runtime.find_end_claim("end-2025-01-02").is_none());
    }

    #[test]
    fn disabled_scheduler_and_scheduler_state_are_strict() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::new();
        let mut input = TickInput::new(
            Some(ts_ms_prec(2025, 1, 2, 7, 59, 50)),
            ts_ms_prec(2025, 1, 2, 8, 0, 5),
            &schedule,
            &calendar,
        );
        input.enabled = false;
        let outcome = state.tick(input).unwrap();
        assert!(outcome.actions.is_empty());
        assert!(outcome.notices.contains(&TickNotice::SchedulingDisabled));
        assert!(state
            .runtime
            .day_record(d(2025, 1, 2))
            .and_then(|record| record.morning_claim.as_ref())
            .is_none());

        let bytes = state.to_bytes().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = json!(2);
        assert!(matches!(
            SchedulerState::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(WorkError::UnsupportedSchemaVersion { found: 2, .. })
        ));
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["unknown"] = json!(true);
        assert!(matches!(
            SchedulerState::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(WorkError::MalformedState(_))
        ));
    }
}
