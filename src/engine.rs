#[path = "engine/editing.rs"]
mod editing;
#[path = "engine/persistence.rs"]
mod persistence;
#[cfg(test)]
#[path = "engine/tests.rs"]
mod regression_tests;
#[path = "engine/reports.rs"]
mod reports;
#[path = "engine/ui_state.rs"]
mod ui_state;
#[cfg(test)]
#[path = "engine/workflow_tools_tests.rs"]
mod workflow_tools_tests;

use crate::{
    ai,
    model::*,
    process, prompts, schedule,
    work_hours::{self, AiMode},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

pub struct App {
    pub(crate) tool_runtime: crate::tool_calling::Runtime,
    pub(crate) plugin_settings: Mutex<crate::plugin_settings::Sessions>,
    pub(crate) notifications: Mutex<crate::task_notifications::Queue>,
    pub(crate) store: Mutex<Option<crate::state_store::Store>>,
    pub(crate) desktop_bridge: Mutex<Option<std::sync::Arc<dyn crate::desktop_bridge::DesktopApi>>>,
    pub(crate) desktop_reading: Mutex<BTreeSet<String>>,
    pub(crate) handling_previews: Mutex<BTreeMap<String, crate::handling::Preview>>,
    pub(crate) handling_preview_revision: AtomicU64,
    pub(crate) updates: crate::updates::Runtime,
    pub(crate) handling_client:
        Mutex<BTreeMap<String, std::sync::Arc<dyn crate::ai_backend::AgentSession>>>,
    pub(crate) handling_action: Mutex<()>,
    pub(crate) handling_followups: Mutex<BTreeSet<String>>,
    pub(crate) handling_decision: Mutex<()>,
    pub(crate) handling_stop: Mutex<()>,
    pub(crate) plugin_access: std::sync::RwLock<()>,
    pub(crate) root: PathBuf,
    pub(crate) calendar: Mutex<crate::holiday::CalendarData>,
    pub(crate) state: Mutex<State>,
    pub(crate) busy: AtomicBool,
    pub(crate) ai_busy: AtomicBool,
    pub(crate) storage_changing: AtomicBool,
    pub(crate) active_ai: Mutex<Option<crate::ai_control::ActiveAi>>,
    pub(crate) analyzing_messages: Mutex<BTreeMap<String, String>>,
    pub(crate) cache_prev: Mutex<BTreeMap<String, Vec<(usize, u64)>>>,
    summary_busy: AtomicBool,
    pub(crate) archive_busy: AtomicBool,
    pub(crate) prompt_busy: AtomicBool,
    pub(crate) activity: Mutex<Vec<Value>>,
    pub(crate) activity_revision: AtomicU64,
    pub(crate) log_writer: crate::log_writer::LogWriter,
    pub(crate) ai_progress: Mutex<String>,
    pub(crate) ai_live: Mutex<crate::ai_progress::LiveProgress>,
    pub(crate) analysis_last_attempt: std::sync::atomic::AtomicI64,
    pub(crate) learning_last_attempt: std::sync::atomic::AtomicI64,
    pub(crate) ai_started: std::sync::atomic::AtomicI64,
    pub(crate) last_attempt: std::sync::atomic::AtomicI64,
    /// Successful saves; part of the UI change pulse.
    saves: AtomicU64,
    /// Serializes rebuilding the on-disk analysis snapshots. Rebuilds run on
    /// a state copy so the global state lock is not held across disk I/O.
    /// Lock order: snapshot_files before state.
    pub(crate) snapshot_files: Mutex<()>,
}
struct Guard<'a>(&'a AtomicBool);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst)
    }
}

/// Inputs captured once for one AI request. Candidate reductions reuse the same
/// config, catalog, project list, memory, frozen clock and retrieval records;
/// no candidate re-reads live state or re-clocks the runtime.
struct PrepareContext {
    decision_context: Value,
    config: Config,
    catalog: Value,
    priority_kind: String,
    projects: Value,
    memory_archived: String,
    memory_temporary: String,
    last_scan: i64,
    runtime_at: i64,
    work_status: String,
    business_date: String,
    records: Vec<Value>,
    runtime: Option<Value>,
}

/// The exact rendered request. The same object is measured during reduction and
/// then passed to the adapter unchanged; its prompt, schema, tool schema,
/// runtime and wire mapping are never rebuilt after acceptance.
pub(crate) struct PreparedAiRequest {
    pub decision_context: Value,
    pub kind: String,
    pub config: Config,
    pub runtime: Value,
    pub payload: Value,
    pub prepared: ai::Prepared,
    pub wire_ids: crate::wire_context::IdMap,
    pub tag_payload: Value,
    pub catalog: Value,
    pub trigger: String,
    pub compression_before: usize,
    pub records: Vec<Value>,
    pub initial_tasks: BTreeSet<String>,
    pub cache_components: Value,
    pub frozen: Option<FrozenView>,
}

/// Authoritative view of what this invocation could see. It is captured before
/// the call and survives snapshot rebuilds; post-call admission never reads the
/// shared snapshot files again.
pub(crate) struct FrozenView {
    pub initial_messages: BTreeMap<String, Message>,
    pub messages: BTreeMap<String, Message>,
    pub fingerprints: BTreeMap<String, String>,
    pub task_versions: BTreeMap<String, i64>,
}

impl FrozenView {
    fn new(
        initial_messages: BTreeMap<String, Message>,
        task_versions: BTreeMap<String, i64>,
        records: &[Value],
    ) -> Self {
        let mut messages = messages_from_records(records);
        for (id, message) in &initial_messages {
            messages.insert(id.clone(), message.clone());
        }
        let fingerprints = messages
            .iter()
            .map(|(id, message)| (id.clone(), fingerprint(message)))
            .collect();
        Self {
            initial_messages,
            messages,
            fingerprints,
            task_versions,
        }
    }
}

fn messages_from_records(records: &[Value]) -> BTreeMap<String, Message> {
    records
        .iter()
        .filter(|record| record["kind"] == "message")
        .filter_map(|record| {
            let mut data = record["data"].clone();
            if let Some(object) = data.as_object_mut() {
                // Snapshot projection decorates the persisted message with
                // display-only truncation metadata.
                object.remove("truncated");
                object.remove("original_chars");
            }
            serde_json::from_value::<Message>(data)
                .ok()
                .map(|message| (message.id.clone(), message))
        })
        .collect()
}

impl App {
    pub fn load(root: PathBuf) -> Result<Self, String> {
        crate::storage::initialize(&root)?;
        crate::storage::plain_directory(&crate::storage::data_dir(&root))?;
        fs::create_dir_all(crate::storage::data_dir(&root).join("logs"))
            .map_err(|e| format!("安装目录不可写，请选择普通用户可写目录：{e}"))?;
        fs::create_dir_all(crate::storage::data_dir(&root).join("tmp"))
            .map_err(|e| e.to_string())?;
        let mut state = crate::managed_data::load_state(&root)?;
        state.task_numbers = crate::task_history::numbers(&state);
        for stored in state
            .analyzed
            .values_mut()
            .chain(state.message_filter.checked.values_mut())
        {
            if let Some(canonical) = canonical_fingerprint(stored) {
                *stored = canonical;
            }
        }
        crate::message_progress::initialize(&mut state);
        crate::tags::initialize(&mut state);
        crate::handling::initialize(&mut state);
        crate::tool_calling::recover(&mut state);
        prompts::initialize(&mut state);
        crate::archive::initialize(&mut state);
        crate::archive::recover(&mut state, now());
        for job in &mut state.morning_jobs {
            if ["queued", "running"].contains(&job.status.as_str()) {
                job.status = "failed".into();
                job.error = "重启中断，请手动生成报表".into();
            }
        }
        crate::retention::prune(&mut state, now());
        crate::ai::prune_traces(&root, state.config.message_retention_days.max(1));
        crate::messages::prune(&mut state);
        state.message_filter.prune(
            &state.messages,
            &state.analyzed,
            now() - state.config.analysis_lookback_hours as i64 * 3_600_000,
        );
        crate::images::prune(&root, state.config.message_retention_days);
        let _ = fs::write(
            crate::storage::data_dir(&root).join("retention-pruned.txt"),
            format!("{}:{}", date(), state.config.message_retention_days.max(1)),
        );
        let daily_log = fs::read_to_string(
            crate::storage::data_dir(&root).join(format!("logs/{}.jsonl", date())),
        )
        .unwrap_or_default();
        // Upgrade only from a committed analysis-result event, never from the
        // scheduler's last attempt timestamp or an AI response not yet applied.
        if state.last_analysis_success_at == 0 {
            state.last_analysis_success_at = daily_log
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|v| v["kind"] == "analysis_result")
                .filter_map(|v| v["time"].as_i64())
                .max()
                .unwrap_or(0);
        }
        let activity = daily_log
            .lines()
            .rev()
            .take(60)
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let a = Self {
            tool_runtime: Default::default(),
            plugin_settings: Mutex::new(Default::default()),
            notifications: Mutex::new(Default::default()),
            store: Mutex::new(None),
            updates: crate::updates::Runtime::load(&root),
            plugin_access: std::sync::RwLock::new(()),
            desktop_bridge: Mutex::new(None),
            desktop_reading: Mutex::new(BTreeSet::new()),
            handling_previews: Mutex::new(BTreeMap::new()),
            handling_preview_revision: AtomicU64::new(0),
            handling_client: Mutex::new(BTreeMap::new()),
            handling_action: Mutex::new(()),
            handling_followups: Mutex::new(BTreeSet::new()),
            handling_decision: Mutex::new(()),
            handling_stop: Mutex::new(()),
            calendar: Mutex::new(crate::calendar_service::load_calendar(&root, now())),
            root,
            storage_changing: AtomicBool::new(false),
            state: Mutex::new(state),
            busy: AtomicBool::new(false),
            ai_busy: AtomicBool::new(false),
            active_ai: Mutex::new(None),
            analyzing_messages: Mutex::new(BTreeMap::new()),
            cache_prev: Mutex::new(BTreeMap::new()),
            summary_busy: AtomicBool::new(false),
            archive_busy: AtomicBool::new(false),
            prompt_busy: AtomicBool::new(false),
            activity: Mutex::new(activity),
            activity_revision: AtomicU64::new(0),
            log_writer: crate::log_writer::LogWriter::start()?,
            ai_progress: Mutex::new(String::new()),
            ai_live: Mutex::new(crate::ai_progress::LiveProgress::default()),
            analysis_last_attempt: std::sync::atomic::AtomicI64::new(0),
            learning_last_attempt: std::sync::atomic::AtomicI64::new(0),
            ai_started: std::sync::atomic::AtomicI64::new(0),
            last_attempt: std::sync::atomic::AtomicI64::new(0),
            saves: AtomicU64::new(0),
            snapshot_files: Mutex::new(()),
        };
        a.persist()?;
        Ok(a)
    }
    /// Submit a complete record; the consumer owns all log-file I/O.
    pub fn log(&self, kind: &str, detail: Value) {
        let record = json!({"time":now(),"kind":kind,"detail":detail});
        {
            let mut a = self.activity.lock().unwrap();
            a.insert(0, record.clone());
            a.truncate(60);
        }
        self.log_writer
            .submit(crate::storage::data_dir(&self.root).join("logs"), record);
        self.activity_revision.fetch_add(1, Ordering::SeqCst);
    }
    pub(crate) fn require_ai_purpose(&self, kind: &str) -> Result<(), String> {
        let config = self.state.lock().unwrap().config.for_kind(kind);
        self.require_ai_config(&config, kind)
    }
    fn require_ai_config(&self, config: &Config, kind: &str) -> Result<(), String> {
        if config.selected_connection != UNCONFIGURED_CONNECTION {
            return Ok(());
        }
        let purpose = match kind {
            "message_filter" => "消息过滤",
            "handling" => "AI一键处置",
            "tool_call" => "工具调用",
            "archive" | "learning" | "prompt_review" | "report" | "handling_learning"
            | "advice_revision" => "归档分析",
            _ => "事项更新",
        };
        let message = format!("{purpose}未配置 AI 接入或模型，本次任务未执行");
        self.log(
            "error",
            json!({"kind":kind,"message":message,"failure_class":"unconfigured"}),
        );
        Err(format!("AI_UNCONFIGURED:{message}"))
    }
    pub(crate) fn audit(
        s: &mut State,
        kind: &str,
        task: &str,
        detail: &str,
        before: Value,
        after: Value,
    ) {
        crate::task_history::reserve(s, task);
        s.audit.push(Audit {
            time: now(),
            kind: kind.into(),
            task_id: task.into(),
            detail: detail.into(),
            change_summary: String::new(),
            before,
            after,
        });
        s.revision += 1;
    }
    fn audit_ai_change(
        s: &mut State,
        action: &Value,
        before: Option<&Task>,
        after: &Task,
        summary_allowed: bool,
    ) {
        let kind = if before.is_some() {
            "ai_update"
        } else {
            "ai_create"
        };
        let before = json!(before);
        let after_value = json!(after);
        let summary = if summary_allowed {
            accepted_change_summary(action, &before, &after_value)
        } else {
            String::new()
        };
        Self::audit(s, kind, &after.id, &after.reason, before, after_value);
        // audit always appends one entry after reserving history capacity.
        s.audit.last_mut().unwrap().change_summary = summary;
    }
    pub(crate) fn ai_allowed(&self, mode: AiMode) -> bool {
        let s = self.state.lock().unwrap();
        self.work_gate(&s, "classify", mode, None, now())
    }
    pub(crate) fn reserve_ai(
        &self,
        mode: AiMode,
        input: Option<(&str, &Value)>,
    ) -> Result<Config, String> {
        self.reserve_ai_inner(mode, input, "handling", None, None, None)
    }
    pub(crate) fn reserve_ai_inner(
        &self,
        mode: AiMode,
        input: Option<(&str, &Value)>,
        kind: &str,
        attempt: Option<&str>,
        filter_decision_at: Option<i64>,
        observed_messages: Option<&BTreeMap<String, Message>>,
    ) -> Result<Config, String> {
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        if mode == AiMode::Automatic
            && ((kind == "classify" && !s.config.automatic_analysis_enabled())
                || (kind == "message_filter" && !s.config.early_analysis_enabled()))
        {
            return Err(crate::message_filter::AUTOMATION_DISABLED.into());
        }
        if !self.work_gate(&s, kind, mode, attempt, now()) {
            return Err(work_hours::DEFERRED.into());
        }
        if let Some((kind, payload)) = input {
            ai::validate_input(&s.config.for_kind(kind), kind, payload)?;
        }
        if s.usage_day != date() {
            s.usage_day = date();
            s.calls = 0;
            s.message_filter.calls = 0;
            s.message_filter.input_tokens = 0;
            s.message_filter.output_tokens = 0;
            s.message_filter.unreported_calls = 0;
            s.input_tokens = 0;
            s.cached_input_tokens = 0;
            s.cached_reports = 0;
            s.unreported_calls = 0;
            s.output_tokens = 0
        }
        if kind != "message_filter" && s.calls >= s.config.daily_calls {
            return Err("已达到每日AI调用上限；消息仍保留，可手动处理或调整上限".into());
        }
        if kind == "message_filter" {
            s.message_filter.calls += 1;
            s.message_filter.unreported_calls += 1;
        } else {
            s.calls += 1;
            s.unreported_calls += 1;
        }
        if kind == "classify" {
            s.message_filter.consume(filter_decision_at);
            if let Some(messages) = observed_messages {
                s.message_filter.observe_analysis(messages, &s.messages);
                let cutoff = now() - s.config.analysis_lookback_hours as i64 * 3_600_000;
                s.message_filter.prune(&s.messages, &s.analyzed, cutoff);
                crate::message_progress::advance_filter(&mut s);
            }
        }
        self.save(&s)?;
        let config = s.config.clone();
        *locked = s;
        Ok(config)
    }
    fn save_temporary_memory(&self, update: &Value) -> Result<(), String> {
        let before = update["before"].as_str().ok_or("笔记原版本无效")?;
        let text = update["text"].as_str().ok_or("笔记内容无效")?;
        if text.chars().count() > crate::analysis_context::MEMORY_LIMIT {
            return Err("笔记超过2000字符，未保存".into());
        }
        let mut lock = self.state.lock().unwrap();
        if lock.analysis_memory.temporary != before
            || update["expected"]
                .as_u64()
                .is_some_and(|n| n != lock.analysis_memory.temporary_version)
        {
            return Err("临时记忆已被修改，本次不覆盖".into());
        }
        let mut s = lock.clone();
        s.analysis_memory.temporary = text.into();
        s.analysis_memory.temporary_version += 1;
        s.analysis_memory.temporary_updated = now();
        self.save(&s)?;
        *lock = s;
        Ok(())
    }

    pub fn ask_ai(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        self.ask_ai_mode(kind, payload, AiMode::Manual)
    }
    pub(crate) fn ask_ai_mode(
        &self,
        kind: &str,
        payload: &Value,
        mode: AiMode,
    ) -> Result<Value, String> {
        if mode == AiMode::Automatic {
            let s = self.state.lock().unwrap();
            if (kind == "classify" && !s.config.automatic_analysis_enabled())
                || (kind == "message_filter" && !s.config.early_analysis_enabled())
            {
                return Err(crate::message_filter::AUTOMATION_DISABLED.into());
            }
        }
        process::check_running()?;
        self.require_ai_purpose(kind)?;
        let call = self.begin_ai_for(kind)?;
        let context_frozen = payload["_context_frozen"].as_bool() == Some(true);
        let mut context =
            self.capture_prepare_context(kind, context_frozen, payload["_tool_task_id"].as_str())?;
        let request = self.prepare_ai_request_with(kind, payload, mode, &mut context)?;
        self.invoke_prepared_request(&request, mode, &call, true)
    }

    /// Capture every input a request preparation may need under one state lock.
    pub(crate) fn prepare_archive_request(
        &self,
        payload: &Value,
        mode: AiMode,
    ) -> Result<PreparedAiRequest, String> {
        let mut context = self.capture_prepare_context("archive", true, None)?;
        self.prepare_ai_request_with("archive", payload, mode, &mut context)
    }

    /// Capture every input a request preparation may need under one state lock.
    /// Snapshot rows are materialised here once, not once per candidate.
    fn capture_prepare_context(
        &self,
        kind: &str,
        context_frozen: bool,
        task_id: Option<&str>,
    ) -> Result<PrepareContext, String> {
        let projects = if ["classify", "format", "edit"].contains(&kind) {
            let projects = self.analysis_projects();
            json!(projects
                .as_array()
                .into_iter()
                .flatten()
                .take(60)
                .map(|p| json!({"id":p["id"],"name":p["name"],"cwd":p["cwd"],"driver":p["driver"],"source":p["source"]}))
                .collect::<Vec<_>>())
        } else {
            json!([])
        };
        let _files = self.snapshot_files.lock().unwrap();
        let state = self.state.lock().unwrap().clone();
        if let Some(task_id) = task_id {
            if !state.tasks.iter().any(|task| task.id == task_id) {
                return Err("事项不存在".into());
            }
        }
        self.capture_prepare_context_from(&state, kind, context_frozen, projects)
    }

    fn capture_prepare_context_from(
        &self,
        state: &State,
        kind: &str,
        context_frozen: bool,
        projects: Value,
    ) -> Result<PrepareContext, String> {
        let records = if !context_frozen
            && ["classify", "prompt_review", "report", "learning", "archive"].contains(&kind)
        {
            // The read-only rows are served on demand and never embedded in the
            // prompt. A failed rebuild aborts before any call; the verified set
            // is reused by every reduction of this request.
            crate::snapshots::ensure(&self.root, state, now())?.records
        } else {
            Vec::new()
        };
        // One clock and one work snapshot for the whole request. The budget,
        // cache diagnostics and the final runtime suffix all read these values,
        // so a boundary crossing during preparation cannot split the request.
        let runtime_at = now();
        let work = self.work_snapshot(state, runtime_at).ok();
        let work_status = work
            .as_ref()
            .and_then(|snapshot| serde_json::to_value(snapshot.status).ok())
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "calendar_unknown".into());
        let business_date = work
            .as_ref()
            .map(|snapshot| snapshot.business_date.to_string())
            .unwrap_or_else(|| "未提供".into());
        Ok(PrepareContext {
            decision_context: json!({
                "prompt_version": prompts::current(state, kind).map(|v| v.id.clone()),
                "rule_version": state.rule_version,
                "rule_ids": state.suggestions.iter().filter(|r| r.active && r.archived_at == 0).map(|r|r.id.clone()).collect::<Vec<_>>(),
                "archived_memory_version": state.analysis_memory.archived_version,
                "temporary_memory_version": state.analysis_memory.temporary_version,
            }),
            config: state.config.for_kind(kind),
            catalog: json!(crate::tags::groups(state)),
            priority_kind: crate::tags::priority_kind(state),
            projects,
            memory_archived: state.analysis_memory.archived_text(),
            memory_temporary: state.analysis_memory.temporary.clone(),
            last_scan: state.last_scan,
            runtime_at,
            work_status,
            business_date,
            records,
            runtime: None,
        })
    }

    /// Build the exact request object used for measurement and invocation.
    /// The runtime clock is captured on the first candidate and reused by every
    /// later reduction. No live state is read here.
    fn prepare_ai_request_with(
        &self,
        kind: &str,
        payload: &Value,
        mode: AiMode,
        context: &mut PrepareContext,
    ) -> Result<PreparedAiRequest, String> {
        let mut enriched = crate::context_budget::prepare(kind, payload);
        if ["classify", "format", "edit"].contains(&kind) {
            enriched["available_projects"] = context.projects.clone();
        }
        if [
            "classify", "format", "edit", "learning", "schedule", "archive",
        ]
        .contains(&kind)
        {
            enriched["tag_catalog"] = context.catalog.clone();
            enriched["priority_kind"] = json!(context.priority_kind);
        }
        let context_frozen = enriched["_context_frozen"].as_bool() == Some(true);
        if !context_frozen
            && ["classify", "prompt_review", "report", "learning", "archive"].contains(&kind)
        {
            crate::analysis_context::attach_memory_text(
                &mut enriched,
                &context.memory_archived,
                &context.memory_temporary,
            );
            enriched["_local_context"]["memory"]["revision"] =
                context.decision_context["temporary_memory_version"].clone();
            crate::analysis_context::attach_records(
                &mut enriched,
                context.records.clone(),
                context.config.retrieval_budget_chars,
            )?;
        }
        if kind != "classify" && enriched["_local_context"]["memory"].is_object() {
            enriched["_local_context"]["memory"]["writable"] = json!(false);
            enriched["memory"]["writable"] = json!(false);
        }
        if kind != "handling_learning" {
            crate::images::attach(&self.root, &mut enriched);
        }
        if let Some(rules) = enriched.as_object_mut().unwrap().remove("rules") {
            enriched["_enabled_rules"] = rules;
        }
        if !context_frozen || enriched["_data_until"].as_i64().is_none() {
            enriched["_data_until"] = json!(context.last_scan);
        }
        let trigger = enriched["_trigger"]
            .as_str()
            .unwrap_or(if mode == AiMode::Manual {
                "manual"
            } else {
                "scheduled"
            })
            .to_string();
        let runtime = if let Some(runtime) = context.runtime.clone() {
            runtime
        } else if let Some(runtime) = enriched
            .get("_runtime_context")
            .filter(|value| value.is_object())
            .cloned()
        {
            context.runtime = Some(runtime.clone());
            runtime
        } else {
            let mut runtime = ai::runtime_context(
                &context.config,
                kind,
                &trigger,
                &enriched,
                context.runtime_at,
            );
            runtime["work_state"] = json!(context.work_status);
            runtime["business_date"] = json!(context.business_date);
            context.runtime = Some(runtime.clone());
            runtime
        };
        enriched["_runtime_context"] = runtime.clone();
        // Tag evidence only needs these small sections; do not deep-clone the
        // full snapshot just to keep original ids for tag collection.
        let tag_payload = json!({
            "changes": enriched["changes"].clone(),
            "context": enriched["context"].clone(),
            "audit": enriched["audit"].clone(),
            "user_description": enriched["user_description"].clone(),
            "user_instruction": enriched["user_instruction"].clone()
        });
        let compression_before = ai::public_payload(&enriched).to_string().chars().count();
        // The frozen view keeps original ids and bodies before wire aliases are
        // applied. Post-call validation uses this copy, never the shared files.
        let records = enriched["_local_context"]["records"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let initial_tasks = enriched["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|task| task["id"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        // Wire compression is deterministic and id mappings are reversible. The
        // mapping is built from the complete private record set, so hidden ids
        // participate in alias allocation exactly as they will at invocation.
        let wire_ids = if ["classify", "format"].contains(&kind) {
            let ids = crate::wire_context::IdMap::from_payload(&enriched);
            ids.encode(&mut enriched);
            crate::wire_context::compact(&mut enriched);
            enriched["wire_format"] = json!(crate::prompt_config::get("wire_format"));
            ids
        } else {
            crate::wire_context::IdMap::default()
        };
        if ["classify", "format", "edit", "archive"].contains(&kind) {
            // Registry IDs and templates are local authority, not evidence aliases.
            enriched["_registered_tools"] = json!(context
                .config
                .tools
                .iter()
                .filter(|tool| tool.enabled && tool.compatibility_issue().is_none())
                .collect::<Vec<_>>());
        }
        let prepared = ai::prepare(&context.config, kind, &enriched);
        crate::analysis_context::acknowledge_inline_memory(&mut enriched, kind, &prepared.prompt);
        let cache_components = ai::prompt_components(&context.config, kind, &enriched, &prepared);
        Ok(PreparedAiRequest {
            decision_context: context.decision_context.clone(),
            kind: kind.into(),
            config: context.config.clone(),
            runtime,
            payload: enriched,
            prepared,
            wire_ids,
            tag_payload,
            catalog: context.catalog.clone(),
            trigger,
            compression_before,
            records,
            initial_tasks,
            cache_components,
            frozen: None,
        })
    }

    /// Invoke an already-prepared object. Analyze uses this after measuring the
    /// accepted candidate; all other purposes reach it through `ask_ai_mode`.
    fn invoke_ai_request(
        &self,
        request: &PreparedAiRequest,
        mode: AiMode,
    ) -> Result<Value, String> {
        process::check_running()?;
        let call = self.begin_ai_for(&request.kind)?;
        self.invoke_prepared_request(request, mode, &call, true)
    }

    pub(crate) fn invoke_prepared_request(
        &self,
        request: &PreparedAiRequest,
        mode: AiMode,
        call: &crate::ai_control::AiCall<'_>,
        finish_call: bool,
    ) -> Result<Value, String> {
        if call.cancel.load(Ordering::SeqCst) {
            return Err(crate::ai_control::CANCELLED.into());
        }
        let kind = request.kind.as_str();
        self.require_ai_config(&request.config, &request.kind)?;
        let payload = &request.payload;
        let prepared = &request.prepared;
        let preview_config = &request.config;
        if let Err(error) = ai::validate_prepared(preview_config, kind, payload, prepared) {
            // This is the single final-request budget check: it includes the
            // fully enriched payload, memory, tool schema and encoded wire
            // format. Automatic analysis must pause here too, not only in the
            // earlier message-budget estimate.
            if mode == AiMode::Automatic && kind == "classify" {
                let mut lock = self.state.lock().unwrap();
                let mut s = lock.clone();
                s.auto_analysis_blocked = true;
                s.auto_analysis_blocked_at = now();
                s.auto_analysis_block_reason = error.clone();
                s.pending_analysis = true;
                if self.save(&s).is_ok() {
                    *lock = s;
                }
            }
            self.log(
                "ai_rejected",
                json!({"request_id":call.id,"kind":kind,"source":crate::ai_backend::source_info(&request.config),"error":error}),
            );
            return Err(error);
        }
        // The captured config is part of the accepted object; reservation only
        // accounts for the call and rechecks time/day limits. A later settings
        // edit cannot silently change this already-rendered request.
        let config = request.config.clone();
        if let Err(error) = self.reserve_ai_inner(
            mode,
            None,
            kind,
            payload["_archive_attempt_id"].as_str(),
            payload["_filter_decision_at"].as_i64(),
            request.frozen.as_ref().map(|f| &f.initial_messages),
        ) {
            if error.starts_with(ai::INPUT_LIMIT_ERROR) {
                self.log(
                    "ai_rejected",
                    json!({"request_id":call.id,"kind":kind,"source":crate::ai_backend::source_info(&request.config),"error":error}),
                );
            }
            return Err(error);
        }
        let trigger = request.trigger.clone();
        let prompt_for_cache = &request.prepared.prompt;
        let checkpoints = ai::prompt_checkpoints(prompt_for_cache);
        let cache_key = format!("{kind}|{}|{}", config.model, config.selected_connection);
        let previous = self
            .cache_prev
            .lock()
            .unwrap()
            .insert(cache_key, checkpoints.clone());
        let stable_prefix = ai::stable_prefix(previous.as_ref(), &checkpoints);
        let cache_components = request.cache_components.clone();
        let cache_diag = json!({
            "stable_prefix_chars":stable_prefix,
            "stable_prefix_scope":"actual_prompt_prefix_across_requests",
            "prompt_chars":prepared.prompt_chars,
            "runtime_chars":cache_components["runtime_chars"].clone(),
            "components":cache_components.clone()
        });
        let start = now();
        self.ai_started.store(start, Ordering::SeqCst);
        let request_id = call.id.clone();
        let source = crate::ai_backend::source_info(&config);
        if let Some(active) = self
            .active_ai
            .lock()
            .unwrap()
            .as_mut()
            .filter(|a| a.id == request_id)
        {
            active.source = source.clone();
        }
        *self.ai_progress.lock().unwrap() = String::new();
        self.ai_live.lock().unwrap().start(&request_id);
        self.log("ai_start",json!({"request_id":request_id,"kind":kind,"source":source,"trigger":trigger,"model":config.model,"reasoning_effort":config.reasoning_effort,"messages":payload["changes"].as_array().map(|a|a.len()).unwrap_or(0),"batch":payload["_batch_metrics"],"runtime":request.runtime,"input_chars":ai::public_payload(payload).to_string().chars().count(),"input_sizes":json!({"prompt_chars":prepared.prompt_chars,"schema_chars":prepared.schema_chars,"tool_schema_chars":prepared.tool_chars,"wire_overhead_chars":prepared.wire_chars,"total_input_chars":prepared.total,"max_input_chars":config.max_input_chars}),"cache_diag":cache_diag}));
        let mut observed = Value::Null;
        let mut cache_diagnostics = Value::Null;
        let mut analysis_thread = String::new();
        let tool_session = if let Some(tools) = payload.get("_registered_tools") {
            let tools: Vec<crate::tool_registry::ToolDefinition> =
                serde_json::from_value(tools.clone()).map_err(|e| e.to_string())?;
            (!tools.is_empty()).then(|| {
                crate::tool_calling::ToolSession::new(
                    self,
                    &request_id,
                    kind,
                    payload["_tool_task_id"].as_str().unwrap_or(""),
                    tools,
                    &config,
                    prepared.total,
                )
            })
        } else {
            None
        };
        let mut res = ai::invoke_prepared(
            request,
            &self.root,
            &request_id,
            &call.cancel,
            tool_session.as_ref(),
            &mut |event| {
                use crate::ai_backend::AnalysisEvent;
                match event {
                    AnalysisEvent::SessionStarted(thread) => analysis_thread = thread,
                    AnalysisEvent::Usage(usage) => {
                        observed = usage;
                    }
                    AnalysisEvent::OutputTokens(count) => {
                        self.ai_live.lock().unwrap().record_output_tokens(
                            &request_id,
                            count,
                            now().saturating_sub(start).max(0) as u64,
                        );
                    }
                    AnalysisEvent::StepFinished {
                        output_tokens,
                        decode_ms,
                    } => {
                        self.ai_live.lock().unwrap().record_step(
                            &request_id,
                            output_tokens,
                            decode_ms,
                        );
                    }
                    AnalysisEvent::CacheDiagnostics(value) => cache_diagnostics = value,
                    AnalysisEvent::Stream { kind, text, append } => {
                        if self
                            .active_ai
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_some_and(|a| a.id == request_id)
                        {
                            if matches!(kind.as_str(), "reasoning" | "content") && !text.is_empty()
                            {
                                self.ai_live.lock().unwrap().record_content(
                                    &request_id,
                                    now().saturating_sub(start).max(0) as u64,
                                );
                            }
                            self.ai_live.lock().unwrap().accept(&kind, &text, append);
                            *self.ai_progress.lock().unwrap() = if kind == "phase" {
                                text.clone()
                            } else {
                                self.ai_live.lock().unwrap().phase.clone()
                            };
                            if config.retain_ai_traces && kind == "phase" {
                                ai::append_trace_event(
                                    &crate::storage::data_dir(&self.root)
                                        .join("ai-traces")
                                        .join(&request_id),
                                    &json!({"type":"phase","text":text}),
                                );
                            }
                        }
                    }
                    AnalysisEvent::VisibilityIssue { thread_id, error } => {
                        // Keep the existing persisted log kind for old UI/history readers.
                        self.log(
                            "codex_visibility",
                            json!({"type":"codex.visibility","thread_id":thread_id,"error":error}),
                        );
                    }
                }
            },
        );
        self.ai_live.lock().unwrap().accept(
            "phase",
            if res.is_ok() {
                "校验结果"
            } else {
                "请求失败"
            },
            false,
        );
        let response_received = res.is_ok();
        if let Err(error) = process::check_running() {
            res = Err(error);
        }
        let usage = if observed.is_object() {
            observed
        } else {
            res.as_ref().map(|(_, u)| u.clone()).unwrap_or(Value::Null)
        };
        let known =
            usage["input_tokens"].as_u64().is_some() && usage["output_tokens"].as_u64().is_some();
        let (cache_status, cached_tokens) = ai::cache_state(&usage);
        if known {
            let saved = self.commit_state(|s| {
                if kind == "message_filter" {
                    s.message_filter.input_tokens += usage["input_tokens"].as_u64().unwrap();
                    s.message_filter.output_tokens += usage["output_tokens"].as_u64().unwrap();
                    s.message_filter.unreported_calls = s
                        .message_filter
                        .unreported_calls
                        .saturating_sub(if usage["partial"] == true { 0 } else { 1 });
                } else {
                    s.input_tokens += usage["input_tokens"].as_u64().unwrap();
                    s.output_tokens += usage["output_tokens"].as_u64().unwrap();
                    if let Some(cache) = cached_tokens {
                        s.cached_input_tokens += cache;
                        s.cached_reports += 1;
                    }
                    s.unreported_calls = s
                        .unreported_calls
                        .saturating_sub(if usage["partial"] == true { 0 } else { 1 });
                }
                Ok(())
            });
            if let Err(error) = saved {
                res = Err(format!("AI usage persistence failed: {error}"));
            }
        }
        if (finish_call && call.finish()) || call.cancel.load(Ordering::SeqCst) {
            res = Err(crate::ai_control::CANCELLED.into());
        }
        let elapsed_ms = now().saturating_sub(start).max(0) as u64;
        let (first_response_ms, generation) = {
            let mut live = self.ai_live.lock().unwrap();
            live.record_usage(&request_id, &usage, elapsed_ms);
            (live.first_response_ms, live.generation)
        };
        let mut detail = json!({"request_id":request_id,"kind":kind,"source":source,"trigger":trigger,"model":config.model,"reasoning_effort":config.reasoning_effort,"elapsed_ms":elapsed_ms,"first_response_ms":first_response_ms,"generation":generation,"usage":usage,"usage_known":known,"batch":payload["_batch_metrics"]});
        detail["cache_diag"] = json!({"stable_prefix_chars":stable_prefix,"prompt_chars":prompt_for_cache.chars().count(),"components":request.cache_components.clone(),"cache":cache_status,"cached_tokens":cached_tokens});
        detail["thread_id"] = json!(analysis_thread);
        detail["usage_status"] = json!(if known && usage["partial"] == true {
            "reported_partial"
        } else if known {
            "reported"
        } else if res.is_err() {
            "request_failed_without_complete_usage"
        } else {
            "provider_did_not_report_complete_usage"
        });
        if cache_diagnostics.is_object() {
            cache_diagnostics["usage"] = crate::cache_diagnostics::usage(&usage);
            cache_diagnostics["thread_id"] = json!(analysis_thread);
            detail["cache_diagnostics"] = cache_diagnostics.clone();
            let _ = std::fs::write(
                crate::storage::data_dir(&self.root)
                    .join("ai-traces")
                    .join(&request_id)
                    .join("cache-diagnostics.json"),
                cache_diagnostics.to_string(),
            );
        }
        detail["wire_chars"] = json!({"before":request.compression_before,"after":ai::public_payload(payload).to_string().chars().count()});
        detail["response_received"] = json!(response_received);
        if tool_session
            .as_ref()
            .is_some_and(|session| session.budget_exhausted.load(Ordering::SeqCst))
        {
            res = Err(format!(
                "{}：工具续轮超过全局输入预算，原事项保留",
                ai::INPUT_LIMIT_ERROR
            ));
        }
        let result =
            self.finalize_ai_response(request, &request_id, detail, res.map(|(value, _)| value));
        if let Some(session) = tool_session {
            session.finish(result.as_ref().err().map(String::as_str), &usage)?;
        }
        result
    }

    fn finalize_ai_response(
        &self,
        request: &PreparedAiRequest,
        request_id: &str,
        mut detail: Value,
        res: Result<Value, String>,
    ) -> Result<Value, String> {
        let kind = request.kind.as_str();
        let config = &request.config;
        let payload = &request.payload;
        let mut failure_stage = "response_validation";
        let finalized = res.and_then(|mut v| {
            request.wire_ids.humanize_prose(&mut v, &request.records);
            request.wire_ids.decode(&mut v)?;
            detail["context_usage"] = v["_context_usage"].clone();
            if let Some(notes) = v.as_object_mut().and_then(|o| o.remove("_image_notes")) {
                if let Err(e) = crate::images::commit(&self.root, &notes) {
                    self.log("image_notes_skipped", json!({"error":e}));
                }
            }
            if let Some(error) = v.as_object_mut().and_then(|o| o.remove("_memory_error")) {
                self.log(
                    "memory_skipped",
                    json!({"request_id":request_id,"error":error}),
                );
            }
            if let Some(update) = v.as_object_mut().and_then(|o| o.remove("_memory_update")) {
                if kind != "classify" {
                    // Only message analysis owns temporary memory.
                    self.log(
                        "memory_skipped",
                        json!({"request_id":request_id,"error":"此用途不可修改临时记忆"}),
                    );
                } else {
                    match self.save_temporary_memory(&update) {
                        Ok(()) => detail["memory_updated"] = json!(true),
                        Err(error) => self.log(
                            "memory_skipped",
                            json!({"request_id":request_id,"error":error}),
                        ),
                    }
                }
            }
            failure_stage = "result_persistence";
            if [
                "classify", "format", "edit", "learning", "schedule", "archive",
            ]
            .contains(&kind)
            {
                let mut lock = self.state.lock().unwrap();
                let mut s = lock.clone();
                crate::tags::collect(&mut s, &v, &request.tag_payload, &request_id);
                self.save(&s)?;
                *lock = s;
                v["_tag_catalog"] = request.catalog.clone();
            }
            if ["classify", "format", "edit"].contains(&kind) {
                let valid = |row: &mut Value| {
                    let id = row["suggested_project_id"].as_str().unwrap_or("");
                    let exists = !id.is_empty()
                        && row["suggested_project_reason"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty())
                        && payload["available_projects"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|p| p["id"] == id);
                    if !exists {
                        row["suggested_project_id"] = json!("");
                        row["suggested_project_reason"] = json!("");
                    }
                };
                if kind == "edit" {
                    valid(&mut v);
                } else if let Some(rows) = v[if kind == "classify" {
                    "actions"
                } else {
                    "tasks"
                }]
                .as_array_mut()
                {
                    for row in rows {
                        valid(row);
                    }
                }
            }
            if let Some(object) = v.as_object_mut() {
                object.remove("_tool_run_id");
            }
            if self
                .state
                .lock()
                .unwrap()
                .tool_runs
                .iter()
                .any(|run| run.id == request_id && !run.calls.is_empty())
            {
                v["_tool_run_id"] = json!(request_id);
            }
            Ok(v)
        });
        match finalized {
            Ok(v) => {
                self.ai_live
                    .lock()
                    .unwrap()
                    .accept("phase", "分析完成", false);
                self.log("ai_success", detail);
                Ok(v)
            }
            Err(e) => {
                self.ai_live.lock().unwrap().accept(
                    "phase",
                    if e == crate::ai_control::CANCELLED {
                        "已中断"
                    } else if crate::ai_control::failure_class(&e) == "provider_refusal" {
                        "供应商拒答"
                    } else if detail["response_received"] == true {
                        "结果处理失败"
                    } else {
                        "请求失败"
                    },
                    false,
                );
                ai::record_error(&self.root, &request_id, &e, config.retain_ai_traces);
                detail["failure_stage"] = json!(if detail["response_received"] == true {
                    failure_stage
                } else {
                    "provider"
                });
                detail["error"] = json!(e);
                detail["failure_class"] = json!(crate::ai_control::failure_class(&e));
                self.log(
                    if e == crate::ai_control::CANCELLED {
                        "ai_cancelled"
                    } else {
                        "ai_failed"
                    },
                    detail,
                );
                Err(e)
            }
        }
    }
    pub fn create_generation(&self, v: &Value) -> Result<Task, String> {
        self.require_ai_purpose("format")?;
        self.commit_state(|state| {
            if let Some(tid) = v["id"].as_str() {
                let t = state
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == tid)
                    .ok_or("事项不存在")?;
                if !["failed", "interrupted"].contains(&t.generation.as_str()) {
                    return Err("该事项不需要重试".into());
                }
                t.generation = "pending".into();
                t.generation_error.clear();
                t.updated = now();
                let t = t.clone();
                return Ok(t);
            }
            let text = v["text"].as_str().unwrap_or("").trim();
            if text.is_empty() || text.chars().count() > 6000 {
                return Err("请输入1–6000字的事项描述".into());
            }
            let t = Task {
                id: id(),
                title: text.chars().take(32).collect(),
                description: text.into(),

                status: "待处理".into(),
                generation: "pending".into(),
                manual: true,
                created: now(),
                updated: now(),
                evidence: TaskEvidence {
                    direct: v["sources"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .filter(|id| {
                                    state
                                        .messages
                                        .get(*id)
                                        .map(|m| !m.withdrawn)
                                        .unwrap_or(false)
                                })
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    ..Default::default()
                },
                ..Default::default()
            };
            state.tasks.insert(0, t.clone());
            Self::audit(
                state,
                "generation_start",
                &t.id,
                "自然语言新建，后台整理",
                Value::Null,
                json!(t),
            );
            Ok(t)
        })
    }
    pub fn generate_background(&self, draft: Task, mode: AiMode) {
        let started = now();
        let result = loop {
            if process::stopping() {
                return;
            }
            if !self.ai_allowed(mode) {
                return;
            }
            if now() - started > 600_000 {
                break Err("等待其他AI请求超时，请重试".into());
            }
            if !self.state.lock().unwrap().tasks.iter().any(|t| {
                t.id == draft.id && t.updated == draft.updated && t.generation == "pending"
            }) {
                return;
            }
            if self.ai_busy.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(300));
                continue;
            }
            let result = self.ask_ai_mode(
                "format",
                &json!({"user_description":draft.description}),
                mode,
            );
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e == work_hours::DEFERRED)
            {
                return;
            }
            if result
                .as_ref()
                .err()
                .map(|e| e.contains("正在处理其他请求"))
                .unwrap_or(false)
            {
                continue;
            }
            break result;
        };
        if let Err(e) = self.complete_generation(&draft, result) {
            self.error(&e)
        }
    }
    pub fn complete_generation(
        &self,
        draft: &Task,
        result: Result<Value, String>,
    ) -> Result<(), String> {
        if process::stopping() {
            return Ok(());
        }
        let mut locked = self.state.lock().unwrap();
        let Some(pos) = locked.tasks.iter().position(|t| {
            t.id == draft.id && t.updated == draft.updated && t.generation == "pending"
        }) else {
            return Ok(());
        };
        let mut candidate = locked.clone();
        let state = &mut candidate;
        let tool_result =
            json!({"_tool_run_id":result.as_ref().ok().and_then(|value|value.get("_tool_run_id"))});
        let parsed = result.and_then(|value| {
            let catalog = crate::tags::result_catalog(state, &value);
            let rows = value["tasks"].as_array().ok_or("AI结果缺少事项列表")?;
            if rows.is_empty() || rows.len() > 12 {
                return Err("生成事项数量应在1–12条之间".into());
            }
            let mut tasks = vec![];
            let mut tool_links = vec![];
            for row in rows {
                // `steps_changed` is an update-action signal, not a Task field.
                let mut row = row.clone();
                let call_ids = row
                    .as_object_mut()
                    .and_then(|object| object.remove("tool_call_ids"));
                if let Some(object) = row.as_object_mut() {
                    object.remove("steps_changed");
                    object.remove("steps");
                }
                let mut t: Task = serde_json::from_value(row).map_err(|e| e.to_string())?;
                if t.title.trim().is_empty() {
                    return Err("AI生成了空标题，请重试".into());
                }
                normalize_task(&mut t);
                if !t.due.is_empty() && deadline_ms(&t.due).is_none() {
                    t.due.clear()
                }
                t.id = if tasks.is_empty() {
                    draft.id.clone()
                } else {
                    id()
                };
                t.created = draft.created;
                t.updated = now();
                t.manual = true;
                t.status = "待处理".into();
                t.evidence = draft.evidence.clone();
                t.reason = "根据你的描述整理".into();
                let updated = t.updated;
                mark_content_changed(&mut t, updated);
                normalize_task(&mut t);
                crate::tags::sanitize(&mut t, None, &catalog);
                let mut link = tool_result.clone();
                if let Some(ids) = call_ids {
                    link["tool_call_ids"] = ids;
                } else if rows.len() > 1 {
                    link["tool_call_ids"] = json!([]);
                }
                tool_links.push((t.id.clone(), link));
                tasks.push(t)
            }
            Ok((tasks, tool_links))
        });
        let count = match parsed {
            Ok((tasks, tool_links)) => {
                let count = tasks.len();
                for (task_id, link) in tool_links {
                    crate::tool_calling::link_tasks(state, &link, &[task_id]);
                }
                state.tasks.remove(pos);
                for t in tasks.into_iter().rev() {
                    Self::audit(
                        state,
                        "generation_done",
                        &t.id,
                        "自然语言事项已生成",
                        json!(draft),
                        json!(t),
                    );
                    state.tasks.insert(pos, t)
                }
                Some(count)
            }
            Err(e) => {
                state.tasks[pos].generation = if e == crate::ai_control::CANCELLED {
                    "interrupted"
                } else {
                    "failed"
                }
                .into();
                state.tasks[pos].generation_error = e.clone();
                state.tasks[pos].updated = now();
                Self::audit(
                    state,
                    if e == crate::ai_control::CANCELLED {
                        "generation_interrupted"
                    } else {
                        "generation_failed"
                    },
                    &draft.id,
                    &e,
                    json!(draft),
                    Value::Null,
                );
                None
            }
        };
        self.save(&candidate)?;
        *locked = candidate;
        drop(locked);
        if let Some(count) = count {
            self.log("generation_result", json!({"count":count}));
        }
        Ok(())
    }
    pub fn format(&self, text: &str) -> Result<Value, String> {
        if text.trim().is_empty() || text.chars().count() > 6000 {
            return Err("请输入1–6000字的事项描述".into());
        }
        self.ask_ai("format", &json!({"user_description":text}))
    }
    pub(crate) fn analyze_after_scan(&self, mode: AiMode) -> Result<Value, String> {
        if mode == AiMode::Automatic {
            if !self
                .state
                .lock()
                .unwrap()
                .config
                .automatic_analysis_enabled()
            {
                return Ok(json!({"skipped":"自动 AI 已关闭，消息已保存"}));
            }
            if !self.ai_allowed(mode) {
                return Ok(json!({"skipped":self.ai_deferred_reason()}));
            }
            if self.ai_busy.load(Ordering::SeqCst) {
                return Ok(json!({"skipped":"AI 正在运行，新消息已排队"}));
            }
            if self.state.lock().unwrap().auto_analysis_blocked {
                return Ok(
                    json!({"skipped":"自动分析已暂停：本批仍超过输入上限，请手动精简后重试"}),
                );
            }
            let due = {
                let mut state_lock = self.state.lock().unwrap();
                let mut s = state_lock.clone();
                let pending = crate::context_budget::pending(&s);
                if s.config.for_kind("classify").selected_connection == UNCONFIGURED_CONNECTION
                    && now() - self.analysis_last_attempt.load(Ordering::SeqCst)
                        < s.config.analysis_interval_minutes as i64 * 60_000
                {
                    return Ok(json!({"skipped":"等待分析间隔"}));
                }
                crate::context_budget::mark_pending(&mut s, !pending.is_empty(), now());
                self.save(&s)?;
                let due = crate::context_budget::automatic_due(
                    &s,
                    &pending,
                    now(),
                    self.analysis_last_attempt.load(Ordering::SeqCst),
                );
                *state_lock = s;
                due
            };
            if !due {
                return Ok(json!({"skipped":"等待分析间隔或单批体积阈值"}));
            }
        }
        self.analyze(mode)
    }
    pub fn analyze(&self, mode: AiMode) -> Result<Value, String> {
        if mode == AiMode::Automatic
            && !self
                .state
                .lock()
                .unwrap()
                .config
                .automatic_analysis_enabled()
        {
            return Ok(json!({"skipped":"自动 AI 已关闭，消息已保存"}));
        }
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        if self.ai_busy.load(Ordering::SeqCst) {
            return Err("AI 正在运行，请稍后重试".into());
        }
        let result = self
            .require_ai_purpose("classify")
            .and_then(|_| self.analyze_batch(mode));
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e == crate::message_filter::AUTOMATION_DISABLED)
        {
            return Ok(json!({"skipped":"自动 AI 已关闭，消息已保存"}));
        }
        let budget_failure = result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with(ai::INPUT_LIMIT_ERROR));
        self.analysis_last_attempt.store(now(), Ordering::SeqCst);
        if !budget_failure {
            let mut state_lock = self.state.lock().unwrap();
            let mut s = state_lock.clone();
            s.last_analysis_at = now();
            s.analysis_interrupted_at = if result
                .as_ref()
                .err()
                .is_some_and(|e| e == crate::ai_control::CANCELLED)
            {
                s.last_analysis_at
            } else {
                0
            };
            self.save(&s)?;
            *state_lock = s;
        } else if mode == AiMode::Automatic {
            // A continuation can overflow after commands have run. Pause the
            // automatic workflow just as for an oversized initial request.
            let mut state_lock = self.state.lock().unwrap();
            if !state_lock.auto_analysis_blocked {
                let mut s = state_lock.clone();
                s.auto_analysis_blocked = true;
                s.auto_analysis_blocked_at = now();
                s.auto_analysis_block_reason = result.as_ref().unwrap_err().clone();
                s.pending_analysis = true;
                self.save(&s)?;
                *state_lock = s;
            }
        }
        result
    }
    fn analyze_batch(&self, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        process::check_running()?;
        {
            let mut state_lock = self.state.lock().unwrap();
            let mut s = state_lock.clone();
            s.pending_analysis = true;
            self.save(&s)?;
            *state_lock = s;
        }
        self.analysis_last_attempt.store(now(), Ordering::SeqCst);
        // Capture every preparation input once. Candidate reductions reuse the
        // same config, project catalog, memory, frozen runtime and snapshot rows.
        let projects = self.analysis_projects();
        let projects = json!(projects
            .as_array()
            .into_iter()
            .flatten()
            .take(60)
            .map(|p| json!({"id":p["id"],"name":p["name"],"cwd":p["cwd"],"driver":p["driver"],"source":p["source"]}))
            .collect::<Vec<_>>());
        let (batch, request, _message_claim) = {
            let mut state_lock = self.state.lock().unwrap();
            let mut s = state_lock.clone();
            let mut context = self.capture_prepare_context_from(&s, "classify", false, projects)?;
            let mut pending = crate::context_budget::pending_for(&s, mode == AiMode::Manual);
            pending.sort_by_key(|m| (m.updated_at, m.time, m.id.clone()));
            let trigger = if mode == AiMode::Manual {
                "manual"
            } else if s.config.message_filter_enabled && s.message_filter.early {
                if s.message_filter.trigger == "message_filter_failed" {
                    "message_filter_failed"
                } else {
                    "message_filter"
                }
            } else {
                crate::context_budget::due_reason(&s, &pending, now())
                    .unwrap_or("analysis_interval")
            };
            let pending_count = pending.len();
            let budget = ai::message_budget(&s.config, "classify");
            if budget == 0 {
                let message = format!(
                    "{}：总输入上限不足以容纳规则、输出格式和上下文，请提高上限",
                    ai::INPUT_LIMIT_ERROR
                );
                if mode == AiMode::Automatic {
                    s.auto_analysis_blocked = true;
                    s.auto_analysis_blocked_at = now();
                    s.auto_analysis_block_reason = message.clone();
                    s.pending_analysis = true;
                    self.save(&s)?;
                    *state_lock = s;
                }
                return Err(message);
            }
            let mut changes = crate::context_budget::batch_messages(pending.clone(), budget);
            if changes.is_empty() {
                if !pending.is_empty() {
                    let message = format!(
                        "{}：单条消息连同规则、格式、工具定义和元数据超过有效消息预算；本次未调用 AI，消息仍保留。请提高上限或手动精简。",
                        ai::INPUT_LIMIT_ERROR
                    );
                    if mode == AiMode::Automatic {
                        s.auto_analysis_blocked = true;
                        s.auto_analysis_blocked_at = now();
                        s.auto_analysis_block_reason = message.clone();
                        s.pending_analysis = true;
                        self.save(&s)?;
                        *state_lock = s;
                    }
                    return Err(message);
                }
                s.pending_analysis = false;
                s.analysis_pending_since = 0;
                if mode == AiMode::Manual {
                    s.auto_analysis_blocked = false;
                    s.auto_analysis_block_reason.clear();
                    s.auto_analysis_blocked_at = 0;
                }
                self.save(&s)?;
                *state_lock = s;
                return Ok(json!({"skipped":"没有相关变化，不调用AI"}));
            }
            let max_input_chars = context.config.max_input_chars;
            let mut task_limit = usize::MAX;
            loop {
                let mut batch =
                    match crate::context_budget::classify_with_task_limit(&s, &changes, task_limit)
                    {
                        Ok(batch) => batch,
                        Err(e) => return Err(e),
                    };
                batch.payload["_trigger"] = json!(trigger);
                batch.payload["_filter_decision_at"] =
                    json!(s.message_filter.early.then_some(s.message_filter.last_run));
                let mut request = match self.prepare_ai_request_with(
                    "classify",
                    &batch.payload,
                    mode,
                    &mut context,
                ) {
                    Ok(request) => request,
                    Err(error) => return Err(error),
                };
                let total = request.prepared.total;
                let task_index = batch.payload["task_index"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row["id"].as_str().map(str::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let metrics = json!({
                    "pending":pending_count,
                    "selected":changes.len(),
                    "remaining":pending_count.saturating_sub(changes.len()),
                    "message_chars":crate::context_budget::message_batch_chars(&changes),
                    "message_budget_chars":budget,
                    "total_input_chars":total,
                    "task_limit":task_limit,
                    "omitted_tasks":task_index.len(),
                    "omitted_task_ids":task_index.iter().take(50).collect::<Vec<_>>(),
                    "truncated_messages":changes.iter().filter(|m| m.text.chars().count() > 2000).count(),
                    "truncated_original_chars":changes.iter().filter(|m| m.text.chars().count() > 2000).map(|m| m.text.chars().count()).sum::<usize>(),
                });
                // `_batch_metrics` is excluded from the public prompt, so adding
                // it after measurement cannot change the accepted bytes.
                request.payload["_batch_metrics"] = metrics.clone();
                batch.payload["_batch_metrics"] = metrics;
                if total > max_input_chars {
                    // Fixed order: messages to at most 10, then task summaries to
                    // at most 10. Never fall back to halving forever.
                    if changes.len() > 10 {
                        changes.truncate(10);
                        continue;
                    }
                    if task_limit > 10 {
                        task_limit = 10;
                        continue;
                    }
                    let message = format!(
                        "{}：截断、缩至最多10条消息并最多10个事项摘要后仍为 {} 字符，超过上限 {}；本次未调用 AI，游标和已分析标记未变更。请手动精简待分析范围或提高上限。",
                        ai::INPUT_LIMIT_ERROR, total, max_input_chars
                    );
                    if mode == AiMode::Automatic {
                        s.auto_analysis_blocked = true;
                        s.auto_analysis_blocked_at = now();
                        s.auto_analysis_block_reason = message.clone();
                        s.pending_analysis = true;
                        self.save(&s)?;
                        *state_lock = s;
                    }
                    self.log("analysis_budget_paused", json!({
                        "pending":pending_count,
                        "selected":changes.len(),
                        "total_input_chars":total,
                        "max_input_chars":max_input_chars,
                        "overflow":total.saturating_sub(max_input_chars),
                        "message_ids":changes.iter().map(|m|m.id.clone()).take(50).collect::<Vec<_>>(),
                        "omitted_message_count":pending_count.saturating_sub(changes.len()),
                        "omitted_task_ids":task_index.iter().take(50).collect::<Vec<_>>(),
                        "reason":"after_message_and_task_reduction",
                    }));
                    return Err(message);
                }
                for m in changes.iter().filter(|m| m.text.chars().count() > 2000) {
                    self.log(
                        "message_truncated",
                        json!({"id":m.id,"original_chars":m.text.chars().count(),"kept_chars":2000,"reason":"body_limit"}),
                    );
                }
                if pending_count > changes.len() {
                    self.log(
                        "messages_omitted",
                        json!({
                            "count":pending_count-changes.len(),
                            "chars":pending.iter().skip(changes.len()).map(|m|m.text.chars().count()).sum::<usize>(),
                            "ids":pending.iter().skip(changes.len()).map(|m|m.id.clone()).take(50).collect::<Vec<_>>(),
                            "reason":"message_budget"
                        }),
                    );
                }
                if !task_index.is_empty() {
                    let omitted_chars = batch.payload["task_index"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|row| row.to_string().chars().count())
                        .sum::<usize>();
                    self.log(
                        "tasks_omitted",
                        json!({"count":task_index.len(),"chars":omitted_chars,"ids":task_index.iter().take(50).collect::<Vec<_>>(),"reason":"summary_limit"}),
                    );
                }
                // Freeze the authoritative view before the request leaves this
                // lock. Post-call validation uses these rows, never the shared
                // snapshot files that another rebuild may replace meanwhile.
                request.frozen = Some(FrozenView::new(
                    batch.allowed.clone(),
                    batch.versions.clone(),
                    &request.records,
                ));
                let claim = self.claim_message_batch(&batch.fingerprints)?;
                break (batch, request, claim);
            }
        };
        let crate::context_budget::Batch {
            payload,
            fingerprints,
            ..
        } = batch;
        let result = self.invoke_ai_request(&request, mode)?;
        let seen = result["_context_seen"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // The frozen view was captured before invocation. Admit only the ids
        // supplied initially or actually returned through the request's own
        // retrieval tool; never consult the shared snapshot files after a call.
        let frozen = request
            .frozen
            .as_ref()
            .ok_or("分析请求缺少冻结证据视图，本次结果未采用")?;
        let mut allowed = frozen.initial_messages.clone();
        admit_retrieved(&mut allowed, &seen, frozen);
        allowed
            .retain(|id, _| frozen.initial_messages.contains_key(id) || seen.contains(&json!(id)));
        let mut task_versions = frozen.task_versions.clone();
        task_versions
            .retain(|id, _| request.initial_tasks.contains(id) || seen.contains(&json!(id)));
        let mut actions = result["actions"]
            .as_array()
            .ok_or("AI结果缺少actions数组")?
            .clone();
        for action in &mut actions {
            action["_tool_run_id"] = result["_tool_run_id"].clone();
            if action.get("tool_call_ids").is_none() {
                action["tool_call_ids"] = json!([]);
            }
        }
        let mut locked = self.state.lock().unwrap();
        // Reject frozen bodies that no longer match the live state before
        // applying anything. The same check is repeated in apply_actions.
        retain_live_frozen(&mut allowed, &mut task_versions, frozen, &locked);
        let mut s = locked.clone();
        let catalog = crate::tags::result_catalog(&s, &result);
        let audit_start = s.audit.len();
        let (applied, retry) =
            Self::apply_actions_detailed(&mut s, &actions, &allowed, &task_versions, &catalog)?;
        Self::stamp_decisions(&mut s, audit_start, &request.decision_context);
        Self::finish_message_batch(&mut s, fingerprints, &retry);
        s.last_analysis_at = now();
        s.last_analysis_success_at = s.last_analysis_at;
        s.auto_analysis_blocked = false;
        s.auto_analysis_block_reason.clear();
        s.auto_analysis_blocked_at = 0;
        let remaining = crate::context_budget::pending(&s).len();
        s.pending_analysis = remaining > 0;
        s.analysis_pending_since = if s.pending_analysis { now() } else { 0 };
        self.save(&s)?;
        self.notifications
            .lock()
            .unwrap()
            .push(&s.audit[audit_start..]);
        *locked = s;
        drop(locked);
        self.log("analysis_result",json!({"applied":applied,"processed":payload["changes"].as_array().map(|a|a.len()).unwrap_or(0),"remaining":remaining}));
        Ok(json!({"applied":applied}))
    }

    /// Apply validated model actions to a cloned state. Kept separate from the
    /// provider call so synthetic regression tests exercise the real commit path.
    #[cfg(test)]
    pub(crate) fn apply_actions(
        s: &mut State,
        actions: &[Value],
        allowed: &BTreeMap<String, Message>,
        task_versions: &BTreeMap<String, i64>,
        catalog: &[crate::tags::Group],
    ) -> Result<usize, String> {
        Self::apply_actions_detailed(s, actions, allowed, task_versions, catalog).map(|v| v.0)
    }

    fn finish_message_batch(
        s: &mut State,
        fingerprints: BTreeMap<String, String>,
        retry: &BTreeSet<String>,
    ) {
        for (id, fp) in fingerprints {
            if s.messages.get(&id).map(fingerprint) != Some(fp.clone()) {
                continue;
            }
            if retry.contains(&id) {
                let count = s.analysis_retry_counts.entry(id.clone()).or_insert(0);
                *count = count.saturating_add(1);
                s.analysis_retries.insert(
                    id,
                    if *count < 3 {
                        "动作未采用，等待重新分析".into()
                    } else {
                        "多次未采用，已停止自动重试；消息未标为已分析，可手动分析".into()
                    },
                );
                continue;
            }
            s.analysis_retries.remove(&id);
            s.analysis_retry_counts.remove(&id);
            s.analyzed.insert(id, fp);
        }
        crate::message_progress::advance_analysis(s);
        crate::message_progress::advance_filter(s);
    }

    fn stamp_decisions(s: &mut State, audit_start: usize, context: &Value) {
        for audit in s.audit.iter_mut().skip(audit_start) {
            if audit.kind.starts_with("ai_") && audit.after.is_object() {
                audit.after["decision_context"] = context.clone();
                if let Some(task) = s.tasks.iter_mut().find(|t| t.id == audit.task_id) {
                    task.decision_context = context.clone();
                }
            }
        }
    }

    fn apply_actions_detailed(
        mut s: &mut State,
        actions: &[Value],
        allowed: &BTreeMap<String, Message>,
        task_versions: &BTreeMap<String, i64>,
        catalog: &[crate::tags::Group],
    ) -> Result<(usize, BTreeSet<String>), String> {
        let mut applied = 0;
        let mut retry = BTreeSet::new();
        let mut task_versions = task_versions.clone();
        for action in actions {
            let action_type = action["action"].as_str().unwrap_or("");
            if action_type == "ignore" {
                continue;
            }
            if !["create", "update"].contains(&action_type) {
                return Err("AI动作类型无效".into());
            }
            let sources = action["sources"]
                .as_array()
                .ok_or("AI动作缺少来源")?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>();
            if sources.is_empty()
                || sources.iter().any(|id| {
                    !allowed.contains_key(id)
                        || s.messages
                            .get(id)
                            .map(|m| m.withdrawn || allowed.get(id) != Some(m))
                            .unwrap_or(true)
                })
            {
                retry.extend(sources.iter().cloned());
                continue;
            }
            // A duplicate create follows exactly the same validation and manual
            // protection as an update, including steps and completion evidence.
            if action_type == "create" {
                if let Some(old) = s.tasks.iter().find(|old| {
                    !old.archived
                        && old.merged_into.is_empty()
                        && old.generation.is_empty()
                        && old.evidence.all().iter().any(|id| sources.contains(id))
                }) {
                    let tid = old.id.clone();
                    let mut update = action.clone();
                    update["action"] = json!("update");
                    update["task_id"] = json!(tid);
                    // A summary of a new task may misdescribe a deduplicated update.
                    update["change_summary"] = json!("");
                    let (count, rejected) = Self::apply_actions_detailed(
                        s,
                        &[update],
                        allowed,
                        &task_versions,
                        catalog,
                    )?;
                    applied += count;
                    retry.extend(rejected);
                    if count > 0 {
                        task_versions.insert(
                            tid.clone(),
                            s.tasks.iter().find(|t| t.id == tid).unwrap().updated,
                        );
                    }
                    continue;
                }
            }
            let background = action["background_evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .filter(|id| allowed.contains_key(id))
                .collect::<Vec<_>>();
            let completion_proof = Self::valid_evidence(
                action["completion_evidence"].as_array(),
                allowed,
                &s.messages,
            );
            let mut t = match crate::context_budget::materialize(action, &s.tasks) {
                Ok(task) => task,
                Err(_) => {
                    retry.extend(sources.iter().cloned());
                    continue;
                }
            };
            t.suggested_project_id = action["suggested_project_id"].as_str().unwrap_or("").into();
            t.suggested_project_reason = action["suggested_project_reason"]
                .as_str()
                .unwrap_or("")
                .into();
            let wants_completion = t.status == "已完成"
                && (action_type == "create"
                    || action["fields"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|c| c["field"] == "status"));
            let mut summary_allowed = true;
            if wants_completion {
                if action["confidence"].as_f64().unwrap_or(0.) < 0.95 || completion_proof.is_empty()
                {
                    summary_allowed = false;
                    t.status = s
                        .tasks
                        .iter()
                        .find(|old| old.id == action["task_id"].as_str().unwrap_or(""))
                        .map(|old| old.status.clone())
                        .unwrap_or_else(|| "待处理".into());
                    retry.extend(sources.iter().cloned());
                }
            }
            t.updated = now();
            t.evidence = TaskEvidence::default();
            merge_evidence(&mut t, &sources, &background, &completion_proof);
            preserve_model_summary(&mut t);
            normalize_task(&mut t);
            if t.title.is_empty() {
                retry.extend(sources.iter().cloned());
                continue;
            }
            if action_type == "create" {
                if !t
                    .evidence
                    .direct
                    .iter()
                    .any(|id| allowed.get(id).is_some_and(Message::relevant))
                {
                    retry.extend(sources.iter().cloned());
                    continue;
                }
                t.id = id();
                t.created = now();
                t.follow_deadline = true;
                t.deadline_changed = 0;
                if !t.due.is_empty() && deadline_ms(&t.due).is_none() {
                    t.due.clear();
                }
                t.content_updated_at = t.updated;
                t.summary_updated_at = 0;
                preserve_model_summary(&mut t);
                crate::tags::sanitize(&mut t, None, catalog);
                normalize_task(&mut t);
                Self::audit_ai_change(&mut s, action, None, &t, summary_allowed);
                crate::tool_calling::link_tasks(s, action, &[t.id.clone()]);
                s.tasks.push(t);
                applied += 1;
            } else {
                let tid = action["task_id"].as_str().unwrap_or("");
                let Some(pos) = s.tasks.iter().position(|t| t.id == tid) else {
                    retry.extend(sources.iter().cloned());
                    continue;
                };
                let old = s.tasks[pos].clone();
                if task_versions.get(tid) != Some(&old.updated) {
                    retry.extend(sources.iter().cloned());
                    continue;
                }
                let progress =
                    has_new_progress(&old, &sources, &s.messages) || !completion_proof.is_empty();
                if !progress {
                    // Evidence-only update: keep every state field and append the
                    // newly observed references. Do not let older evidence roll
                    // status/title/next-step backwards.
                    let mut evidence = old.clone();
                    merge_evidence(&mut evidence, &sources, &background, &completion_proof);
                    crate::tool_calling::link_tasks(s, action, &[tid.to_string()]);
                    if evidence.evidence == old.evidence {
                        continue;
                    }
                    evidence.updated = now().max(old.updated + 1);
                    Self::audit(
                        &mut s,
                        "ai_evidence",
                        &old.id,
                        &format!(
                            "追加未展示过的依据：{}",
                            action["reason"].as_str().unwrap_or("")
                        ),
                        json!(old),
                        json!(evidence),
                    );
                    s.tasks[pos] = evidence;
                    task_versions.insert(tid.to_string(), s.tasks[pos].updated);
                    applied += 1;
                    continue;
                }
                let deadline_proof = action["deadline_evidence"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let rescheduled = t.due != old.due
                    && (t.due.is_empty() || deadline_ms(&t.due).is_some())
                    && !deadline_proof.is_empty()
                    && deadline_proof.iter().all(|id| {
                        allowed
                            .get(*id)
                            .map(|m| {
                                !m.withdrawn
                                    && m.time > old.deadline_changed
                                    && s.messages.get(*id) == Some(m)
                            })
                            .unwrap_or(false)
                    });
                if old.manual {
                    let last_manual = s
                        .audit
                        .iter()
                        .filter(|a| {
                            a.task_id == tid
                                && (a.kind.starts_with("manual")
                                    || a.kind == "merge"
                                    || a.kind == "undo")
                        })
                        .map(|a| a.time)
                        .max()
                        .unwrap_or(old.created);
                    if action["confidence"].as_f64().unwrap_or(0.) < 0.9
                        || t.reason.trim().is_empty()
                        || !sources.iter().any(|id| {
                            allowed
                                .get(id)
                                .map(|m| m.time > last_manual)
                                .unwrap_or(false)
                        })
                    {
                        retry.extend(sources.iter().cloned());
                        continue;
                    }
                }
                let mut candidate = t.clone();
                candidate.updated = now().max(old.updated + 1);
                candidate.evidence = old.evidence.clone();
                merge_evidence(&mut candidate, &sources, &background, &completion_proof);
                candidate.archive_note = old.archive_note.clone();
                candidate.deferred_until = old.deferred_until;
                candidate.schedule_id = old.schedule_id.clone();
                candidate.scheduled_for = old.scheduled_for.clone();
                candidate.archived_at = old.archived_at;
                candidate.manual = old.manual;
                candidate.locked = false;
                candidate.follow_deadline = old.follow_deadline;
                if rescheduled {
                    candidate.deadline_changed = deadline_proof
                        .iter()
                        .filter_map(|id| allowed.get(*id).map(|m| m.time))
                        .max()
                        .unwrap_or(now())
                } else {
                    candidate.due = old.due.clone();
                    candidate.deadline_changed = old.deadline_changed;
                }
                candidate.id = old.id.clone();
                candidate.created = old.created;
                if candidate.description != old.description || candidate.title != old.title {
                    candidate.content_updated_at = candidate.updated;
                    candidate.summary_updated_at = 0;
                }
                preserve_model_summary(&mut candidate);
                normalize_task(&mut candidate);
                crate::tags::sanitize(&mut candidate, Some(&old), catalog);
                Self::audit_ai_change(&mut s, action, Some(&old), &candidate, summary_allowed);
                crate::tool_calling::link_tasks(s, action, &[candidate.id.clone()]);
                s.tasks[pos] = candidate;
                task_versions.insert(tid.to_string(), s.tasks[pos].updated);
                applied += 1;
            }
        }
        Ok((applied, retry))
    }

    fn valid_evidence(
        rows: Option<&Vec<Value>>,
        allowed: &BTreeMap<String, Message>,
        messages: &BTreeMap<String, Message>,
    ) -> Vec<String> {
        let Some(rows) = rows else {
            return vec![];
        };
        if rows.is_empty() {
            return vec![];
        }
        let mut result = Vec::new();
        for value in rows {
            let Some(id) = value.as_str() else {
                return vec![];
            };
            let Some(message) = allowed.get(id) else {
                return vec![];
            };
            if messages.get(id) != Some(message) || message.withdrawn {
                return vec![];
            }
            result.push(id.to_string());
        }
        result.sort();
        result.dedup();
        result
    }
    pub fn resume_pending(&self) {
        let (drafts, scan, analysis, learning) = {
            let s = self.state.lock().unwrap();
            (
                s.tasks
                    .iter()
                    .filter(|t| t.generation == "pending")
                    .cloned()
                    .collect::<Vec<_>>(),
                s.pending_scan,
                s.pending_analysis,
                s.pending_learning,
            )
        };
        if !drafts.is_empty() || scan || analysis || learning {
            self.log(
                "resume_pending",
                json!({"tasks":drafts.len(),"scan":scan,"analysis":analysis,"learning":learning}),
            );
        }
        for draft in drafts {
            if process::stopping() {
                return;
            }
            self.generate_background(draft, AiMode::Automatic);
        }
        if scan {
            if let Err(e) = self.scan(AiMode::Automatic) {
                self.error(&e)
            }
        }
        if self.state.lock().unwrap().pending_analysis {
            if let Err(e) = self.analyze_after_scan(AiMode::Automatic) {
                self.error(&e)
            }
        }
    }
    pub fn scan_tick(&self) {
        let (config, last) = {
            let s = self.state.lock().unwrap();
            if !s.chat_sources.iter().any(|source| source.enabled) {
                return;
            }
            (s.config.clone(), s.last_scan.max(s.last_scan_finished_at))
        };
        if config.enabled
            && !process::stopping()
            && !self.busy.load(Ordering::SeqCst)
            && now() - last.max(self.last_attempt.load(Ordering::SeqCst))
                >= config.interval_minutes as i64 * 60_000
        {
            if let Err(e) = self.scan(AiMode::Automatic) {
                self.error(&e);
            }
        }
    }
    pub fn tick(&self) {
        if process::stopping() {
            return;
        }
        // Archive has priority over a new analysis in the same tick. Claim and
        // attempt validation happen before any AI call.
        if let Err(e) = self.archive_tick() {
            self.error(&e);
        }
        if let Err(e) = self.morning_tick() {
            self.error(&e);
        }
        if let Err(e) = self.filter_tick() {
            self.error(&e);
        }
        if let Err(e) = self.analyze_after_scan(AiMode::Automatic) {
            self.error(&e);
        }
        if self.ai_allowed(AiMode::Automatic) && !self.ai_busy.load(Ordering::SeqCst) {
            let draft = self
                .state
                .lock()
                .unwrap()
                .tasks
                .iter()
                .find(|t| t.generation == "pending")
                .cloned();
            if let Some(draft) = draft {
                self.generate_background(draft, AiMode::Automatic);
            }
        }
    }
    pub fn error(&self, e: &str) {
        if e == crate::ai_control::CANCELLED || e.starts_with("AI_UNCONFIGURED:") {
            return;
        }
        let mut s = self.state.lock().unwrap();
        // Repeated identical failures (for example a provider that stays down
        // for hours) are kept in state once instead of appending a new row on
        // every poll. A different failure still updates and logs immediately.
        if s.last_error == e {
            return;
        }
        let mut next = s.clone();
        next.last_error = e.into();
        if self.save(&next).is_ok() {
            *s = next;
        }
        drop(s);
        self.log(
            "error",
            json!({"message":e,"failure_class":crate::ai_control::failure_class(e)}),
        );
    }
    pub fn evidence(&self, tid: &str) -> Value {
        let s = self.state.lock().unwrap();
        let Some(task) = s.tasks.iter().find(|t| t.id == tid) else {
            return json!([]);
        };
        let role = |id: &str| {
            if task.evidence.completion.contains(&id.to_string()) {
                "completion"
            } else if task.evidence.background.contains(&id.to_string()) {
                "background"
            } else {
                "direct"
            }
        };
        let msgs = task
            .evidence
            .all()
            .iter()
            .filter_map(|id| s.messages.get(id))
            .filter(|m| !m.withdrawn)
            .map(|m| {
                let mut value = json!(m);
                value["evidence_role"] = json!(role(&m.id));
                value
            })
            .collect::<Vec<_>>();
        json!(msgs)
    }
}
pub(crate) fn reconcile_withdrawals(s: &mut State) {
    let withdrawn = s
        .messages
        .iter()
        .filter(|(_, m)| m.withdrawn)
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    for t in &mut s.tasks {
        let all = t.evidence.all();
        if all.iter().any(|id| withdrawn.contains(id)) {
            for rows in [
                &mut t.evidence.direct,
                &mut t.evidence.background,
                &mut t.evidence.completion,
            ] {
                rows.retain(|id| !withdrawn.contains(id));
            }
            if t.evidence.all().is_empty() {
                if !t.manual {
                    t.status = "已取消".into();
                    t.reason = "唯一消息来源已撤回".into()
                } else {
                    t.reason = "消息来源已撤回；保留人工事项，请确认".into()
                }
            } else if t.status == "已完成" {
                t.status = "待处理".into();
                t.reason = "来源发生撤回，重新核对完成依据".into()
            }
            t.updated = now();
        }
    }
}

/// A model-provided summary is retained unless the content changed after it
/// was produced. An empty summary keeps the deterministic content fallback.
fn preserve_model_summary(t: &mut Task) {
    if !t.summary.trim().is_empty() {
        t.content_updated_at = t.content_updated_at.max(t.updated);
        t.summary_updated_at = t.content_updated_at;
        t.summary_fallback = false;
    }
}

/// Presentation metadata is best-effort. Keep it only when every requested
/// field survived validation, and the accepted task has a substantive change.
fn accepted_change_summary(action: &Value, before: &Value, after: &Value) -> String {
    let Some(summary) = action["change_summary"].as_str() else {
        return String::new();
    };
    if summary.trim().is_empty()
        || summary.chars().count() > 30
        || summary.chars().any(char::is_control)
        || summary.contains(['\u{2028}', '\u{2029}'])
    {
        return String::new();
    }
    let Some(fields) = action["fields"].as_array() else {
        return String::new();
    };
    if fields.iter().any(|change| {
        change["field"]
            .as_str()
            .is_none_or(|field| after.get(field) != Some(&change["value"]))
    }) {
        return String::new();
    }
    let tags = |value: &Value| {
        serde_json::from_value::<Vec<crate::tags::Tag>>(value.clone())
            .ok()
            .map(|tags| tags.into_iter().collect::<BTreeSet<_>>())
    };
    if action
        .get("tags")
        .is_some_and(|requested| tags(requested) != tags(&after["tags"]))
    {
        return String::new();
    }
    if !before.is_null()
        && fields.iter().all(|change| {
            let key = change["field"].as_str().unwrap();
            before[key] == after[key]
        })
        && tags(&before["tags"]) == tags(&after["tags"])
    {
        return String::new();
    }
    summary.trim().to_owned()
}
/// Admit only ids the model actually retrieved (`_context_seen`) from the
/// frozen view captured before invocation. The frozen record is kept so
/// `apply_actions` can compare it with live state.
pub(crate) fn admit_retrieved(
    allowed: &mut BTreeMap<String, Message>,
    seen: &[Value],
    frozen: &FrozenView,
) {
    for id in seen.iter().filter_map(Value::as_str) {
        if let Some(message) = frozen.messages.get(id) {
            allowed
                .entry(id.to_string())
                .or_insert_with(|| message.clone());
        }
    }
}

/// Drop evidence whose live body no longer matches the fingerprint the request
/// actually saw. Task updates retain only versions observed at freeze time.
pub(crate) fn retain_live_frozen(
    allowed: &mut BTreeMap<String, Message>,
    task_versions: &mut BTreeMap<String, i64>,
    frozen: &FrozenView,
    live: &State,
) {
    allowed.retain(|id, message| {
        !message.withdrawn
            && frozen.fingerprints.get(id).is_some_and(|expected| {
                live.messages.get(id).map(fingerprint).as_ref() == Some(expected)
            })
    });
    task_versions.retain(|id, version| {
        live.tasks.iter().any(|task| {
            task.id == *id
                && task.updated == *version
                && !task.archived
                && task.merged_into.is_empty()
        })
    });
}

// Do not let an older backlog batch roll a task back to a previous stage.
fn has_new_progress(old: &Task, sources: &[String], messages: &BTreeMap<String, Message>) -> bool {
    let existing = old.evidence.all();
    let latest = existing
        .iter()
        .filter_map(|id| messages.get(id))
        .filter(|m| !m.withdrawn)
        .map(|m| m.time)
        .max()
        .unwrap_or(0);
    sources.iter().any(|id| {
        !existing.contains(id)
            && messages
                .get(id)
                .map(|m| !m.withdrawn && m.time >= latest)
                .unwrap_or(false)
    })
}

/// UI snapshots decorate tasks with display-only fields; they are not part of
/// the persisted schema and must be removed before strict deserialization.
fn task_from_ui(value: &Value) -> Result<Task, String> {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        for key in [
            "number",
            "evidence_count",
            "has_tool_calls",
            "effective_priority",
            "effective_tags",
            "priority_rank",
            "status_changed_at",
            "today",
            "deferred",
        ] {
            object.remove(key);
        }
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

const EDIT_FIELDS: [&str; 7] = [
    "title",
    "description",
    "status",
    "blocker",
    "waiting_for",
    "next_step",
    "due",
];
fn validate_edit(t: &Task) -> Result<(), String> {
    if t.title.trim().is_empty() || t.title.chars().count() > 200 {
        return Err("标题需要1–200字".into());
    }
    if [&t.description, &t.blocker, &t.waiting_for, &t.next_step]
        .iter()
        .any(|s| s.chars().count() > 6000)
    {
        return Err("修改字段最多6000字".into());
    }
    if t.tags.len() > 40 || t.tags.iter().any(|tag| !crate::tags::valid(tag)) {
        return Err("事项标签无效或超过40个".into());
    }
    if !["待处理", "已完成", "无需处理", "已取消"].contains(&t.status.as_str()) {
        return Err("修改后的状态无效".into());
    }
    if !t.due.is_empty() && deadline_ms(&t.due).is_none() {
        return Err("修改后的截止时间无效，原事项未改动".into());
    }
    Ok(())
}
fn parse_schedule_draft(v: Value) -> Result<Schedule, String> {
    if let Some(error) = v["error"].as_str().filter(|s| !s.trim().is_empty()) {
        return Err(error.into());
    }
    let mut s: Schedule =
        serde_json::from_value(v["schedule"].clone()).map_err(|e| e.to_string())?;
    schedule::validate(&mut s)?;
    s.id.clear();
    s.created = 0;
    s.updated = 0;
    s.active_from = 0;
    s.last_generated_on.clear();
    s.enabled = true;
    Ok(s)
}

/// When each task last entered each status, from one pass over the audit, so
/// per-task lookups stay O(1) instead of rescanning the audit for every task.
struct StatusChanges<'a>(HashMap<(&'a str, &'a str), i64>);
impl<'a> StatusChanges<'a> {
    fn new(audit: &'a [Audit]) -> Self {
        let mut latest = HashMap::new();
        for a in audit {
            if let Some(status) = a.after["status"].as_str() {
                if a.before["status"] != a.after["status"] {
                    latest.insert((a.task_id.as_str(), status), a.time);
                }
            }
        }
        Self(latest)
    }
    fn changed_at(&self, t: &Task) -> i64 {
        self.0
            .get(&(t.id.as_str(), t.status.as_str()))
            .copied()
            .unwrap_or(t.updated)
    }
    fn today(&self, t: &Task, at: i64) -> bool {
        if !t.merged_into.is_empty() || t.deferred_until > at {
            return false;
        }
        let day = |ts: i64| (ts + 8 * 3_600_000).div_euclid(86_400_000);
        (!t.archived && t.status == "待处理")
            || day(t.created) == day(at)
            || (t.status != "待处理" && day(self.changed_at(t)) == day(at))
    }
}
#[cfg(test)]
fn is_today_task(t: &Task, audit: &[Audit], at: i64) -> bool {
    StatusChanges::new(audit).today(t, at)
}

pub(crate) fn learning_evidence(a: &Audit) -> bool {
    a.kind == "manual_edit"
        || a.kind == "manual_ai_edit"
        || a.kind == "undo"
        || a.kind == "merge"
        || (a.kind == "manual_status" && ["dismiss", "reopen"].contains(&a.detail.as_str()))
        || (a.kind == "manual_archive"
            && !a.after["archive_note"]
                .as_str()
                .unwrap_or("")
                .trim()
                .is_empty())
}

pub(crate) fn apply_learning(
    s: &mut State,
    rows: &[Value],
    eligible_ids: &[usize],
    history_ids: &[usize],
) -> Result<(usize, usize), String> {
    let mut added = 0;
    let mut updated = 0;
    for v in rows.iter().take(3) {
        let mut r: Suggestion = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        r.evidence.sort();
        r.evidence.dedup();
        if r.title.trim().is_empty()
            || r.rule.trim().is_empty()
            || !r.evidence.iter().any(|i| eligible_ids.contains(i))
        {
            continue;
        }
        let sid = v["suggestion_id"].as_str().unwrap_or("");
        let existing = s
            .suggestions
            .iter()
            .position(|x| x.id == sid || (sid.is_empty() && x.rule.trim() == r.rule.trim()));
        let prior = existing
            .map(|i| s.suggestions[i].evidence.clone())
            .unwrap_or_default();
        if r.evidence.iter().any(|i| {
            (!eligible_ids.contains(i) && !history_ids.contains(i) && !prior.contains(i))
                || s.audit
                    .get(*i)
                    .map(|a| !learning_evidence(a))
                    .unwrap_or(true)
        }) {
            continue;
        }
        let explicit_note = r.evidence.iter().any(|i| {
            s.audit[*i].kind == "manual_archive"
                && !s.audit[*i].after["archive_note"]
                    .as_str()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
        });
        if existing.is_none() && r.evidence.len() < 2 && !explicit_note {
            continue;
        }
        if let Some(pos) = existing {
            if s.suggestions[pos].archived_at > 0
                || crate::evolution::decision(&s.suggestions[pos]) != "pending"
            {
                continue;
            }
            r.evidence.extend(prior);
            r.evidence.sort();
            r.evidence.dedup();
            s.suggestions[pos].updated = now();
            s.suggestions[pos].title = r.title;
            s.suggestions[pos].rule = r.rule;
            s.suggestions[pos].evidence = r.evidence;
            updated += 1;
        } else if sid.is_empty() {
            r.id = id();
            r.created = now();
            r.active = false;
            r.decision = "pending".into();
            r.archived_at = 0;
            r.updated = now();
            s.suggestions.push(r);
            added += 1;
        }
    }
    Ok((added, updated))
}
