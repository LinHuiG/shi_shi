//! AI layer storage: daily usage, the analysis state row, the work runtime
//! and calendar, the adopted policies and the AI activity log.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::kv;
use super::{rows, Db};
use crate::ai::ask::{Inputs, Proposal};
use crate::ai::calendar::{Calendar, DayKind, Year};
use crate::ai::prompt::{self, Policies};
use crate::ai::usage::{self, State, Usage};
use crate::ai::work::{self, Action, Claim, DayRecord, Eligibility, Runtime, Snapshot, WorkSchedule};
use crate::ai::{Mode, AUTOMATION_DISABLED, DAILY_LIMIT, DEFERRED, UNCONFIGURED_PREFIX};
use crate::analysis::FilterTaken;
use crate::params::Params;
use crate::state::Scope;
use crate::{prompts, time, Config, Error, Result};

// ---- usage and analysis state ---------------------------------------------

/// Today's counters for `purpose` (`main` or `message_filter`); a row of an
/// earlier day reads as zero.
pub(crate) fn usage_today(conn: &Connection, purpose: &str) -> Result<Usage> {
    let day = time::today();
    let found: Option<Usage> =
        rows::one(conn, "SELECT * FROM usage_daily WHERE day = ?1 AND purpose = ?2", params![day, purpose])?;
    Ok(Usage { day, ..found.unwrap_or_default() })
}

/// Count one call before it is made. Checks, in order: automation switch,
/// work gate (`gate` is the caller's [`ai_gate`] result), day change, daily
/// limit (the message filter has none).
pub(crate) fn reserve_call(conn: &Connection, config: &Config, kind: &str, mode: Mode, gate: bool) -> Result<()> {
    if mode == Mode::Automatic
        && ((kind == "classify" && !config.automatic_analysis_enabled())
            || (kind == "message_filter" && !config.early_analysis_enabled()))
    {
        crate::bail!("{AUTOMATION_DISABLED}");
    }
    if !gate {
        crate::bail!("{DEFERRED}");
    }
    let day = time::today();
    conn.execute("DELETE FROM usage_daily WHERE day <> ?1", [&day])?;
    let purpose = usage::row(kind);
    if purpose == "main" && usage_today(conn, purpose)?.calls >= config.daily_calls {
        crate::bail!("{DAILY_LIMIT}");
    }
    conn.execute(
        "INSERT INTO usage_daily(day, purpose, calls, unreported_calls) VALUES (?1, ?2, 1, 1)
         ON CONFLICT(day, purpose) DO UPDATE SET calls = calls + 1, unreported_calls = unreported_calls + 1",
        params![day, purpose],
    )?;
    Ok(())
}

/// Add reported tokens after the call. Unknown usage changes nothing; a
/// partial report adds tokens but leaves the call unreported.
pub(crate) fn record_usage(conn: &Connection, kind: &str, usage: &Value) -> Result<()> {
    if !usage::known(usage) {
        return Ok(());
    }
    let purpose = usage::row(kind);
    let (_, cached) = usage::cache_state(usage);
    let cached = if purpose == "main" { cached } else { None };
    let settled = i64::from(usage["partial"] != true);
    conn.execute(
        "INSERT INTO usage_daily(day, purpose, input_tokens, output_tokens, cached_input_tokens, cached_reports)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(day, purpose) DO UPDATE SET input_tokens = input_tokens + ?3, output_tokens = output_tokens + ?4,
             cached_input_tokens = cached_input_tokens + ?5, cached_reports = cached_reports + ?6,
             unreported_calls = max(unreported_calls - ?7, 0)",
        params![
            time::today(),
            purpose,
            usage["input_tokens"].as_u64().unwrap_or(0) as i64,
            usage["output_tokens"].as_u64().unwrap_or(0) as i64,
            cached.unwrap_or(0) as i64,
            i64::from(cached.is_some()),
            settled,
        ],
    )?;
    Ok(())
}

/// The `analysis_state` row.
pub(crate) fn analysis_state(conn: &Connection) -> Result<State> {
    Ok(rows::one(conn, "SELECT * FROM analysis_state WHERE id = 1", [])?.unwrap_or_default())
}

/// Change the analysis state in the caller's transaction.
pub(crate) fn update_analysis_state(conn: &Connection, change: impl FnOnce(&mut State)) -> Result<State> {
    let mut s = analysis_state(conn)?;
    change(&mut s);
    let mut row = json!(s);
    row["id"] = 1.into();
    rows::update(conn, "analysis_state", &["id"], &row)?;
    Ok(s)
}

/// Record a failure once: repeating the same message neither rewrites the
/// state nor adds a log row. Cancellation and unconfigured purposes are
/// not errors. Returns whether a row was logged.
pub(crate) fn ai_error(conn: &Connection, message: &str) -> Result<bool> {
    if message == crate::exec::CANCELLED || message.starts_with(UNCONFIGURED_PREFIX) {
        return Ok(false);
    }
    if analysis_state(conn)?.last_error == message {
        return Ok(false);
    }
    update_analysis_state(conn, |s| s.last_error = message.into())?;
    super::activity::log(
        conn,
        "error",
        &json!({"message": message, "failure_class": crate::ai::failure_class(message)}),
    )?;
    Ok(true)
}

/// Log an unconfigured purpose.
fn log_unconfigured(conn: &Connection, kind: &str, error: &str) -> Result<()> {
    let message = error.strip_prefix(UNCONFIGURED_PREFIX).unwrap_or(error);
    super::activity::log(conn, "error", &json!({"kind": kind, "message": message, "failure_class": "unconfigured"}))?;
    Ok(())
}

/// Snapshot keys of the AI layer.
pub(crate) fn part(conn: &Connection, _scope: &Scope, out: &mut Map<String, Value>) -> Result<()> {
    let main = usage_today(conn, "main")?;
    let s = analysis_state(conn)?;
    out.insert("usage".into(), serde_json::to_value(&main)?);
    out.insert("last_analysis_success_at".into(), s.last_success_at.into());
    Ok(())
}

// ---- policies -------------------------------------------------------------

/// Policy text of a purpose: the adopted prompt version where the user can
/// edit it (other purposes share the classify prompt), the factory text for
/// fixed purposes.
fn effective_policy(conn: &Connection, kind: &str) -> Result<String> {
    if prompt::FIXED_POLICIES.contains(&kind) {
        return Ok(prompts::default(kind).into());
    }
    let target = if prompts::TARGETS.contains(&kind) { kind } else { "classify" };
    super::prompts::effective(conn, target)
}

pub(crate) fn policies(conn: &Connection, kind: &str) -> Result<Policies> {
    Ok(Policies { purpose: effective_policy(conn, kind)?, tool_call: effective_policy(conn, "tool_call")? })
}

// ---- calendar -------------------------------------------------------------

/// The publication years and overrides covering `from..=to`.
fn load_calendar(conn: &Connection, from: NaiveDate, to: NaiveDate) -> Result<Calendar> {
    let mut calendar = Calendar::default();
    let mut stmt =
        conn.prepare_cached("SELECT year, days, fetched_at FROM holiday_years WHERE year BETWEEN ?1 AND ?2")?;
    let rows = stmt.query_map(params![from.year(), to.year() + 1], |r| {
        Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    for row in rows {
        let (year, days, fetched_at) = row?;
        // A row this module wrote is valid; anything else is skipped like a
        // missing year rather than stopping the calendar.
        if let Ok(days) = stored_days(&days) {
            calendar.insert_year(Year { year, fetched_at, days });
        }
    }
    let mut stmt = conn.prepare_cached("SELECT date, kind FROM calendar_overrides WHERE date BETWEEN ?1 AND ?2")?;
    let rows = stmt.query_map(params![from.to_string(), to.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (date, kind) = row?;
        if let (Ok(date), Some(kind)) = (date.parse(), DayKind::parse(&kind)) {
            calendar.set_override(date, kind);
        }
    }
    Ok(calendar)
}

fn stored_days(text: &str) -> std::result::Result<BTreeMap<NaiveDate, DayKind>, ()> {
    let raw: BTreeMap<String, String> = serde_json::from_str(text).map_err(drop)?;
    raw.into_iter().map(|(d, k)| Ok((d.parse().map_err(drop)?, DayKind::parse(&k).ok_or(())?))).collect()
}

/// Store a fetched year, replacing the older copy.
pub(crate) fn save_holiday_year(conn: &Connection, year: &Year) -> Result<()> {
    let days: BTreeMap<String, &str> = year.days.iter().map(|(d, k)| (d.to_string(), k.as_str())).collect();
    conn.execute(
        "INSERT INTO holiday_years(year, days, fetched_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(year) DO UPDATE SET days = excluded.days, fetched_at = excluded.fetched_at",
        params![year.year, serde_json::to_string(&days)?, year.fetched_at],
    )?;
    Ok(())
}

// ---- work runtime ---------------------------------------------------------

/// The runtime row and the day records in `from..=to`.
fn load_runtime(conn: &Connection, from: NaiveDate, to: NaiveDate) -> Result<Runtime> {
    let (started, day, schedule, floor) = conn.query_row(
        "SELECT overtime_started_at, overtime_day, last_schedule, clock_floor FROM work_runtime WHERE id = 1",
        [],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    )?;
    let overtime = match (started, day.and_then(|d| d.parse().ok())) {
        (Some(at), Some(day)) => Some((at, day)),
        _ => None,
    };
    let mut days = BTreeMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT date, first_worked_at, first_overtime_at, end_eligibility, morning_claim, morning_claimed_at,
                end_claim, end_claimed_at FROM work_days WHERE date BETWEEN ?1 AND ?2",
    )?;
    let rows = stmt.query_map(params![from.to_string(), to.to_string()], |r| {
        let claim = |id: Option<String>, at: Option<i64>| id.map(|id| Claim { id, at: at.unwrap_or(0) });
        Ok((
            r.get::<_, String>(0)?,
            DayRecord {
                first_worked: r.get(1)?,
                first_overtime: r.get(2)?,
                end_eligibility: r.get::<_, Option<String>>(3)?.as_deref().and_then(Eligibility::parse),
                morning_claim: claim(r.get(4)?, r.get(5)?),
                end_claim: claim(r.get(6)?, r.get(7)?),
            },
        ))
    })?;
    for row in rows {
        let (date, record) = row?;
        if let Ok(date) = date.parse() {
            days.insert(date, record);
        }
    }
    Ok(Runtime {
        overtime,
        days,
        last_schedule: schedule.as_deref().and_then(WorkSchedule::from_text),
        clock_floor: floor,
    })
}

/// Write the runtime row and every loaded day record.
fn save_runtime(conn: &Connection, runtime: &Runtime) -> Result<()> {
    conn.execute(
        "UPDATE work_runtime SET overtime_started_at = ?1, overtime_day = ?2, last_schedule = ?3, clock_floor = ?4
         WHERE id = 1",
        params![
            runtime.overtime.map(|o| o.0),
            runtime.overtime.map(|o| o.1.to_string()),
            runtime.last_schedule.map(WorkSchedule::text),
            runtime.clock_floor
        ],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO work_days(date, first_worked_at, first_overtime_at, end_eligibility, morning_claim,
                               morning_claimed_at, end_claim, end_claimed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(date) DO UPDATE SET first_worked_at = excluded.first_worked_at,
             first_overtime_at = excluded.first_overtime_at, end_eligibility = excluded.end_eligibility,
             morning_claim = excluded.morning_claim, morning_claimed_at = excluded.morning_claimed_at,
             end_claim = excluded.end_claim, end_claimed_at = excluded.end_claimed_at",
    )?;
    for (date, r) in &runtime.days {
        stmt.execute(params![
            date.to_string(),
            r.first_worked,
            r.first_overtime,
            r.end_eligibility.map(Eligibility::as_str),
            r.morning_claim.as_ref().map(|c| &c.id),
            r.morning_claim.as_ref().map(|c| c.at),
            r.end_claim.as_ref().map(|c| &c.id),
            r.end_claim.as_ref().map(|c| c.at),
        ])?;
    }
    Ok(())
}

/// Runtime and calendar for the days around `at` (and `previous` when it is
/// a live tick).
fn load_work(conn: &Connection, previous: Option<i64>, at: i64) -> Result<(Runtime, Calendar)> {
    let (from, to) = work::window(previous, at).map_err(Error)?;
    Ok((load_runtime(conn, from, to)?, load_calendar(conn, from, to)?))
}

/// The `work_state` snapshot at `at`; `None` when the schedule or date is
/// invalid (the page shows the calendar as not ready).
pub(crate) fn work_snapshot(conn: &Connection, config: &Config, at: i64) -> Result<Option<Snapshot>> {
    let Ok(schedule) = WorkSchedule::of(config) else {
        return Ok(None);
    };
    let (runtime, calendar) = load_work(conn, None, at)?;
    Ok(runtime.snapshot(at, schedule, &calendar).ok())
}

/// Snapshot key of the work runtime.
pub(crate) fn work_part(conn: &Connection, config: &Config, out: &mut Map<String, Value>) -> Result<()> {
    let state = work_snapshot(conn, config, time::now())?.map_or(Value::Null, |s| s.json());
    out.insert("work_state".into(), state);
    Ok(())
}

/// The single AI time gate. Manual work only needs a readable calendar; a
/// queued work-end archive needs its exact claim and a live attempt, even
/// while working; everything else needs working or overtime.
pub(crate) fn ai_gate(
    conn: &Connection,
    config: &Config,
    kind: &str,
    mode: Mode,
    attempt: Option<&str>,
    at: i64,
) -> Result<bool> {
    let Some(snapshot) = work_snapshot(conn, config, at)? else {
        return Ok(false);
    };
    if mode == Mode::Manual {
        return Ok(true);
    }
    if let (Mode::Nightly, "archive", Some(id)) = (mode, kind, attempt) {
        let sql = "SELECT 1 FROM work_days d JOIN archive_attempts a ON a.id = d.end_claim
                   WHERE d.end_claim = ?1 AND d.end_eligibility IN ('worked_before_end', 'overtime_before_end')
                   AND d.end_claimed_at <= ?2 AND a.status IN ('queued', 'running')";
        return Ok(conn.prepare_cached(sql)?.exists(params![id, at])?);
    }
    Ok(snapshot.status.ai_allowed())
}

/// The archive domain's check for a queued work-end attempt: the claim must
/// name this attempt on its business day, and the gate must pass.
pub(crate) fn claim_check(
    conn: &Connection,
    config: &Config,
    attempt: &crate::archive::Attempt,
    at: i64,
) -> std::result::Result<(), String> {
    let day: Option<String> = conn
        .query_row("SELECT date FROM work_days WHERE end_claim = ?1", [&attempt.id], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?;
    if day.as_deref() != Some(attempt.business_day.as_str()) {
        return Err(work::NO_CLAIM.into());
    }
    match ai_gate(conn, config, "archive", Mode::Nightly, Some(&attempt.id), at) {
        Ok(true) => Ok(()),
        Ok(false) => Err(work::CLAIM_EXPIRED.into()),
        Err(e) => Err(e.0),
    }
}

/// `work_state`: start or end overtime, or correct one calendar date.
pub(crate) fn set_work_state(conn: &Connection, config: &Config, p: &Value, at: i64) -> Result<Value> {
    let schedule = WorkSchedule::of(config).map_err(Error)?;
    let (mut runtime, mut calendar) = load_work(conn, None, at)?;
    if let Some(enabled) = p["overtime"].as_bool() {
        if enabled {
            runtime.begin_overtime(at, schedule).map_err(Error)?;
        } else {
            runtime.end_overtime();
        }
    } else {
        let Some(date) = p["date"].as_str() else {
            crate::bail!("缺少日期");
        };
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| Error::new("日期格式应为 YYYY-MM-DD"))?;
        let Some(kind) = DayKind::parse(p.str_of("kind")) else {
            crate::bail!("请选择工作日或假期");
        };
        if !(work::MIN_YEAR..=work::MAX_YEAR).contains(&date.year()) {
            crate::bail!("manual day override is invalid: date is out of range");
        }
        conn.execute(
            "INSERT INTO calendar_overrides(date, kind, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT(date) DO UPDATE SET kind = excluded.kind, updated = excluded.updated",
            params![date.to_string(), kind.as_str(), at],
        )?;
        calendar.set_override(date, kind);
    }
    runtime.observe(at, schedule, &calendar).map_err(Error)?;
    save_runtime(conn, &runtime)?;
    Ok(json!(true))
}

/// The runtime and jobs a boundary tick at `at` produces, or none when it
/// would change nothing. The clock floor alone is not worth a write; it is
/// saved with the next real change.
fn planned_tick(conn: &Connection, config: &Config, previous: Option<i64>, at: i64) -> Result<Option<(Runtime, Vec<Action>)>> {
    let schedule = WorkSchedule::of(config).map_err(Error)?;
    let (before, calendar) = load_work(conn, previous, at)?;
    let mut runtime = before.clone();
    let (_, actions) = runtime.tick(previous, at, schedule, &calendar).map_err(Error)?;
    let changed = Runtime { clock_floor: before.clock_floor, ..runtime.clone() } != before;
    Ok((changed || !actions.is_empty()).then_some((runtime, actions)))
}

/// One boundary tick in one transaction: persist observations and claims
/// together with the jobs they queue. Nothing is written when nothing
/// changed. Returns the number of jobs queued.
pub(crate) fn boundary_tick(conn: &Connection, config: &Config, previous: Option<i64>, at: i64) -> Result<usize> {
    let Some((runtime, actions)) = planned_tick(conn, config, previous, at)? else { return Ok(0) };
    let mut queued = 0;
    for action in &actions {
        match action {
            Action::EndArchive { day, claim } => {
                queued += usize::from(super::archive::queue_end_archive(conn, &claim.id, &day.to_string(), at)?);
            }
            Action::MorningReport { day, claim } => {
                super::archive::queue_morning(conn, &claim.id, &day.to_string(), at)?;
                queued += 1;
            }
        }
    }
    save_runtime(conn, &runtime)?;
    if !actions.is_empty() {
        prune_work_days(conn, at)?;
    }
    Ok(queued)
}

/// Drop day records older than 90 days unless an unfinished job cites them.
fn prune_work_days(conn: &Connection, at: i64) -> Result<()> {
    let Ok(today) = work::shanghai_date(at) else {
        return Ok(());
    };
    conn.execute(
        "DELETE FROM work_days WHERE date < ?1
           AND (end_claim IS NULL OR end_claim NOT IN
                (SELECT id FROM archive_attempts WHERE status IN ('queued', 'running')))
           AND (morning_claim IS NULL OR morning_claim NOT IN
                (SELECT id FROM morning_jobs WHERE status IN ('queued', 'running')))
           AND date IS NOT (SELECT overtime_day FROM work_runtime WHERE id = 1)",
        [(today - Duration::days(work::KEEP_DAYS)).to_string()],
    )?;
    Ok(())
}

/// Whether a tick at `at` would write anything; the loop only opens a
/// write transaction (and bumps the revision) when it would.
pub(crate) fn tick_pending(conn: &Connection, config: &Config, previous: Option<i64>, at: i64) -> Result<bool> {
    Ok(planned_tick(conn, config, previous, at)?.is_some())
}

// ---- Db methods -----------------------------------------------------------

impl Db {
    /// Whether the work-hours gate lets `kind` run now in `mode`.
    pub async fn work_gate(&self, config: Arc<Config>, kind: &'static str, mode: Mode) -> Result<bool> {
        self.read(move |c| ai_gate(c, &config, kind, mode, None, time::now())).await
    }

    /// Whether the failed request `request_id` received a reply that failed
    /// validation.
    pub async fn ai_reply_invalid(&self, request_id: String) -> Result<bool> {
        self.read(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM activity WHERE kind = 'ai_failed' \
                 AND json_extract(detail, '$.request_id') = ?1 \
                 AND json_extract(detail, '$.failure_class') = 'response_validation')",
                [request_id],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// The adopted policies of `kind`.
    pub async fn ai_policy(&self, kind: String) -> Result<Policies> {
        self.read(move |c| policies(c, &kind)).await
    }

    /// The adopted policies of each purpose, read together.
    pub async fn ai_policies(&self, purposes: &'static [&'static str]) -> Result<BTreeMap<&'static str, Policies>> {
        self.read(move |c| purposes.iter().map(|p| Ok((*p, policies(c, p)?))).collect()).await
    }

    /// Log the unconfigured-purpose error of `kind`.
    pub async fn log_ai_unconfigured(&self, kind: String, error: String) -> Result<()> {
        self.write(move |tx| log_unconfigured(tx, &kind, &error)).await
    }

    /// The failure row of a call, and the failure recorded once. A failed
    /// task edit is answered in its own dialog, so it stays out of the
    /// analysis error.
    pub async fn log_ai_failure(&self, kind: &'static str, detail: Value, message: String) -> Result<()> {
        self.write(move |tx| {
            super::activity::log(tx, kind, &detail)?;
            if detail["kind"] == "edit" {
                return Ok(());
            }
            ai_error(tx, &message).map(drop)
        })
        .await
    }

    /// Count one call before it is made: the work gate (with the archive
    /// claim of `attempt`), the daily limit, then the filter decision taken.
    pub async fn reserve_ai_call(
        &self,
        config: Arc<Config>,
        kind: String,
        mode: Mode,
        attempt: Option<String>,
        reserve: Option<FilterTaken>,
    ) -> Result<()> {
        self.write(move |tx| {
            let gate = ai_gate(tx, &config, &kind, mode, attempt.as_deref(), time::now())?;
            reserve_call(tx, &config, &kind, mode, gate)?;
            reserve.map_or(Ok(()), |taken| super::analysis::take_filter(tx, &taken))
        })
        .await
    }

    /// Add the tokens a call reported.
    pub async fn record_ai_usage(&self, kind: String, usage: Value) -> Result<()> {
        self.write(move |tx| record_usage(tx, &kind, &usage)).await
    }

    /// What a plain request is rendered with: the tag catalog, the enabled
    /// tools and both memories when asked for, and the last scan time.
    pub(crate) async fn ask_inputs(&self, tools: bool, memory: bool, projects: Value) -> Result<Inputs> {
        self.read(move |c| {
            let (groups, priority_kind) = super::tags::catalog(c)?;
            let memory = |slot: &str| -> Result<String> {
                Ok(if memory { super::prompts::memory(c, slot)?.text } else { String::new() })
            };
            Ok(Inputs {
                tags: json!(groups),
                priority_kind,
                projects,
                tools: if tools { super::tools::enabled_tools(c)? } else { Vec::new() },
                archived: memory("archived")?,
                temporary: memory("temporary")?,
                last_scan: kv::get_i64(c, "last_scan")?,
            })
        })
        .await
    }

    /// Queue the tag suggestions of a plain request.
    pub(crate) async fn propose_ask_tags(&self, proposals: Vec<Proposal>) -> Result<()> {
        self.write(move |tx| {
            proposals.iter().try_for_each(|(tag, reason, proof)| super::tags::propose(tx, tag, reason, proof).map(drop))
        })
        .await
    }

    /// `work_state` at `at`.
    pub async fn set_work_state(&self, config: Arc<Config>, p: Value, at: i64) -> Result<Value> {
        self.write(move |tx| set_work_state(tx, &config, &p, at)).await
    }

    /// One boundary tick; the write transaction is opened only when the
    /// tick would change something.
    pub async fn tick_work_boundary(&self, config: Arc<Config>, previous: Option<i64>, at: i64) -> Result<()> {
        let c = config.clone();
        if self.read(move |conn| tick_pending(conn, &c, previous, at)).await? {
            self.write(move |tx| boundary_tick(tx, &config, previous, at)).await?;
        }
        Ok(())
    }

    /// Fetch time of a stored holiday year, if any.
    pub async fn holiday_fetched_at(&self, year: i32) -> Result<Option<i64>> {
        self.read(move |c| {
            Ok(c.query_row("SELECT fetched_at FROM holiday_years WHERE year = ?1", [year], |r| r.get(0)).optional()?)
        })
        .await
    }

    /// Store a fetched holiday year.
    pub async fn save_holiday_year(&self, year: Year) -> Result<()> {
        self.write(move |tx| save_holiday_year(tx, &year)).await
    }
}
