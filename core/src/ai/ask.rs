//! One AI request of a domain without its own call layer (task edits,
//! natural-language generation, schedule drafts, advice revisions, prompt
//! review and learning): the payload is enriched
//! for its purpose, rendered with the adopted policy, sent on the AI slot
//! within the configured timeout and finalized (aliases decoded, tag
//! suggestions queued, the catalog the request showed attached, offered
//! projects checked).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::invoke;
use super::wire::IdMap;
use super::Mode;
use crate::tasks::tags::{self, Tag};
use crate::tools::Tool;
use crate::{Ctx, Error, Result};

/// Kinds that read and get back the tag catalog.
const TAGGED: [&str; 4] = ["format", "edit", "learning", "schedule"];
/// Kinds offered the analysis projects.
const PROJECTS: [&str; 2] = ["format", "edit"];
/// Kinds offered the registered tools.
const TOOLS: [&str; 2] = ["format", "edit"];
/// Kinds that read both memories (read-only).
const MEMORY: [&str; 2] = ["prompt_review", "learning"];

/// What the request is rendered with, read in one transaction.
pub(crate) struct Inputs {
    pub tags: Value,
    pub priority_kind: String,
    pub projects: Value,
    pub tools: Vec<Tool>,
    pub archived: String,
    pub temporary: String,
    pub last_scan: i64,
}

/// Ask `kind` with `payload`. A busy slot is the slot's error (`edit`
/// never takes the slot); the work gate and daily limit apply to
/// non-manual modes.
pub async fn ask(ctx: &Arc<Ctx>, kind: &'static str, payload: Value, mode: Mode) -> Result<Value> {
    let inputs = read_inputs(ctx, kind).await?;
    let mut payload = payload;
    let ids = enrich(kind, &mut payload, &inputs);
    let trigger =
        payload["_trigger"].as_str().unwrap_or(if mode == Mode::Manual { "manual" } else { "scheduled" }).to_string();
    let invocation = invoke::build(ctx, kind, mode, &trigger, payload).await?;
    send(ctx, kind, inputs, ids, invocation).await
}

/// Ask `kind` manually with the largest `payload(n)`, `n` in `1..=count`,
/// whose rendered request fits `max_input_chars`; the measured render is
/// the one sent. `None` when even one item is too large.
pub async fn ask_fitted(
    ctx: &Arc<Ctx>,
    kind: &'static str,
    count: usize,
    payload: impl Fn(usize) -> Value,
) -> Result<Option<(usize, Value)>> {
    let inputs = read_inputs(ctx, kind).await?;
    let policies = ctx.db.ai_policy(kind.into()).await?;
    let config = ctx.config.get();
    let render = |n: usize| -> Result<Option<(IdMap, invoke::Invocation)>> {
        let mut p = payload(n);
        let ids = enrich(kind, &mut p, &inputs);
        let invocation = invoke::render(config.clone(), &policies, kind, Mode::Manual, "manual", p)?;
        Ok((invocation.prepared.total <= config.max_input_chars).then_some((ids, invocation)))
    };
    let fitted = match render(count)? {
        Some(r) => Some((count, r)),
        None => crate::analysis::batch::fit_prefix(count.saturating_sub(1), &render)?,
    };
    let Some((n, (ids, mut invocation))) = fitted else { return Ok(None) };
    invocation.payload["_batch_metrics"] = json!({"selected": n, "initial_selected": count,
        "total_input_chars": invocation.prepared.total, "max_input_chars": config.max_input_chars});
    Ok(Some((n, send(ctx, kind, inputs, ids, invocation).await?)))
}

/// Send a rendered request on the AI slot and finalize the reply.
async fn send(
    ctx: &Arc<Ctx>,
    kind: &'static str,
    inputs: Inputs,
    ids: IdMap,
    mut invocation: invoke::Invocation,
) -> Result<Value> {
    let payload = &invocation.payload;
    let tool_task = payload["_tool_task_id"].as_str().unwrap_or("").to_string();
    let shown = json!({"audit": payload["audit"], "user_description": payload["user_description"],
        "user_instruction": payload["user_instruction"]});
    invocation.tools = inputs.tools.clone();
    invocation.tool_task = tool_task.clone();
    let timeout = Duration::from_secs(ctx.config.get().ai_timeout_seconds.max(1));
    let work = |cancel: crate::exec::Cancel| async move {
        let request_id = cancel.request_id();
        Ok((invoke::call(ctx, invocation, &cancel).await?, request_id))
    };
    // A one-sentence edit runs beside internal work; everything else
    // takes the slot or fails busy.
    let (mut value, request_id) = if kind == "edit" {
        ctx.exec.run_manual(kind, &tool_task, timeout, work).await?
    } else {
        ctx.exec.run_ai(kind, timeout, work).await?
    };
    ids.decode(&mut value)?;
    if TAGGED.contains(&kind) {
        let catalog: Vec<tags::Group> = serde_json::from_value(inputs.tags.clone()).unwrap_or_default();
        ctx.db.propose_ask_tags(proposals(&value, &shown, &catalog, &request_id)).await?;
        value["_tag_catalog"] = inputs.tags.clone();
    }
    match kind {
        "edit" => crate::analysis::layer::check_project(&mut value, &inputs.projects),
        "format" => {
            for row in value["tasks"].as_array_mut().into_iter().flatten() {
                crate::analysis::layer::check_project(row, &inputs.projects);
            }
        }
        _ => {}
    }
    Ok(value)
}

async fn read_inputs(ctx: &Arc<Ctx>, kind: &'static str) -> Result<Inputs> {
    let projects = if PROJECTS.contains(&kind) { crate::handling::offered(ctx).await } else { json!([]) };
    ctx.db.ask_inputs(TOOLS.contains(&kind), MEMORY.contains(&kind), projects).await
}

/// Add what the purpose reads to the domain payload; returns the alias map
/// of a `format` request.
fn enrich(kind: &str, payload: &mut Value, inputs: &Inputs) -> IdMap {
    if !payload.is_object() {
        *payload = json!({});
    }
    if PROJECTS.contains(&kind) {
        payload["available_projects"] = inputs.projects.clone();
    }
    if TAGGED.contains(&kind) {
        payload["tag_catalog"] = inputs.tags.clone();
        payload["priority_kind"] = json!(inputs.priority_kind);
    }
    if MEMORY.contains(&kind) {
        let (archived, temporary) = (inputs.archived.as_str(), inputs.temporary.as_str());
        payload["_archived_memory"] = json!(archived);
        payload["_temporary_memory"] = json!(temporary);
        payload["memory"] = json!({
            "available": !temporary.trim().is_empty(), "archived_available": !archived.trim().is_empty(),
            "writable": false, "max_chars": crate::prompts::memory::TEMPORARY_LIMIT,
            "archived_max_chars": crate::prompts::memory::ARCHIVED_LIMIT,
        });
    }
    if let Some(rules) = payload.as_object_mut().and_then(|o| o.remove("rules")) {
        payload["_enabled_rules"] = rules;
    }
    if payload["_data_until"].as_i64().is_none() {
        payload["_data_until"] = json!(inputs.last_scan);
    }
    let mut ids = IdMap::default();
    if kind == "format" {
        ids = IdMap::from_payload(payload);
        ids.encode(payload);
        super::wire::compact(payload);
        payload["wire_format"] = json!(crate::prompts::default("wire_format"));
    }
    if TOOLS.contains(&kind) && !inputs.tools.is_empty() {
        payload["_registered_tools"] = json!(inputs.tools.iter().map(Tool::declaration).collect::<Vec<_>>());
    }
    ids
}

/// A tag suggestion to queue: the tag, its reason and the evidence shown.
pub(crate) type Proposal = (Tag, String, Vec<(String, String)>);

/// The result's tag suggestions worth queueing (at most 8). Evidence is the
/// audit rows and the user's own text the request showed (`input`,
/// recorded as `input:<request id>`).
fn proposals(result: &Value, shown: &Value, catalog: &[tags::Group], request_id: &str) -> Vec<Proposal> {
    let mut evidence: BTreeMap<String, String> = BTreeMap::new();
    for a in shown["audit"].as_array().into_iter().flatten() {
        if let Some(i) = a["audit_index"].as_u64() {
            evidence
                .insert(format!("audit:{i}"), a["event"]["detail"].as_str().unwrap_or("").chars().take(350).collect());
        }
    }
    for key in ["user_description", "user_instruction"] {
        if let Some(text) = shown[key].as_str() {
            evidence.insert("input".into(), text.chars().take(350).collect());
        }
    }
    let mut out = Vec::new();
    for v in result["tag_suggestions"].as_array().into_iter().flatten().take(8) {
        let Ok(tag) = serde_json::from_value::<Tag>(json!({"kind": v["kind"], "value": v["value"]})) else {
            continue;
        };
        let reason = v["reason"].as_str().unwrap_or("").trim();
        let ids: Vec<&str> = v["evidence_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !tags::valid(&tag)
            || tags::known(catalog, &tag.kind, &tag.value)
            || reason.is_empty()
            || ids.is_empty()
            || ids.iter().any(|id| !evidence.contains_key(*id))
        {
            continue;
        }
        let proof: Vec<(String, String)> = ids
            .iter()
            .map(|id| {
                let shown = if *id == "input" { format!("input:{request_id}") } else { id.to_string() };
                (shown, evidence[*id].clone())
            })
            .collect();
        out.push((tag, reason.to_string(), proof));
    }
    out
}

/// `AI_UNCONFIGURED:` (logged) when `kind` has no route, before any write.
pub async fn require(ctx: &Ctx, kind: &str) -> Result<()> {
    if super::prompt::configured(&ctx.config.get(), kind) {
        return Ok(());
    }
    let message = super::prompt::unconfigured(kind);
    ctx.db.log_ai_unconfigured(kind.to_string(), message.clone()).await?;
    crate::bail!("{}{message}", super::UNCONFIGURED_PREFIX)
}

/// Whether an error means the slot or the archive queue was taken.
pub fn busy(error: &Error) -> bool {
    error.0 == super::BUSY || error.0 == super::ARCHIVE_BUSY
}
