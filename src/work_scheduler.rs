//! Deterministic boundary scheduler. It owns no clock, network or model call:
//! the live previous tick and current time are supplied in Unix **milliseconds**
//! (matching the application's `model::now()`), and it only mutates persisted
//! claims/observations in [`SchedulerState`].

use std::collections::BTreeSet;

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

shishi_api::default_struct! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SchedulerState {
        pub schema_version: u32 = SCHEDULER_STATE_SCHEMA_VERSION,
        pub runtime: WorkRuntime,
        pub last_schedule: Option<WorkSchedule>,
        /// Monotonic anti-retro guard, in Unix milliseconds. It is not a catch-up
        /// cursor: startup still supplies `previous_tick_at_ms = None`, and a
        /// backward clock cannot make a boundary that was already passed eligible
        /// again.
        clock_floor_ms: Option<i64>,
    }
}

impl SchedulerState {
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
        Ok(())
    }

    /// Compute a tick against a clone without mutating `self` or persisting.
    ///
    /// The application commits this candidate together with its archive/report
    /// jobs before dispatching any returned actions.
    pub fn tick_preview(&self, input: TickInput<'_>) -> Result<(Self, TickOutcome), WorkError> {
        let mut next = self.clone();
        let outcome = next.tick(input)?;
        Ok((next, outcome))
    }

    /// Private mutating implementation. Public integration code must use the
    /// preview wrapper and persist the complete application transaction.
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

        let mut actions = Vec::new();
        if live_crossing {
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
                                    claim_id: format!("morning-{}", boundary.business_day),
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
                            record.end_eligibility = Some(eligibility);
                            if eligibility.is_eligible() && record.end_claim.is_none() {
                                let claim = EndArchiveClaim {
                                    attempt_id: format!("end-{}", boundary.business_day),
                                    claimed_at_ms: boundary.at_ms,
                                    eligibility,
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
        Ok(TickOutcome { snapshot, actions })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    pub previous_tick_at_ms: Option<i64>,
    pub now_ms: i64,
    pub schedule: &'a WorkSchedule,
    pub calendar: &'a CalendarData,
}

#[cfg(test)]
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    pub snapshot: WorkSnapshot,
    pub actions: Vec<SchedulerAction>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::holiday::{DayKind, ManualOverride};
    use crate::test_support::{calendar, date as d, ts_ms, ts_ms_prec};
    use chrono::Datelike;
    use serde_json::json;

    fn schedule() -> WorkSchedule {
        WorkSchedule::parse("08:00", "20:00").unwrap()
    }

    #[test]
    fn startup_after_end_never_catches_up_even_with_persisted_work() {
        let calendar = calendar();
        let schedule = schedule();
        let mut runtime = WorkRuntime::default();
        runtime
            .observe(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        let mut state = SchedulerState::default();
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
        assert_eq!(outcome.snapshot.status, WorkStatus::OffDuty);
        assert!(state
            .runtime
            .day_record(d(2025, 1, 2))
            .unwrap()
            .end_claim
            .is_none());
    }

    #[test]
    fn supported_year_edges_do_not_probe_out_of_range_candidates() {
        let calendar = calendar();
        let schedule = schedule();
        for now_ms in [
            ts_ms_prec(1900, 1, 1, 8, 0, 5),
            ts_ms_prec(2100, 12, 31, 8, 0, 5),
        ] {
            let mut state = SchedulerState::default();
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
        let mut state = SchedulerState::default();
        let input = TickInput::new(
            Some(ts_ms_prec(2025, 1, 2, 7, 59, 50)),
            ts_ms_prec(2025, 1, 2, 8, 0, 5),
            &schedule,
            &calendar,
        );

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
            let mut state = SchedulerState::default();
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

        let mut holiday_overtime = SchedulerState::default();
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
        let mut state = SchedulerState::default();
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
        let mut state = SchedulerState::default();
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

        let mut regular = SchedulerState::default();
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

        let mut holiday = SchedulerState::default();
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

        let mut no_work_holiday = SchedulerState::default();
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
        let mut state = SchedulerState::default();

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
        let mut state = SchedulerState::default();

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
    fn end_claim_survives_restart_without_duplicate_actions() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::default();
        state
            .runtime
            .observe(ts_ms(2025, 1, 2, 10, 0), &schedule, &calendar)
            .unwrap();
        let previous = ts_ms_prec(2025, 1, 2, 19, 59, 50);
        let now = ts_ms_prec(2025, 1, 2, 20, 0, 5);
        let input = TickInput::new(Some(previous), now, &schedule, &calendar);
        let outcome = state.tick(input).unwrap();
        assert!(matches!(
            outcome.actions.as_slice(),
            [SchedulerAction::EndArchive { claim, .. }]
                if claim.attempt_id == "end-2025-01-02"
        ));
        // Repeating the same boundary must not create a second queue entry.
        let repeat = state.tick(input).unwrap();
        assert!(repeat.actions.is_empty());

        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored: SchedulerState = serde_json::from_slice(&bytes).unwrap();
        restored.validate().unwrap();
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
        assert!(restored.runtime.find_end_claim("end-2025-01-02").is_some());
    }

    #[test]
    fn exited_holiday_overtime_still_gets_end_claim_and_gate_exception() {
        let calendar = calendar();
        let schedule = schedule();
        let mut state = SchedulerState::default();
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

        let (_root, app) = crate::test_support::scoped_app("holiday-archive-gate");
        *app.calendar.lock().unwrap() = calendar;
        let mut locked = app.state.lock().unwrap();
        locked.archive_attempts.push(crate::model::ArchiveAttempt {
            id: claim.attempt_id.clone(),
            status: "queued".into(),
            ..Default::default()
        });
        let attempt_id = claim.attempt_id.clone();
        locked.work_scheduler = state;
        assert!(app.work_gate(
            &locked,
            "archive",
            crate::work_hours::AiMode::Nightly,
            Some(&attempt_id),
            ts_ms(2025, 1, 1, 20, 5),
        ));
        drop(locked);
    }

    #[test]
    fn suspend_backward_clock_and_time_config_change_do_not_backfill() {
        let schedule = schedule();
        let calendar = calendar();

        let mut suspended = SchedulerState::default();
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

        let mut changed = SchedulerState::default();
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

        let mut at_boundary = SchedulerState::default();
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

        let mut later = SchedulerState::default();
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
        let mut state = SchedulerState::default();
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
    fn scheduler_state_is_versioned_and_strict() {
        let state = SchedulerState::default();
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = json!(2);
        assert!(matches!(
            serde_json::from_value::<SchedulerState>(value)
                .unwrap()
                .validate(),
            Err(WorkError::UnsupportedSchemaVersion { found: 2, .. })
        ));
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["unknown"] = json!(true);
        assert!(serde_json::from_value::<SchedulerState>(value)
            .unwrap_err()
            .to_string()
            .contains("unknown field `unknown`"));
    }
}
