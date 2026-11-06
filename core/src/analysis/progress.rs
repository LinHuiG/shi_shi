//! Analysis progress rules: pending rows, failure kinds, due logic and
//! fingerprints. The rows and the small analysis state are stored by
//! `data::analysis`; the state is the AI layer's `ai::usage::State`.

use serde_json::{json, Value};
use sha2::Digest;

use super::COMMAND_RETRY_REASON;
use crate::{time, Config};

/// Minimum gap between an automatic round and the last trigger time.
pub const COOLDOWN: i64 = time::MINUTE;
/// Body characters counted per message (the projection keeps 2000).
pub const BODY_CHARS: usize = 2000;
/// Per-message wire overhead of the packed form `["r1",true,0]`.
pub const ROW_OVERHEAD: usize = 13;

pub use crate::ai::usage::State;

/// `pending_since` starts when work appears and clears when none is left.
pub fn mark_pending(s: &mut State, pending: bool, at: i64) {
    s.pending = pending;
    if !pending {
        s.pending_since = 0;
    } else if s.pending_since == 0 {
        s.pending_since = at;
    }
}

/// One pending message version.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub hash: String,
    pub time: i64,
    pub updated_at: i64,
    /// Failures of this version (0 when the counters belong to another).
    pub errors: i64,
    pub reason: String,
    /// Body characters, capped at [`BODY_CHARS`].
    pub chars: usize,
    pub automatic: bool,
}

/// Recorded input-data errors after which a content version is no longer
/// selected, manually or automatically; a content change starts again.
pub const MAX_INPUT_ERRORS: i64 = 3;

/// Whether automatic work may take a failed message: any failure unless
/// commands may have run.
pub fn automatic_retry(reason: &str) -> bool {
    !reason.ends_with(COMMAND_RETRY_REASON)
}

/// Kinds of failure recorded against the messages of a batch. Only
/// [`Failure::InputData`] counts towards [`MAX_INPUT_ERRORS`]; the others
/// keep the count of the version and only set the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The input itself was rejected and the failure is attributable to
    /// this one message (the provider refused a single-message batch).
    InputData,
    /// A response arrived but failed validation, or the batch was rejected.
    InvalidResult,
    /// Commands may have run: only a manual retry may take the batch.
    CommandUnconfirmed,
    /// Part of an accepted batch whose action was not adopted.
    NotAdopted,
}

/// The first interval boundary after an interruption, or after the anchor.
pub fn next_interval_at(anchor: i64, interval: i64, interrupted_at: i64) -> i64 {
    let interval = interval.max(1);
    if anchor <= 0 {
        return if interrupted_at > 0 { interrupted_at + interval } else { 0 };
    }
    let steps = (interrupted_at - anchor).max(0) / interval + 1;
    anchor + interval * steps
}

fn interval(config: &Config) -> i64 {
    config.analysis_interval_minutes.max(1) as i64 * time::MINUTE
}

/// Pending versions and their estimated packed size, for the due checks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Backlog {
    pub count: usize,
    pub volume: usize,
}

impl Backlog {
    pub fn of(rows: &[Row]) -> Self {
        Self { count: rows.len(), volume: rows.iter().map(|r| r.chars + ROW_OVERHEAD).sum() }
    }
}

/// Why automatic analysis is due now, if it is.
pub fn due_reason(
    s: &State,
    config: &Config,
    backlog: Backlog,
    message_budget: usize,
    now: i64,
) -> Option<&'static str> {
    if backlog.count == 0 || (s.last_analysis_at > 0 && now - s.last_analysis_at < COOLDOWN) {
        return None;
    }
    let anchor = if s.last_analysis_at > 0 { s.last_analysis_at } else { s.pending_since };
    let scheduled = next_interval_at(anchor, interval(config), s.analysis_interrupted_at);
    if s.analysis_interrupted_at > 0 && now < scheduled {
        return None;
    }
    if backlog.volume >= message_budget * 4 / 5 {
        return Some("message_volume");
    }
    if anchor > 0 && now >= scheduled {
        return Some("analysis_interval");
    }
    None
}

pub fn automatic_due(
    s: &State,
    filter_early: bool,
    config: &Config,
    backlog: Backlog,
    message_budget: usize,
    now: i64,
) -> bool {
    if !config.automatic_analysis_enabled() || backlog.count == 0 {
        return false;
    }
    if config.message_filter_enabled && filter_early {
        return true;
    }
    due_reason(s, config, backlog, message_budget, now).is_some()
}

/// The next moment automatic analysis may become due by time alone; 0 when
/// only a data change can make it due.
pub fn next_due(s: &State, config: &Config) -> i64 {
    if !config.automatic_analysis_enabled() || !s.pending {
        return 0;
    }
    let anchor = if s.last_analysis_at > 0 { s.last_analysis_at } else { s.pending_since };
    let scheduled = next_interval_at(anchor, interval(config), s.analysis_interrupted_at);
    scheduled.max(s.last_analysis_at + COOLDOWN)
}

pub fn hash(v: &Value) -> String {
    format!("{:x}", sha2::Sha256::digest(v.to_string().as_bytes()))
}

/// The route of `purpose`: connection identity, model and effort.
pub fn route_value(config: &Config, purpose: &str) -> Value {
    let choice = config.model_choice(purpose);
    let connection = config.connection(&choice.connection_id).map(|c| {
        json!({"id": c.id, "driver": c.driver, "executable": c.executable, "base_url": c.base_url,
               "key_env": c.key_env, "credential_id": c.credential_id, "stream": c.stream})
    });
    json!([choice.connection_id, choice.model, choice.reasoning_effort, connection])
}
