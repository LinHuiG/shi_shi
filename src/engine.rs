#[path = "engine/analysis_batch.rs"]
mod analysis_batch;
#[cfg(test)]
#[path = "engine/analysis_round_tests.rs"]
mod analysis_round_tests;
#[path = "engine/background.rs"]
mod background;
#[path = "engine/editing.rs"]
mod editing;
#[path = "engine/persistence.rs"]
mod persistence;
#[cfg(test)]
#[path = "engine/tests.rs"]
mod regression_tests;
#[cfg(test)]
#[path = "engine/reply_context_tests.rs"]
mod reply_context_tests;
#[cfg(test)]
#[path = "engine/report_status_tests.rs"]
mod report_status_tests;
#[path = "engine/reports.rs"]
mod reports;
#[cfg(test)]
#[path = "engine/tag_assignment_tests.rs"]
mod tag_assignment_tests;
#[cfg(test)]
#[path = "engine/task_status_tests.rs"]
mod task_status_tests;
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
        Arc, Mutex,
    },
};

pub struct App {
    pub(crate) background: background::BackgroundWake,
    pub(crate) local_runtime: crate::local_service::Runtime,
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
    pub(crate) ai_live: Mutex<crate::ai_progress::LiveProgress>,
    pub(crate) analysis_last_attempt: std::sync::atomic::AtomicI64,
    pub(crate) ai_started: std::sync::atomic::AtomicI64,
    pub(crate) last_attempt: std::sync::atomic::AtomicI64,
    /// Successful saves; part of the UI change pulse.
    saves: AtomicU64,
    /// Deterministic cancellation during preparation; no production lock.
    #[cfg(test)]
    pub(crate) prepare_gate: Mutex<()>,
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
pub(crate) struct PrepareContext {
    decision_context: Value,
    config: Config,
    catalog: Value,
    priority_kind: String,
    projects: Value,
    memory_archived: String,
    memory_temporary: String,
    last_scan: i64,
    runtime_at: i64,
    work_status: crate::work_state::WorkStatus,
    business_date: String,
    records: Vec<Value>,
    record_set: Option<PreparedRecordSet>,
    runtime: Option<Value>,
}

struct PreparedRecordSet {
    records: Arc<Vec<Value>>,
    wire_ids: Arc<crate::wire_context::IdMap>,
    metadata: Value,
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
    pub wire_ids: Arc<crate::wire_context::IdMap>,
    pub tag_payload: Value,
    pub catalog: Value,
    pub trigger: String,
    pub compression_before: usize,
    pub records: Arc<Vec<Value>>,
    pub initial_tasks: BTreeSet<String>,
    pub cache_components: Value,
    pub frozen: Option<FrozenView>,
    /// Definitive host validation failure, separate from provider/I/O errors.
    response_validation_failed: AtomicBool,
}
impl PreparedAiRequest {
    /// Budget candidates leave large private records shared. Only an accepted
    /// request materializes/encodes them for the adapter; public bytes do not
    /// change, including image hints, tools and memory receipts.
    pub(crate) fn materialize_records(&mut self) {
        if self.payload["_local_context"].is_object() {
            let mut records = json!(&*self.records);
            self.wire_ids.encode(&mut records);
            self.payload["_local_context"]["records"] = records;
        }
    }
}

/// Authoritative view of what this invocation could see. It is captured before
/// the call and survives snapshot rebuilds; post-call admission never reads the
/// shared snapshot files again.
pub(crate) struct FrozenView {
    pub initial_messages: BTreeMap<String, Message>,
    context_messages: BTreeMap<String, Message>,
    pub task_versions: BTreeMap<String, i64>,
}

impl FrozenView {
    fn new(
        initial_messages: BTreeMap<String, Message>,
        task_versions: BTreeMap<String, i64>,
        records: &[Value],
    ) -> Self {
        let context_messages = records
            .iter()
            .filter(|record| {
                record["kind"] == "message"
                    && !initial_messages.contains_key(record["data"]["id"].as_str().unwrap_or(""))
            })
            .filter_map(|record| {
                from_json_ignoring::<Message>(&record["data"], &["truncated", "original_chars"])
                    .ok()
                    .map(|message| (message.id.clone(), message))
            })
            .collect();
        Self {
            initial_messages,
            context_messages,
            task_versions,
        }
    }
    fn message(&self, id: &str) -> Option<&Message> {
        self.initial_messages
            .get(id)
            .or_else(|| self.context_messages.get(id))
    }
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
        // Recover only an unambiguous legacy deferral, never guess from task text.
        for task in state
            .tasks
            .iter_mut()
            .filter(|t| t.status == "待处理" && t.deferred_until > 0 && !t.resume_processing)
        {
            if let Some(audit) = state.audit.iter().rev().find(|a| a.task_id == task.id) {
                if audit.kind == "manual_status"
                    && audit.detail == "延后处理"
                    && audit.after.get("resume_processing").is_none()
                    && audit.after["deferred_until"].as_i64() == Some(task.deferred_until)
                    && audit.before["status"] == "处理中"
                {
                    task.resume_processing = true;
                }
            }
        }
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
        let report_recovered_at = state.next_report_finished(now());
        for job in &mut state.morning_jobs {
            if ["queued", "running"].contains(&job.status.as_str()) {
                job.status = "failed".into();
                job.error = "重启中断，请手动生成报表".into();
                job.finished = report_recovered_at;
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
            local_runtime: Default::default(),
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
            background: background::BackgroundWake::default(),
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
            ai_live: Mutex::new(crate::ai_progress::LiveProgress::default()),
            analysis_last_attempt: std::sync::atomic::AtomicI64::new(0),
            ai_started: std::sync::atomic::AtomicI64::new(0),
            last_attempt: std::sync::atomic::AtomicI64::new(0),
            saves: AtomicU64::new(0),
            #[cfg(test)]
            prepare_gate: Mutex::new(()),
        };
        a.persist()?;
        a.resume_deferred(now())?;
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
        let configured = self
            .state
            .lock()
            .unwrap()
            .config
            .model_choice(kind)
            .connection_id
            != UNCONFIGURED_CONNECTION;
        self.require_ai_route(configured, kind)
    }
    fn require_ai_route(&self, configured: bool, kind: &str) -> Result<(), String> {
        if configured {
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
        before: Option<Value>,
        after: &Task,
        summary_allowed: bool,
    ) {
        let kind = if before.is_some() {
            "ai_update"
        } else {
            "ai_create"
        };
        let before = before.unwrap_or(Value::Null);
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
    pub(crate) fn reserve_ai(&self, mode: AiMode) -> Result<Config, String> {
        self.reserve_ai_inner(mode, "handling", None, None, None)
    }
    pub(crate) fn reserve_ai_inner(
        &self,
        mode: AiMode,
        kind: &str,
        attempt: Option<&str>,
        filter_decision_at: Option<i64>,
        observed_messages: Option<&[&Message]>,
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
        self.commit_state(|s| Self::update_temporary_memory(s, update))
    }
    fn update_temporary_memory(s: &mut State, update: &Value) -> Result<(), String> {
        let before = update["before"].as_str().ok_or("笔记原版本无效")?;
        let text = update["text"].as_str().ok_or("笔记内容无效")?;
        if text.chars().count() > crate::analysis_context::MEMORY_LIMIT {
            return Err("笔记超过2000字符，未保存".into());
        }
        if s.analysis_memory.temporary != before
            || update["expected"]
                .as_u64()
                .is_some_and(|n| n != s.analysis_memory.temporary_version)
        {
            return Err("临时记忆已被修改，本次不覆盖".into());
        }
        s.analysis_memory.temporary = text.into();
        s.analysis_memory.temporary_version += 1;
        s.analysis_memory.temporary_updated = now();
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
    pub(crate) fn capture_archive_context(
        &self,
        state: &State,
        records: Vec<Value>,
    ) -> PrepareContext {
        let mut context = self.capture_prepare_context_from(state, "archive", true, json!([]));
        context.records = records;
        context
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
        #[cfg(test)]
        let _gate = self.prepare_gate.lock().unwrap();
        let state = self.state.lock().unwrap().clone();
        if let Some(task_id) = task_id {
            if !state.tasks.iter().any(|task| task.id == task_id) {
                return Err("事项不存在".into());
            }
        }
        Ok(self.capture_prepare_context_from(&state, kind, context_frozen, projects))
    }

    pub(crate) fn capture_prepare_context_from(
        &self,
        state: &State,
        kind: &str,
        context_frozen: bool,
        projects: Value,
    ) -> PrepareContext {
        self.capture_prepare_context_cached(
            state,
            kind,
            context_frozen,
            projects,
            &mut crate::snapshots::Cache::default(),
        )
    }

    fn capture_prepare_context_cached(
        &self,
        state: &State,
        kind: &str,
        context_frozen: bool,
        projects: Value,
        snapshots: &mut crate::snapshots::Cache,
    ) -> PrepareContext {
        let records = if !context_frozen
            && ["classify", "prompt_review", "report", "learning", "archive"].contains(&kind)
        {
            // The frozen projection is reused by every reduction of this request.
            snapshots.records(state, now())
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
            .map_or(crate::work_state::WorkStatus::CalendarUnknown, |snapshot| {
                snapshot.status
            });
        let business_date = work
            .as_ref()
            .map(|snapshot| snapshot.business_date.to_string())
            .unwrap_or_else(|| "未提供".into());
        PrepareContext {
            decision_context: json!({
                "prompt_version": prompts::current(state, kind).map(|v| &v.id),
                "rule_version": state.rule_version,
                "rule_ids": state.suggestions.iter().filter(|r| r.active && r.archived_at == 0).map(|r| &r.id).collect::<Vec<_>>(),
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
            record_set: None,
            runtime: None,
        }
    }

    /// Build the exact request object used for measurement and invocation.
    /// The runtime clock is captured on the first candidate and reused by every
    /// later reduction. No live state is read here.
    pub(crate) fn prepare_ai_request_with(
        &self,
        kind: &str,
        payload: &Value,
        mode: AiMode,
        context: &mut PrepareContext,
    ) -> Result<PreparedAiRequest, String> {
        let mut request = self.prepare_ai_candidate_with(kind, payload, mode, context)?;
        request.materialize_records();
        Ok(request)
    }

    pub(crate) fn prepare_ai_candidate_with(
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
            if context.record_set.is_none() {
                let mut records = std::mem::take(&mut context.records);
                if kind == "classify" {
                    // A quoted parent may be older than the ordinary snapshot
                    // window and already analyzed without becoming task evidence.
                    // Keep its complete body from this same frozen batch available
                    // for paged retrieval when the inline excerpt is insufficient.
                    let mut ids = records
                        .iter()
                        .filter_map(|row| row["id"].as_str().map(str::to_owned))
                        .collect::<BTreeSet<_>>();
                    for row in enriched["_local_context"]["records"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        if row["kind"] == "message"
                            && row["id"]
                                .as_str()
                                .is_some_and(|id| ids.insert(id.to_owned()))
                        {
                            records.push(row.clone());
                        }
                    }
                }
                crate::analysis_context::attach_records(
                    &mut enriched,
                    records,
                    context.config.retrieval_budget_chars,
                )?;
            }
        }
        if context_frozen && context.record_set.is_none() && !context.records.is_empty() {
            crate::analysis_context::attach_records(
                &mut enriched,
                std::mem::take(&mut context.records),
                context.config.retrieval_budget_chars,
            )?;
        }
        if kind != "classify" && enriched["_local_context"]["memory"].is_object() {
            enriched["_local_context"]["memory"]["writable"] = json!(false);
            enriched["memory"]["writable"] = json!(false);
        }
        if let Some(record_set) = &context.record_set {
            let room_aliases = enriched["_local_context"].get("room_aliases").cloned();
            enriched["_local_context"] = record_set.metadata.clone();
            if let Some(local) = enriched["_local_context"].as_object_mut() {
                local.remove("room_aliases");
                if let Some(aliases) = room_aliases {
                    local.insert("room_aliases".into(), aliases);
                }
            }
            if let Some(images) = record_set.metadata["images"].as_array() {
                crate::images::attach_hints(&self.root, &mut enriched, images);
            }
        } else if kind != "handling_learning" {
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
            "changes": enriched["changes"],
            "context": enriched["context"],
            "audit": enriched["audit"],
            "user_description": enriched["user_description"],
            "user_instruction": enriched["user_instruction"]
        });
        let compression_before = json_chars(&ai::public_payload(&enriched));
        // The frozen view keeps original ids and bodies before wire aliases are
        // applied. Post-call validation uses this copy, never the shared files.
        let initial_tasks = enriched["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|task| task["id"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        // Wire compression is deterministic and id mappings are reversible. The
        // mapping is built from the complete private record set, so hidden ids
        // participate in alias allocation exactly as they will at invocation.
        if context.record_set.is_none() {
            let wire_ids = if ["classify", "format"].contains(&kind) {
                crate::wire_context::IdMap::from_payload(&enriched)
            } else {
                crate::wire_context::IdMap::default()
            };
            let records = enriched
                .get_mut("_local_context")
                .and_then(|local| local.get_mut("records"))
                .and_then(Value::as_array_mut)
                .map(std::mem::take)
                .unwrap_or_default();
            context.record_set = Some(PreparedRecordSet {
                records: Arc::new(records),
                wire_ids: Arc::new(wire_ids),
                metadata: enriched["_local_context"].clone(),
            });
        }
        let record_set = context.record_set.as_ref().unwrap();
        let records = record_set.records.clone();
        let wire_ids = record_set.wire_ids.clone();
        if ["classify", "format"].contains(&kind) {
            wire_ids.encode(&mut enriched);
            crate::wire_context::compact(&mut enriched);
            enriched["wire_format"] = json!(crate::prompt_config::get("wire_format"));
        }
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
            response_validation_failed: AtomicBool::new(false),
        })
    }

    /// Invoke a standalone prepared request; multi-batch runs keep their own
    /// AiCall reservation and use invoke_prepared_request directly.
    #[cfg(test)]
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
        self.require_ai_route(
            request.config.selected_connection != UNCONFIGURED_CONNECTION,
            &request.kind,
        )?;
        let payload = &request.payload;
        let prepared = &request.prepared;
        let preview_config = &request.config;
        if let Err(error) = ai::validate_prepared(preview_config, payload, prepared) {
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
                s.auto_analysis_block_fingerprint = analysis_block_fingerprint(&s);
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
        let observed_messages = request.frozen.as_ref().map(|frozen| {
            request.tag_payload["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["id"].as_str())
                .filter_map(|id| frozen.initial_messages.get(id))
                .collect::<Vec<_>>()
        });
        self.reserve_ai_inner(
            mode,
            kind,
            payload["_archive_attempt_id"].as_str(),
            payload["_filter_decision_at"].as_i64(),
            observed_messages.as_deref(),
        )?;
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
        let cache_components = &request.cache_components;
        let cache_diag = json!({
            "stable_prefix_chars":stable_prefix,
            "stable_prefix_scope":"actual_prompt_prefix_across_requests",
            "prompt_chars":prepared.prompt_chars,
            "runtime_chars":cache_components["runtime_chars"],
            "components":cache_components
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
        self.ai_live.lock().unwrap().start(&request_id);
        self.log("ai_start",json!({"request_id":request_id,"kind":kind,"source":source,"trigger":trigger,"model":config.model,"reasoning_effort":config.reasoning_effort,"messages":payload["changes"].as_array().map(|a|a.len()).unwrap_or(0),"batch":payload["_batch_metrics"],"runtime":request.runtime,"input_chars":json_chars(&ai::public_payload(payload)),"input_sizes":json!({"prompt_chars":prepared.prompt_chars,"schema_chars":prepared.schema_chars,"tool_schema_chars":prepared.tool_chars,"wire_overhead_chars":prepared.wire_chars,"total_input_chars":prepared.total,"max_input_chars":config.max_input_chars}),"cache_diag":cache_diag}));
        let mut observed = Value::Null;
        let mut cache_diagnostics = Value::Null;
        let mut analysis_thread = String::new();
        let tool_session = if let Some(tools) = payload.get("_registered_tools") {
            let tools: Vec<crate::tool_registry::ToolDefinition> = from_json(tools)?;
            (!tools.is_empty()).then(|| {
                crate::tool_calling::ToolSession::new(
                    self,
                    &request_id,
                    kind,
                    payload["_tool_task_id"].as_str().unwrap_or(""),
                    tools,
                    &config,
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
                    AnalysisEvent::OutputValidationFailed => {
                        request
                            .response_validation_failed
                            .store(true, Ordering::SeqCst);
                    }
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
                        if kind == "diagnostic" {
                            self.log(
                                "ai_diagnostic",
                                json!({"request_id":request_id,"text":text}),
                            );
                        }
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
        detail["cache_diag"] = json!({"stable_prefix_chars":stable_prefix,"prompt_chars":prompt_for_cache.chars().count(),"components":request.cache_components,"cache":cache_status,"cached_tokens":cached_tokens});
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
        detail["wire_chars"] = json!({"before":request.compression_before,"after":json_chars(&ai::public_payload(payload))});
        detail["response_received"] = json!(response_received);
        let result = self.finalize_ai_response(
            request,
            &request_id,
            detail,
            res.map(|(value, _)| value),
            !finish_call && kind == "classify",
        );
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
        defer_classify_commit: bool,
    ) -> Result<Value, String> {
        let kind = request.kind.as_str();
        let config = &request.config;
        let payload = &request.payload;
        let mut failure_stage = "response_validation";
        detail["validation_ms"] = json!(0);
        let finalized = res.and_then(|mut v| {
            let validation_started = std::time::Instant::now();
            request.wire_ids.humanize_prose(&mut v, &request.records);
            let decoded = request.wire_ids.decode(&mut v);
            detail["validation_ms"] = json!(validation_started.elapsed().as_millis() as u64);
            if decoded.is_err() {
                request
                    .response_validation_failed
                    .store(true, Ordering::SeqCst);
            }
            decoded?;
            detail["context_usage"] = v["_context_usage"].clone();
            detail["analysis_degraded"] = v["_analysis_degraded"].clone();
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
            let memory_update = if defer_classify_commit {
                None
            } else {
                v.as_object_mut().and_then(|o| o.remove("_memory_update"))
            };
            if let Some(update) = memory_update {
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
                if !defer_classify_commit {
                    self.commit_state(|s| {
                        crate::tags::collect(s, &v, &request.tag_payload, &request_id);
                        Ok(())
                    })?;
                }
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
                detail["failure_class"] = json!(if request
                    .response_validation_failed
                    .load(Ordering::SeqCst)
                {
                    "response_validation"
                } else {
                    crate::ai_control::failure_class(&e)
                });
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
            let wake = self.background.generation();
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
                if mode == AiMode::Automatic {
                    return;
                }
                self.background.wait(wake);
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
                .map(|e| e.contains("正在处理其他请求") || e.starts_with("归档已排队"))
                .unwrap_or(false)
            {
                if mode == AiMode::Automatic {
                    return;
                }
                self.background.wait(wake);
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
        let parsed = result.and_then(|value| {
            let catalog = crate::tags::result_catalog(state, &value);
            let rows = value["tasks"].as_array().ok_or("AI结果缺少事项列表")?;
            if rows.is_empty() {
                return Err("AI未生成事项，请重试".into());
            }
            let mut tasks = vec![];
            for row in rows {
                // `steps_changed` is an update-action signal, not a Task field.
                let mut t: Task =
                    from_json_ignoring(row, &["tool_call_ids", "steps_changed", "steps"])?;
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
                tasks.push(t)
            }
            for (task, row) in tasks.iter().zip(rows) {
                let mut link = json!({"_tool_run_id":value.get("_tool_run_id")});
                if let Some(ids) = row.get("tool_call_ids") {
                    link["tool_call_ids"] = ids.clone();
                } else if rows.len() > 1 {
                    link["tool_call_ids"] = json!([]);
                }
                crate::tool_calling::link_tasks(state, &link, std::slice::from_ref(&task.id));
            }
            Ok(tasks)
        });
        let count = match parsed {
            Ok(tasks) => {
                let count = tasks.len();
                for t in tasks.iter().rev() {
                    Self::audit(
                        state,
                        "generation_done",
                        &t.id,
                        "自然语言事项已生成",
                        json!(draft),
                        json!(t),
                    );
                }
                state.tasks.splice(pos..=pos, tasks);
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
            if analysis_budget_blocked(&self.state.lock().unwrap()) {
                return Ok(
                    json!({"skipped":"自动分析已暂停：本批仍超过输入上限，请手动精简后重试"}),
                );
            }
            let due = {
                let mut state_lock = self.state.lock().unwrap();
                let mut s = state_lock.clone();
                let pending =
                    crate::context_budget::pending_refs(&state_lock, false).collect::<Vec<_>>();
                if s.config.model_choice("classify").connection_id == UNCONFIGURED_CONNECTION
                    && now() - self.analysis_last_attempt.load(Ordering::SeqCst)
                        < s.config.analysis_interval_minutes as i64 * 60_000
                {
                    return Ok(json!({"skipped":"等待分析间隔"}));
                }
                let before = (s.pending_analysis, s.analysis_pending_since);
                crate::context_budget::mark_pending(&mut s, !pending.is_empty(), now());
                if before != (s.pending_analysis, s.analysis_pending_since) {
                    self.save(&s)?;
                }
                let due = crate::context_budget::automatic_due(
                    &s,
                    pending.iter().copied(),
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
        self.run_analysis_with(mode, true, &mut |request, call| {
            self.invoke_prepared_request(request, mode, call, false)
        })
    }
    pub fn analyze(&self, mode: AiMode) -> Result<Value, String> {
        self.run_analysis_with(mode, false, &mut |request, call| {
            self.invoke_prepared_request(request, mode, call, false)
        })
    }
    fn run_analysis_with(
        &self,
        mode: AiMode,
        after_scan: bool,
        invoke: &mut dyn FnMut(
            &PreparedAiRequest,
            &crate::ai_control::AiCall<'_>,
        ) -> Result<Value, String>,
    ) -> Result<Value, String> {
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
        process::check_running()?;
        let started = std::time::Instant::now();
        let timeout = self.state.lock().unwrap().config.ai_timeout_seconds;
        let deadline = started + std::time::Duration::from_secs(timeout);
        // Own the AI reservation for the whole round. In particular, a losing
        // concurrent starter must not reset the scheduler's attempt timestamp.
        let mut call = if after_scan && mode == AiMode::Manual {
            // Saving the scan wakes the scheduler, which may acquire AI first.
            self.begin_analysis_after_scan(deadline)?
        } else {
            self.begin_ai_for("classify")?
        };
        let (mut boundary, filter_decision_at) = {
            let mut locked = self.state.lock().unwrap();
            if mode == AiMode::Automatic {
                if !locked.config.automatic_analysis_enabled() {
                    return Ok(json!({"skipped":"自动 AI 已关闭，消息已保存"}));
                }
                if !self.work_gate(&locked, "classify", mode, None, now()) {
                    return Ok(json!({"skipped":self.ai_deferred_reason()}));
                }
                if analysis_budget_blocked(&locked) {
                    return Ok(
                        json!({"skipped":"自动分析已暂停：本批仍超过输入上限，请手动精简后重试"}),
                    );
                }
            }
            if !locked.analysis_retry_scopes.is_empty() {
                let scope = analysis_block_fingerprint(&locked);
                let resumed = locked
                    .analysis_retry_scopes
                    .iter()
                    .filter(|(_, previous)| **previous != scope)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                if !resumed.is_empty() {
                    let mut next = locked.clone();
                    for id in resumed {
                        next.analysis_retry_counts.remove(&id);
                        next.analysis_retry_scopes.remove(&id);
                        next.analysis_retries.remove(&id);
                    }
                    self.save(&next)?;
                    *locked = next;
                }
            }
            let mut pending = crate::context_budget::pending_refs(&locked, mode == AiMode::Manual)
                .collect::<Vec<_>>();
            if after_scan
                && mode == AiMode::Automatic
                && !crate::context_budget::automatic_due(
                    &locked,
                    pending.iter().copied(),
                    now(),
                    self.analysis_last_attempt.load(Ordering::SeqCst),
                )
            {
                return Ok(json!({"skipped":"等待分析间隔或单批体积阈值"}));
            }
            // First attempts make progress before a previously rejected batch
            // spends another call. Each frozen body is attempted once per round.
            pending.sort_by(|a, b| {
                (
                    locked
                        .analysis_retry_counts
                        .get(&a.id)
                        .copied()
                        .unwrap_or(0),
                    a.updated_at,
                    a.time,
                    &a.id,
                )
                    .cmp(&(
                        locked
                            .analysis_retry_counts
                            .get(&b.id)
                            .copied()
                            .unwrap_or(0),
                        b.updated_at,
                        b.time,
                        &b.id,
                    ))
            });
            let boundary = pending
                .iter()
                .map(|m| (m.id.clone(), fingerprint(m)))
                .collect::<std::collections::VecDeque<_>>();
            let mut s = locked.clone();
            crate::context_budget::mark_pending(&mut s, !boundary.is_empty(), now());
            if (boundary.is_empty() && mode == AiMode::Manual)
                || (s.auto_analysis_blocked && !analysis_budget_blocked(&s))
            {
                s.auto_analysis_blocked = false;
                s.auto_analysis_block_reason.clear();
                s.auto_analysis_blocked_at = 0;
                s.auto_analysis_block_fingerprint.clear();
            }
            if (
                s.pending_analysis,
                s.analysis_pending_since,
                s.auto_analysis_blocked,
                &s.auto_analysis_block_reason,
                s.auto_analysis_blocked_at,
            ) != (
                locked.pending_analysis,
                locked.analysis_pending_since,
                locked.auto_analysis_blocked,
                &locked.auto_analysis_block_reason,
                locked.auto_analysis_blocked_at,
            ) {
                self.save(&s)?;
                *locked = s;
            }
            (
                boundary,
                locked
                    .message_filter
                    .early
                    .then_some(locked.message_filter.last_run),
            )
        };
        if boundary.is_empty() {
            return Ok(json!({"skipped":"没有相关变化，不调用AI"}));
        }
        let previous_attempt = self.analysis_last_attempt.swap(now(), Ordering::SeqCst);
        let mut deadline_guard =
            crate::ai_control::DeadlineGuard::new(deadline, call.cancel.clone());
        let mut applied = 0;
        let mut processed = 0;
        let mut batches = 0;
        let result = (|| -> Result<Value, String> {
            self.require_ai_purpose("classify")?;
            // Discover native projects once per round; querying integrations
            // again between batches would add avoidable startup latency.
            let projects = self.analysis_projects();
            let projects = json!(projects
                .as_array()
                .into_iter()
                .flatten()
                .take(60)
                .map(|p| json!({"id":p["id"],"name":p["name"],"cwd":p["cwd"],"driver":p["driver"],"source":p["source"]}))
                .collect::<Vec<_>>());
            let mut snapshots = crate::snapshots::Cache::default();
            while !boundary.is_empty() {
                process::check_running()?;
                if std::time::Instant::now() >= deadline {
                    return Err("分析总时限已到，未提交的消息保留待下一轮处理".into());
                }
                call.next_request()?;
                let Some((count, ids)) = self.analyze_batch(
                    mode,
                    &mut boundary,
                    &projects,
                    &mut snapshots,
                    &call,
                    deadline,
                    invoke,
                )?
                else {
                    break;
                };
                applied += count;
                processed += ids.len();
                batches += 1;
                // Conflicts remain pending for a later round. Removing every
                // attempted id also prevents retry-only batches from spinning.
                let last = ids.last().unwrap();
                while let Some((id, _)) = boundary.pop_front() {
                    if &id == last {
                        break;
                    }
                }
            }
            let remaining =
                crate::context_budget::pending_refs(&self.state.lock().unwrap(), true).count();
            Ok(
                json!({"applied":applied,"processed":processed,"batches":batches,"remaining":remaining}),
            )
        })();
        deadline_guard.finish();
        let result = if call.cancel.load(Ordering::SeqCst)
            && self.state.lock().unwrap().ai_interrupted_at > 0
        {
            // A user interruption owns the stop reason even if the deadline
            // expires while an adapter is returning its cancelled result.
            Err(crate::ai_control::CANCELLED.into())
        } else if result.is_err() && std::time::Instant::now() >= deadline {
            Err("分析总时限已到，未提交的消息保留待下一轮处理".into())
        } else {
            result
        };
        let budget_failure = result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with(ai::INPUT_LIMIT_ERROR));
        let cancelled = result
            .as_ref()
            .err()
            .is_some_and(|e| e == crate::ai_control::CANCELLED);
        // We still own the AI slot, so restoring the original attempt cannot
        // overwrite a concurrent starter. Cancellation does not move its clock.
        self.analysis_last_attempt.store(
            if cancelled { previous_attempt } else { now() },
            Ordering::SeqCst,
        );
        let finish = (|| -> Result<(), String> {
            if !budget_failure {
                let mut state_lock = self.state.lock().unwrap();
                let mut s = state_lock.clone();
                s.analysis_interrupted_at = if cancelled {
                    // A cancellation can precede reservation (for example during
                    // project discovery). Consume only this round's old handoff.
                    s.message_filter.consume(filter_decision_at);
                    // Preserve the prior anchor, including any successful
                    // batches committed before this round was interrupted.
                    if s.ai_interrupted_at > 0 {
                        s.ai_interrupted_at
                    } else {
                        now()
                    }
                } else {
                    s.last_analysis_at = now();
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
                    s.auto_analysis_block_fingerprint = analysis_block_fingerprint(&s);
                    s.pending_analysis = true;
                    self.save(&s)?;
                    *state_lock = s;
                }
            }
            Ok(())
        })();
        let result = if let Err(error) = finish {
            self.log("analysis_finish_failed", json!({"error":error}));
            result.and_then(|_| Err(error))
        } else {
            result
        };
        let result = result.map_err(|error| {
            if batches == 0 {
                error
            } else {
                let detail = format!("已提交 {batches} 批、处理 {processed} 条消息、采用 {applied} 个动作，已提交结果保留，其余消息仍待处理");
                self.log("analysis_partial", json!({"error":error,"detail":detail,"batches":batches,"processed":processed,"applied":applied}));
                // Cancellation has existing exact-match callers. Its partial
                // progress is visible in the log without changing that signal.
                if error == crate::ai_control::CANCELLED {
                    error
                } else {
                    format!("{error}；{detail}")
                }
            }
        });
        if batches == 0
            && result
                .as_ref()
                .err()
                .is_some_and(|e| e == crate::message_filter::AUTOMATION_DISABLED)
        {
            return Ok(json!({"skipped":"自动 AI 已关闭，消息已保存"}));
        }
        result
    }
    fn pause_analysis_budget(&self, authority: &Value, message: &str) -> Result<bool, String> {
        let mut live = self.state.lock().unwrap();
        if analysis_authority(&live) != *authority {
            return Ok(false);
        }
        let mut next = live.clone();
        next.auto_analysis_blocked = true;
        next.auto_analysis_blocked_at = now();
        next.auto_analysis_block_reason = message.into();
        next.auto_analysis_block_fingerprint = analysis_block_fingerprint(&next);
        next.pending_analysis = true;
        self.save(&next)?;
        *live = next;
        Ok(true)
    }

    fn analyze_batch(
        &self,
        mode: AiMode,
        boundary: &mut std::collections::VecDeque<(String, String)>,
        projects: &Value,
        snapshots: &mut crate::snapshots::Cache,
        call: &crate::ai_control::AiCall<'_>,
        deadline: std::time::Instant,
        invoke: &mut dyn FnMut(
            &PreparedAiRequest,
            &crate::ai_control::AiCall<'_>,
        ) -> Result<Value, String>,
    ) -> Result<Option<(usize, Vec<String>)>, String> {
        if !self.ai_allowed(mode) {
            return Err(self.ai_deferred_reason().into());
        }
        process::check_running()?;
        // Capture every preparation input once. Candidate reductions reuse the
        // same config, project catalog, memory, frozen runtime and snapshot rows.
        let (batch, mut request, _message_claim, authority) = loop {
            if call.cancel.load(Ordering::SeqCst) {
                return Err(crate::ai_control::CANCELLED.into());
            }
            if std::time::Instant::now() >= deadline {
                return Err("分析总时限已到，本批未调用 AI".into());
            }
            process::check_running()?;
            let s = self.state.lock().unwrap().clone();
            let authority = analysis_authority(&s);
            if mode == AiMode::Automatic && !s.config.automatic_analysis_enabled() {
                return Err(crate::message_filter::AUTOMATION_DISABLED.into());
            }
            // The round boundary already passed relevance/lookback checks.
            // Do not rescan or clone newly arriving messages on every batch.
            let eligible = |(id, version): &(String, String)| {
                s.messages.get(id).filter(|m| {
                    !m.withdrawn
                        && !crate::message_progress::analyzed(&s, m)
                        && fingerprint(m) == *version
                })
            };
            while boundary
                .front()
                .is_some_and(|entry| eligible(entry).is_none())
            {
                boundary.pop_front();
            }
            if boundary.is_empty() {
                return Ok(None);
            }
            let pending_count = boundary.len();
            let changes = crate::context_budget::analysis_candidates(
                boundary.iter().filter_map(eligible),
                s.config.max_input_chars,
            );
            if changes.is_empty() {
                return Ok(None);
            }
            let trigger = if mode == AiMode::Manual {
                "manual"
            } else if s.config.message_filter_enabled && s.message_filter.early {
                if s.message_filter.trigger == "message_filter_failed" {
                    "message_filter_failed"
                } else {
                    "message_filter"
                }
            } else {
                crate::context_budget::due_reason(&s, &changes, now())
                    .unwrap_or("analysis_interval")
            };
            let mut context = {
                self.capture_prepare_context_cached(
                    &s,
                    "classify",
                    false,
                    projects.clone(),
                    snapshots,
                )
            };
            let mut batch = crate::context_budget::classify(&s, &changes);
            batch.payload["_trigger"] = json!(trigger);
            batch.payload["_filter_decision_at"] =
                json!(s.message_filter.early.then_some(s.message_filter.last_run));
            let (mut batch, mut request) =
                match self.fit_analysis_batch(batch, mode, &mut context, &call.cancel, deadline) {
                    Ok(accepted) => accepted,
                    Err(analysis_batch::FitFailure::MessageTooLarge(error)) => {
                        let _acceptance = self.active_ai.lock().unwrap();
                        if call.cancel.load(Ordering::SeqCst) {
                            return Err(crate::ai_control::CANCELLED.into());
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err("分析总时限已到，本批未调用 AI".into());
                        }
                        let mut live = self.state.lock().unwrap();
                        if analysis_authority(&live) != authority {
                            continue;
                        }
                        let first = &changes[0];
                        let mut next = live.clone();
                        Self::record_analysis_failure(
                            &mut next,
                            &BTreeMap::from([(first.id.clone(), fingerprint(first))]),
                            &error,
                            AnalysisFailure::InputLimit,
                        );
                        self.save(&next)?;
                        *live = next;
                        self.log(
                            "analysis_input_deferred",
                            json!({"id":first.id,"error":error}),
                        );
                        // Only this indivisible row failed; the frozen tail can
                        // still fit. No model or command has run for it.
                        while let Some((id, _)) = boundary.pop_front() {
                            if id == first.id {
                                break;
                            }
                        }
                        continue;
                    }
                    Err(analysis_batch::FitFailure::Stop(error)) => {
                        if mode == AiMode::Automatic && error.starts_with(ai::INPUT_LIMIT_ERROR) {
                            if !self.pause_analysis_budget(&authority, &error)? {
                                continue;
                            }
                        }
                        return Err(error);
                    }
                };
            let changes = &changes[..batch.fingerprints.len()];
            let task_index = batch.payload["task_index"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            let metrics = request.payload["_batch_metrics"].as_object_mut().unwrap();
            metrics.insert("pending".into(), json!(pending_count));
            metrics.insert(
                "remaining".into(),
                json!(pending_count.saturating_sub(changes.len())),
            );
            metrics.insert(
                "message_chars".into(),
                json!(crate::context_budget::message_batch_chars(changes)),
            );
            metrics.insert(
                "omitted_task_ids".into(),
                json!(task_index.iter().take(50).collect::<Vec<_>>()),
            );
            metrics.insert(
                "truncated_messages".into(),
                json!(changes
                    .iter()
                    .filter(|m| m.text.chars().count() > 2000)
                    .count()),
            );
            metrics.insert(
                "truncated_original_chars".into(),
                json!(changes
                    .iter()
                    .filter(|m| m.text.chars().count() > 2000)
                    .map(|m| m.text.chars().count())
                    .sum::<usize>()),
            );
            batch.payload["_batch_metrics"] = request.payload["_batch_metrics"].clone();
            for m in changes.iter().filter(|m| m.text.chars().count() > 2000) {
                self.log(
                    "message_truncated",
                    json!({"id":m.id,"original_chars":m.text.chars().count(),"kept_chars":2000,"reason":"body_limit"}),
                );
            }
            if pending_count > changes.len() {
                let remaining_start = boundary
                    .iter()
                    .position(|(id, _)| id == &changes.last().unwrap().id)
                    .unwrap()
                    + 1;
                self.log(
                    "messages_omitted",
                    json!({
                        "count":pending_count-remaining_start,
                        "ids":boundary.iter().skip(remaining_start).map(|(id,_)|id).take(50).collect::<Vec<_>>(),
                        "reason":"message_budget"
                    }),
                );
            }
            if !task_index.is_empty() {
                let omitted_chars = batch.payload["task_index"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| json_chars(&row))
                    .sum::<usize>();
                self.log(
                    "tasks_omitted",
                    json!({"count":task_index.len(),"chars":omitted_chars,"ids":task_index.iter().take(50).collect::<Vec<_>>(),"reason":"summary_limit"}),
                );
            }
            // Freeze the authoritative view outside the state lock.
            // Post-call validation uses these rows, never the shared
            // snapshot files that another rebuild may replace meanwhile.
            request.frozen = Some(FrozenView::new(
                batch.allowed.clone(),
                batch.versions.clone(),
                &request.records,
            ));
            let live = self.state.lock().unwrap();
            if analysis_authority(&live) != authority
                || batch.fingerprints.iter().any(|(id, expected)| {
                    live.messages.get(id).is_none_or(|m| {
                        m.withdrawn
                            || fingerprint(m) != *expected
                            || crate::message_progress::analyzed(&live, m)
                    })
                })
                || batch.versions.iter().any(|(id, version)| {
                    !live.tasks.iter().any(|t| {
                        t.id == *id
                            && t.updated == *version
                            && !t.archived
                            && t.merged_into.is_empty()
                    })
                })
            {
                // Nothing was sent. Rebuild from the live state, still within
                // the original message boundary and shared total deadline.
                // Edited/new message versions remain for a subsequent round.
                continue;
            }
            let claim = self.claim_message_batch(&batch.fingerprints)?;
            drop(live);
            request.materialize_records();
            break (batch, request, claim, authority);
        };
        let crate::context_budget::Batch {
            payload,
            fingerprints,
            ..
        } = batch;
        if std::time::Instant::now() >= deadline {
            return Err("分析总时限已到，本批未提交".into());
        }
        if call.cancel.load(Ordering::SeqCst) {
            return Err(crate::ai_control::CANCELLED.into());
        }
        request.config.ai_timeout_seconds = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
            .max(1);
        let outcome = (|| -> Result<_, String> {
            let mut result = invoke(&request, call)?;
            let seen = result["_context_seen"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            // The frozen view was captured before invocation. Admit only the ids
            // supplied initially or actually returned through the request's own
            // retrieval tool; never consult the shared snapshot files after a call.
            let frozen = request.frozen.as_ref().unwrap();
            let mut allowed = frozen.initial_messages.clone();
            admit_retrieved(&mut allowed, seen, frozen);
            let mut task_versions = frozen.task_versions.clone();
            task_versions.retain(|id, _| {
                request.initial_tasks.contains(id)
                    || seen.iter().any(|value| value.as_str() == Some(id.as_str()))
            });
            // Match interrupt_ai's lock order: cancellation and acceptance are
            // serialized without releasing the round's reservation between batches.
            let _acceptance = self.active_ai.lock().unwrap();
            if call.cancel.load(Ordering::SeqCst) {
                return Err(crate::ai_control::CANCELLED.into());
            }
            if std::time::Instant::now() >= deadline {
                return Err("分析总时限已到，本批未提交".into());
            }
            let mut locked = self.state.lock().unwrap();
            if analysis_authority(&locked) != authority {
                return Err("分析期间配置或来源已变化，本批结果未采用，消息仍待处理".into());
            }
            // Reject frozen bodies that no longer match the live state before
            // applying anything. The same check is repeated in apply_actions.
            retain_live_frozen(&mut allowed, &mut task_versions, frozen, &locked);
            let mut s = locked.clone();
            let catalog = crate::tags::result_catalog(&s, &result);
            let audit_start = s.audit.len();
            // The shared provider loop already validated the output and handled
            // empty conclusions. Only business acceptance remains here.
            let tool_run_id = result["_tool_run_id"].clone();
            let actions = result["actions"].as_array_mut().unwrap();
            for action in actions.iter_mut() {
                action["_tool_run_id"] = tool_run_id.clone();
                if action.get("tool_call_ids").is_none() {
                    action["tool_call_ids"] = json!([]);
                }
            }
            let (applied, retry) = match Self::apply_actions_detailed(
                &mut s,
                actions,
                &allowed,
                &task_versions,
                &catalog,
            ) {
                Ok(valid) => valid,
                Err(error) => {
                    self.log(
                        "analysis_rejected",
                        json!({"kind":"classify","request_id":call.id,"error":error}),
                    );
                    // The rejected draft is discarded in full. Only a known
                    // command-free result can be queued for a later automatic attempt;
                    // provider/persistence failures never take this path.
                    let commands_ran = analysis_commands_may_have_run(&locked, &call.id);
                    if !commands_ran && std::time::Instant::now() < deadline {
                        let mut rejected = locked.clone();
                        Self::record_analysis_failure(
                            &mut rejected,
                            &fingerprints,
                            &error,
                            AnalysisFailure::InvalidResult,
                        );
                        self.save(&rejected)?;
                        *locked = rejected;
                    }
                    return Err(error);
                }
            };
            Self::stamp_decisions(&mut s, audit_start, &request.decision_context);
            let memory_updated = if let Some(update) = result.get("_memory_update") {
                match Self::update_temporary_memory(&mut s, update) {
                    Ok(()) => true,
                    Err(error) => {
                        self.log(
                            "memory_skipped",
                            json!({"request_id":call.id,"error":error}),
                        );
                        false
                    }
                }
            } else {
                false
            };
            // Proposal evidence must survive the same live-version check as task
            // evidence. Omitted or concurrently edited rows cannot create proposals.
            let mut tag_payload = json!({});
            for key in ["changes", "context"] {
                tag_payload[key] = json!(request.tag_payload[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|row| row["id"]
                        .as_str()
                        .is_some_and(|id| allowed.contains_key(id)))
                    .collect::<Vec<_>>());
            }
            crate::tags::collect(&mut s, &result, &tag_payload, &call.id);
            let ids = payload["changes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            Self::finish_message_batch(&mut s, &fingerprints, &retry);
            if !retry.is_empty() && analysis_commands_may_have_run(&s, &call.id) {
                let conflicted = fingerprints
                    .iter()
                    .filter(|(id, _)| retry.contains(*id))
                    .map(|(id, version)| (id.clone(), version.clone()))
                    .collect();
                Self::record_analysis_failure(
                    &mut s,
                    &conflicted,
                    "部分动作因证据或事项版本变化未采用",
                    AnalysisFailure::CommandUnconfirmed,
                );
            }
            s.last_analysis_at = now();
            s.last_analysis_success_at = s.last_analysis_at;
            s.auto_analysis_blocked = false;
            s.auto_analysis_block_reason.clear();
            s.auto_analysis_blocked_at = 0;
            s.auto_analysis_block_fingerprint.clear();
            let remaining = crate::context_budget::pending_refs(&s, false).count();
            let remaining_total = crate::context_budget::pending_refs(&s, true).count();
            s.pending_analysis = remaining > 0;
            s.analysis_pending_since = if s.pending_analysis { now() } else { 0 };
            if call.cancel.load(Ordering::SeqCst) {
                return Err(crate::ai_control::CANCELLED.into());
            }
            if std::time::Instant::now() >= deadline {
                return Err("分析总时限已到，本批未提交".into());
            }
            self.save(&s)?;
            self.notifications
                .lock()
                .unwrap()
                .push(&s.audit[audit_start..]);
            *locked = s;
            drop(locked);
            self.log("analysis_result",json!({"request_id":call.id,"applied":applied,"processed":payload["changes"].as_array().map(|a|a.len()).unwrap_or(0),"remaining":remaining_total,"memory_updated":memory_updated,"analysis_degraded":result["_analysis_degraded"],"no_action_reason":result["no_action_reason"].as_str().unwrap().trim()}));
            Ok(Some((applied, ids)))
        })();
        if let Err(error) = &outcome {
            let _acceptance = self.active_ai.lock().unwrap();
            let mut live = self.state.lock().unwrap();
            let commands_ran = analysis_commands_may_have_run(&live, &call.id);
            let invalid_response = request.response_validation_failed.load(Ordering::SeqCst)
                && !call.cancel.load(Ordering::SeqCst)
                && std::time::Instant::now() < deadline
                && analysis_authority(&live) == authority;
            if commands_ran || invalid_response {
                let mut next = live.clone();
                Self::record_analysis_failure(
                    &mut next,
                    &fingerprints,
                    error,
                    if commands_ran {
                        AnalysisFailure::CommandUnconfirmed
                    } else {
                        AnalysisFailure::InvalidResult
                    },
                );
                // Only retry metadata is published here, never the rejected
                // business draft. A storage failure must not re-enable an
                // already executed command when the disk becomes writable.
                let saved = self.save(&next);
                if let Err(save_error) = saved {
                    if !commands_ran {
                        return Err(save_error);
                    }
                    *live = next;
                    self.log(
                        "analysis_retry_guard_unsaved",
                        json!({
                            "request_id":call.id,"error":save_error,
                            "detail":"命令防重放状态仅保留在本进程，未能持久化"
                        }),
                    );
                    return Err(format!("{error}；命令重试保护未能保存：{save_error}"));
                }
                *live = next;
            }
        }
        outcome
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
        fingerprints: &BTreeMap<String, String>,
        retry: &BTreeSet<String>,
    ) {
        for (id, fp) in fingerprints {
            if s.messages.get(id).map(fingerprint).as_ref() != Some(fp) {
                continue;
            }
            if retry.contains(id) {
                let count = s.analysis_retry_counts.entry(id.clone()).or_insert(0);
                *count = count.saturating_add(1);
                s.analysis_retries
                    .insert(id.clone(), "动作未采用，等待后续自动分析".into());
                continue;
            }
            s.analysis_retries.remove(id);
            s.analysis_retry_counts.remove(id);
            s.analysis_retry_scopes.remove(id);
            s.analyzed.insert(id.clone(), fp.clone());
        }
        crate::message_progress::advance_analysis(s);
        crate::message_progress::advance_filter(s);
    }

    /// A failed body stays unacknowledged. Local packing failures are retried
    /// when preparation inputs change; invalid final results remain queued for
    /// later attempts, behind messages that have not failed before.
    fn record_analysis_failure(
        state: &mut State,
        fingerprints: &BTreeMap<String, String>,
        error: &str,
        failure: AnalysisFailure,
    ) {
        let scope = matches!(failure, AnalysisFailure::InputLimit)
            .then(|| analysis_block_fingerprint(state));
        for (id, expected) in fingerprints {
            if state.messages.get(id).is_none_or(|message| {
                fingerprint(message) != *expected
                    || crate::message_progress::analyzed(state, message)
            }) {
                continue;
            }
            let count = state.analysis_retry_counts.entry(id.clone()).or_default();
            *count = if matches!(failure, AnalysisFailure::InvalidResult) {
                count.saturating_add(1)
            } else {
                3
            };
            state.analysis_retries.insert(
                id.clone(),
                format!(
                    "{}；{}",
                    crate::context_budget::clip(error, 240),
                    if matches!(failure, AnalysisFailure::CommandUnconfirmed) {
                        crate::context_budget::COMMAND_RETRY_REASON
                    } else if matches!(failure, AnalysisFailure::InputLimit) {
                        "输入超限，调整分析范围或额度后重试，未标为已分析"
                    } else {
                        "本批未提交，等待后续自动分析"
                    }
                ),
            );
            if let Some(scope) = &scope {
                state
                    .analysis_retry_scopes
                    .insert(id.clone(), scope.clone());
            } else {
                state.analysis_retry_scopes.remove(id);
            }
        }
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
        let mut applied_versions = BTreeMap::new();
        for action in actions {
            let action_type = action["action"].as_str().unwrap_or("");
            if action_type == "ignore" {
                continue;
            }
            if !["create", "update"].contains(&action_type) {
                return Err("AI动作类型无效".into());
            }
            if action.get("tags").is_some() {
                return Err(
                    "AI 分析必须使用 tag_changes 明确增删标签，不能整组替换；事项未保存".into(),
                );
            }
            let sources = action["sources"]
                .as_array()
                .ok_or("AI动作缺少来源")?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>();
            if sources.is_empty()
                || sources.iter().any(|id| {
                    s.messages
                        .get(id)
                        .is_none_or(|m| m.withdrawn || allowed.get(id) != Some(m))
                })
            {
                retry.extend(sources.iter().cloned());
                continue;
            }
            // A redirected create still has only create-time tag permissions.
            let create_tags = (action_type == "create")
                .then(|| crate::tags::apply_changes(&[], &action["tag_changes"], catalog, false))
                .transpose()?;
            let (position, deduplicated) = if create_tags.is_some() {
                let position = s.tasks.iter().position(|old| {
                    !old.archived
                        && old.merged_into.is_empty()
                        && old.generation.is_empty()
                        && old
                            .evidence
                            .iter()
                            .any(|id| sources.iter().any(|source| source == id))
                });
                (position, position.map(|pos| s.tasks[pos].id.clone()))
            } else {
                (
                    s.tasks
                        .iter()
                        .position(|t| t.id == action["task_id"].as_str().unwrap_or("")),
                    None,
                )
            };
            let updating = action_type == "update" || deduplicated.is_some();
            let target_id = deduplicated
                .as_deref()
                .or_else(|| action["task_id"].as_str());
            let tid = target_id.unwrap_or("");
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
            let old = position
                .filter(|_| target_id.is_some())
                .map(|p| &s.tasks[p]);
            let mut t = match crate::context_budget::materialize(action, old) {
                Ok(task) => task,
                Err(_) => {
                    retry.extend(sources.iter().cloned());
                    continue;
                }
            };
            t.tags = if updating {
                crate::tags::apply_changes(&t.tags, &action["tag_changes"], catalog, true)?
            } else {
                create_tags.unwrap()
            };
            t.suggested_project_id = action["suggested_project_id"].as_str().unwrap_or("").into();
            t.suggested_project_reason = action["suggested_project_reason"]
                .as_str()
                .unwrap_or("")
                .into();
            let wants_completion = t.status == "已完成"
                && (!updating
                    || action["fields"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|c| c["field"] == "status"));
            let mut summary_allowed = deduplicated.is_none();
            if wants_completion
                && (action["confidence"].as_f64().unwrap_or(0.) < 0.95
                    || completion_proof.is_empty())
            {
                summary_allowed = false;
                t.status = s
                    .tasks
                    .iter()
                    .find(|old| old.id == tid)
                    .map(|old| old.status.clone())
                    .unwrap_or_else(|| "待处理".into());
                retry.extend(sources.iter().cloned());
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
            if !updating {
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
                t.deferred_until = 0;
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
                let Some(pos) = position else {
                    retry.extend(sources.iter().cloned());
                    continue;
                };
                let old = &s.tasks[pos];
                if applied_versions.get(tid).or_else(|| task_versions.get(tid))
                    != Some(&old.updated)
                {
                    retry.extend(sources.iter().cloned());
                    continue;
                }
                let progress =
                    has_new_progress(old, &sources, &s.messages) || !completion_proof.is_empty();
                if !progress {
                    // Evidence-only update: keep every state field and append the
                    // newly observed references. Do not let older evidence roll
                    // status/title/next-step backwards.
                    let mut evidence = old.clone();
                    merge_evidence(&mut evidence, &sources, &background, &completion_proof);
                    crate::tool_calling::link_tasks(s, action, &[tid.to_string()]);
                    if evidence.evidence == s.tasks[pos].evidence {
                        continue;
                    }
                    evidence.updated = now().max(evidence.updated + 1);
                    let before = json!(s.tasks[pos]);
                    Self::audit(
                        &mut s,
                        "ai_evidence",
                        tid,
                        &format!(
                            "追加未展示过的依据：{}",
                            action["reason"].as_str().unwrap_or("")
                        ),
                        before,
                        json!(evidence),
                    );
                    s.tasks[pos] = evidence;
                    applied_versions.insert(tid.to_string(), s.tasks[pos].updated);
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
                let proposed_deferral = t.deferred_until;
                let mut candidate = t;
                candidate.updated = now().max(old.updated + 1);
                candidate.evidence = old.evidence.clone();
                merge_evidence(&mut candidate, &sources, &background, &completion_proof);
                candidate.deferred_until = old.deferred_until;
                candidate.resume_processing = old.resume_processing;
                let changing_deferral = proposed_deferral != old.deferred_until;
                if changing_deferral {
                    let proof = Self::valid_evidence(
                        action["deferral_evidence"].as_array(),
                        allowed,
                        &s.messages,
                    );
                    let last_manual = s
                        .audit
                        .iter()
                        .filter(|a| a.task_id == tid && a.kind.starts_with("manual"))
                        .map(|a| a.time)
                        .max()
                        .unwrap_or(old.created);
                    if action["confidence"].as_f64().unwrap_or(0.) >= 0.9
                        && action["deferral_reason"]
                            .as_str()
                            .is_some_and(|r| !r.trim().is_empty())
                        && proof
                            .iter()
                            .any(|id| allowed.get(id).is_some_and(|m| m.time > last_manual))
                        && (proposed_deferral == 0 || proposed_deferral > now())
                    {
                        candidate.deferred_until = proposed_deferral;
                        candidate.reason = format!(
                            "{}；延后调整：{}",
                            candidate.reason,
                            action["deferral_reason"].as_str().unwrap()
                        );
                    } else {
                        retry.extend(sources.iter().cloned());
                    }
                }
                // A status-only patch must not bypass a manual deferral.
                if candidate.deferred_until > now() && candidate.status == "处理中" {
                    candidate.status = "待处理".into();
                }
                if !["待处理", "处理中"].contains(&candidate.status.as_str()) {
                    candidate.deferred_until = 0;
                }
                if candidate.deferred_until == 0 || candidate.status != "待处理" {
                    candidate.resume_processing = false;
                }
                candidate.locked = false;
                if rescheduled {
                    candidate.deadline_changed = deadline_proof
                        .iter()
                        .filter_map(|id| allowed.get(*id).map(|m| m.time))
                        .max()
                        .unwrap_or(now())
                } else {
                    candidate.due = old.due.clone();
                }
                if candidate.description != old.description || candidate.title != old.title {
                    candidate.content_updated_at = candidate.updated;
                    candidate.summary_updated_at = 0;
                }
                preserve_model_summary(&mut candidate);
                normalize_task(&mut candidate);
                crate::tags::sanitize(&mut candidate, Some(old), catalog);
                let before = json!(old);
                Self::audit_ai_change(&mut s, action, Some(before), &candidate, summary_allowed);
                crate::tool_calling::link_tasks(s, action, &[candidate.id.clone()]);
                s.tasks[pos] = candidate;
                applied_versions.insert(tid.to_string(), s.tasks[pos].updated);
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
        let mut result = Vec::new();
        for value in rows.into_iter().flatten() {
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
        if process::stopping() || self.busy.load(Ordering::SeqCst) {
            return;
        }
        let due = {
            let s = self.state.lock().unwrap();
            s.config.enabled
                && now()
                    - s.last_scan
                        .max(s.last_scan_finished_at)
                        .max(self.last_attempt.load(Ordering::SeqCst))
                    >= s.config.interval_minutes as i64 * 60_000
                && s.chat_sources.iter().any(|source| source.enabled)
        };
        if due {
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
}
/// Compare authority without treating ordinary source cursor progress or new
/// messages as a configuration change. This is small enough to check under
/// the state lock after expensive preparation and again before acceptance.
#[derive(Clone, Copy)]
enum AnalysisFailure {
    InputLimit,
    InvalidResult,
    CommandUnconfirmed,
}

fn analysis_commands_may_have_run(state: &State, request_id: &str) -> bool {
    state.tool_runs.iter().any(|run| {
        (run.id == request_id || run.request_id == request_id)
            && run.calls.iter().any(|call| {
                call.result
                    .as_ref()
                    .is_none_or(|result| result.status != "rejected")
            })
    })
}

fn analysis_authority(s: &State) -> Value {
    json!({
        "config": crate::context_budget::config_authority(&s.config, "classify"),
        "sources": s.chat_sources.iter().map(|source| json!({
            "id":source.id,"plugin":source.plugin_id,"account":source.account_key,
            "enabled":source.enabled,"updated":source.updated,"config":source.config,
        })).collect::<Vec<_>>(),
        "prompt":prompts::current(s, "classify").map(|p| &p.id),
        "rule_version":s.rule_version,
        "rules":s.suggestions.iter().filter(|r| r.active && r.archived_at == 0).map(|r| &r.id).collect::<Vec<_>>(),
        "catalog":crate::tags::groups(s),
        "priority_kind":crate::tags::priority_kind(s),
        "archived_memory_version":s.analysis_memory.archived_version,
        "temporary_memory_version":s.analysis_memory.temporary_version,
    })
}

/// Internal equality fingerprint, not a security token. Ordinary arrivals and
/// source cursor progress cannot repeatedly wake a fixed-input budget failure.
pub(crate) fn analysis_block_fingerprint(s: &State) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    json!({"authority":analysis_authority(s),
        "local_projects":s.handling.local_projects,
        "project_sources":s.config.ai_connections.iter()
            .filter(|connection| matches!(connection.driver.as_str(), "codex" | "dsh")
                && !connection.executable.trim().is_empty())
            .map(|connection| connection.cache_key()).collect::<Vec<_>>(),
        "default_codex":s.config.codex,
    })
    .to_string()
    .hash(&mut hash);
    format!("{:016x}", hash.finish())
}

pub(crate) fn analysis_budget_blocked(s: &State) -> bool {
    s.auto_analysis_blocked && s.auto_analysis_block_fingerprint == analysis_block_fingerprint(s)
}

pub(crate) fn reconcile_withdrawals(s: &mut State) {
    let withdrawn = s
        .messages
        .iter()
        .filter(|(_, m)| m.withdrawn)
        .map(|(id, _)| id.as_str())
        .collect::<BTreeSet<_>>();
    if withdrawn.is_empty() {
        return;
    }
    let affected = s
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            task.evidence
                .iter()
                .any(|id| withdrawn.contains(id))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    for index in affected {
        let t = &mut s.tasks[index];
        for rows in [
            &mut t.evidence.direct,
            &mut t.evidence.background,
            &mut t.evidence.completion,
        ] {
            rows.retain(|id| !withdrawn.contains(id.as_str()));
        }
        if t.evidence.iter().next().is_none() {
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
        from_json::<Vec<crate::tags::Tag>>(value)
            .ok()
            .map(|tags| tags.into_iter().collect::<BTreeSet<_>>())
    };
    let Some(assignments) = tags(&after["tags"]) else {
        return String::new();
    };
    if action["tag_changes"].as_array().is_none_or(|changes| {
        changes.iter().any(|change| {
            let tag = crate::tags::Tag {
                kind: change["kind"].as_str().unwrap_or("").into(),
                value: change["value"].as_str().unwrap_or("").into(),
            };
            match change["action"].as_str() {
                Some("add") => !assignments.contains(&tag),
                Some("remove") => assignments.contains(&tag),
                _ => true,
            }
        })
    }) {
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
        if let Some(message) = frozen.message(id) {
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
            && frozen
                .message(id)
                .zip(live.messages.get(id))
                .is_some_and(|(expected, current)| {
                    expected.text == current.text && expected.withdrawn == current.withdrawn
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
        .filter_map(|id| messages.get(*id))
        .filter(|m| !m.withdrawn)
        .map(|m| m.time)
        .max()
        .unwrap_or(0);
    sources.iter().any(|id| {
        !existing.contains(id.as_str())
            && messages
                .get(id)
                .map(|m| !m.withdrawn && m.time >= latest)
                .unwrap_or(false)
    })
}

/// UI snapshots decorate tasks with display-only fields; they are not part of
/// the persisted schema and must be removed before strict deserialization.
fn task_from_ui(value: &Value) -> Result<Task, String> {
    from_json_ignoring(
        value,
        &[
            "number",
            "evidence_count",
            "change_count",
            "has_tool_calls",
            "effective_priority",
            "effective_tags",
            "priority_rank",
            "status_changed_at",
            "today",
            "deferred",
        ],
    )
}

const EDIT_FIELDS: [&str; 6] = [
    "title",
    "description",
    "status",
    "blocker",
    "waiting_for",
    "due",
];
fn validate_edit(t: &Task) -> Result<(), String> {
    if t.title.trim().is_empty() || t.title.chars().count() > 200 {
        return Err("标题需要1–200字".into());
    }
    if [&t.description, &t.blocker, &t.waiting_for]
        .iter()
        .any(|s| s.chars().count() > 6000)
    {
        return Err("修改字段最多6000字".into());
    }
    if t.tags.len() > 40 || t.tags.iter().any(|tag| !crate::tags::valid(tag)) {
        return Err("事项标签无效或超过40个".into());
    }
    if !["待处理", "处理中", "已完成", "无需处理", "已取消"].contains(&t.status.as_str())
    {
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
    let mut s: Schedule = from_json(&v["schedule"])?;
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
        (!t.archived && ["待处理", "处理中"].contains(&t.status.as_str()))
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
        let mut r: Suggestion = from_json(v)?;
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
