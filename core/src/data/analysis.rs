//! Analysis storage: pending selection, retry marking, the authority
//! fingerprint, rounds and their batches. The message filter, the
//! `read_context` source and applying results live in the submodules.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::ai::{analysis_state as state, policies, update_analysis_state as update_state};
use super::{kv, rows, Db};
use crate::ai::prompt::{self, Policies};
use crate::ai::usage::State;
use crate::analysis::apply::Applied;
use crate::analysis::batch::{self, Material};
use crate::analysis::progress::{
    self, hash, route_value, Backlog, Failure, Row, BODY_CHARS, MAX_INPUT_ERRORS, ROW_OVERHEAD,
};
use crate::analysis::round::{Accept, Accepted, Prepared, Start, PARTIAL_COMMANDS};
use crate::analysis::{
    backlog, clip, Mode, AUTHORITY_CHANGED, COMMAND_RETRY_REASON, DISABLED, NOT_DUE};
use crate::ai::{AUTOMATION_DISABLED, DEFERRED};
use crate::messages::Message;
use crate::tools::Tool;
use crate::{time, Config, Result};

mod apply;
mod live;
mod message_filter;

pub(crate) use apply::collect_tags;
use apply::{apply_actions, decision_context, write_memory};
use message_filter::{early_at, filter_usage};
pub(crate) use message_filter::{consume_filter, filter_state, reset_filter, take_filter};

const RELATED: &str = "(json_extract(m.payload, '$.mine') = 1 OR json_extract(m.payload, '$.mention_me') = 1 \
     OR json_extract(m.payload, '$.mention_all') = 1 OR json_extract(m.payload, '$.private') = 1 \
     OR EXISTS (SELECT 1 FROM task_evidence e JOIN tasks t ON t.id = e.task_id \
                JOIN messages em ON em.id = e.message_id \
                WHERE t.archived = 0 AND t.merged_into = '' \
                AND em.source_id = m.source_id AND em.chat = m.chat))";

/// Unanalysed versions within the lookback window and below the input-error limit.
fn pending_where(config: &Config) -> String {
    let related = if config.analysis_related_only { format!(" AND {RELATED}") } else { String::new() };
    format!(
        "m.withdrawn = 0 AND m.analyzed_hash IS NOT m.content_hash AND (m.time >= ?1 OR m.time = 0) \
         AND NOT (m.error_hash IS m.content_hash AND m.error_count >= {MAX_INPUT_ERRORS}){related}"
    )
}

fn cutoff(config: &Config) -> i64 {
    time::now() - config.analysis_lookback_hours as i64 * time::HOUR
}

/// Pending versions sorted by `(errors, updated_at, time, id)`.
pub fn pending_rows(conn: &Connection, config: &Config) -> Result<Vec<Row>> {
    let sql = format!(
        "SELECT m.id, m.content_hash, m.time, m.updated_at, \
         CASE WHEN m.error_hash = m.content_hash THEN m.error_count ELSE 0 END AS errors, \
         CASE WHEN m.error_hash = m.content_hash THEN coalesce(m.retry_reason, '') ELSE '' END, \
         min(length(m.text), {BODY_CHARS}) \
         FROM messages m WHERE {} ORDER BY errors, m.updated_at, m.time, m.id",
        pending_where(config)
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([cutoff(config)], |r| {
        let reason: String = r.get(5)?;
        Ok(Row {
            id: r.get(0)?,
            hash: r.get(1)?,
            time: r.get(2)?,
            updated_at: r.get(3)?,
            errors: r.get(4)?,
            reason,
            chars: r.get::<_, i64>(6)? as usize,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Waiting messages, by the same condition as [`pending_rows`].
pub fn pending_count(conn: &Connection, config: &Config) -> Result<usize> {
    let sql = format!("SELECT count(*) FROM messages m WHERE {}", pending_where(config));
    let n: i64 = conn.prepare_cached(&sql)?.query_row([cutoff(config)], |r| r.get(0))?;
    Ok(n as usize)
}

/// Pending versions counted in SQL, as [`pending_rows`] would list them.
struct Pending {
    /// Not among the versions being analysed now.
    waiting: usize,
    backlog: Backlog,
}

/// Pending versions limited to `ids` when supplied.
fn pending_stats(
    conn: &Connection,
    config: &Config,
    analyzing: &BTreeMap<String, String>,
    ids: Option<&BTreeSet<&str>>,
) -> Result<Pending> {
    let sql = format!(
        "SELECT count(*), \
         coalesce(sum(NOT EXISTS (SELECT 1 FROM json_each(?2) a WHERE a.key = m.id AND a.value = m.content_hash)), 0), \
         coalesce(sum(min(length(m.text), {BODY_CHARS}) + {ROW_OVERHEAD}), 0) \
         FROM messages m WHERE {} AND (?3 IS NULL OR m.id IN (SELECT value FROM json_each(?3)))",
        pending_where(config)
    );
    let ids = ids.map(|ids| json!(ids).to_string());
    let row = conn.prepare_cached(&sql)?.query_row(
        params![cutoff(config), json!(analyzing).to_string(), ids],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
    )?;
    let (count, waiting, volume) = row;
    Ok(Pending {
        waiting: waiting as usize,
        backlog: Backlog { count: count as usize, volume: volume as usize },
    })
}

/// Current versions not yet analysed, whatever the window: what archive
/// material and reports call `pending_analysis`.
pub fn unanalyzed(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM messages WHERE withdrawn = 0 AND analyzed_hash IS NOT content_hash",
        [],
        |r| r.get(0),
    )?)
}

/// The version of `id` if it is still pending and unchanged.
#[cfg(test)]
pub fn still_pending(conn: &Connection, id: &str, hash: &str) -> Result<bool> {
    Ok(!pending_of(conn, &BTreeMap::from([(id.to_string(), hash.to_string())]), false)?.is_empty())
}

/// Drop the stale head of `queue` and return its versions that are still
/// pending, and with `unfiltered` not looked at by the filter either.
fn trim(conn: &Connection, queue: &mut VecDeque<(String, String)>, unfiltered: bool) -> Result<BTreeMap<String, String>> {
    let live = pending_of(conn, &queue.iter().cloned().collect(), unfiltered)?;
    while queue.front().is_some_and(|(id, hash)| live.get(id) != Some(hash)) {
        queue.pop_front();
    }
    Ok(live)
}

/// The listed versions that are still pending, and with `unfiltered` not
/// looked at by the filter either.
fn pending_of(
    conn: &Connection,
    versions: &BTreeMap<String, String>,
    unfiltered: bool,
) -> Result<BTreeMap<String, String>> {
    rows::pairs(
        conn,
        "SELECT id, content_hash FROM messages WHERE (id, content_hash) IN (SELECT key, value FROM json_each(?1)) \
         AND withdrawn = 0 AND analyzed_hash IS NOT content_hash AND NOT (?2 AND filter_hash IS content_hash)",
        params![json!(versions).to_string(), unfiltered],
    )
}

/// Record a failure against the listed versions that are still pending.
pub fn record_failure(
    conn: &Connection,
    versions: &BTreeMap<String, String>,
    kind: Failure,
    error: &str,
) -> Result<()> {
    let (counted, reason) = match kind {
        Failure::InputData => (
            1,
            format!(
                "{}；本条输入被拒，同一内容累计{MAX_INPUT_ERRORS}次后不再选取，内容修改后重新计数",
                clip(error, 240)
            ),
        ),
        Failure::InvalidResult => (0, format!("{}；本批未提交，等待后续自动分析", clip(error, 240))),
        Failure::CommandUnconfirmed => (0, format!("{}；{COMMAND_RETRY_REASON}", clip(error, 240))),
        Failure::NotAdopted => (0, "动作未采用，等待后续自动分析".to_string()),
    };
    conn.prepare_cached(&format!(
        "UPDATE messages SET error_count = CASE WHEN error_hash = content_hash THEN error_count ELSE 0 END + ?2, \
         error_hash = content_hash, retry_reason = ?3 WHERE {VERSIONS} AND analyzed_hash IS NOT content_hash"
    ))?
    .execute(params![json!(versions).to_string(), counted, reason])?;
    Ok(())
}

/// Rows of `messages` whose current version is one of the `?1` JSON map.
const VERSIONS: &str = "(id, content_hash) IN (SELECT key, value FROM json_each(?1))";

/// Mark the listed versions analysed and clear their retry state.
pub fn mark_analyzed(conn: &Connection, versions: &BTreeMap<String, String>) -> Result<usize> {
    let sql = format!("UPDATE messages SET analyzed_hash = content_hash, error_hash = NULL, error_count = 0, retry_reason = NULL WHERE {VERSIONS}");
    Ok(conn.prepare_cached(&sql)?.execute([json!(versions).to_string()])?)
}

/// Retry reasons of current versions, for the page.
pub fn retries(conn: &Connection) -> Result<Map<String, Value>> {
    let sql = "SELECT id, retry_reason FROM messages WHERE retry_reason IS NOT NULL \
               AND error_hash = content_hash AND analyzed_hash IS NOT content_hash AND withdrawn = 0";
    Ok(rows::pairs::<String, String>(conn, sql, [])?.into_iter().map(|(k, v)| (k, v.into())).collect())
}

/// Every source's identity, switch, revision and configuration.
pub fn sources_value(conn: &Connection) -> Result<Value> {
    let sql = "SELECT json_group_array(json_array(id, plugin_id, account_key, json(iif(enabled, 'true', 'false')), updated, config)) \
               FROM (SELECT * FROM sources ORDER BY id)";
    Ok(serde_json::from_str(&conn.query_row(sql, [], |r| r.get::<_, String>(0))?)?)
}

/// Everything a classify result depends on besides messages and tasks:
/// route, budgets, window, leaders, tools, sources, prompt, rules, tag
/// catalog and both memories. Cursors and new messages are not part of it.
fn authority_value(conn: &Connection, config: &Config) -> Result<Value> {
    let sources = sources_value(conn)?;
    let rules: Vec<Value> = super::rules::active(conn)?.into_iter().map(|r| r["id"].clone()).collect();
    let (groups, priority) = super::tags::catalog(conn)?;
    let tools: Vec<Value> =
        super::tools::enabled_tools(conn)?.iter().map(serde_json::to_value).collect::<std::result::Result<_, _>>()?;
    Ok(json!({
        "route": route_value(config, "classify"),
        "codex": config.codex,
        "workdir": config.analysis_workdir,
        "budget": [config.max_input_chars, config.retrieval_budget_chars],
        "window": [config.analysis_lookback_hours, config.analysis_related_only],
        "leaders": config.leaders,
        "tools": tools,
        "sources": sources,
        "prompt": super::prompts::effective(conn, "classify")?,
        "rules": [kv::get_i64(conn, super::rules::RULE_VERSION)?, rules],
        "tags": [groups, priority],
        "memory": super::prompts::memory_revisions(conn)?,
    }))
}

/// The analysis fingerprint: compared before and after a call (authority).
pub fn fingerprint(conn: &Connection, config: &Config) -> Result<String> {
    Ok(hash(&authority_value(conn, config)?))
}

// ---- snapshot ------------------------------------------------------------------

pub(crate) fn part(
    conn: &Connection,
    config: &Config,
    analyzing: &BTreeMap<String, String>,
    out: &mut Map<String, Value>,
) -> Result<()> {
    let p = pending_stats(conn, config, analyzing, None)?;
    out.insert("pending".into(), p.waiting.into());
    out.insert("analyzing".into(), analyzing.len().into());
    out.insert("analysis_retries".into(), Value::Object(retries(conn)?));
    out.insert("filter_usage".into(), filter_usage(conn)?);
    Ok(())
}

// ---- analysis rounds ----

impl Db {
    /// Start a round: recheck due-ness and set the one trigger time in the
    /// same transaction; the boundary is frozen here.
    pub(crate) async fn start_analysis_round(
        &self,
        config: Arc<Config>,
        mode: Mode,
        gate: bool,
        after_scan: bool,
        budget: usize,
    ) -> Result<Start> {
        self.write(move |tx| {
            let now = time::now();
            let s = state(tx)?;
            if mode == Mode::Automatic {
                if !config.automatic_analysis_enabled() {
                    return Ok(Start::Skip(DISABLED));
                }
                if !gate {
                    return Ok(Start::Skip(DEFERRED));
                }
            }
            let rows = pending_rows(tx, &config)?;
            let early = early_at(tx)?;
            if after_scan
                && mode == Mode::Automatic
                && !progress::automatic_due(&s, early.is_some(), &config, Backlog::of(&rows), budget, now)
            {
                return Ok(Start::Skip(NOT_DUE));
            }
            let empty = rows.is_empty();
            // The round really starts here: the one trigger time of manual
            // and automatic analysis, never moved back.
            update_state(tx, |s| {
                progress::mark_pending(s, !empty, now);
                if !empty {
                    s.last_analysis_at = now;
                }
            })?;
            Ok(Start::Go { boundary: rows.into_iter().map(|r| (r.id, r.hash)).collect(), decision_at: early })
        })
        .await
    }

    /// Assemble the next batch from current data: pop ineligible heads, take
    /// candidates and build the payload. Never calls the model.
    pub(crate) async fn prepare_analysis_batch(
        &self,
        config: Arc<Config>,
        mode: Mode,
        mut boundary: VecDeque<(String, String)>,
        budget: usize,
    ) -> Result<Prepared> {
        self.read(move |c| {
            let authority = fingerprint(c, &config)?;
            if mode == Mode::Automatic && !config.automatic_analysis_enabled() {
                crate::bail!("{AUTOMATION_DISABLED}");
            }
            let live = trim(c, &mut boundary, false)?;
            if boundary.is_empty() {
                return Ok(Prepared { authority, boundary, batch: None });
            }
            let ids: Vec<String> =
                boundary.iter().filter(|(id, hash)| live.get(id) == Some(hash)).map(|(id, _)| id.clone()).collect();
            let changes = candidates(c, &ids, config.max_input_chars)?;
            let mut fresh = batch::classify(&config, &changes, &material(c)?, &mut |id| super::messages::get(c, id))?;
            let hashes: BTreeMap<&str, &str> = boundary.iter().map(|(i, h)| (i.as_str(), h.as_str())).collect();
            // The version sent is the one frozen in the boundary.
            for (id, hash) in fresh.fingerprints.iter_mut() {
                if let Some(h) = hashes.get(id.as_str()) {
                    *hash = h.to_string();
                }
            }
            fresh.payload["_trigger"] = json!(trigger(c, &config, mode, &boundary, budget)?);
            let sent: Vec<String> = changes.iter().map(|m| m.id.clone()).collect();
            Ok(Prepared { authority, boundary, batch: Some((fresh, sent)) })
        })
        .await
    }

    /// Log the fitted batch's trim rows, then whether the authority, the
    /// sent versions and the listed tasks are all still current.
    pub async fn analysis_batch_current(
        &self,
        config: Arc<Config>,
        logs: Vec<(&'static str, Value)>,
        authority: String,
        fingerprints: BTreeMap<String, String>,
        versions: BTreeMap<String, i64>,
    ) -> Result<bool> {
        self.write(move |tx| {
            for (kind, row) in &logs {
                super::activity::log(tx, kind, row)?;
            }
            if fingerprint(tx, &config)? != authority {
                return Ok(false);
            }
            Ok(pending_of(tx, &fingerprints, false)?.len() == fingerprints.len()
                && live_versions(tx, &versions)?.len() == versions.len())
        })
        .await
    }

    /// Record a failed call against the batch, unless the authority changed
    /// meanwhile and no command may have run.
    pub async fn record_batch_failure(
        &self,
        config: Arc<Config>,
        authority: String,
        versions: BTreeMap<String, String>,
        kind: Failure,
        error: String,
        commands_ran: bool,
    ) -> Result<()> {
        self.write(move |tx| {
            if !commands_ran && fingerprint(tx, &config)? != authority {
                return Ok(());
            }
            record_failure(tx, &versions, kind, &error)
        })
        .await
    }

    /// The accept transaction of one batch (§3.2, §4.3).
    pub(crate) async fn accept_analysis_batch(&self, accept: Accept) -> Result<Accepted> {
        self.write(move |tx| accept_batch(tx, accept)).await
    }

    /// The end of a round that ran: a cancellation gives the filter decision
    /// back and records the interruption.
    pub async fn finish_analysis_round(
        &self,
        cancelled: bool,
        decision_at: Option<i64>,
        interrupted: Option<i64>,
    ) -> Result<()> {
        self.write(move |tx| {
            let now = time::now();
            if cancelled {
                consume_filter(tx, decision_at)?;
                update_state(tx, |s| s.analysis_interrupted_at = interrupted.unwrap_or(now))?;
            } else {
                update_state(tx, |s| s.analysis_interrupted_at = 0)?;
            }
            Ok(())
        })
        .await
    }

    /// Messages still waiting for analysis.
    pub async fn analysis_remaining(&self, config: Arc<Config>) -> Result<usize> {
        self.read(move |c| pending_count(c, &config)).await
    }

    /// What automatic due-ness reads: the state, the automatic backlog and
    /// the filter's early decision.
    pub async fn analysis_probe(&self, config: Arc<Config>) -> Result<(State, Backlog, Option<i64>)> {
        self.read(move |c| {
            let backlog = pending_stats(c, &config, &BTreeMap::new(), None)?.backlog;
            Ok((state(c)?, backlog, early_at(c)?))
        })
        .await
    }

    pub async fn mark_analysis_pending(&self, pending: bool, now: i64) -> Result<()> {
        self.write(move |tx| update_state(tx, |s| progress::mark_pending(s, pending, now)).map(|_| ())).await
    }
}

/// Why this batch runs (§2.4).
fn trigger(
    conn: &Connection,
    config: &Config,
    mode: Mode,
    boundary: &VecDeque<(String, String)>,
    budget: usize,
) -> Result<String> {
    if mode == Mode::Manual {
        return Ok("manual".into());
    }
    let f = filter_state(conn)?;
    if config.message_filter_enabled && f.early && !f.trigger.is_empty() {
        return Ok(f.trigger);
    }
    let ids: BTreeSet<&str> = boundary.iter().map(|(id, _)| id.as_str()).collect();
    let backlog = pending_stats(conn, config, &BTreeMap::new(), Some(&ids))?.backlog;
    let s = state(conn)?;
    Ok(progress::due_reason(&s, config, backlog, budget, time::now()).unwrap_or("analysis_interval").into())
}

/// The listed task versions that are still live and unchanged.
fn live_versions(conn: &Connection, versions: &BTreeMap<String, i64>) -> Result<BTreeMap<String, i64>> {
    let sql = "SELECT id, updated FROM tasks WHERE (id, updated) IN (SELECT key, value FROM json_each(?1)) AND archived = 0 AND merged_into = ''";
    rows::pairs(conn, sql, [json!(versions).to_string()])
}

fn accept_batch(tx: &Connection, mut a: Accept) -> Result<Accepted> {
    a.check()?;
    if fingerprint(tx, &a.config)? != a.authority {
        crate::bail!("{AUTHORITY_CHANGED}");
    }
    let mut live = BTreeMap::new();
    for (id, m) in std::mem::take(&mut a.allowed) {
        if super::messages::get(tx, &id)?.is_some_and(|now| now == m && !now.withdrawn) {
            live.insert(id, m);
        }
    }
    a.allowed = live;
    a.versions = live_versions(tx, &a.versions)?;
    let fingerprints = pending_of(tx, &a.fingerprints, false)?;
    let (groups, _) = super::tags::catalog(tx)?;
    let catalog = crate::tasks::tags::result_catalog(&groups, &a.value);
    let context = decision_context(tx)?;
    let applied: Applied = match apply_actions(tx, &a.value, &a.allowed, &a.versions, &catalog, &context) {
        Ok(applied) => applied,
        Err(e) => {
            super::activity::log(tx, "analysis_rejected", &json!({"request_id": a.request_id, "error": e.0}))?;
            let kind = if a.commands_ran { Failure::CommandUnconfirmed } else { Failure::InvalidResult };
            record_failure(tx, &fingerprints, kind, &e.0)?;
            return Ok(Accepted::Rejected(e.0));
        }
    };
    let memory_updated = write_memory(tx, "classify", &a.value["_memory_update"])?;
    collect_tags(tx, &a.value, &a.payload, &a.allowed, &catalog)?;
    let (retry, done): (BTreeMap<_, _>, BTreeMap<_, _>) =
        fingerprints.iter().map(|(k, v)| (k.clone(), v.clone())).partition(|(id, _)| applied.retry.contains(id));
    mark_analyzed(tx, &done)?;
    record_failure(tx, &retry, Failure::NotAdopted, "")?;
    if a.commands_ran && !retry.is_empty() {
        record_failure(tx, &retry, Failure::CommandUnconfirmed, PARTIAL_COMMANDS)?;
    }
    let now = time::now();
    let pending = pending_count(tx, &a.config)? > 0;
    update_state(tx, |s| {
        s.last_success_at = now;
        s.pending = pending;
        s.pending_since = if pending { now } else { 0 };
    })?;
    a.check()?;
    let remaining = pending_count(tx, &a.config)?;
    super::activity::log(
        tx,
        "analysis_result",
        &json!({
            "request_id": a.request_id, "applied": applied.applied, "processed": fingerprints.len(),
            "remaining": remaining, "memory_updated": memory_updated,
            "analysis_degraded": a.value["_analysis_degraded"].as_bool().unwrap_or(false),
            "no_action_reason": a.value["no_action_reason"],
        }),
    )?;
    Ok(Accepted::Done { applied: applied.applied, processed: fingerprints.len(), notices: applied.notices() })
}

// ---- batch material ----

/// Load the boundary head until the lower bound of the packed bodies passes
/// `limit`, plus one row so an oversized message is reported alone.
fn candidates(conn: &Connection, ids: &[String], limit: usize) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    let mut bound = 0usize;
    for id in ids {
        let Some(m) = super::messages::get(conn, id)? else { continue };
        bound += m.text.chars().take(BODY_CHARS).count() + progress::ROW_OVERHEAD;
        out.push(m);
        if bound > limit {
            break;
        }
    }
    Ok(out)
}

/// What `batch::classify` reads besides the changes.
fn material(conn: &Connection) -> Result<Material> {
    Ok(Material {
        live: super::tasks::list(conn, "archived = 0 AND merged_into = '' AND generation = ''")?,
        evidence: evidence_rooms(conn)?,
        history: manual_history(conn)?,
        prompt: super::prompts::effective(conn, "classify")?,
        rules: super::rules::active(conn)?,
        temporary: super::prompts::memory(conn, "temporary")?,
        archived: super::prompts::memory(conn, "archived")?,
    })
}

/// Rooms of each live task's evidence.
fn evidence_rooms(conn: &Connection) -> Result<BTreeMap<String, BTreeSet<(String, String)>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT e.task_id, m.source_id, m.chat FROM task_evidence e \
         JOIN tasks t ON t.id = e.task_id JOIN messages m ON m.id = e.message_id \
         WHERE t.archived = 0 AND t.merged_into = '' AND t.generation = ''",
    )?;
    let mut out: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (task, source, chat) = row?;
        out.entry(task).or_default().insert((source, chat));
    }
    Ok(out)
}

/// Manual corrections, merges and undos of live tasks, newest first.
fn manual_history(conn: &Connection) -> Result<Vec<Value>> {
    let sql = "SELECT a.task_id, a.time, a.kind, a.detail, a.before, a.after FROM task_audit a \
               JOIN tasks t ON t.id = a.task_id AND t.archived = 0 AND t.merged_into = '' AND t.generation = '' \
               WHERE a.kind LIKE 'manual%' OR a.kind IN ('merge', 'undo') ORDER BY a.time DESC, a.seq DESC";
    let rows = rows::objects(conn, sql, [], &json!({"before": null, "after": null}))?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let head = json!({"task_id": r["task_id"], "time": r["time"], "kind": r["kind"], "detail": clip(r["detail"].as_str().unwrap_or(""), 160)});
            batch::audit_changes(head, &r["before"], &r["after"])
        })
        .collect())
}

// ---- backlog and the loop ----

impl Db {
    /// Pending versions (manual included) minus the ones being analysed now.
    pub async fn analysis_waiting(&self, config: Arc<Config>, analyzing: BTreeMap<String, String>) -> Result<Vec<Row>> {
        self.read(move |c| waiting(c, &config, &analyzing)).await
    }

    /// Skip the previewed versions older than `hours` that `expected` still
    /// names; a message changed since the preview stays pending.
    pub async fn skip_analysis_backlog(
        &self,
        config: Arc<Config>,
        analyzing: BTreeMap<String, String>,
        hours: u64,
        expected: Map<String, Value>,
    ) -> Result<Value> {
        self.write(move |tx| {
            let (_, records) = backlog::preview(&waiting(tx, &config, &analyzing)?, hours, analyzing.len());
            let chosen: BTreeMap<String, String> = records
                .into_iter()
                .filter(|(id, hash)| expected.get(id).and_then(Value::as_str) == Some(hash.as_str()))
                .collect();
            let skipped = mark_analyzed(tx, &chosen)?;
            let remaining = pending_stats(tx, &config, &analyzing, None)?.waiting;
            let pending = pending_count(tx, &config)? > 0 || !analyzing.is_empty();
            let now = time::now();
            update_state(tx, |s| progress::mark_pending(s, pending, now))?;
            super::activity::log(
                tx,
                "analysis_skipped",
                &json!({"hours": hours, "skipped": skipped, "remaining": remaining}),
            )?;
            Ok(json!({"skipped_count": skipped, "remaining": remaining}))
        })
        .await
    }

    /// The next moment automatic analysis may become due by time alone.
    pub async fn analysis_next_due(&self, config: Arc<Config>) -> Result<i64> {
        self.read(move |c| Ok(progress::next_due(&state(c)?, &config))).await
    }

    /// Record a failure once.
    pub async fn record_ai_error(&self, message: String) -> Result<bool> {
        self.write(move |tx| super::ai::ai_error(tx, &message)).await
    }
}

fn waiting(conn: &Connection, config: &Config, analyzing: &BTreeMap<String, String>) -> Result<Vec<Row>> {
    Ok(pending_rows(conn, config)?.into_iter().filter(|r| analyzing.get(&r.id) != Some(&r.hash)).collect())
}

// ---- the AI call layer ----

impl Db {
    /// What the analysis layer reads once per round: each purpose's
    /// policies, the fixed classify overhead, the tag catalog, the priority
    /// kind and the enabled tools.
    pub async fn analysis_layer(
        &self,
        config: Arc<Config>,
        purposes: &'static [&'static str],
    ) -> Result<(BTreeMap<&'static str, Policies>, Option<usize>, Value, String, Vec<Tool>)> {
        self.read(move |c| {
            let policies = purposes.iter().map(|p| Ok((*p, policies(c, p)?))).collect::<Result<BTreeMap<_, _>>>()?;
            let fixed = prompt::message_budget(&config, "classify", &super::ai::policies(c, "classify")?)?
                .map(|budget| config.max_input_chars - budget);
            let (groups, priority_kind) = super::tags::catalog(c)?;
            Ok((policies, fixed, json!(groups), priority_kind, super::tools::enabled_tools(c)?))
        })
        .await
    }
}
