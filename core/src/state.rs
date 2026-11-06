//! The `state` snapshot; its business keys come from one read in
//! [`crate::Db::snapshot`].
//!
//! The page sends back the `core_revision` it holds; when nothing it can
//! see has changed it gets the small `_unchanged` snapshot carrying only the
//! runtime fields, which the page merges shallowly over its copy.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::params::Params;
use crate::{activity, time, Ctx, Result};

/// What the page is looking at; it decides which sections and how much of
/// each row a snapshot carries.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Scope {
    /// The page shown; each domain sends only what it renders.
    pub page: String,
    /// Task whose handling window is open.
    pub task: String,
    /// Task selected on the tasks page.
    pub selected: String,
    /// Day shown on the logs page.
    pub log_day: String,
}

impl Scope {
    pub fn from(params: &Value) -> Self {
        let page = params.str_of("page").to_string();
        let log_day = match params.str_of("log_day") {
            "" if page == "logs" => time::today(),
            day => day.to_string(),
        };
        Self { task: params.str_of("task").into(), selected: params.str_of("selected_task").into(), log_day, page }
    }

    /// Whether `id` is the task the page points at and so needs full detail.
    pub fn refers(&self, id: &str) -> bool {
        !id.is_empty() && (id == self.task || id == self.selected)
    }
}

/// Business revision plus everything else that changes what the page
/// renders, but not live AI output or log appends.
pub fn core_revision(ctx: &Ctx, scope: &Scope) -> String {
    let (flags, scope) = (&ctx.flags, serde_json::to_string(scope).unwrap_or_default());
    format!(
        "{}:{}:{}:{}:{}:{scope}",
        ctx.db.revision(),
        time::now() / time::MINUTE,
        flags.busy(),
        ctx.exec.ai_busy(),
        flags.analyzing().len()
    )
}

const ACTIVITY_SHOWN: usize = 60;

/// Answer the `state` request.
pub async fn snapshot(ctx: &std::sync::Arc<Ctx>, params: &Value) -> Result<Value> {
    let scope = Scope::from(params);
    let revision = core_revision(ctx, &scope);
    let unchanged = params.str_of("core_revision") == revision;
    let (activity, business) =
        ctx.db.snapshot(scope.clone(), unchanged, ctx.config.get(), ctx.analysis.analyzing(), ACTIVITY_SHOWN).await?;
    let mut out = match business {
        Some(mut out) => {
            out.insert("version".into(), env!("CARGO_PKG_VERSION").into());
            out.insert("build_time".into(), option_env!("SHISHI_BUILD_TIME").unwrap_or("").into());
            out.insert("data_directory".into(), ctx.root.display().to_string().into());
            // Domains may add keys to `config` (the tool list); settings win.
            let mut config = out.remove("config").unwrap_or_else(|| json!({}));
            config
                .as_object_mut()
                .unwrap()
                .extend(serde_json::to_value(&*ctx.config.get())?.as_object().unwrap().clone());
            out.insert("config".into(), config);
            out
        }
        None => Map::from_iter([("_unchanged".into(), true.into())]),
    };
    out.insert("ai_interrupted_at".into(), ctx.exec.last_interrupt().unwrap_or(0).into());
    // Fields that change without a business commit; every snapshot has them.
    let status = ctx.exec.ai_status().unwrap_or_default();
    let edits: Vec<Value> = ctx
        .exec
        .manual_status()
        .into_iter()
        .map(|s| {
            json!({"task": s.subject, "request_id": s.request_id, "started": s.started,
            "cancelling": s.cancelling, "progress": s.live.phase, "live": s.live})
        })
        .collect();
    let busy = !status.request_id.is_empty();
    let runtime = json!({
        "ai_busy": busy, "busy": ctx.flags.busy(), "ai_request_id": status.request_id, "ai_cancelling": status.cancelling,
        "ai_source": status.source, "ai_started": status.started, "ai_progress": status.live.phase,
        "ai_live": if busy { json!(status.live) } else { Value::Null }, "ai_edits": edits, "updates": ctx.flags.updates(),
        "activity": activity, "_page": scope.page, "core_revision": revision,
    });
    out.extend(runtime.as_object().unwrap().clone());
    crate::handling::part(ctx, &scope, &mut out);
    if scope.page == "logs" {
        out.insert("activity_day_revision".into(), activity::day_revision(ctx, &scope.log_day).await);
    }
    Ok(Value::Object(out))
}

/// Runtime-only state shared across domains, held in memory.
pub struct Flags {
    busy: AtomicUsize,
    analyzing: Mutex<BTreeSet<String>>,
    updates: Mutex<Value>,
}

impl Default for Flags {
    fn default() -> Self {
        Self { busy: Default::default(), analyzing: Default::default(), updates: Mutex::new(json!({"status": "idle"})) }
    }
}

impl Flags {
    /// Non-AI background work such as a scan is running.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    /// Mark non-AI work as running until the guard drops.
    pub fn working<'a>(&'a self, ctx: &'a Ctx) -> impl Drop + 'a {
        self.busy.fetch_add(1, Ordering::SeqCst);
        ctx.db.bump();
        Working(ctx)
    }

    pub fn analyzing(&self) -> Vec<String> {
        self.analyzing.lock().unwrap().iter().cloned().collect()
    }

    pub fn set_analyzing(&self, ctx: &Ctx, ids: impl IntoIterator<Item = String>) {
        *self.analyzing.lock().unwrap() = ids.into_iter().collect();
        ctx.db.bump();
    }

    pub fn updates(&self) -> Value {
        self.updates.lock().unwrap().clone()
    }

    /// Update progress is runtime-only.
    pub fn set_updates(&self, ctx: &Ctx, value: Value) {
        *self.updates.lock().unwrap() = value;
        ctx.db.poke();
    }
}

struct Working<'a>(&'a Ctx);

impl Drop for Working<'_> {
    fn drop(&mut self) {
        self.0.flags.busy.fetch_sub(1, Ordering::SeqCst);
        self.0.db.bump();
    }
}

#[cfg(test)]
mod tests;
