//! The AI call layer behind [`super::Model`]: archive and report requests
//! are rendered once with the adopted policy ([`invoke::render`]), measured
//! exactly as the final check measures them, and the chosen render is sent
//! through [`invoke::call`] on the slot `archive::ask` already holds.
//! Archive requests read their frozen records through [`archive_context`].
//!
//! The mode comes from the payload's `_trigger`: `manual_*` is manual,
//! `scheduled_report` automatic, every other archive trigger (`work_end`)
//! nightly with its `_archive_attempt_id` claim.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use shishi_api::{ToolFailure, ToolFailureKind, ToolResult};

use super::{Measured, Model, ModelFuture, Prepared};
use crate::ai::invoke::{self, Invocation};
use crate::ai::prompt::Policies;
use crate::ai::provider::Context;
use crate::ai::Mode;
use crate::analysis::context::{self, Meter};
use crate::analysis::layer::{attach_result, fits, progress};
use crate::exec::Cancel;
use crate::{Ctx, Error, Result};

const PURPOSES: [&str; 2] = ["archive", "report"];

pub struct Layer {
    ctx: Arc<Ctx>,
    policies: BTreeMap<&'static str, Policies>,
}

/// The model of the archive domain, with the policies read once; an error
/// only when the adopted policies cannot be read.
pub async fn model(ctx: &Arc<Ctx>) -> Result<Arc<dyn Model>> {
    let policies = ctx.db.ai_policies(&PURPOSES).await?;
    Ok(Arc::new(Layer { ctx: ctx.clone(), policies }))
}

pub fn mode(purpose: &str, payload: &Value) -> Mode {
    let trigger = payload["_trigger"].as_str().unwrap_or("");
    if trigger.starts_with("manual") {
        Mode::Manual
    } else if purpose == "archive" {
        Mode::Nightly
    } else {
        Mode::Automatic
    }
}

impl Model for Layer {
    fn measure(&self, request: Prepared) -> Result<Measured<Prepared>> {
        let kind = request.purpose;
        let policies = self.policies.get(kind).ok_or_else(|| Error::new("未知请求"))?;
        let trigger = request.payload["_trigger"].as_str().unwrap_or("").to_string();
        let mode = mode(kind, &request.payload);
        let invocation =
            invoke::render(self.ctx.config.get(), policies, kind, mode, &trigger, request.payload.clone())?;
        Ok(Measured::rendered(request, invocation.prepared.total, invocation))
    }

    fn invoke<'a>(&'a self, mut request: Measured<Prepared>, cancel: Cancel) -> ModelFuture<'a> {
        Box::pin(async move {
            let mut invocation: Invocation = request.take()?;
            invocation.context = archive_context(&invocation.kind, &invocation.payload);
            invoke::call(&self.ctx, invocation, &cancel).await
        })
    }
}

/// Frozen retrieval of an archive or report request.
struct ArchiveContext {
    snapshot: Value,
    meter: Meter,
}

/// The retrieval context of archive requests: they read their frozen records
/// and may replace the archived memory.
pub fn archive_context(kind: &str, payload: &Value) -> Option<Box<dyn Context>> {
    if kind != "archive" || !payload["_local_context"].is_object() {
        return None;
    }
    let mut snapshot = payload["_local_context"].clone();
    let before = payload["_archived_memory"].as_str().unwrap_or("").to_string();
    snapshot["archived_memory"]["before"] = json!(before);
    snapshot["archived_memory"]["writable"] = json!(true);
    // The prompt shows the archived memory segment trimmed.
    if !before.trim().is_empty() {
        snapshot["archived_memory"]["inline_text"] = json!(before);
    }
    Some(Box::new(ArchiveContext { snapshot, meter: Meter::default() }))
}

impl Context for ArchiveContext {
    fn read(
        &mut self,
        name: &str,
        arguments: &Value,
        budget: Option<usize>,
    ) -> std::result::Result<ToolResult, ToolFailure> {
        let mut staged = self.meter.clone();
        let result = match name {
            "read_context" => ToolResult::Text(context::query(&self.snapshot, &mut staged, arguments, budget)),
            "read_image" => return Err(ToolFailure::new(ToolFailureKind::Unavailable)),
            _ => return Err(ToolFailure::new(ToolFailureKind::Denied)),
        };
        fits(&mut self.meter, staged, &result, budget)?;
        Ok(result)
    }

    fn progress(&self) -> usize {
        progress(&self.meter)
    }

    fn finish(&mut self, value: &mut Value) {
        attach_result(value, &self.snapshot, &self.meter);
        if let Some(map) = value.as_object_mut() {
            map.remove("_image_notes");
            map.remove("_memory_update");
            map.remove("_memory_error");
        }
    }
}
