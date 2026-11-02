use super::{analysis_budget_blocked, App};
use crate::{model::*, process, work_hours::AiMode};
use std::{
    sync::{atomic::Ordering, Condvar, Mutex},
    time::{Duration, Instant},
};

const RETRY_MS: i64 = 60_000;

#[derive(Default)]
pub(crate) struct BackgroundWake {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl BackgroundWake {
    pub(crate) fn wake(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    pub(crate) fn generation(&self) -> u64 {
        *self.generation.lock().unwrap()
    }

    pub(crate) fn wait(&self, seen: u64) {
        self.wait_for(seen, Duration::from_millis(200));
    }

    fn wait_for(&self, seen: u64, timeout: Duration) {
        // The predicate and notification share this mutex, including the case
        // where a save completes between the caller's check and this wait.
        let _guard = self
            .changed
            .wait_timeout_while(self.generation.lock().unwrap(), timeout, |n| *n == seen)
            .unwrap();
    }
}

struct BackgroundPlan {
    revision: u64,
    generation: u64,
    next_at: i64,
}

impl App {
    pub fn run_background(&self) {
        self.resume_pending();
        let mut plan = BackgroundPlan {
            revision: u64::MAX,
            generation: u64::MAX,
            next_at: 0,
        };
        let mut maintenance_at = Instant::now() + Duration::from_secs(15);
        while !process::stopping() {
            let seen = self.background.generation();
            if Instant::now() >= maintenance_at {
                if let Err(error) = self.prune_messages() {
                    self.error(&error);
                }
                maintenance_at = Instant::now() + Duration::from_secs(15);
            }
            if self.background_ready(&mut plan, now()) {
                let before = plan.revision;
                self.tick();
                let at = now();
                plan = self.background_plan(at);
                // A failed/no-op starter must not spin. A busy contender keeps
                // the work ready so its release is retried within 200 ms.
                if plan.next_at <= at
                    && plan.revision == before
                    && !self.ai_busy.load(Ordering::SeqCst)
                    && !self.archive_busy.load(Ordering::SeqCst)
                {
                    plan.next_at = at.saturating_add(RETRY_MS);
                }
            }
            self.background.wait(seen);
        }
    }

    fn background_ready(&self, plan: &mut BackgroundPlan, at: i64) -> bool {
        if self.ai_busy.load(Ordering::SeqCst) || self.archive_busy.load(Ordering::SeqCst) {
            return false;
        }
        if self.saves_revision() != plan.revision
            || self.background.generation() != plan.generation
            || at >= plan.next_at
        {
            *plan = self.background_plan(at);
        }
        plan.next_at <= at
    }

    fn background_plan(&self, at: i64) -> BackgroundPlan {
        // Never hold the wake mutex while taking state/calendar/store locks.
        // Reading the revision under state also observes the state publication
        // that follows save(), rather than acknowledging an unfinished commit.
        let generation = self.background.generation();
        let state = self.state.lock().unwrap();
        BackgroundPlan {
            generation,
            revision: self.saves_revision(),
            next_at: self.next_background_at(&state, at),
        }
    }

    fn next_background_at(&self, state: &State, at: i64) -> i64 {
        if state.archive_attempts.iter().any(|a| a.status == "queued")
            || state.morning_jobs.iter().any(|j| j.status == "queued")
        {
            return at;
        }
        let draft = state.tasks.iter().any(|t| t.generation == "pending");
        let automatic = state.config.automatic_analysis_enabled();
        if !draft && (!automatic || state.messages.is_empty()) {
            return i64::MAX;
        }
        let configured =
            state.config.model_choice("classify").connection_id != UNCONFIGURED_CONNECTION;
        let quota = state.config.daily_calls > 0
            && (state.usage_day != date_from(at) || state.calls < state.config.daily_calls);
        let analyze = automatic && configured && quota && !analysis_budget_blocked(state);
        let filter = state.config.early_analysis_enabled()
            && state.config.model_choice("message_filter").connection_id != UNCONFIGURED_CONNECTION;
        // format and classify use the same analysis route.
        let generate = draft && configured && quota;
        let mut next = if !quota && state.config.daily_calls > 0 && configured {
            // Daily accounting uses Shanghai dates, independent of host locale.
            (at + 28_800_000).div_euclid(86_400_000) * 86_400_000 + 57_600_000
        } else {
            i64::MAX
        };
        if !analyze && !filter && !generate {
            return next;
        }
        if !self.work_gate(state, "classify", AiMode::Automatic, None, at) {
            return next.min((at.div_euclid(60_000) + 1) * 60_000);
        }
        if generate {
            return at;
        }
        let pending = crate::context_budget::pending_refs(state, false).collect::<Vec<_>>();
        if pending.is_empty() {
            return if analyze && state.pending_analysis {
                at
            } else {
                next
            };
        }
        if analyze && (!state.pending_analysis || state.analysis_pending_since == 0) {
            return at;
        }
        let interval = (state.config.analysis_interval_minutes as i64).saturating_mul(60_000);
        let anchor = if state.last_analysis_at > 0 {
            state.last_analysis_at
        } else {
            state.analysis_pending_since
        };
        let scheduled_at = crate::context_budget::next_interval_at(
            anchor,
            interval,
            state.analysis_interrupted_at,
        );
        let interrupted_until = if state.analysis_interrupted_at > 0 {
            scheduled_at
        } else {
            0
        };
        let due_now = crate::context_budget::due(state, pending.iter().copied(), at);
        if analyze {
            let attempted = self.analysis_last_attempt.load(Ordering::SeqCst);
            if state.config.message_filter_enabled && state.message_filter.early {
                // A preparation failure can leave the one-shot decision
                // unconsumed. Only a genuinely newer decision bypasses retry.
                let early_at = if attempted > state.message_filter.last_run {
                    attempted.saturating_add(RETRY_MS).max(at)
                } else {
                    at
                };
                // Cancellation consumes only its own decision. A subsequent
                // filter result still hands off immediately, like automatic_due.
                next = next.min(early_at);
            } else {
                let earliest = at
                    .max(state.last_analysis_at.saturating_add(RETRY_MS))
                    .max(attempted.saturating_add(RETRY_MS))
                    .max(interrupted_until);
                let due = if earliest == at {
                    due_now
                } else {
                    crate::context_budget::due(state, pending.iter().copied(), earliest)
                };
                next = next.min(if due {
                    earliest
                } else {
                    earliest.max(scheduled_at)
                });
            }
        }
        if filter
            && !state.message_filter.early
            && !due_now
            && pending
                .iter()
                .any(|m| !crate::message_progress::filtered(state, m))
        {
            next = next.min(at.max(crate::context_budget::next_interval_at(
                state.message_filter.last_run,
                RETRY_MS,
                state.analysis_interrupted_at,
            )));
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};

    fn fixture() -> (std::path::PathBuf, App) {
        let (root, app) = crate::test_support::app("shishi-background");
        app.test_force_working();
        {
            let mut state = app.state.lock().unwrap();
            state.config.model_profiles = None;
            state.config.message_filter_enabled = false;
            state.config.analysis_interval_minutes = 10;
        }
        (root, app)
    }

    fn pending(state: &mut State, at: i64) {
        state.messages.insert(
            "followup".into(),
            crate::test_support::message("followup", "synthetic follow-up", 0),
        );
        state.pending_analysis = true;
        state.analysis_pending_since = at;
    }

    #[test]
    fn notification_before_wait_and_broadcast_do_not_lose_wakeups() {
        let wake = Arc::new(BackgroundWake::default());
        let seen = wake.generation();
        wake.wake();
        let started = Instant::now();
        wake.wait_for(seen, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));

        let barrier = Arc::new(Barrier::new(3));
        let (sent, received) = mpsc::channel();
        let workers = (0..2)
            .map(|_| {
                let (wake, barrier, sent) = (wake.clone(), barrier.clone(), sent.clone());
                std::thread::spawn(move || {
                    let seen = wake.generation();
                    barrier.wait();
                    wake.wait_for(seen, Duration::from_secs(5));
                    sent.send(()).unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        wake.wake();
        for _ in 0..2 {
            received.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn idle_checks_do_not_save_or_reacquire_state() {
        let (root, app) = fixture();
        app.tick();
        let at = now();
        let mut plan = app.background_plan(at);
        let saves = app.saves_revision();
        for step in 1..=100 {
            assert!(!app.background_ready(&mut plan, at + step * 200));
        }
        assert_eq!(app.saves_revision(), saves);
        // A cached 200 ms check must also avoid message traversal and the
        // state lock; the dispatcher can answer while another writer owns it.
        std::thread::scope(|scope| {
            let locked = app.state.lock().unwrap();
            let (sent, received) = mpsc::channel();
            let (app, plan) = (&app, &mut plan);
            scope.spawn(move || {
                sent.send(app.background_ready(plan, at + 200)).unwrap();
            });
            let result = received.recv_timeout(Duration::from_secs(1));
            drop(locked);
            assert!(!result.unwrap());
        });
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_scan_wakes_immediately_and_busy_does_not_consume_it() {
        let (root, app) = fixture();
        let at = now();
        let mut plan = app.background_plan(at);
        app.commit_state(|state| {
            pending(state, at - 600_000);
            Ok(())
        })
        .unwrap();
        app.ai_busy.store(true, Ordering::SeqCst);
        assert!(!app.background_ready(&mut plan, at));
        app.ai_busy.store(false, Ordering::SeqCst);
        assert!(app.background_ready(&mut plan, at + 200));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn next_attempt_respects_interval_volume_and_failed_early_decision() {
        let (root, app) = fixture();
        let at = now();
        {
            let mut state = app.state.lock().unwrap();
            pending(&mut state, at - 30_000);
            state.last_analysis_at = at - 30_000;
        }
        app.analysis_last_attempt
            .store(at - 5_000, Ordering::SeqCst);
        assert_eq!(app.background_plan(at).next_at, at + 570_000);
        {
            let mut state = app.state.lock().unwrap();
            for n in 0..50 {
                let key = format!("large-{n}");
                state.messages.insert(
                    key.clone(),
                    crate::test_support::message(key, "x".repeat(2_000), 0),
                );
            }
        }
        assert_eq!(app.background_plan(at).next_at, at + 55_000);
        {
            let mut state = app.state.lock().unwrap();
            state.config.message_filter_enabled = true;
            state.message_filter.early = true;
            state.message_filter.last_run = at - 10_000;
        }
        assert_eq!(app.background_plan(at).next_at, at + 55_000);
        app.state.lock().unwrap().analysis_interrupted_at = at - 1_000;
        // A retained positive decision belongs to a newer round; it is not
        // delayed by an earlier cancellation.
        assert_eq!(app.background_plan(at).next_at, at + 55_000);
        {
            let mut state = app.state.lock().unwrap();
            state.analysis_interrupted_at = 0;
            state.message_filter.last_run = at;
        }
        assert_eq!(app.background_plan(at).next_at, at);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exhausted_quota_and_cancelled_filter_keep_their_own_schedules() {
        let (root, app) = fixture();
        let at = now();
        {
            let mut state = app.state.lock().unwrap();
            pending(&mut state, at - 30_000);
            state.config.daily_calls = 0;
            state.config.message_filter_enabled = true;
            state.message_filter.early = true;
        }
        let saves = app.saves_revision();
        let mut plan = app.background_plan(at);
        assert_eq!(plan.next_at, i64::MAX);
        for step in 0..5 {
            app.background.wake(); // Calendar/AI release is not a new decision.
            assert!(!app.background_ready(&mut plan, at + step * 200));
        }
        assert_eq!(app.saves_revision(), saves);
        {
            let mut state = app.state.lock().unwrap();
            state.message_filter.early = false;
            state.message_filter.last_run = at - 120_000;
            state.analysis_interrupted_at = at - 5_000;
        }
        // The original next minute boundary has arrived after cancellation.
        assert_eq!(app.background_plan(at).next_at, at);
        {
            let mut state = app.state.lock().unwrap();
            state.config.daily_calls = 1;
            state.calls = 1;
            state.usage_day = date_from(at - 86_400_000);
            state.analysis_interrupted_at = 0;
            state.message_filter.early = true;
            state.message_filter.last_run = at;
        }
        assert_eq!(app.background_plan(at).next_at, at);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_preserves_upcoming_boundaries_and_new_filter_handoffs() {
        let (root, app) = fixture();
        let at = now();
        {
            let mut state = app.state.lock().unwrap();
            state.config.analysis_interval_minutes = 30;
            state.config.message_filter_enabled = true;
            pending(&mut state, at - 29 * 60_000);
            state.last_analysis_at = state.analysis_pending_since;
            state.message_filter.last_run = at - 45_000;
            state.analysis_interrupted_at = at - 5_000;
        }
        // Cancellation five seconds ago must preserve the next filter tick
        // in fifteen seconds and the original analysis boundary in one minute.
        assert_eq!(app.background_plan(at).next_at, at + 15_000);
        {
            let mut state = app.state.lock().unwrap();
            state.message_filter.last_run = at + 15_000;
            state.message_filter.early = true;
        }
        assert_eq!(app.background_plan(at + 15_000).next_at, at + 15_000);
        {
            let mut state = app.state.lock().unwrap();
            let messages = crate::context_budget::pending(&state);
            assert!(crate::context_budget::automatic_due(
                &state,
                &messages,
                at + 15_000,
                0
            ));
            state.message_filter.early = false;
            state.config.message_filter_enabled = false;
            assert!(!crate::context_budget::automatic_due(
                &state,
                &messages,
                at + 59_999,
                0
            ));
            assert!(crate::context_budget::automatic_due(
                &state,
                &messages,
                at + 60_000,
                0
            ));
        }
        assert_eq!(app.background_plan(at).next_at, at + 60_000);
        // An exact tie consumes that round, not the next original boundary.
        app.state.lock().unwrap().analysis_interrupted_at = at + 60_000;
        assert_eq!(app.background_plan(at + 60_000).next_at, at + 31 * 60_000);
        assert_eq!(crate::context_budget::next_interval_at(0, 60_000, 0), 0);
        assert_eq!(
            crate::context_budget::next_interval_at(1, 60_000, i64::MAX),
            i64::MAX
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
