//! The AI call layer behind [`super::Model`]: classify and message-filter
//! preparation and finalization, and the retrieval meter shared with the
//! archive layer.
//!
//! A layer is built per round with the policies, tag catalog, registered
//! tools and analysis projects read once. Rendering enriches the domain
//! payload (catalogs, rules, memories, image hints), wire-encodes the ids
//! of classify requests and measures the request exactly as the final
//! check does. Retrieval runs through [`ClassifyContext`] on the request's
//! [`Reader`]; ids returned by a read join the request's alias map. The
//! reply is humanized, strictly decoded and checked before the analysis
//! domain applies it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shishi_api::schema_validate::ValidationError;
use shishi_api::{ToolFailure, ToolFailureKind, ToolResult};

use super::context::{self, Meter, Reader};
use super::{GateFuture, Measured, Model, Reply, ReplyFuture, Request};
use crate::ai::invoke::{self, Invocation, Stage};
use crate::ai::prompt::Policies;
use crate::ai::provider::Context;
use crate::ai::wire::IdMap;
use crate::ai::Mode;
use crate::exec::Cancel;
use crate::tools::Tool;
use crate::{time, Ctx, Error, Result};

const PURPOSES: [&str; 2] = ["classify", "message_filter"];

pub const BUDGET_EXHAUSTED: &str = "本次续轮额度已用尽，未交付新的补查内容；请依据已读资料完成，并说明仍缺少的证据。";
pub const DEGRADED_REASON: &str = "模型未给出结论，本批按无事项变更降级接收";
const IMAGE_LIMIT: usize = 2;
const IMAGE_CHANGED: &str = "图片不可用或文件已变化，请重新扫描";
/// The fixed call id used to measure a tool message; only its difference
/// to the empty message counts, so the id cancels out.
const PROBE: &str = "budget-probe";

pub struct Layer {
    ctx: Arc<Ctx>,
    policies: BTreeMap<&'static str, Policies>,
    /// Fixed prompt, schema and tool overhead of classify; `None` while
    /// classify has no route.
    fixed: Option<usize>,
    tags: Value,
    priority_kind: String,
    tools: Vec<Tool>,
    projects: Value,
}

/// The analysis model with policies, catalogs and tools read once; built
/// per round so a policy edit applies to the next one.
pub async fn model(ctx: &Arc<Ctx>) -> Result<Arc<dyn Model>> {
    let config = ctx.config.get();
    let projects = crate::handling::offered(ctx).await;
    let (policies, fixed, tags, priority_kind, tools) = ctx.db.analysis_layer(config, &PURPOSES).await?;
    Ok(Arc::new(Layer { ctx: ctx.clone(), policies, fixed, tags, priority_kind, tools, projects }))
}

/// One rendered request with what finalization needs.
struct Rendered {
    invocation: Invocation,
    ids: IdMap,
    /// The raw `_local_context` the retrieval tools answer from.
    snapshot: Value,
}

impl Layer {
    fn render(&self, request: &Request) -> Result<Rendered> {
        let kind = request.purpose;
        let policies = self.policies.get(kind).ok_or_else(|| Error::new("未知请求"))?;
        let mut payload = request.payload.clone();
        let object = payload.as_object_mut().ok_or_else(|| Error::new("分析请求无效"))?;
        object.remove("_filter_decision_at");
        let mut ids = IdMap::default();
        let mut snapshot = Value::Null;
        if kind == "classify" {
            snapshot = object.remove("_local_context").unwrap_or(Value::Null);
            ids = IdMap::from_payload(&json!({"_local_context": &snapshot, "changes": object.get("changes"),
                "context": object.get("context"), "tasks": object.get("tasks"), "task_index": object.get("task_index")}));
            object.insert("available_projects".into(), self.projects.clone());
            object.insert("tag_catalog".into(), self.tags.clone());
            object.insert("priority_kind".into(), json!(self.priority_kind));
            let memory = |slot: &str| json!(snapshot[slot]["before"].as_str().unwrap_or(""));
            object.insert("_archived_memory".into(), memory("archived_memory"));
            object.insert("_temporary_memory".into(), memory("memory"));
            if let Some(rules) = object.remove("rules") {
                object.insert("_enabled_rules".into(), rules);
            }
            image_hints(&self.ctx.root, &mut payload, &mut snapshot);
            ids.encode(&mut payload);
            crate::ai::wire::compact(&mut payload);
            payload["wire_format"] = json!(crate::prompts::default("wire_format"));
            if !self.tools.is_empty() {
                payload["_registered_tools"] = json!(self.tools.iter().map(Tool::declaration).collect::<Vec<_>>());
            }
            payload["_local_context"] = snapshot.clone();
        }
        let trigger = payload["_trigger"].as_str().unwrap_or("").to_string();
        let mode = if trigger.starts_with("manual") { Mode::Manual } else { Mode::Automatic };
        let mut invocation = invoke::render(self.ctx.config.get(), policies, kind, mode, &trigger, payload)?;
        if kind == "classify" {
            let shown = &invocation.payload["_temporary_memory"];
            acknowledge_inline(&mut snapshot, &invocation.prepared.prompt, "memory", shown, "临时记忆", "增量消息");
            invocation.tools = self.tools.clone();
        }
        Ok(Rendered { invocation, ids, snapshot })
    }

    async fn ask(&self, mut measured: Measured<Request>, cancel: Cancel) -> Reply {
        let mut rendered: Rendered = match measured.take() {
            Ok(rendered) => rendered,
            Err(e) => return Reply::err(e.0),
        };
        let request = measured.request;
        let kind = request.purpose;
        if let Some(metrics) = request.payload.get("_batch_metrics") {
            rendered.invocation.payload["_batch_metrics"] = metrics.clone();
        }
        rendered.invocation.reserve = request.reserve.clone();
        let ids = Arc::new(Mutex::new(rendered.ids));
        let records: Vec<Value> = rendered.snapshot["records"].as_array().cloned().unwrap_or_default();
        if kind == "classify" {
            rendered.invocation.context = Some(Box::new(ClassifyContext {
                ctx: self.ctx.clone(),
                reader: request.reader.clone(),
                snapshot: rendered.snapshot,
                meter: Meter::default(),
                ids: ids.clone(),
            }));
        }
        let request_id = cancel.request_id();
        let outcome = match invoke::invoke(&self.ctx, rendered.invocation, &cancel).await {
            Ok(outcome) => outcome,
            Err(e) => {
                let invalid_response = self.ctx.db.ai_reply_invalid(request_id.clone()).await.unwrap_or(false);
                let commands_ran = self.commands_ran(&request_id).await;
                let refused = crate::ai::refused(&e.0);
                return Reply { result: Err(e), commands_ran, invalid_response, refused };
            }
        };
        let commands_ran = self.commands_ran(&request_id).await;
        if kind != "classify" {
            return Reply {
                result: outcome.accept(&self.ctx).await,
                commands_ran,
                invalid_response: false,
                refused: false,
            };
        }
        let mut outcome = outcome;
        let mut value = std::mem::take(&mut outcome.value);
        let decoded = {
            let ids = ids.lock().unwrap();
            let read = request.reader.as_ref().map(|r| r.accepted().records).unwrap_or_default();
            ids.humanize_prose(&mut value, records.iter().chain(&read));
            ids.decode(&mut value)
        };
        if let Err(e) = decoded {
            let error = outcome.reject(&self.ctx, e, Stage::Validation).await;
            return Reply { result: Err(error), commands_ran, invalid_response: true, refused: false };
        }
        let object = value.as_object_mut();
        let (notes, memory_error) = match object {
            Some(o) => (o.remove("_image_notes"), o.remove("_memory_error")),
            None => (None, None),
        };
        if let Some(notes) = notes {
            if let Err(e) = commit_notes(&self.ctx.root, &notes) {
                self.ctx.db.record_activity("image_notes_skipped", json!({"error": e.0})).await;
            }
        }
        if let Some(error) = memory_error {
            self.ctx.db.record_activity("memory_skipped", json!({"request_id": request_id, "error": error})).await;
        }
        value["_tag_catalog"] = self.tags.clone();
        for row in value["actions"].as_array_mut().into_iter().flatten() {
            check_project(row, &self.projects);
        }
        outcome.value = value;
        Reply { result: outcome.accept(&self.ctx).await, commands_ran, invalid_response: false, refused: false }
    }

    /// Whether a tool call of the request may have run.
    async fn commands_ran(&self, request_id: &str) -> bool {
        if request_id.is_empty() {
            return false;
        }
        self.ctx
            .db
            .tool_calls(request_id.into())
            .await
            .map(|calls| calls.iter().any(|call| call.result.as_ref().is_none_or(|r| r.status != "rejected")))
            .unwrap_or(true)
    }
}

impl Model for Layer {
    fn measure(&self, request: Request) -> Result<Measured<Request>> {
        let rendered = self.render(&request)?;
        let total = rendered.invocation.prepared.total;
        Ok(Measured::rendered(request, total, rendered))
    }

    fn message_budget(&self, max_input_chars: usize) -> usize {
        self.fixed.map_or(max_input_chars, |fixed| max_input_chars.saturating_sub(fixed))
    }

    fn work_gate<'a>(&'a self, purpose: &'static str) -> GateFuture<'a> {
        Box::pin(async move {
            self.ctx.db.work_gate(self.ctx.config.get(), purpose, Mode::Automatic).await.unwrap_or(false)
        })
    }

    fn invoke<'a>(&'a self, request: Measured<Request>, cancel: Cancel) -> ReplyFuture<'a> {
        Box::pin(self.ask(request, cancel))
    }
}

// ---- rendering helpers --------------------------------------------------------

/// Clear a suggested project the request did not offer, or one without a
/// reason.
pub fn check_project(row: &mut Value, projects: &Value) {
    let id = row["suggested_project_id"].as_str().unwrap_or("");
    let exists = !id.is_empty()
        && row["suggested_project_reason"].as_str().is_some_and(|s| !s.trim().is_empty())
        && projects.as_array().into_iter().flatten().any(|p| p["id"] == id);
    if !exists && row.is_object() {
        row["suggested_project_id"] = json!("");
        row["suggested_project_reason"] = json!("");
    }
}

/// The text between two prompt segments.
fn segment<'a>(prompt: &'a str, title: &str, next: &str) -> Option<&'a str> {
    let start = prompt.find(&format!("\n\n## {title}\n"))? + title.len() + 6;
    let end = prompt[start..].find(&format!("\n\n## {next}\n"))? + start;
    Some(&prompt[start..end])
}

/// Mark a memory the prompt carries in full, so replacing it needs no read.
fn acknowledge_inline(snapshot: &mut Value, prompt: &str, slot: &str, shown: &Value, title: &str, next: &str) {
    let Some(before) = snapshot[slot]["before"].as_str().map(str::to_string) else { return };
    if before.trim().is_empty() || snapshot[slot]["writable"] != true || shown.as_str() != Some(before.as_str()) {
        return;
    }
    if segment(prompt, title, next).is_some_and(|text| text == before.trim()) {
        snapshot[slot]["inline_text"] = json!(before);
    }
}

/// Replace private image paths with a catalog the tools resolve, and show
/// whether each image is readable and its cached note.
fn image_hints(root: &std::path::Path, payload: &mut Value, snapshot: &mut Value) {
    let mut catalog = Vec::new();
    for record in snapshot["records"].as_array_mut().into_iter().flatten() {
        if record["kind"] != "message" {
            continue;
        }
        let refs: Vec<crate::messages::ImageRef> =
            serde_json::from_value(record["data"]["images"].clone()).unwrap_or_default();
        if refs.is_empty() {
            continue;
        }
        record["data"]["images"] = json!(refs
            .iter()
            .take(8)
            .map(|i| json!({"id": i.id, "available": !i.path.is_empty()}))
            .collect::<Vec<_>>());
        for image in refs {
            let key = crate::messages::preview_key(std::path::Path::new(&image.path));
            catalog.push(json!({"id": image.id, "message_id": record["id"], "key": key}));
        }
    }
    if catalog.is_empty() {
        return;
    }
    for field in ["changes", "context"] {
        for row in payload[field].as_array_mut().into_iter().flatten() {
            for image in row["images"].as_array_mut().into_iter().flatten() {
                let Some(found) = catalog.iter().find(|c| c["id"] == image["id"]) else { continue };
                image["available"] = json!(found["key"].is_string());
                if let Some(key) = found["key"].as_str() {
                    if let Ok(text) = std::fs::read_to_string(root.join("media").join(format!("{key}.note"))) {
                        image["summary"] = json!(text.chars().take(300).collect::<String>());
                    }
                }
            }
        }
    }
    snapshot["images"] = json!(catalog);
}

/// Write accepted image notes next to the preview cache.
fn commit_notes(root: &std::path::Path, notes: &Value) -> Result<()> {
    let dir = root.join("media");
    std::fs::create_dir_all(&dir)?;
    for note in notes.as_array().into_iter().flatten() {
        let key = note["key"].as_str().unwrap_or("");
        if key.len() != 16 || !key.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        std::fs::write(dir.join(format!("{key}.note")), note["summary"].as_str().unwrap_or(""))?;
    }
    Ok(())
}

/// Characters a tool result adds to the conversation beyond its empty
/// message, images included.
fn result_chars(result: &ToolResult) -> std::result::Result<usize, ToolFailure> {
    let (message, images) = result.clone().messages(PROBE).map_err(|_| ToolFailure::new(ToolFailureKind::Failed))?;
    let empty = json!({"role": "tool", "tool_call_id": PROBE, "content": ""});
    let mic = shishi_api::openai::message_input_chars;
    let images = if images.is_empty() { 0 } else { mic(&json!({"role": "user", "content": images})) + 1 };
    Ok(mic(&message).saturating_sub(mic(&empty)) + images)
}

/// The shared allowance check: only a response that fits counts as read.
pub(crate) fn fits(
    meter: &mut Meter,
    staged: Meter,
    result: &ToolResult,
    budget: Option<usize>,
) -> std::result::Result<(), ToolFailure> {
    let chars = result_chars(result)?;
    if staged.exhausted || budget.is_some_and(|remaining| chars > remaining) {
        meter.exhausted = true;
        return Err(ToolFailure::with_message(ToolFailureKind::BudgetExhausted, BUDGET_EXHAUSTED));
    }
    *meter = staged;
    Ok(())
}

pub(crate) fn progress(meter: &Meter) -> usize {
    meter.pages.len() + meter.image_hashes.len() + usize::from(meter.exhausted)
}

/// `_context_seen`, `_context_usage`, image notes and memory replacements.
pub(crate) fn attach_result(value: &mut Value, snapshot: &Value, meter: &Meter) {
    value["_context_seen"] = json!(meter.seen);
    value["_context_usage"] = json!({"calls": meter.calls, "chars": meter.chars, "image_calls": meter.image_calls,
        "image_bytes": meter.image_bytes, "budget_exhausted": meter.exhausted});
    let notes: Vec<Value> = value["image_notes"]
        .as_array()
        .into_iter()
        .flatten()
        .take(IMAGE_LIMIT)
        .filter_map(|v| {
            let key = meter.images.get(v["image_id"].as_str()?)?;
            let summary = v["summary"].as_str()?.trim();
            (!summary.is_empty() && summary.chars().count() <= 300).then(|| json!({"key": key, "summary": summary}))
        })
        .collect();
    value["_image_notes"] = json!(notes);
    let updates: [(fn(&Value, &Meter, &Value) -> Result<Option<Value>>, &str, &str); 2] = [
        (context::memory_update, "_memory_update", "_memory_error"),
        (context::archived_memory_update, "_archived_memory_update", "_archived_memory_error"),
    ];
    for (update, ok, err) in updates {
        match update(snapshot, meter, value) {
            Ok(Some(update)) => value[ok] = update,
            Ok(None) => {}
            Err(error) => value[err] = json!(error.0),
        }
    }
    if let Some(map) = value.as_object_mut() {
        map.remove("memory_update");
        map.remove("archived_memory");
        map.remove("image_notes");
    }
}

// ---- classify retrieval ---------------------------------------------------------

/// Retrieval of one classify request: live reads through the [`Reader`],
/// aliases kept in the request's map.
struct ClassifyContext {
    ctx: Arc<Ctx>,
    reader: Option<Arc<Reader>>,
    snapshot: Value,
    meter: Meter,
    ids: Arc<Mutex<IdMap>>,
}

impl ClassifyContext {
    /// Give the ids a read returned aliases and encode the page.
    fn encode(&self, text: String) -> String {
        let Ok(mut page) = serde_json::from_str::<Value>(&text) else { return text };
        let rows = page["records"].as_array().cloned().unwrap_or_default();
        let mut extra_messages = BTreeSet::new();
        let mut extra_tasks = BTreeSet::new();
        for row in &rows {
            if let Some(reply) = row["data"]["reply"].as_str().filter(|r| !r.is_empty()) {
                extra_messages.insert(reply.to_string());
            }
            if let Some(task) = row["data"]["task_id"].as_str().filter(|t| !t.is_empty()) {
                extra_tasks.insert(task.to_string());
            }
        }
        let mut ids = self.ids.lock().unwrap();
        ids.extend_from(IdMap::from_payload(&json!({"_local_context": {"records": rows}})));
        ids.extend_from(IdMap::from_ids(extra_messages, extra_tasks));
        ids.encode(&mut page);
        page.to_string()
    }

    /// Read an image within the request's image scope.
    fn image(&self, meter: &mut Meter, args: &Value) -> std::result::Result<Value, String> {
        if self.snapshot["expires"].as_i64().is_some_and(|at| at < time::now()) {
            return Err("图片授权已过期".into());
        }
        if meter.image_calls >= IMAGE_LIMIT {
            return Err(format!("本批已达到{IMAGE_LIMIT}次图片读取上限"));
        }
        meter.image_calls += 1;
        let Some(args) = args.as_object() else { return Err("缺少图片编号".into()) };
        if args.len() != 1 {
            return Err("只接受image_id，不接受文件路径".into());
        }
        let Some(id) = args.get("image_id").and_then(Value::as_str) else { return Err("缺少图片编号".into()) };
        let image = match &self.reader {
            Some(reader) => reader.image(id).map_err(|e| e.0)?,
            None => return Err(context::IMAGE_OUT_OF_SCOPE.into()),
        };
        let message_id = self.message_of(id);
        let (key, data) = crate::messages::preview_data(
            &self.ctx.root,
            &self.ctx.host,
            std::path::Path::new(&image.path),
            IMAGE_CHANGED,
        )
        .map_err(|e| e.0)?;
        let hash = {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(data.as_bytes()))
        };
        if !meter.image_hashes.insert(hash) {
            return Ok(json!({"content": [{"type": "text", "text": "这张图片在本批已经查看，不重复传入。"}]}));
        }
        meter.images.insert(id.into(), key);
        if !message_id.is_empty() {
            meter.seen.insert(message_id);
        }
        meter.image_bytes += data.len() * 3 / 4;
        Ok(json!({"content": [
            {"type": "text", "text": format!("图片 {id}，仅本地图片资料，长边≤1280像素。")},
            {"type": "image", "mimeType": "image/jpeg", "data": data},
        ]}))
    }

    /// The message an image belongs to: the request's own or an accepted read.
    fn message_of(&self, image_id: &str) -> String {
        let own = self.snapshot["images"].as_array().into_iter().flatten().find(|i| i["id"] == image_id);
        if let Some(id) = own.and_then(|i| i["message_id"].as_str()) {
            return id.into();
        }
        self.reader
            .as_ref()
            .and_then(|r| {
                r.accepted().messages.into_values().find(|m| m.images.iter().any(|i| i.id == image_id)).map(|m| m.id)
            })
            .unwrap_or_default()
    }
}

impl Context for ClassifyContext {
    fn read(
        &mut self,
        name: &str,
        arguments: &Value,
        budget: Option<usize>,
    ) -> std::result::Result<ToolResult, ToolFailure> {
        let mut staged = self.meter.clone();
        let result = match name {
            "read_context" => {
                let mut args = arguments.clone();
                self.ids.lock().unwrap().decode_lenient(&mut args);
                let text = match &self.reader {
                    Some(reader) => reader.query_blocking(&self.snapshot, &mut staged, &args, budget),
                    None => context::query(&self.snapshot, &mut staged, &args, budget),
                };
                ToolResult::Text(self.encode(text))
            }
            "read_image" => {
                let image = self
                    .image(&mut staged, arguments)
                    .map_err(|e| ToolFailure::with_message(ToolFailureKind::Unavailable, e))?;
                ToolResult::from_mcp(&image)?
            }
            _ => return Err(ToolFailure::new(ToolFailureKind::Denied)),
        };
        fits(&mut self.meter, staged, &result, budget)?;
        if let Some(reader) = &self.reader {
            reader.accept();
        }
        Ok(result)
    }

    fn progress(&self) -> usize {
        progress(&self.meter)
    }

    /// An empty classify conclusion is accepted as degraded; the model is
    /// not asked again.
    fn validate_output(&mut self, kind: &str, value: &mut Value) -> std::result::Result<(), ValidationError> {
        let empty = value["actions"].as_array().is_some_and(Vec::is_empty)
            && value["no_action_reason"].as_str().is_none_or(|s| s.trim().is_empty());
        if kind == "classify" && empty {
            value["no_action_reason"] = json!(DEGRADED_REASON);
            value["_analysis_degraded"] = json!(true);
        }
        Ok(())
    }

    fn finish(&mut self, value: &mut Value) {
        attach_result(value, &self.snapshot, &self.meter);
    }
}
