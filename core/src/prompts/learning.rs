//! `learn`: which audit rows count as correction evidence, how an AI answer
//! may change the rule suggestions, and the request that asks for it.
//! Suggestions are stored by the tasks domain. A learned rule always starts
//! disabled and pending: the user enables it.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ai::INPUT_LIMIT_ERROR;
use crate::ai::{ask, Mode};
use crate::analysis::{batch, clip};
use crate::archive::Audit;
use crate::tasks::rules;
use crate::{Ctx, Result};

/// An audit row as far as learning looks at it.
pub struct Evidence<'a> {
    pub kind: &'a str,
    pub detail: &'a str,
    pub after: &'a Value,
}

fn has_note(after: &Value) -> bool {
    after["archive_note"].as_str().is_some_and(|t| !t.trim().is_empty())
}

/// Whether an audit row is a user correction worth learning from.
pub fn is_evidence(e: &Evidence) -> bool {
    rules::learning_evidence(e.kind, e.detail, e.after)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Suggestion {
    pub id: String,
    pub title: String,
    pub rule: String,
    /// Audit row ids.
    pub evidence: Vec<i64>,
    pub active: bool,
    pub decision: String,
    pub created: i64,
    pub updated: i64,
    pub archived_at: i64,
}

impl Suggestion {
    pub fn decision(&self) -> &str {
        match self.decision.as_str() {
            "" if self.active => "enabled",
            "" => "pending",
            d => d,
        }
    }
}

/// Apply up to three proposed suggestions. Invalid proposals are dropped
/// silently. `eligible` is this batch, `history` the earlier evidence the
/// request showed, and `audit` looks a row up by id. Returns (added, updated).
pub fn apply<'a>(
    suggestions: &mut Vec<Suggestion>,
    rows: &[Value],
    eligible: &[i64],
    history: &[i64],
    audit: impl Fn(i64) -> Option<Evidence<'a>>,
    now: i64,
) -> (usize, usize) {
    let (mut added, mut updated) = (0, 0);
    for v in rows.iter().take(3) {
        let Ok(mut r) = Suggestion::deserialize(v) else { continue };
        r.evidence.sort();
        r.evidence.dedup();
        if r.title.trim().is_empty() || r.rule.trim().is_empty() || !r.evidence.iter().any(|i| eligible.contains(i)) {
            continue;
        }
        let sid = v["suggestion_id"].as_str().unwrap_or("");
        let existing =
            suggestions.iter().position(|x| x.id == sid || (sid.is_empty() && x.rule.trim() == r.rule.trim()));
        let prior = existing.map(|i| suggestions[i].evidence.clone()).unwrap_or_default();
        if r.evidence.iter().any(|i| {
            (!eligible.contains(i) && !history.contains(i) && !prior.contains(i))
                || !audit(*i).is_some_and(|e| is_evidence(&e))
        }) {
            continue;
        }
        let explicit_note =
            r.evidence.iter().any(|i| audit(*i).is_some_and(|e| e.kind == "manual_archive" && has_note(e.after)));
        if existing.is_none() && r.evidence.len() < 2 && !explicit_note {
            continue;
        }
        if let Some(pos) = existing {
            let s = &mut suggestions[pos];
            if s.archived_at > 0 || s.decision() != "pending" {
                continue;
            }
            r.evidence.extend(prior);
            r.evidence.sort();
            r.evidence.dedup();
            s.updated = now;
            s.title = r.title;
            s.rule = r.rule;
            s.evidence = r.evidence;
            updated += 1;
        } else if sid.is_empty() {
            suggestions.push(Suggestion {
                id: uuid::Uuid::new_v4().to_string(),
                created: now,
                updated: now,
                active: false,
                decision: "pending".into(),
                archived_at: 0,
                ..r
            });
            added += 1;
        }
    }
    (added, updated)
}

/// The message `learn` returns for a commit.
pub fn message(added: usize, updated: usize) -> &'static str {
    if added + updated == 0 {
        "本次无新增建议，既有建议继续保留"
    } else {
        "建议已更新，未处理建议继续保留"
    }
}

/// Returned instead of an AI call when no new evidence exists.
pub const NOTHING_NEW: &str = "无新增纠错或归档备注，沿用既有建议";

/// What one `learn` request is built from, read in one transaction.
pub(crate) struct Batch {
    /// Unlearned evidence rows after the cursor, oldest first.
    pub(crate) eligible: Vec<Audit>,
    /// Audit seq the cursor moves to when the whole batch is processed.
    pub(crate) end: i64,
    pub(crate) history_ids: Vec<i64>,
    /// `{audit_index, event, messages}` per eligible row.
    pub(crate) audit: Vec<Value>,
    pub(crate) base: Value,
}

/// The model's view of an audit row.
pub(crate) fn event(a: &Audit) -> Value {
    batch::audit_changes(
        json!({"task_id": a.task_id, "time": a.time, "kind": a.kind, "detail": clip(&a.detail, 160)}),
        &a.before,
        &a.after,
    )
}

/// Existing suggestions, the ones sharing most terms with `context` first.
pub(crate) fn existing(all: &[Suggestion], context: &str) -> Value {
    let terms = batch::terms([context]);
    let mut entries: Vec<&Suggestion> = all.iter().collect();
    entries.sort_by_cached_key(|r| {
        let shared = terms.intersection(&batch::terms([r.title.as_str(), r.rule.as_str()])).count();
        (std::cmp::Reverse(shared), std::cmp::Reverse(r.updated))
    });
    batch::rows(
        entries.into_iter().map(|r| {
            json!({"id": r.id, "title": clip(&r.title, 80), "rule": clip(&r.rule, 240), "decision": r.decision(),
                "evidence": r.evidence.iter().rev().take(3).collect::<Vec<_>>()})
        }),
        2400,
    )
}

/// `learn`: ask for rule suggestions from the corrections after the cursor.
pub async fn learn(ctx: &Arc<Ctx>) -> Result<Value> {
    ask::require(ctx, "learning").await?;
    let config = ctx.config.get();
    let open = ctx.db.work_gate(config, "learning", Mode::Manual).await?;
    if !open {
        return Ok(json!({"skipped": crate::ai::DEFERRED}));
    }
    let Some(b) = ctx.db.open_learning().await? else {
        return Ok(json!({"skipped": NOTHING_NEW}));
    };
    let payload = |n: usize| {
        let mut p = b.base.clone();
        p["audit"] = json!(&b.audit[..n]);
        p
    };
    let (count, result) = ask::ask_fitted(ctx, "learning", b.eligible.len(), payload).await?.ok_or_else(|| {
        crate::Error(format!("{INPUT_LIMIT_ERROR}：单条纠错记录及必需上下文超过单次输入上限，未调用 AI"))
    })?;
    let (added, updated) = ctx.db.commit_learning(b, count, result).await?;
    Ok(json!({"added": added, "updated": updated, "message": message(added, updated)}))
}
