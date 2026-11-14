//! The message filter: its state row, the versions it looks at, its
//! batches and the decision a full analysis then takes.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::{cutoff, pending_of, pending_stats, pending_where, sources_value, trim, VERSIONS};
use crate::analysis::filter::{self, FilterState, Flight, Gate, Picked};
use crate::analysis::progress::{self, hash, route_value};
use crate::analysis::{batch, FilterTaken, FILTER_TIMEOUT};
use crate::data::ai::{analysis_state as state, update_analysis_state as update_state};
use crate::data::Db;
use crate::exec::CANCELLED;
use crate::{time, Config, Error, Result};

impl Db {
    /// The versions the filter would take now, or none when it must not start.
    pub(crate) async fn filter_boundary(
        &self,
        config: Arc<Config>,
        gate: Gate,
        now: i64,
    ) -> Result<Option<VecDeque<(String, String)>>> {
        self.read(move |c| boundary(c, &config, &gate, now)).await
    }

    /// Recheck the boundary in the slot and start the cooldown: the
    /// versions and the authority fixed for the pass.
    pub(crate) async fn start_filter(
        &self,
        config: Arc<Config>,
        gate: Gate,
    ) -> Result<Option<(VecDeque<(String, String)>, String)>> {
        self.write(move |tx| {
            let now = time::now();
            let Some(boundary) = boundary(tx, &config, &gate, now)? else { return Ok(None) };
            let authority = filter_authority(tx, &config)?;
            let mut s = filter_state(tx)?;
            s.last_run = now;
            save_filter(tx, &s)?;
            Ok(Some((boundary, authority)))
        })
        .await
    }

    /// The next batch from `queue` within `max_input`: the queue without its
    /// stale head, the message rows, their versions and whether the budget
    /// bounded it. None when the filter must stop.
    pub(crate) async fn pick_filter_batch(
        &self,
        config: Arc<Config>,
        open: bool,
        authority: String,
        mut queue: VecDeque<(String, String)>,
    ) -> Result<Option<Picked>> {
        self.read(move |c| {
            if !config.early_analysis_enabled() || !open || filter_authority(c, &config)? != authority {
                return Ok(None);
            }
            let max_input = config.max_input_chars;
            let live = trim(c, &mut queue, true)?;
            let mut picked = Picked { queue: VecDeque::new(), rows: Vec::new(), versions: Vec::new(), bounded: false };
            let mut chars = 2usize;
            for (id, hash) in queue.iter().filter(|(id, hash)| live.get(id) == Some(hash)) {
                let Some(message) = crate::data::messages::get(c, id)? else { continue };
                let value = batch::message(&message, filter::BODY_CHARS);
                let size = batch::chars(&value) + usize::from(!picked.rows.is_empty());
                if !picked.rows.is_empty() && chars.saturating_add(size) > max_input {
                    picked.bounded = true;
                    break;
                }
                chars += size;
                picked.rows.push(value);
                picked.versions.push((id.clone(), hash.clone()));
                if chars > max_input {
                    break;
                }
            }
            picked.queue = queue;
            Ok(Some(picked))
        })
        .await
    }

    /// Whether any chosen version changed or was looked at meanwhile.
    pub async fn filter_batch_stale(&self, versions: BTreeMap<String, String>) -> Result<bool> {
        self.read(move |c| Ok(pending_of(c, &versions, true)?.len() < versions.len())).await
    }

    /// Commit one batch's decision; returns whether to filter the next one.
    pub(crate) async fn commit_filter(
        &self,
        config: Arc<Config>,
        flight: Flight,
        result: Result<Value>,
        deadline: i64,
        interrupted: Option<i64>,
        cancelled_flag: bool,
    ) -> Result<bool> {
        self.write(move |tx| {
            if !config.early_analysis_enabled() || filter_authority(tx, &config)? != flight.authority {
                return Ok(false);
            }
            let now = time::now();
            let result = if interrupted.is_some() {
                Err(Error::new(CANCELLED))
            } else if now >= deadline {
                Err(Error::new(FILTER_TIMEOUT))
            } else if cancelled_flag {
                Err(Error::new(CANCELLED))
            } else {
                result
            };
            let cancelled = result.as_ref().is_err_and(|e| e.0 == CANCELLED);
            if result.is_ok() {
                observe(tx, &flight.versions)?;
            }
            let mut s = filter_state(tx)?;
            s.early = !cancelled && !result.as_ref().is_ok_and(|v| v["decision"] == "wait");
            s.trigger = if !s.early {
                String::new()
            } else if result.is_err() {
                "message_filter_failed".into()
            } else {
                "message_filter".into()
            };
            s.reason = match &result {
                Ok(v) => v["reason"].as_str().unwrap_or("").chars().take(200).collect(),
                Err(e) if cancelled => e.0.clone(),
                Err(e) => format!("过滤失败，已回退完整分析：{}", e.0).chars().take(200).collect(),
            };
            save_filter(tx, &s)?;
            if cancelled {
                update_state(tx, |a| a.analysis_interrupted_at = interrupted.unwrap_or(now))?;
            }
            Ok(!cancelled && !s.early)
        })
        .await
    }
}

pub(crate) fn filter_state(conn: &Connection) -> Result<FilterState> {
    Ok(conn.query_row("SELECT last_run, early, trigger, reason FROM filter_state WHERE id = 1", [], |r| {
        Ok(FilterState { last_run: r.get(0)?, early: r.get(1)?, trigger: r.get(2)?, reason: r.get(3)? })
    })?)
}

fn save_filter(conn: &Connection, s: &FilterState) -> Result<()> {
    conn.execute(
        "UPDATE filter_state SET last_run = ?1, early = ?2, trigger = ?3, reason = ?4 WHERE id = 1",
        params![s.last_run, s.early, s.trigger, s.reason],
    )?;
    Ok(())
}

/// The snapshot's `filter_usage`; call and token counts are the AI layer's
/// daily `message_filter` usage.
pub(crate) fn filter_usage(conn: &Connection) -> Result<Value> {
    let s = filter_state(conn)?;
    let u = crate::data::ai::usage_today(conn, "message_filter")?;
    Ok(json!({"last_run": s.last_run, "early": s.early, "calls": u.calls, "input_tokens": u.input_tokens,
              "output_tokens": u.output_tokens, "unreported_calls": u.unreported_calls, "reason": s.reason}))
}

/// A switched-off filter keeps no early hint and no looked-at marks.
pub(crate) fn reset_filter(conn: &Connection) -> Result<()> {
    let mut s = filter_state(conn)?;
    s.early = false;
    s.trigger.clear();
    s.reason.clear();
    save_filter(conn, &s)?;
    conn.execute("UPDATE messages SET filter_hash = NULL WHERE filter_hash IS NOT NULL", [])?;
    Ok(())
}

/// The decision a full analysis started from: `last_run` while early.
pub(crate) fn early_at(conn: &Connection) -> Result<Option<i64>> {
    let s = filter_state(conn)?;
    Ok(s.early.then_some(s.last_run))
}

/// A full analysis took the early decision made at `at`; a later decision
/// is never cleared by an older request.
pub(crate) fn consume_filter(conn: &Connection, at: Option<i64>) -> Result<()> {
    let mut s = filter_state(conn)?;
    if s.early && at == Some(s.last_run) {
        s.early = false;
        s.trigger.clear();
        save_filter(conn, &s)?;
    }
    Ok(())
}

/// A reserved full analysis took the filter decision and looked at its
/// changes; runs inside the reserving transaction.
pub(crate) fn take_filter(conn: &Connection, taken: &FilterTaken) -> Result<()> {
    consume_filter(conn, taken.decision_at)?;
    observe(conn, &taken.observed)
}

/// Mark the listed versions as looked at.
fn observe(conn: &Connection, versions: &BTreeMap<String, String>) -> Result<()> {
    conn.prepare_cached(&format!("UPDATE messages SET filter_hash = content_hash WHERE {VERSIONS}"))?
        .execute([json!(versions).to_string()])?;
    Ok(())
}

/// What a filter decision depends on: its route, budgets, window and
/// the sources.
fn filter_authority(conn: &Connection, config: &Config) -> Result<String> {
    Ok(hash(&json!({
        "route": route_value(config, "message_filter"),
        "codex": config.codex,
        "workdir": config.analysis_workdir,
        "budget": [config.max_input_chars, config.retrieval_budget_chars],
        "window": [config.analysis_lookback_hours, config.analysis_related_only],
        "sources": sources_value(conn)?,
    })))
}

/// The versions to filter now, or none when the filter must not start.
fn boundary(conn: &Connection, config: &Config, gate: &Gate, now: i64) -> Result<Option<VecDeque<(String, String)>>> {
    let s = filter_state(conn)?;
    let analysis = state(conn)?;
    if !config.early_analysis_enabled()
        || s.early
        || now < progress::next_interval_at(s.last_run, filter::COOLDOWN, analysis.analysis_interrupted_at)
        || !gate.open
    {
        return Ok(None);
    }
    let backlog = pending_stats(conn, config, &BTreeMap::new(), None)?.backlog;
    if progress::automatic_due(&analysis, false, config, backlog, gate.message_budget, now) {
        return Ok(None);
    }
    let sql = format!(
        "SELECT m.id, m.content_hash FROM messages m WHERE {} AND m.filter_hash IS NOT m.content_hash \
         ORDER BY m.updated_at, m.time, m.id",
        pending_where(config)
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let pending: VecDeque<(String, String)> =
        stmt.query_map([cutoff(config)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok((!pending.is_empty()).then_some(pending))
}
