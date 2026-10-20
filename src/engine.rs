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
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

pub struct App {
    pub(crate) handling_client:
        Mutex<BTreeMap<String, std::sync::Arc<dyn crate::ai_backend::AgentSession>>>,
    pub(crate) handling_action: Mutex<()>,
    pub(crate) plugin_access: std::sync::RwLock<()>,
    pub root: PathBuf,
    pub(crate) calendar: Mutex<crate::holiday::CalendarData>,
    pub state: Mutex<State>,
    pub busy: AtomicBool,
    pub ai_busy: AtomicBool,
    pub(crate) active_ai: Mutex<Option<crate::ai_control::ActiveAi>>,
    pub(crate) analyzing_messages: Mutex<BTreeMap<String, String>>,
    pub(crate) cache_prev: Mutex<BTreeMap<String, Vec<(usize, u64)>>>,
    summary_busy: AtomicBool,
    pub(crate) archive_busy: AtomicBool,
    pub(crate) prompt_busy: AtomicBool,
    pub activity: Mutex<Vec<Value>>,
    pub ai_progress: Mutex<String>,
    pub ai_live: Mutex<crate::ai_progress::LiveProgress>,
    pub analysis_last_attempt: std::sync::atomic::AtomicI64,
    pub learning_last_attempt: std::sync::atomic::AtomicI64,
    pub ai_started: std::sync::atomic::AtomicI64,
    pub last_attempt: std::sync::atomic::AtomicI64,
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
        fs::create_dir_all(root.join("data/logs"))
            .map_err(|e| format!("安装目录不可写，请选择普通用户可写目录：{e}"))?;
        fs::create_dir_all(root.join("data/tmp")).map_err(|e| e.to_string())?;
        let mut state = crate::managed_data::load_state(&root)?;
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
        if state.config.codex.is_empty() {
            state.config.codex = ai::detect_cli();
        }
        crate::tags::initialize(&mut state);
        crate::handling::initialize(&mut state);
        if state.config.handling_prompt.is_empty() {
            state.config.handling_prompt = state
                .handling
                .versions
                .last()
                .unwrap()
                .policy
                .prompt
                .clone();
        }
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
            root.join("data/retention-pruned.txt"),
            format!("{}:{}", date(), state.config.message_retention_days.max(1)),
        );
        let daily_log = fs::read_to_string(root.join(format!("data/logs/{}.jsonl", date())))
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
            plugin_access: std::sync::RwLock::new(()),
            handling_client: Mutex::new(BTreeMap::new()),
            handling_action: Mutex::new(()),
            calendar: Mutex::new(crate::calendar_service::load_calendar(&root, now())),
            root,
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
    pub fn persist(&self) -> Result<(), String> {
        let s = self.state.lock().unwrap();
        self.save(&s)
    }
    pub(crate) fn save(&self, s: &State) -> Result<(), String> {
        process::check_running()?;
        let p = self.root.join("data/state.json");
        let tmp = self.root.join("data/state.next.json");
        s.work_scheduler.validate().map_err(|e| e.to_string())?;
        if s.schema_version != STATE_SCHEMA_VERSION {
            return Err("状态 schema_version 与当前程序不一致，未保存".into());
        }
        // Stream straight to the temp file: no intermediate Value or String.
        let file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, s).map_err(|e| e.to_string())?;
        writer
            .into_inner()
            .map_err(|e| e.to_string())?
            .sync_all()
            .map_err(|e| e.to_string())?;
        if p.exists() {
            fs::copy(&p, self.root.join("data/state.backup.json")).map_err(|e| e.to_string())?;
        }
        fs::rename(tmp, p).map_err(|e| e.to_string())?;
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    /// Cheap fingerprint of everything the UI shows. The notifier compares it
    /// once a second and pushes a refresh only when it changes; the minute
    /// bucket covers clock-derived fields. `None` while the state is busy.
    pub fn ui_pulse(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        {
            let s = self.state.try_lock().ok()?;
            (s.revision, s.last_scan, s.tasks.len(), s.messages.len()).hash(&mut h);
        }
        self.saves.load(Ordering::SeqCst).hash(&mut h);
        (now() / 60_000).hash(&mut h);
        for flag in [
            &self.busy,
            &self.ai_busy,
            &self.archive_busy,
            &self.prompt_busy,
        ] {
            flag.load(Ordering::SeqCst).hash(&mut h);
        }
        self.ai_started.load(Ordering::SeqCst).hash(&mut h);
        self.ai_progress.lock().unwrap().hash(&mut h);
        self.ai_live.lock().unwrap().revision.hash(&mut h);
        {
            let activity = self.activity.lock().unwrap();
            (activity.len(), activity.first().map(|a| a["time"].as_i64())).hash(&mut h);
        }
        self.active_ai
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| {
                (
                    a.id.clone(),
                    a.cancel.load(Ordering::SeqCst),
                    a.source.clone(),
                )
            })
            .hash(&mut h);
        self.analyzing_messages.lock().unwrap().len().hash(&mut h);
        Some(h.finish())
    }
    pub fn log(&self, kind: &str, detail: Value) {
        {
            let mut a = self.activity.lock().unwrap();
            a.insert(0, json!({"time":now(),"kind":kind,"detail":detail}));
            a.truncate(60);
        }
        let dir = self.root.join("data/logs");
        let p = dir.join(format!("{}.jsonl", date()));
        if fs::metadata(&p)
            .map(|m| m.len() > 8_000_000)
            .unwrap_or(false)
        {
            let _ = fs::rename(&p, dir.join(format!("{}-{}.jsonl", date(), now())));
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{}", json!({"time":now(),"kind":kind,"detail":detail}));
        }
    }
    pub(crate) fn audit(
        s: &mut State,
        kind: &str,
        task: &str,
        detail: &str,
        before: Value,
        after: Value,
    ) {
        s.audit.push(Audit {
            time: now(),
            kind: kind.into(),
            task_id: task.into(),
            detail: detail.into(),
            before,
            after,
        });
        s.revision += 1;
    }
    /// Full snapshot: every section. Used by tests and internal callers.
    pub fn state_version(&self) -> Value {
        json!(self.ui_pulse())
    }
    pub fn snapshot_page(&self, page: Option<&str>) -> Value {
        let Some(page) = page else {
            return self.snapshot();
        };
        let mut sections = vec!["handling", "evolution", "messages"];
        if page == "reports" {
            sections.push("reports");
        }
        if page == "logs" {
            sections.push("audit");
        }
        let mut value = self.snapshot_sections(Some(&json!(sections)));
        value["_page"] = json!(page);
        value
    }
    pub fn snapshot(&self) -> Value {
        self.snapshot_sections(None)
    }
    /// UI snapshot. The core (tasks, config, status, counts) is always sent;
    /// large page-specific sections are built only when listed in `sections`
    /// (`None` = all): handling, audit, reports, evolution, messages.
    pub fn snapshot_sections(&self, sections: Option<&Value>) -> Value {
        let want = |name: &str| {
            sections
                .and_then(Value::as_array)
                .is_none_or(|list| list.iter().any(|v| v == name))
        };
        let (ai_request_id, ai_cancelling, ai_source) = {
            let active = self.active_ai.lock().unwrap();
            active
                .as_ref()
                .map(|a| {
                    (
                        a.id.clone(),
                        a.cancel.load(Ordering::SeqCst),
                        a.source.clone(),
                    )
                })
                .unwrap_or_default()
        };
        let s = self.state.lock().unwrap().clone();
        let at = now();
        let status_changes = StatusChanges::new(&s.audit);
        let catalog = crate::tags::groups(&s);
        let priority_kind = crate::tags::priority_kind(&s);
        let (waiting, analyzing) = self.message_queue(&s);
        let mut snapshot = json!({"async_ai_requests":true,"messages_revision":[s.messages.len(),s.last_scan,s.revision],"ai_request_id":ai_request_id,"ai_source":ai_source,"ai_cancelling":ai_cancelling,"ai_interrupted_at":s.ai_interrupted_at,"tag_state":crate::tags::snapshot(&s),"disabled_plugins":s.disabled_plugins,"handling_version":s.handling.versions.last().map(|v|&v.id),"handling_proposals":s.handling.proposals.len(),"prompt_state":{"versions":s.prompt_versions.len(),"reviews":s.prompt_reviews.iter().map(|r|json!([r.id,r.status,r.error])).collect::<Vec<_>>()},"chat_sources":s.chat_sources,"prompt_pending":s.prompt_reviews.iter().filter(|r|r.archived_at==0 && r.status=="ready" && prompts::has_result(r)).count(),"work_state":self.work_snapshot(&s,now()).ok(),"morning_jobs":s.morning_jobs,"filter_usage":s.message_filter.summary(),"analysis_retries":s.analysis_retries,"in_work_hours":self.work_gate(&s,"classify",AiMode::Automatic,None,now()),"activity":*self.activity.lock().unwrap(),"ai_started":self.ai_started.load(Ordering::SeqCst),"ai_progress":*self.ai_progress.lock().unwrap(),"ai_live":*self.ai_live.lock().unwrap(),"config":s.config,"schedules":s.schedules.iter().map(|s|{let mut v=json!(s);v["next_at"]=json!(schedule::next_occurrence(s,now()));v}).collect::<Vec<_>>(),"tasks":s.tasks.iter().map(|t|{let mut v=json!(t);v["evidence_count"]=json!(t.evidence.all().iter().filter_map(|id|s.messages.get(id)).filter(|m|!m.withdrawn).count());v["effective_tags"]=json!(crate::tags::effective_tags(t,&catalog,&priority_kind,at));v["priority_rank"]=json!(crate::tags::priority_rank(&crate::tags::effective_tags(t,&catalog,&priority_kind,at),&catalog,&priority_kind));v["status_changed_at"]=json!(status_changes.changed_at(t));v["today"]=json!(status_changes.today(t,at));v["deferred"]=json!(t.deferred_until>at);v}).collect::<Vec<_>>(),"suggestions":s.suggestions,"last_scan":s.last_scan,"last_analysis_success_at":s.last_analysis_success_at,"auto_analysis_blocked":s.auto_analysis_blocked,"auto_analysis_block_reason":s.auto_analysis_block_reason,"archive_attempt":s.archive_attempts.last(),"last_error":s.last_error,"usage":{"day":date(),"calls":if s.usage_day==date(){s.calls}else{0},"input_tokens":if s.usage_day==date(){s.input_tokens}else{0},"output_tokens":if s.usage_day==date(){s.output_tokens}else{0},"cached_input_tokens":if s.usage_day==date(){s.cached_input_tokens}else{0},"cached_reports":if s.usage_day==date(){s.cached_reports}else{0},"unreported_calls":if s.usage_day==date(){s.unreported_calls}else{0}},"retention_digest":s.retention_digest,"busy":self.busy.load(Ordering::SeqCst),"ai_busy":self.ai_busy.load(Ordering::SeqCst),"root":self.root,"rule_version":s.rule_version,"pending":waiting.len(),"analyzing":analyzing});
        snapshot["analysis_retry_counts"] = json!(s.analysis_retry_counts);
        if want("handling") {
            let connection = crate::ai_backend::connection(&s.config.for_kind("handling")).ok();
            snapshot["handling"] = json!(s
                .handling
                .runs
                .iter()
                .map(|r| {
                    let mut value = json!(r);
                    value["restartable"] = json!(crate::handling::can_restart(r));
                    value["current_source"] = json!(connection
                        .as_ref()
                        .is_some_and(|c| crate::handling::source_matches(r, c)));
                    value
                })
                .collect::<Vec<_>>());
        }
        if want("audit") {
            snapshot["audit"] = json!(s
                .audit
                .iter()
                .enumerate()
                .rev()
                .take(150)
                .map(|(i, a)| {
                    let mut v = json!(a);
                    v["index"] = json!(i);
                    v
                })
                .collect::<Vec<_>>());
        }
        if want("evolution") {
            snapshot["evolution"] = json!({"prompts":s.prompt_reviews.iter().filter(|r|r.archived_at==0 && prompts::has_result(r)).map(|r|{let mut v=json!(r);v.as_object_mut().unwrap().remove("evidence");v}).collect::<Vec<_>>(),"handling":s.handling.proposals.iter().filter(|r|r.archived_at==0).collect::<Vec<_>>(),"revision_count":s.advice_revisions.len()});
        }
        if want("reports") {
            snapshot["reports"] = json!(s.reports);
        }
        snapshot["memory_state"] = json!([
            s.analysis_memory.temporary_version,
            s.analysis_memory.archived_version
        ]);
        if want("messages") {
            let mut messages = s
                .messages
                .values()
                .filter(|m| m.relevant())
                .cloned()
                .collect::<Vec<_>>();
            messages.sort_by_key(|m| std::cmp::Reverse(m.time));
            messages.truncate(200);
            snapshot["messages"] = json!(messages);
        }
        snapshot
    }
    pub fn schedule_edit(&self, v: &Value) -> Result<Value, String> {
        // The UI snapshot decorates schedules with a derived `next_at`; it is
        // display-only and not part of the persisted schema.
        let mut value = v["schedule"].clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("next_at");
        }
        let mut schedule: Schedule = serde_json::from_value(value).map_err(|e| e.to_string())?;
        schedule::validate(&mut schedule)?;
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let at = now();
        let before;
        if schedule.id.is_empty() {
            schedule.id = id();
            schedule.created = at;
            schedule.updated = at;
            schedule.active_from = at;
            schedule.last_generated_on.clear();
            before = Value::Null;
            s.schedules.push(schedule.clone());
        } else {
            let old = s
                .schedules
                .iter_mut()
                .find(|x| x.id == schedule.id)
                .ok_or("定时事项不存在")?;
            if old.updated != schedule.updated {
                return Err("定时事项已更新，请重新打开编辑".into());
            }
            before = json!(old);
            schedule.created = old.created;
            schedule.updated = at.max(old.updated + 1);
            schedule.last_generated_on = old.last_generated_on.clone();
            schedule.active_from = if schedule.frequency != old.frequency
                || schedule.weekdays != old.weekdays
                || schedule.time != old.time
                || (schedule.enabled && !old.enabled)
            {
                at
            } else {
                old.active_from
            };
            *old = schedule.clone();
        }
        Self::audit(
            &mut s,
            "schedule_edit",
            "",
            "保存定时事项",
            before,
            json!(schedule),
        );
        self.save(&s)?;
        *locked = s;
        Ok(json!(schedule))
    }
    pub fn schedule_delete(&self, v: &Value) -> Result<Value, String> {
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let pos = s
            .schedules
            .iter()
            .position(|x| Some(x.id.as_str()) == v["id"].as_str())
            .ok_or("定时事项不存在")?;
        if Some(s.schedules[pos].updated) != v["updated"].as_i64() {
            return Err("定时事项已更新，请刷新后重试".into());
        }
        let old = s.schedules.remove(pos);
        Self::audit(
            &mut s,
            "schedule_delete",
            "",
            "删除定时事项，保留已生成的事项",
            json!(old),
            Value::Null,
        );
        self.save(&s)?;
        *locked = s;
        Ok(json!(true))
    }
    pub fn schedule_format(&self, text: &str) -> Result<Value, String> {
        if text.trim().is_empty() || text.chars().count() > 6000 {
            return Err("请输入1–6000字的定时事项描述".into());
        }
        let v = self.ask_ai("schedule", &json!({"user_description":text}))?;
        parse_schedule_draft(v).map(|s| json!(s))
    }
    pub fn generate_scheduled(&self, at: i64) -> Result<usize, String> {
        let mut locked = self.state.lock().unwrap();
        if !locked
            .schedules
            .iter()
            .any(|s| schedule::due_today(s, at).is_some())
        {
            return Ok(0);
        }
        let mut s = locked.clone();
        let mut count = 0;
        let mut changed = false;
        for pos in 0..s.schedules.len() {
            let schedule = s.schedules[pos].clone();
            let Some(due) = schedule::due_today(&schedule, at) else {
                continue;
            };
            let day = stamp(due)[..10].to_string();
            // The durable marker and occurrence identity also guard retries after restart.
            s.schedules[pos].last_generated_on = day.clone();
            changed = true;
            if s.tasks
                .iter()
                .any(|t| t.schedule_id == schedule.id && t.scheduled_for == day)
            {
                continue;
            }
            let task = Task {
                id: id(),
                title: schedule.title,
                description: schedule.description,
                tags: schedule.tags,
                status: "待处理".into(),
                manual: true,
                created: at,
                updated: at,
                schedule_id: schedule.id,
                scheduled_for: day,
                reason: format!("定时生成 · {}（上海时间）", stamp(due)),
                ..Default::default()
            };
            Self::audit(
                &mut s,
                "schedule_generate",
                &task.id,
                "定时生成事项",
                Value::Null,
                json!(task),
            );
            s.tasks.insert(0, task);
            count += 1;
        }
        if changed {
            self.save(&s)?;
            *locked = s;
        }
        Ok(count)
    }
    pub fn scan_control(&self, v: &Value) -> Result<Value, String> {
        let mut locked = self.state.lock().unwrap();
        let mut state = locked.clone();
        let interval = match v.get("interval_minutes") {
            Some(value) => value.as_u64().ok_or("扫描间隔必须是整数分钟")?,
            None => state.config.interval_minutes,
        };
        if !(1..=120).contains(&interval) {
            return Err("扫描间隔应为1–120分钟".into());
        }
        let enabled = v["enabled"].as_bool().unwrap_or(state.config.enabled);
        if enabled && !state.chat_sources.iter().any(|s| s.enabled) {
            return Err("请先在消息来源中配置并启用一个插件".into());
        }
        let before = json!({"enabled":state.config.enabled,"interval_minutes":state.config.interval_minutes});
        state.config.enabled = enabled;
        state.config.interval_minutes = interval;
        let after = json!({"enabled":enabled,"interval_minutes":interval});
        Self::audit(
            &mut state,
            "scan_control",
            "",
            "修改自动扫描",
            before,
            after.clone(),
        );
        self.save(&state)?;
        *locked = state;
        Ok(after)
    }
    pub fn configure(&self, v: Value) -> Result<Value, String> {
        self.configure_candidate(v, None).map(|_| json!(true))
    }
    pub fn configure_patch(&self, patch: &Value) -> Result<Value, String> {
        let fields = patch.as_object().ok_or("设置补丁必须是对象")?;
        let allowed = [
            "ai_timeout_seconds",
            "window_backdrop",
            "retain_ai_traces",
            "model_profiles",
            "message_retention_days",
            "analysis_workdir",
            "interval_minutes",
            "analysis_interval_minutes",
            "analysis_lookback_hours",
            "analysis_related_only",
            "daily_calls",
            "max_input_chars",
            "retrieval_budget_chars",
            "enabled",
            "auto_ai",
            "work_start",
            "work_end",
            "message_filter_enabled",
            "model",
            "reasoning_effort",
            "leaders",
        ];
        if fields.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err("设置补丁包含不可修改字段".into());
        }
        for _ in 0..3 {
            let before = json!(self.state.lock().unwrap().config);
            let mut candidate = before.clone();
            for (key, value) in fields {
                if key == "model_profiles" {
                    let profiles = value.as_object().ok_or("模型用途配置无效")?;
                    for (kind, route) in profiles {
                        if !["analysis", "handling", "review", "filter"].contains(&kind.as_str()) {
                            return Err("模型用途无效".into());
                        }
                        if !candidate["model_profiles"].is_object() {
                            candidate["model_profiles"] = json!({});
                        }
                        candidate["model_profiles"][kind] = route.clone();
                    }
                } else {
                    candidate[key] = value.clone();
                }
            }
            if candidate == before {
                return Ok(json!({"config":before}));
            }
            match self.configure_candidate(candidate, Some(&before)) {
                Ok(config) => return Ok(json!({"config":config})),
                Err(error) if error == "settings_concurrent_change" => continue,
                Err(error) => return Err(error),
            }
        }
        Err("设置正在被其他操作更新，请重试".into())
    }
    fn configure_candidate(&self, v: Value, expected: Option<&Value>) -> Result<Value, String> {
        let c: Config = serde_json::from_value(v).map_err(|e| e.to_string())?;
        ai::analysis_workdir(&self.root, &c)?;
        crate::ai_backend::validate_connections(&c)?;
        if !(5_000..=200_000).contains(&c.max_input_chars) {
            return Err("AI 输入上限应为5000–200000字符".into());
        }
        crate::analysis_context::validate_budget(c.retrieval_budget_chars)?;
        if !(30..=1800).contains(&c.ai_timeout_seconds) {
            return Err("AI 总时限须为 30—1800 秒".into());
        }
        if !(1..=1440).contains(&c.analysis_interval_minutes) {
            return Err("AI 分析间隔应为1–1440分钟".into());
        }
        if !(1..=87600).contains(&c.analysis_lookback_hours) {
            return Err("待分析消息时间范围应为1–87600小时".into());
        }
        if ai::message_budget(&c, "classify") == 0 {
            return Err("总输入上限不足以容纳规则、输出格式和上下文，请提高上限".into());
        }
        crate::ai_backend::validate_connections(&c)?;
        if !(1..=3650).contains(&c.message_retention_days) {
            return Err("消息保留期限应为1–3650天".into());
        }
        work_hours::validate(&c)?;
        if [
            &c.classify_prompt,
            &c.format_prompt,
            &c.learning_prompt,
            &c.report_prompt,
            &c.handling_prompt,
            &c.review_prompt,
            &c.archive_prompt,
        ]
        .iter()
        .any(|p| p.chars().count() > 20000)
        {
            return Err("每套提示词最多20000字".into());
        }

        if !(1..=120).contains(&c.interval_minutes) {
            return Err("扫描间隔应在 1–120 分钟之间".into());
        }
        if !(30..=1800).contains(&c.ai_timeout_seconds) {
            return Err("AI 总时限须为 30—1800 秒".into());
        }
        if !(1..=1440).contains(&c.analysis_interval_minutes) {
            return Err("分析间隔应在 1–1440 分钟之间".into());
        }
        if !(1..=87_600).contains(&c.analysis_lookback_hours) {
            return Err("待分析消息时间范围应为 1–87600 小时".into());
        }
        if c.daily_calls > 500 {
            return Err("每日调用上限不能超过500".into());
        }
        if !(5_000..=200_000).contains(&c.max_input_chars) {
            return Err("单次总输入上限应为 5000–200000 字符".into());
        }
        crate::analysis_context::validate_budget(c.retrieval_budget_chars)?;
        crate::work_state::WorkSchedule::parse(&c.work_start, &c.work_end)
            .map_err(|error| format!("上下班时间无效：{error}"))?;
        let mut locked = self.state.lock().unwrap();
        if expected.is_some_and(|before| *before != json!(locked.config)) {
            return Err("settings_concurrent_change".into());
        }
        let mut s = locked.clone();
        if c.enabled && !s.chat_sources.iter().any(|s| s.enabled) {
            return Err("请先配置并启用消息来源".into());
        }
        crate::evolution::sync_handling_prompt(
            &mut s,
            &prompts::content(&c, "handling"),
            "在系统提示词中编辑处置规则",
        );
        prompts::record_config_changes(&mut s, &c);
        let old = json!(s.config);
        let filter_disabled = s.config.message_filter_enabled && !c.message_filter_enabled;
        let scope_changed = s.config.analysis_related_only != c.analysis_related_only;
        s.config = c;
        if scope_changed {
            let pending = !crate::context_budget::pending(&s).is_empty()
                || !self.analyzing_messages.lock().unwrap().is_empty();
            crate::context_budget::mark_pending(&mut s, pending, now());
        }
        if filter_disabled {
            // A disabled filter must not keep an old "run early" hint alive.
            s.message_filter.early = false;
            s.message_filter.checked.clear();
            s.message_filter.reason.clear();
        }
        // A schedule edit must not leave overtime attributed to a stale
        // business day or a claim that the new schedule no longer supports.
        let schedule =
            crate::work_state::WorkSchedule::parse(&s.config.work_start, &s.config.work_end)
                .map_err(|error| format!("上下班时间无效：{error}"))?;
        s.work_scheduler
            .runtime
            .expire_overtime(now(), &schedule)
            .map_err(|error| error.to_string())?;
        s.work_scheduler
            .runtime
            .observe(now(), &schedule, &self.calendar_for(&s))
            .map_err(|error| error.to_string())?;
        s.work_scheduler.last_schedule = Some(schedule);
        if !self.archive_busy.load(Ordering::SeqCst) {
            crate::messages::prune(&mut s);
        }
        let new = json!(s.config);
        Self::audit(&mut s, "settings", "", "修改设置", old, new);
        self.save(&s)?;
        let config = json!(s.config);
        *locked = s;
        Ok(config)
    }
    pub fn task_edit(&self, v: &Value) -> Result<Value, String> {
        let submitted = &v["task"];
        let mut incoming = task_from_ui(submitted)?;
        incoming.steps.clear(); // Legacy stored steps are preserved below, never edited or newly created.
        incoming.generation.clear();
        incoming.generation_error.clear();
        normalize_task(&mut incoming);
        if incoming.tags.len() > 40 || incoming.tags.iter().any(|tag| !crate::tags::valid(tag)) {
            return Err("事项标签无效".into());
        }
        if !incoming.due.is_empty() && deadline_ms(&incoming.due).is_none() {
            return Err("截止时间需要完整日期和时间".into());
        }
        let mut s = self.state.lock().unwrap();
        let before;
        let t;
        if let Some(pos) = s.tasks.iter().position(|x| x.id == incoming.id) {
            if incoming.updated != s.tasks[pos].updated {
                return Err("事项已被更新，请刷新后重新编辑".into());
            }
            before = json!(s.tasks[pos]);
            let old = s.tasks[pos].clone();
            // A partial/old UI payload must not reset evidence, summaries,
            // archive metadata or scheduling fields that it did not edit.
            let patch = json!(incoming);
            let mut next_value = json!(old);
            for field in EDIT_FIELDS {
                if submitted.get(field).is_some() {
                    next_value[field] = patch[field].clone();
                }
            }
            if submitted.get("tags").is_some_and(Value::is_array) {
                next_value["tags"] = patch["tags"].clone();
            } else {
                let mut tags = old.tags.clone();
                for (field, kind) in [
                    ("priority", "事项等级"),
                    ("category", "事项类型"),
                    ("environment", "适用环境"),
                ] {
                    if submitted.get(field).is_some() {
                        tags.retain(|tag| tag.kind != kind);
                        tags.extend(incoming.tags.iter().filter(|tag| tag.kind == kind).cloned());
                    }
                }
                next_value["tags"] = json!(tags);
            }
            if let Some(follow) = submitted.get("follow_deadline").and_then(Value::as_bool) {
                next_value["follow_deadline"] = json!(follow);
            }
            let mut next: Task = serde_json::from_value(next_value).map_err(|e| e.to_string())?;
            let content_changed = next.title != old.title || next.description != old.description;
            if next.due != old.due {
                next.deadline_changed = now();
            } else {
                next.deadline_changed = old.deadline_changed;
            }
            next.updated = now().max(old.updated + 1);
            next.manual = true;
            next.locked = false;
            if content_changed {
                let updated = next.updated;
                mark_content_changed(&mut next, updated);
            } else {
                next.summary = old.summary.clone();
                next.summary_updated_at = old.summary_updated_at;
                next.summary_fallback = old.summary_fallback;
                next.content_updated_at = old.content_updated_at;
            }
            if next.title.trim().is_empty() {
                return Err("标题不能为空".into());
            }
            next.evidence = old.evidence.clone();
            next.archive_note = old.archive_note.clone();
            next.archived_at = old.archived_at;
            next.schedule_id = old.schedule_id.clone();
            next.scheduled_for = old.scheduled_for.clone();
            next.deferred_until = old.deferred_until;
            normalize_task(&mut next);
            s.tasks[pos] = next.clone();
            t = next;
        } else {
            if incoming.title.trim().is_empty() {
                return Err("标题不能为空".into());
            }
            before = Value::Null;
            incoming.id = id();
            incoming.created = now();
            incoming.updated = incoming.created;
            incoming.deadline_changed = now();
            incoming.manual = true;
            incoming.locked = false;
            incoming.archived = false;
            incoming.archived_at = 0;
            incoming
                .evidence
                .direct
                .retain(|id| s.messages.get(id).map(|m| !m.withdrawn).unwrap_or(false));
            normalize_evidence(&mut incoming);
            let updated = incoming.updated;
            mark_content_changed(&mut incoming, updated);
            s.tasks.push(incoming.clone());
            t = incoming;
        }
        Self::audit(
            &mut s,
            "manual_edit",
            &t.id,
            "人工修改，后续AI需结合修改记录和新证据判断",
            before,
            json!(t),
        );
        self.save(&s)?;
        Ok(json!(t))
    }
    pub fn task_ai_edit(&self, v: &Value) -> Result<Value, String> {
        let text = v["text"].as_str().unwrap_or("").trim();
        if text.is_empty() || text.chars().count() > 6000 {
            return Err("请输入1–6000字的修改说明".into());
        }
        let submitted = task_from_ui(&v["task"])?;
        let original = {
            let s = self.state.lock().unwrap();
            let old = s
                .tasks
                .iter()
                .find(|t| t.id == submitted.id)
                .ok_or("事项不存在，请刷新后重试")?;
            if old.updated != submitted.updated {
                return Err("事项已被更新，请重新打开编辑".into());
            }
            if !old.merged_into.is_empty() || !old.generation.is_empty() {
                return Err("此事项暂不能用一句话修改".into());
            }
            old.clone()
        };
        let mut draft = original.clone();
        let submitted = json!(submitted);
        let mut data = json!(draft);
        for field in EDIT_FIELDS {
            data[field] = submitted[field].clone();
        }
        draft = serde_json::from_value(data).map_err(|e| e.to_string())?;
        validate_edit(&draft)?;
        draft.tags = submitted["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_else(|| original.tags.clone());
        let current = json!(draft);
        let fields = EDIT_FIELDS
            .iter()
            .map(|f| ((*f).to_string(), current[*f].clone()))
            .collect::<serde_json::Map<_, _>>();
        let result = self.ask_ai(
            "edit",
            &json!({"current_task":fields,"current_tags":draft.tags,"user_instruction":text}),
        )?;
        self.complete_ai_edit(&original, draft, text, result)
    }
    fn complete_ai_edit(
        &self,
        original: &Task,
        mut draft: Task,
        text: &str,
        result: Value,
    ) -> Result<Value, String> {
        let changes = result["changes"].as_array().ok_or("AI结果缺少修改内容")?;
        let mut data = json!(draft);
        let mut fields = BTreeSet::new();
        for change in changes {
            let field = change["field"].as_str().ok_or("AI修改字段无效")?;
            let value = change["value"].as_str().ok_or("AI修改内容无效")?;
            if !EDIT_FIELDS.contains(&field) || !fields.insert(field) {
                return Err("AI返回了不允许或重复的修改字段，原事项未改动".into());
            }
            data[field] = json!(value);
        }
        draft = serde_json::from_value(data).map_err(|e| e.to_string())?;
        validate_edit(&draft)?;
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let pos = s
            .tasks
            .iter()
            .position(|t| t.id == original.id)
            .ok_or("事项已不存在")?;
        if json!(s.tasks[pos]) != json!(original) {
            return Err("分析期间事项已有新修改，本次结果未覆盖。请重新打开编辑后再试".into());
        }
        let catalog = crate::tags::result_catalog(&s, &result);
        for change in result["tag_changes"]
            .as_array()
            .into_iter()
            .flatten()
            .take(40)
        {
            if let Ok(tag) = serde_json::from_value::<crate::tags::Tag>(change["tag"].clone()) {
                match change["action"].as_str() {
                    Some("add") if crate::tags::known(&catalog, &tag) => draft.tags.push(tag),
                    Some("remove") => draft.tags.retain(|t| t != &tag),
                    _ => (),
                }
            }
        }
        crate::tags::sanitize(&mut draft, Some(original), &catalog);
        draft.updated = now().max(original.updated + 1);
        if draft.title != original.title || draft.description != original.description {
            let updated = draft.updated;
            mark_content_changed(&mut draft, updated);
        }
        if draft.due != original.due {
            draft.deadline_changed = draft.updated;
        }
        if draft.status == "待处理" && original.status != "待处理" {
            draft.archived = false;
        }
        draft.manual = true;
        draft.locked = false;
        draft.reason = result["reason"]
            .as_str()
            .unwrap_or("根据你的修改说明更新")
            .chars()
            .take(1000)
            .collect();
        s.tasks[pos] = draft.clone();
        Self::audit(
            &mut s,
            "manual_ai_edit",
            &draft.id,
            &format!("一句话修改：{text}"),
            json!(original),
            json!(draft),
        );
        self.save(&s)?;
        *locked = s;
        Ok(json!(draft))
    }
    pub fn task_action(&self, v: &Value) -> Result<Value, String> {
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let tid = v["id"].as_str().unwrap_or("");
        let pos = s
            .tasks
            .iter()
            .position(|t| t.id == tid)
            .ok_or("事项不存在")?;
        let before = json!(s.tasks[pos]);
        let action = v["action"].as_str().unwrap_or("");
        match action {
            "tomorrow" => {
                let t = &mut s.tasks[pos];
                if t.status != "待处理"
                    || t.archived
                    || !t.merged_into.is_empty()
                    || !t.generation.is_empty()
                {
                    return Err("只有未归档、已生成的待处理事项可以移至明日".into());
                }
                t.deferred_until = schedule::tomorrow_start(now());
            }
            "resume_today" => s.tasks[pos].deferred_until = 0,
            "done" => s.tasks[pos].status = "已完成".into(),
            "reopen" => {
                s.tasks[pos].status = "待处理".into();
                s.tasks[pos].archived = false;
                s.tasks[pos].deferred_until = 0;
            }
            "dismiss" => s.tasks[pos].status = "无需处理".into(),
            "unlock" => s.tasks[pos].locked = false,
            _ => return Err("未知操作".into()),
        }
        if action != "unlock" {
            s.tasks[pos].manual = true;
            s.tasks[pos].locked = false
        }
        s.tasks[pos].generation.clear();
        s.tasks[pos].generation_error.clear();
        s.tasks[pos].updated = now().max(s.tasks[pos].updated + 1);
        let after = json!(s.tasks[pos]);
        let detail = match action {
            "tomorrow" => "明日处理",
            "resume_today" => "恢复今日处理",
            _ => action,
        };
        Self::audit(&mut s, "manual_status", tid, detail, before, after);
        self.save(&s)?;
        *locked = s;
        Ok(json!(true))
    }
    pub fn merge(&self, v: &Value) -> Result<Value, String> {
        let ids: Vec<String> =
            serde_json::from_value(v["ids"].clone()).map_err(|e| e.to_string())?;
        if ids.len() != 2 || ids[0] == ids[1] {
            return Err("请选择两个不同事项".into());
        }
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let a = s
            .tasks
            .iter()
            .position(|t| t.id == ids[0])
            .ok_or("主事项不存在")?;
        let b = s
            .tasks
            .iter()
            .position(|t| t.id == ids[1])
            .ok_or("待合并事项不存在")?;
        if s.tasks[a].archived
            || s.tasks[b].archived
            || !s.tasks[a].merged_into.is_empty()
            || !s.tasks[b].merged_into.is_empty()
        {
            return Err("归档或已经合并的事项不能再次合并".into());
        }
        let before = json!([s.tasks[a], s.tasks[b]]);
        let other = s.tasks[b].clone();
        for tag in &other.tags {
            if !s.tasks[a].tags.contains(tag) {
                s.tasks[a].tags.push(tag.clone());
            }
        }
        merge_evidence(
            &mut s.tasks[a],
            &other.evidence.direct,
            &other.evidence.background,
            &other.evidence.completion,
        );
        s.tasks[a].description = format!(
            "{}\n\n合并事项：{}\n{}",
            s.tasks[a].description, other.title, other.description
        );
        s.tasks[a].manual = true;
        s.tasks[a].locked = false;
        s.tasks[a].updated = now();
        normalize_task(&mut s.tasks[a]);
        s.tasks[b].merged_into = ids[0].clone();
        s.tasks[b].updated = now();
        let after = json!([s.tasks[a], s.tasks[b]]);
        Self::audit(&mut s, "merge", &ids[0], "人工合并", before, after);
        self.save(&s)?;
        *locked = s;
        Ok(json!(true))
    }
    pub fn undo(&self, index: usize) -> Result<Value, String> {
        let mut s = self.state.lock().unwrap();
        let a = s.audit.get(index).cloned().ok_or("记录不存在")?;
        if !["merge", "manual_edit", "manual_status", "manual_ai_edit"].contains(&a.kind.as_str()) {
            return Err("此记录不支持撤销".into());
        }
        let olds: Vec<Task> = if a.kind == "merge" {
            serde_json::from_value(a.before.clone()).map_err(|e| e.to_string())?
        } else if a.before.is_null() {
            vec![]
        } else {
            vec![serde_json::from_value(a.before.clone()).map_err(|e| e.to_string())?]
        };
        let expected: Vec<Task> = if a.kind == "merge" {
            serde_json::from_value(a.after.clone()).map_err(|e| e.to_string())?
        } else {
            vec![serde_json::from_value(a.after.clone()).map_err(|e| e.to_string())?]
        };
        for e in &expected {
            if !s
                .tasks
                .iter()
                .any(|t| t.id == e.id && t.updated == e.updated)
            {
                return Err("事项此后已有修改，不能直接撤销，请手动编辑".into());
            }
        }
        if olds.is_empty() {
            if let Some(t) = s.tasks.iter_mut().find(|t| t.id == a.task_id) {
                t.status = "无需处理".into();
                t.updated = now()
            }
        } else {
            for mut old in olds {
                old.updated = now();
                old.manual = true;
                old.locked = false;
                if let Some(t) = s.tasks.iter_mut().find(|t| t.id == old.id) {
                    *t = old
                }
            }
        }
        Self::audit(
            &mut s,
            "undo",
            &a.task_id,
            "撤销人工操作",
            a.after,
            a.before,
        );
        self.save(&s)?;
        Ok(json!(true))
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
    fn reserve_ai_inner(
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
        process::check_running()?;
        let call = self.begin_ai_for(kind)?;
        let context_frozen = payload["_context_frozen"].as_bool() == Some(true);
        let mut context = self.capture_prepare_context(kind, context_frozen)?;
        let request = self.prepare_ai_request_with(kind, payload, mode, &mut context)?;
        self.invoke_prepared_request(&request, mode, &call, true)
    }

    /// Capture every input a request preparation may need under one state lock.
    pub(crate) fn prepare_archive_request(
        &self,
        payload: &Value,
        mode: AiMode,
    ) -> Result<PreparedAiRequest, String> {
        let mut context = self.capture_prepare_context("archive", true)?;
        self.prepare_ai_request_with("archive", payload, mode, &mut context)
    }

    /// Capture every input a request preparation may need under one state lock.
    /// Snapshot rows are materialised here once, not once per candidate.
    fn capture_prepare_context(
        &self,
        kind: &str,
        context_frozen: bool,
    ) -> Result<PrepareContext, String> {
        let projects = if ["classify", "format"].contains(&kind) {
            let projects = self.handling_projects().unwrap_or_else(|_| json!([]));
            json!(projects
                .as_array()
                .into_iter()
                .flatten()
                .take(60)
                .map(|p| json!({"id":p["id"],"name":p["name"],"cwd":p["cwd"]}))
                .collect::<Vec<_>>())
        } else {
            json!([])
        };
        let _files = self.snapshot_files.lock().unwrap();
        let state = self.state.lock().unwrap().clone();
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
        if ["classify", "format"].contains(&kind) {
            enriched["available_projects"] = context.projects.clone();
            enriched["project_selection_policy"] =
                json!(crate::prompt_config::get("project_selection"));
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
        let prepared = ai::prepare(&context.config, kind, &enriched);
        crate::analysis_context::acknowledge_inline_memory(&mut enriched, kind, &prepared.prompt);
        let cache_components = ai::prompt_components(&context.config, kind, &enriched);
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
        self.log("ai_start",json!({"request_id":request_id,"kind":kind,"source":source,"trigger":trigger,"model":config.model,"reasoning_effort":config.reasoning_effort,"messages":payload["changes"].as_array().map(|a|a.len()).unwrap_or(0),"batch":payload["_batch_metrics"],"runtime":request.runtime,"input_chars":ai::public_payload(payload).to_string().chars().count(),"input_sizes":json!({"prompt_chars":prepared.prompt_chars,"schema_chars":prepared.schema_chars,"tool_schema_chars":prepared.tool_chars,"total_input_chars":prepared.total,"max_input_chars":config.max_input_chars}),"cache_diag":cache_diag}));
        let mut observed = Value::Null;
        let mut cache_diagnostics = Value::Null;
        let mut analysis_thread = String::new();
        let mut res = ai::invoke_prepared(
            &config,
            kind,
            payload,
            prepared,
            &self.root,
            &request_id,
            &call.cancel,
            &mut |event| {
                use crate::ai_backend::AnalysisEvent;
                match event {
                    AnalysisEvent::SessionStarted(thread) => analysis_thread = thread,
                    AnalysisEvent::Usage(usage) => observed = usage,
                    AnalysisEvent::CacheDiagnostics(value) => cache_diagnostics = value,
                    AnalysisEvent::Progress(text) => {
                        self.ai_live.lock().unwrap().accept("content", &text, false);
                        *self.ai_progress.lock().unwrap() = text;
                    }
                    AnalysisEvent::Stream { kind, text, append } => {
                        if self
                            .active_ai
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_some_and(|a| a.id == request_id)
                        {
                            self.ai_live.lock().unwrap().accept(&kind, &text, append);
                            *self.ai_progress.lock().unwrap() = if kind == "phase" {
                                text.clone()
                            } else {
                                self.ai_live.lock().unwrap().phase.clone()
                            };
                            if config.retain_ai_traces && kind == "phase" {
                                ai::append_trace_event(
                                    &self.root.join("data/ai-traces").join(&request_id),
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
            let mut s = self.state.lock().unwrap();
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
            if let Err(error) = self.save(&s) {
                res = Err(format!("AI usage persistence failed: {error}"));
            }
        }
        if (finish_call && call.finish()) || call.cancel.load(Ordering::SeqCst) {
            res = Err(crate::ai_control::CANCELLED.into());
        }
        let mut detail = json!({"request_id":request_id,"kind":kind,"source":source,"trigger":trigger,"model":config.model,"reasoning_effort":config.reasoning_effort,"elapsed_ms":now()-start,"usage":usage,"usage_known":known,"batch":payload["_batch_metrics"]});
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
                self.root
                    .join("data/ai-traces")
                    .join(&request_id)
                    .join("cache-diagnostics.json"),
                cache_diagnostics.to_string(),
            );
        }
        detail["wire_chars"] = json!({"before":request.compression_before,"after":ai::public_payload(payload).to_string().chars().count()});
        detail["response_received"] = json!(response_received);
        self.finalize_ai_response(request, &request_id, detail, res.map(|(value, _)| value))
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
            if ["classify", "format"].contains(&kind) {
                let key = if kind == "classify" {
                    "actions"
                } else {
                    "tasks"
                };
                if let Some(rows) = v[key].as_array_mut() {
                    for row in rows {
                        let id = row["suggested_project_id"].as_str().unwrap_or("");
                        let valid = !id.is_empty()
                            && row["suggested_project_reason"]
                                .as_str()
                                .is_some_and(|s| !s.trim().is_empty())
                            && payload["available_projects"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|p| p["id"] == id);
                        if !valid {
                            row["suggested_project_id"] = json!("");
                            row["suggested_project_reason"] = json!("");
                        }
                    }
                }
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
        let mut state = self.state.lock().unwrap();
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
            self.save(&state)?;
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
            &mut state,
            "generation_start",
            &t.id,
            "自然语言新建，后台整理",
            Value::Null,
            json!(t),
        );
        self.save(&state)?;
        Ok(t)
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
        let mut state = self.state.lock().unwrap();
        if process::stopping() {
            return Ok(());
        }
        let Some(pos) = state.tasks.iter().position(|t| {
            t.id == draft.id && t.updated == draft.updated && t.generation == "pending"
        }) else {
            return Ok(());
        };
        let parsed = result.and_then(|value| {
            let catalog = crate::tags::result_catalog(&state, &value);
            let rows = value["tasks"].as_array().ok_or("AI结果缺少事项列表")?;
            if rows.is_empty() || rows.len() > 12 {
                return Err("生成事项数量应在1–12条之间".into());
            }
            let mut tasks = vec![];
            for row in rows {
                // `steps_changed` is an update-action signal, not a Task field.
                let mut row = row.clone();
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
                tasks.push(t)
            }
            Ok(tasks)
        });
        match parsed {
            Ok(tasks) => {
                let count = tasks.len();
                state.tasks.remove(pos);
                for t in tasks.into_iter().rev() {
                    Self::audit(
                        &mut state,
                        "generation_done",
                        &t.id,
                        "自然语言事项已生成",
                        json!(draft),
                        json!(t),
                    );
                    state.tasks.insert(pos, t)
                }
                self.save(&state)?;
                drop(state);
                self.log("generation_result", json!({"count":count}));
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
                    &mut state,
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
                self.save(&state)?;
            }
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
            if !self.state.lock().unwrap().config.auto_ai {
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
                let mut s = self.state.lock().unwrap();
                let pending = crate::context_budget::pending(&s);
                crate::context_budget::mark_pending(&mut s, !pending.is_empty(), now());
                self.save(&s)?;
                crate::context_budget::automatic_due(
                    &s,
                    &pending,
                    now(),
                    self.analysis_last_attempt.load(Ordering::SeqCst),
                )
            };
            if !due {
                return Ok(json!({"skipped":"等待分析间隔或单批体积阈值"}));
            }
        }
        self.analyze(mode)
    }
    pub fn analyze(&self, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        if self.ai_busy.load(Ordering::SeqCst) {
            return Err("AI 正在运行，请稍后重试".into());
        }
        let result = self.analyze_batch(mode);
        let budget_failure = result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with(ai::INPUT_LIMIT_ERROR));
        self.analysis_last_attempt.store(now(), Ordering::SeqCst);
        if !budget_failure {
            let mut s = self.state.lock().unwrap();
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
        }
        result
    }
    fn analyze_batch(&self, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        process::check_running()?;
        {
            let mut s = self.state.lock().unwrap();
            s.pending_analysis = true;
            self.save(&s)?;
        }
        self.analysis_last_attempt.store(now(), Ordering::SeqCst);
        // Capture every preparation input once. Candidate reductions reuse the
        // same config, project catalog, memory, frozen runtime and snapshot rows.
        let projects = self.handling_projects().unwrap_or_else(|_| json!([]));
        let projects = json!(projects
            .as_array()
            .into_iter()
            .flatten()
            .take(60)
            .map(|p| json!({"id":p["id"],"name":p["name"],"cwd":p["cwd"]}))
            .collect::<Vec<_>>());
        let (batch, request, _message_claim) = {
            let mut s = self.state.lock().unwrap();
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
        let actions = result["actions"]
            .as_array()
            .ok_or("AI结果缺少actions数组")?
            .clone();
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
            if wants_completion {
                if action["confidence"].as_f64().unwrap_or(0.) < 0.95 || completion_proof.is_empty()
                {
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
                Self::audit(&mut s, "ai_create", &t.id, &t.reason, Value::Null, json!(t));
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
                Self::audit(
                    &mut s,
                    "ai_update",
                    &candidate.id,
                    &candidate.reason,
                    json!(old),
                    json!(candidate),
                );
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
    pub fn report(&self, label: &str, mode: AiMode) -> Result<Value, String> {
        let mut s = self.state.lock().unwrap();
        let start = s.report_cursor.min(s.audit.len());
        let delta = s.audit[start..].to_vec();
        let changed = delta
            .iter()
            .map(|a| a.task_id.clone())
            .collect::<BTreeSet<_>>();
        let previous=s.reports.first().map(|r|json!({"id":r["id"],"time":r["time"],"label":r["label"],"suggestions":r["suggestions"]}));
        let status_changes = StatusChanges::new(&s.audit);
        let at = now();
        let tasks = s
            .tasks
            .iter()
            .filter(|t| {
                (!t.archived || changed.contains(&t.id) || status_changes.today(t, at))
                    && t.merged_into.is_empty()
                    && t.generation.is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut r = json!({"id":id(),"previous_report":previous,"delta":delta,"suggestions":s.suggestions,"time":now(),"label":label,"date":date(),"data_until":s.last_scan,"pending_analysis":crate::context_budget::pending(&s).len(),"tasks":tasks,"usage":{"calls":s.calls,"input_tokens":s.input_tokens,"output_tokens":s.output_tokens}});
        let prior = s.reports.first().cloned();
        let reuse = prior.as_ref().filter(|old| {
            delta.is_empty()
                && old["tasks"] == r["tasks"]
                && old["suggestions"] == r["suggestions"]
                && old["ai_summary"].is_object()
        });
        if let Some(old) = reuse {
            r["ai_summary"] = old["ai_summary"].clone();
            r["summary_status"] = json!("reused");
        } else {
            r["summary_status"] = json!("pending");
        }
        fs::create_dir_all(self.root.join("data/reports")).map_err(|e| e.to_string())?;
        fs::write(
            self.root.join(format!(
                "data/reports/{}-{}.json",
                date(),
                r["id"].as_str().unwrap()
            )),
            serde_json::to_vec_pretty(&r).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        s.report_cursor = s.audit.len();
        s.reports.insert(0, r.clone());
        s.reports.truncate(60);
        self.save(&s)?;
        drop(s);
        if r["summary_status"] == "pending" {
            return self.summarize_report(r, mode);
        }
        Ok(r)
    }
    fn summarize_report(&self, mut r: Value, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(r);
        }
        if self.summary_busy.swap(true, Ordering::SeqCst) {
            return Ok(r);
        }
        let _summary_guard = Guard(&self.summary_busy);
        let tasks=r["tasks"].as_array().unwrap().iter().map(|t| json!({"id":t["id"],"title":t["title"],"description":t["description"],"status":t["status"],"tags":t["tags"],"due":t["due"],"blocker":t["blocker"],"waiting_for":t["waiting_for"],"archive_note":t["archive_note"]})).collect::<Vec<_>>();
        let prior = {
            let s = self.state.lock().unwrap();
            s.reports
                .iter()
                .find(|x| x["id"] == r["previous_report"]["id"])
                .map(|x| x["ai_summary"].clone())
        };
        let payload = json!({"report_tasks":tasks,"delta":r["delta"],"suggestions":r["suggestions"],"previous_summary":prior,"label":r["label"],"time":stamp(now()),"_trigger":if mode == AiMode::Manual {"manual_report"} else {"scheduled_report"}});
        match self.ask_ai_mode("report", &payload, mode) {
            Ok(v)
                if v["summary"].is_string()
                    && ["completed", "attention", "next_steps"]
                        .iter()
                        .all(|k| v[k].is_array()) =>
            {
                r["ai_summary"] = v;
                r["summary_status"] = json!("done");
                r.as_object_mut().unwrap().remove("summary_error");
            }
            Err(e) if e == work_hours::DEFERRED => return Ok(r),
            Err(e) if e == crate::ai_control::CANCELLED => {
                r["summary_status"] = json!("interrupted");
                r["summary_error"] = json!(e);
            }
            Ok(_) => {
                r["summary_status"] = json!("failed");
                r["summary_error"] = json!("汇总输出格式不正确");
            }
            Err(e) => {
                r["summary_status"] = json!("failed");
                r["summary_error"] = json!(e);
            }
        }
        let mut s = self.state.lock().unwrap();
        if let Some(old) = s.reports.iter_mut().find(|x| x["id"] == r["id"]) {
            *old = r.clone();
        }
        fs::write(
            self.root.join(format!(
                "data/reports/{}-{}.json",
                r["date"].as_str().unwrap_or(""),
                r["id"].as_str().unwrap_or("")
            )),
            serde_json::to_vec_pretty(&r).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        self.save(&s)?;
        if r["summary_error"] == crate::ai_control::CANCELLED {
            return Err(crate::ai_control::CANCELLED.into());
        }
        Ok(r)
    }
    pub fn learn(&self, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        process::check_running()?;
        self.learning_last_attempt.store(now(), Ordering::SeqCst);
        let (payload, cutoff, eligible_ids, history_ids) = {
            let mut s = self.state.lock().unwrap();
            let eligible = s
                .audit
                .iter()
                .enumerate()
                .skip(s.learning_cursor)
                .filter(|(_, a)| learning_evidence(a))
                .take(6)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            if eligible.is_empty() {
                s.pending_learning = false;
                s.learning_cursor = s.audit.len();
                self.save(&s)?;
                return Ok(json!({"skipped":"无新增纠错或归档备注，沿用既有建议"}));
            }
            let cutoff = if eligible.len() == 6 {
                eligible[5] + 1
            } else {
                s.audit.len()
            };
            s.pending_learning = true;
            self.save(&s)?;
            let history_ids = s
                .audit
                .iter()
                .enumerate()
                .take(s.learning_cursor)
                .filter(|(_, a)| {
                    learning_evidence(a)
                        && eligible
                            .iter()
                            .any(|i| s.audit[*i].task_id == a.task_id || s.audit[*i].kind == a.kind)
                })
                .map(|(i, _)| i)
                .rev()
                .take(3)
                .collect::<Vec<_>>();
            let history = history_ids
                .iter()
                .map(
                    |i| json!({"audit_index":i,"event":crate::context_budget::audit(&s.audit[*i])}),
                )
                .collect::<Vec<_>>();
            let audit=eligible.iter().map(|i|{
                let a=&s.audit[*i];let task=s.tasks.iter().find(|t|t.id==a.task_id);
                let messages=task.map(|t|{let all=t.evidence.all(); all.iter().filter_map(|id|s.messages.get(id)).filter(|m|!m.withdrawn).rev().take(1).map(|m|json!({"id":m.id,"sender":m.sender,"mine":m.mine,"text":m.text.chars().take(400).collect::<String>()})).collect::<Vec<_>>()}).unwrap_or_default();
                json!({"audit_index":i,"event":crate::context_budget::audit(a),"messages":messages})
            }).collect::<Vec<_>>();
            let relevant = eligible
                .iter()
                .map(|i| s.audit[*i].task_id.as_str())
                .collect::<BTreeSet<_>>();
            (
                json!({"audit":audit,"history":history,"existing_suggestions":crate::context_budget::suggestions(&s,&json!(audit).to_string()),"current_prompt":prompts::content(&s.config,"classify"),"current_tasks":crate::context_budget::rows(s.tasks.iter().filter(|t|relevant.contains(t.id.as_str())).map(crate::context_budget::task),6,3000)}),
                cutoff,
                eligible,
                history_ids,
            )
        };
        let result = self.ask_ai_mode("learning", &payload, mode)?;
        let rows = result["suggestions"].as_array().ok_or("缺少建议数组")?;
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let (added, updated) = apply_learning(&mut s, rows, &eligible_ids, &history_ids)?;
        s.learning_cursor = cutoff;
        s.pending_learning = s.audit.iter().skip(cutoff).any(learning_evidence);
        self.save(&s)?;
        *locked = s;
        drop(locked);
        self.log(
            "learning_result",
            json!({"added":added,"updated":updated,"processed":eligible_ids.len()}),
        );
        Ok(
            json!({"added":added,"updated":updated,"message":if added+updated==0{"本次无新增建议，既有建议继续保留"}else{"建议已更新，未处理建议继续保留"}}),
        )
    }
    pub fn archive_task(&self, v: &Value) -> Result<Value, String> {
        let tid = v["id"].as_str().ok_or("缺少事项ID")?;
        let note = v["note"].as_str().unwrap_or("").trim();
        if note.chars().count() > 4000 {
            return Err("归档备注最多4000字".into());
        }
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let pos = s
            .tasks
            .iter()
            .position(|t| t.id == tid)
            .ok_or("事项不存在")?;
        if v["updated"].as_i64() != Some(s.tasks[pos].updated) {
            return Err("事项已更新，请重新打开归档窗口".into());
        }
        let status = v["status"]
            .as_str()
            .unwrap_or(&s.tasks[pos].status)
            .to_string();
        if !["已完成", "无需处理", "已取消"].contains(&status.as_str()) {
            return Err("请先选择归档结果".into());
        }
        let before = json!(s.tasks[pos]);
        let t = &mut s.tasks[pos];
        t.status = status;
        t.archived = true;
        t.archived_at = now();
        t.archive_note = note.into();
        t.updated = now();
        t.manual = true;
        t.generation.clear();
        t.generation_error.clear();
        let after = json!(t);
        Self::audit(
            &mut s,
            "manual_archive",
            tid,
            "人工归档并记录说明",
            before,
            after.clone(),
        );
        if !note.is_empty() {
            s.pending_learning = true;
        }
        self.save(&s)?;
        *locked = s;
        Ok(after)
    }
    pub fn rule_toggle(&self, tid: &str) -> Result<Value, String> {
        let mut s = self.state.lock().unwrap();
        let pos = s
            .suggestions
            .iter()
            .position(|r| r.id == tid)
            .ok_or("建议不存在")?;
        let before = json!(s.suggestions[pos]);
        if s.suggestions[pos].archived_at > 0 {
            return Err("此建议已归档，请编辑系统提示词或回滚版本".into());
        }
        s.suggestions[pos].active = !s.suggestions[pos].active;
        s.suggestions[pos].decision = if s.suggestions[pos].active {
            "enabled"
        } else {
            "pending"
        }
        .into();
        s.rule_version += 1;
        let after = json!(s.suggestions[pos]);
        Self::audit(
            &mut s,
            "rule_change",
            "",
            "规则启用/撤销，仅影响后续判断",
            before,
            after,
        );
        self.save(&s)?;
        Ok(json!(true))
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
        if e == crate::ai_control::CANCELLED {
            return;
        }
        let mut s = self.state.lock().unwrap();
        // Repeated identical failures (for example a provider that stays down
        // for hours) are kept in state once instead of appending a new row on
        // every poll. A different failure still updates and logs immediately.
        if s.last_error == e {
            return;
        }
        s.last_error = e.into();
        let _ = self.save(&s);
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_snapshot_roundtrips_through_manual_and_ai_edit_boundaries() {
        let root = std::env::temp_dir().join(format!("shishi-snapshot-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.config.daily_calls = 0;
            s.tasks.push(Task {
                id: "t".into(),
                title: "Synthetic".into(),
                status: "待处理".into(),
                updated: 10,
                ..Default::default()
            });
        }
        let mut task = app.snapshot()["tasks"][0].clone();
        assert_eq!(task["evidence_count"], 0);
        assert!(task_from_ui(&task).is_ok());
        task["unknown_business_field"] = json!(true);
        assert!(task_from_ui(&task).is_err());
        task.as_object_mut()
            .unwrap()
            .remove("unknown_business_field");
        task["title"] = json!("Saved synthetic title");
        let saved = app.task_edit(&json!({"task":task})).unwrap();
        assert_eq!(saved["title"], "Saved synthetic title");
        assert!(saved.get("evidence_count").is_none());
        let snapshot = app.snapshot()["tasks"][0].clone();
        // The zero quota proves the real AI-edit boundary accepted the snapshot
        // without permitting any provider invocation.
        let error = app
            .task_ai_edit(&json!({"task":snapshot,"text":"Update the synthetic description"}))
            .unwrap_err();
        assert!(error.contains("每日AI调用上限"), "{error}");
        assert_eq!(app.state.lock().unwrap().calls, 0);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_fingerprints_preserve_real_pending_edits_and_new_replies_after_reload() {
        let root = std::env::temp_dir().join(format!("shishi-fingerprint-{}", id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            for id in ["same", "edited", "invalid"] {
                let mut m = Message {
                    id: id.into(),
                    text: "Original".into(),
                    time: now() - 86_000_000,
                    ..Default::default()
                };
                let legacy = serde_json::to_string(&m).unwrap();
                s.analyzed.insert(
                    id.into(),
                    if id == "invalid" {
                        "invalid".into()
                    } else {
                        legacy.clone()
                    },
                );
                s.message_filter.checked.insert(id.into(), legacy);
                m.source_name = "Renamed source".into();
                m.group = "Renamed room".into();
                m.reply = "reply metadata".into();
                if id == "edited" {
                    m.text = "Actual edit".into();
                }
                s.messages.insert(id.into(), m);
            }
            s.messages.insert(
                "new-reply".into(),
                Message {
                    id: "new-reply".into(),
                    text: "Reply body".into(),
                    reply: "same".into(),
                    time: now(),
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        let s = app.state.lock().unwrap();
        let pending = crate::context_budget::pending(&s)
            .into_iter()
            .map(|m| m.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            pending,
            BTreeSet::from(["edited".into(), "invalid".into(), "new-reply".into()])
        );
        assert!(!s.message_filter.checked.contains_key("same"));
        assert!(!fingerprint_matches(
            &s.messages["edited"],
            s.message_filter.checked.get("edited")
        ));
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn filter_decision_consumed_at_durable_reservation_not_success() {
        let root = std::env::temp_dir().join(format!("shishi-filter-once-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.enabled = false;
            s.config.auto_ai = true;
            s.config.message_filter_enabled = true;
            s.message_filter.early = true;
            s.message_filter.trigger = "message_filter".into();
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "Synthetic".into(),
                    time: now(),
                    ..Default::default()
                },
            );
            s.config.daily_calls = 0;
        }
        assert!(app
            .reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .is_err());
        assert!(app.state.lock().unwrap().message_filter.early);
        app.state.lock().unwrap().config.daily_calls = 100;
        // Failure to persist a start must neither consume the decision nor invoke AI.
        fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .is_err());
        assert!(app.state.lock().unwrap().message_filter.early);
        fs::remove_dir(app.root.join("data/state.next.json")).unwrap();
        app.reserve_ai_inner(AiMode::Manual, None, "classify", None, Some(0), None)
            .unwrap();
        {
            let s = app.state.lock().unwrap();
            assert!(!s.message_filter.early);
            assert!(s.message_filter.trigger.is_empty());
            assert!(s.analyzed.is_empty());
            assert_eq!(crate::context_budget::pending(&s).len(), 1);
        }
        // No success commit: covers provider failure, bad output and cancellation.
        drop(app);
        let app = App::load(root.clone()).unwrap();
        assert!(!app.state.lock().unwrap().message_filter.early);
        {
            let mut s = app.state.lock().unwrap();
            s.message_filter.early = true;
            s.message_filter.trigger = "message_filter".into();
            App::finish_message_batch(&mut s, BTreeMap::new(), &BTreeSet::new());
            assert!(
                s.message_filter.early,
                "a later decision survives the previous batch commit"
            );
        }
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn full_analysis_reservation_observes_the_whole_batch_without_marking_it_analyzed() {
        let root = std::env::temp_dir().join(format!("shishi-filter-batch-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let mut messages = BTreeMap::new();
        for n in 0..700 {
            let m = Message {
                id: format!("m{n}"),
                text: "Synthetic body".into(),
                time: now(),
                ..Default::default()
            };
            messages.insert(m.id.clone(), m);
        }
        {
            let mut s = app.state.lock().unwrap();
            s.messages = messages.clone();
            s.message_filter.early = true;
            s.message_filter.last_run = 1;
            s.config.daily_calls = 100;
        }
        app.reserve_ai_inner(
            AiMode::Manual,
            None,
            "classify",
            None,
            Some(1),
            Some(&messages),
        )
        .unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.message_filter.checked.len(), 700);
            assert!(s.analyzed.is_empty());
        }
        // A concurrently edited message must still be eligible for a new decision.
        {
            let mut s = app.state.lock().unwrap();
            s.messages.get_mut("m0").unwrap().text = "Real edit".into();
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        let s = app.state.lock().unwrap();
        assert!(!fingerprint_matches(
            &s.messages["m0"],
            s.message_filter.checked.get("m0")
        ));
        assert!(crate::message_progress::filtered(&s, &s.messages["m1"]));
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn evidence_counts_only_live_unique_messages_across_roles() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-evidence-{}", id()))).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.messages.insert(
                "live".into(),
                Message {
                    id: "live".into(),
                    ..Default::default()
                },
            );
            s.messages.insert(
                "withdrawn".into(),
                Message {
                    id: "withdrawn".into(),
                    withdrawn: true,
                    ..Default::default()
                },
            );
            s.tasks.push(Task {
                id: "t".into(),
                evidence: TaskEvidence {
                    direct: vec!["live".into(), "missing".into()],
                    background: vec!["live".into()],
                    completion: vec!["withdrawn".into()],
                },
                ..Default::default()
            });
        }
        let snapshot = app.snapshot();
        assert_eq!(snapshot["tasks"][0]["evidence_count"], 1);
        assert_eq!(app.evidence("t").as_array().unwrap().len(), 1);
    }
    #[test]
    fn background_generation_splits_and_respects_manual_edits() {
        let root = std::env::temp_dir().join(format!("shishi-gen-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let draft = app
            .create_generation(&json!({"text":"新增客户查询需求，并调研缓存优化"}))
            .unwrap();
        assert_eq!(draft.generation, "pending");
        app.complete_generation(&draft,Ok(json!({"tasks":[{"title":"客户查询需求","category":"新需求","environment":"不适用","steps_changed":true,"steps":[{"id":"s1","title":"确认需求","status":"pending","depends_on":[],"waiting_for":"","next_step":"等待回复"}]},{"title":"缓存优化调研","category":"调研","environment":"不适用"}]}))).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.tasks.len(), 2);
            assert!(s.tasks.iter().all(|t| t.generation.is_empty()));
            // Retired default categories fall back to the compact adopted catalog.
            assert!(s.tasks[0].category.is_empty());
            assert!(s.tasks[0].tags.is_empty());
            assert!(s.tasks[0].steps.is_empty());
        }
        let draft2 = app
            .create_generation(&json!({"text":"待修改事项"}))
            .unwrap();
        let mut edited = json!(draft2);
        edited["title"] = json!("人工修改的标题");
        app.task_edit(&json!({"task":edited})).unwrap();
        app.complete_generation(&draft2, Ok(json!({"tasks":[{"title":"不应覆盖"}]})))
            .unwrap();
        assert!(app
            .state
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|t| t.title == "人工修改的标题"));
        let draft3 = app
            .create_generation(&json!({"text":"会失败的请求"}))
            .unwrap();
        app.complete_generation(&draft3, Err("测试错误".into()))
            .unwrap();
        assert!(app
            .state
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|t| t.id == draft3.id && t.generation == "failed"));
        for name in ["state.json", "state.backup.json"] {
            let _ = fs::remove_file(root.join("data").join(name));
        }
        let _ = fs::remove_file(root.join(format!("data/logs/{}.jsonl", date())));
        let _ = fs::remove_dir(root.join("data/logs"));
        let _ = fs::remove_dir(root.join("data/tmp"));
        let _ = fs::remove_dir(root.join("data"));
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn scan_control_validates_and_persists_without_replacing_other_settings() {
        let root = std::env::temp_dir().join(format!("shishi-control-{}", id()));
        let app = App::load(root.clone()).unwrap();
        assert!(app.scan_control(&json!({"interval_minutes":0})).is_err());
        assert!(app.scan_control(&json!({"interval_minutes":1.5})).is_err());
        app.scan_control(&json!({"enabled":false,"interval_minutes":12}))
            .unwrap();
        let reloaded = App::load(root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        assert!(!s.config.enabled);
        assert_eq!(s.config.interval_minutes, 12);
        assert!(s.config.leaders.is_empty());
        drop(s);
        for name in ["state.json", "state.backup.json"] {
            let _ = fs::remove_file(root.join("data").join(name));
        }
        let _ = fs::remove_dir(root.join("data/logs"));
        let _ = fs::remove_dir(root.join("data/tmp"));
        let _ = fs::remove_dir(root.join("data"));
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn manual_edits_are_audited_not_locked_and_persisted() {
        let root = std::env::temp_dir().join(format!("shishi-test-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let task=app.task_edit(&json!({"task":{"title":"测试延期","due":"2026-10-16T17:30:00+08:00","priority":"高"}})).unwrap();
        assert_eq!(task["manual"], true);
        assert_eq!(task["locked"], false);
        let s = app.state.lock().unwrap();
        assert_eq!(s.audit.last().unwrap().kind, "manual_edit");
        assert!(s.audit.last().unwrap().before.is_null());
        drop(s);
        let reloaded = App::load(root.clone()).unwrap();
        assert_eq!(
            reloaded.state.lock().unwrap().tasks[0].due,
            "2026-10-16T17:30:00+08:00"
        );
        for name in ["state.json", "state.backup.json"] {
            let _ = std::fs::remove_file(root.join("data").join(name));
        }
        let _ = std::fs::remove_dir(root.join("data/logs"));
        let _ = std::fs::remove_dir(root.join("data/tmp"));
        let _ = std::fs::remove_dir(root.join("data"));
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn stale_edit_cannot_overwrite_newer_task() {
        let root = std::env::temp_dir().join(format!("shishi-test-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let mut task = app.task_edit(&json!({"task":{"title":"旧版本"}})).unwrap();
        task["updated"] = json!(0);
        assert!(app.task_edit(&json!({"task":task})).is_err());
        for name in ["state.json", "state.backup.json"] {
            let _ = std::fs::remove_file(root.join("data").join(name));
        }
        let _ = std::fs::remove_dir(root.join("data/logs"));
        let _ = std::fs::remove_dir(root.join("data/tmp"));
        let _ = std::fs::remove_dir(root.join("data"));
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn source_withdrawal_cancels_auto_only() {
        let mut s = State::default();
        s.messages.insert(
            "m".into(),
            Message {
                id: "m".into(),
                withdrawn: true,
                ..Default::default()
            },
        );
        s.tasks = vec![
            Task {
                id: "a".into(),
                evidence: TaskEvidence {
                    direct: vec!["m".into()],
                    ..Default::default()
                },
                status: "待处理".into(),
                ..Default::default()
            },
            Task {
                id: "b".into(),
                manual: true,
                evidence: TaskEvidence {
                    direct: vec!["m".into()],
                    ..Default::default()
                },
                status: "待处理".into(),
                ..Default::default()
            },
        ];
        reconcile_withdrawals(&mut s);
        assert_eq!(s.tasks[0].status, "已取消");
        assert_eq!(s.tasks[1].status, "待处理");
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn pending_work_survives_restart() {
        let root = std::env::temp_dir().join(format!("shishi-recovery-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let task = app
            .create_generation(&json!({"text":"准备方案并调研连接池"}))
            .unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.pending_scan = true;
            s.pending_analysis = true;
            s.pending_learning = true;
            app.save(&s).unwrap();
        }
        drop(app);
        let app = App::load(root.clone()).unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.pending_scan && s.pending_analysis && s.pending_learning);
        assert_eq!(s.tasks[0].id, task.id);
        assert_eq!(s.tasks[0].generation, "pending");
        assert!(s.tasks[0].generation_error.is_empty());
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod work_hours_tests {
    use super::*;
    #[test]
    fn automatic_is_gated_off_hours_and_manual_still_uses_the_budget() {
        let root = std::env::temp_dir().join(format!("shishi-refresh-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let mut state = State::default();
        state.config.work_start = (local + chrono::Duration::hours(1))
            .format("%H:%M")
            .to_string();
        state.config.work_end = (local + chrono::Duration::hours(2))
            .format("%H:%M")
            .to_string();
        // Reaching the normal budget guard proves analysis ran without launching a real model.
        state.config.daily_calls = 0;
        state.messages.insert(
            "pending".into(),
            Message {
                id: "pending".into(),
                private: true,
                text: "请检查发布结果".into(),
                time: now(),
                ..Default::default()
            },
        );
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let app = App::load(root.clone()).unwrap();
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert!(app
            .analyze_after_scan(AiMode::Manual)
            .unwrap_err()
            .contains("每日"));
        assert!(app.reserve_ai(AiMode::Manual, None).is_err());
        assert_eq!(app.state.lock().unwrap().calls, 0);
        app.test_force_working();
        assert!(app.ai_allowed(AiMode::Automatic));
        assert!(app
            .analyze_after_scan(AiMode::Manual)
            .unwrap_err()
            .starts_with("已达到每日AI调用上限"));
        app.state.lock().unwrap().config.auto_ai = false;
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "自动 AI 已关闭，消息已保存"
        );
        app.state.lock().unwrap().config.auto_ai = true;
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "等待分析间隔或单批体积阈值"
        );
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.calls, 0);
            assert!(s.pending_analysis);
            assert!(s.analyzed.is_empty());
        }
        app.state.lock().unwrap().messages.clear();
        assert_eq!(
            app.analyze_after_scan(AiMode::Manual).unwrap()["skipped"],
            "没有相关变化，不调用AI"
        );
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn after_hours_defers_automatic_ai_and_preserves_manual_access() {
        let root = std::env::temp_dir().join(format!("shishi-hours-{}", id()));
        fs::create_dir_all(root.join("data")).unwrap();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let config = Config {
            codex: root.join("must-not-launch.exe").to_string_lossy().into(),
            work_start: (local + chrono::Duration::hours(1))
                .format("%H:%M")
                .to_string(),
            work_end: (local + chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string(),
            ..Default::default()
        };
        let mut state = State {
            config,
            pending_analysis: true,
            pending_learning: true,
            ..Default::default()
        };
        state.messages.insert(
            "waiting".into(),
            Message {
                id: "waiting".into(),
                private: true,
                text: "请排查连接异常".into(),
                time: now(),
                ..Default::default()
            },
        );
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let app = App::load(root.clone()).unwrap();
        let draft = app
            .create_generation(&json!({"text":"明天准备发布"}))
            .unwrap();
        assert!(!app.snapshot()["in_work_hours"].as_bool().unwrap());
        assert_eq!(
            app.reserve_ai(AiMode::Automatic, None).err().unwrap(),
            work_hours::DEFERRED
        );
        assert_eq!(
            app.analyze(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert_eq!(
            app.learn(AiMode::Automatic).unwrap()["skipped"],
            work_hours::DEFERRED
        );
        assert_eq!(
            app.report("晚报", AiMode::Automatic).unwrap()["summary_status"],
            "pending"
        );
        assert_eq!(
            app.archive(AiMode::Automatic).unwrap()["skipped"],
            "归档失败或等待下次到点；失败仅手动重试"
        );
        app.generate_background(draft.clone(), AiMode::Automatic);
        app.resume_pending();
        app.tick();
        // A manual reservation is independent of the automatic work schedule.
        assert!(app.ai_allowed(AiMode::Manual));
        drop(app);
        let app = App::load(root.clone()).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.calls, 0);
            assert_eq!(s.unreported_calls, 0);
            assert!(s.pending_analysis && s.pending_learning);
            assert!(s.analyzed.is_empty());
            assert_eq!(s.messages["waiting"].text, "请排查连接异常");
            assert_eq!(
                s.tasks
                    .iter()
                    .find(|t| t.id == draft.id)
                    .unwrap()
                    .generation,
                "pending"
            );
            assert!(s.reports.iter().all(|r| r["summary_status"] == "pending"));
        }
        assert!(!root.join("data/ai-traces").exists());
        // Automatic work still needs a working/overtime state.
        app.test_force_working();
        assert!(app.reserve_ai(AiMode::Manual, None).is_ok());
        assert_eq!(app.state.lock().unwrap().calls, 1);
        drop(app);
        fs::remove_dir_all(root).unwrap();
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
#[cfg(test)]
mod progress_tests {
    use super::*;
    #[test]
    fn progress_must_be_new_and_not_older_than_current_stage() {
        let task = Task {
            evidence: TaskEvidence {
                direct: vec!["current".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut messages = BTreeMap::new();
        for (id, time, withdrawn) in [
            ("current", 200, false),
            ("older", 100, false),
            ("new", 300, false),
            ("same_time", 200, false),
            ("withdrawn", 400, true),
        ] {
            messages.insert(
                id.into(),
                Message {
                    id: id.into(),
                    time,
                    withdrawn,
                    ..Default::default()
                },
            );
        }
        assert!(has_new_progress(&task, &["new".into()], &messages));
        assert!(has_new_progress(&task, &["same_time".into()], &messages));
        for id in ["current", "older", "withdrawn", "missing"] {
            assert!(!has_new_progress(&task, &[id.into()], &messages));
        }
    }
}

/// UI snapshots decorate tasks with display-only fields; they are not part of
/// the persisted schema and must be removed before strict deserialization.
fn task_from_ui(value: &Value) -> Result<Task, String> {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        for key in [
            "evidence_count",
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
#[cfg(test)]
mod today_tests {
    use super::*;
    #[test]
    fn tomorrow_survives_restart_and_returns_at_midnight_without_changing_deadline() {
        let root = std::env::temp_dir().join(format!("shishi-tomorrow-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let task = app
            .task_edit(&json!({"task":{"title":"发布","due":"2026-12-31T17:00:00+08:00"}}))
            .unwrap();
        app.task_action(&json!({"id":task["id"],"action":"tomorrow"}))
            .unwrap();
        let restored = App::load(root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        let t = &s.tasks[0];
        assert_eq!(t.due, task["due"]);
        assert_eq!(t.status, "待处理");
        assert!(!is_today_task(t, &s.audit, t.deferred_until - 1));
        assert!(is_today_task(t, &s.audit, t.deferred_until));
        let last = s.audit.len() - 1;
        drop(s);
        restored.undo(last).unwrap();
        assert_eq!(restored.state.lock().unwrap().tasks[0].deferred_until, 0);
        restored
            .task_action(&json!({"id":task["id"],"action":"tomorrow"}))
            .unwrap();
        restored
            .task_action(&json!({"id":task["id"],"action":"resume_today"}))
            .unwrap();
        assert!(restored.snapshot()["tasks"][0]["today"].as_bool().unwrap());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn today_keeps_completed_even_archived_but_not_yesterdays_edited_completion() {
        let at = 1_792_080_000_000;
        let yesterday = at - 86_400_000;
        let mut t = Task {
            id: "x".into(),
            status: "已完成".into(),
            created: yesterday,
            updated: at,
            archived: true,
            ..Default::default()
        };
        let mut audit = vec![Audit {
            time: yesterday,
            kind: "manual_status".into(),
            task_id: "x".into(),
            detail: String::new(),
            before: json!({"status":"待处理"}),
            after: json!({"status":"已完成"}),
        }];
        assert!(!is_today_task(&t, &audit, at));
        audit[0].time = at;
        assert!(is_today_task(&t, &audit, at));
        t.status = "待处理".into();
        t.archived = false;
        t.updated = yesterday;
        assert!(is_today_task(&t, &[], at));
        t.merged_into = "other".into();
        assert!(!is_today_task(&t, &audit, at));
    }
    #[test]
    fn status_index_picks_the_latest_entry_into_the_current_status() {
        let entry = |time, task: &str, from: &str, to: &str| Audit {
            time,
            kind: "manual_status".into(),
            task_id: task.into(),
            detail: String::new(),
            before: json!({"status":from}),
            after: json!({"status":to}),
        };
        let audit = vec![
            entry(10, "a", "待处理", "已完成"),
            entry(20, "a", "已完成", "待处理"),
            entry(30, "a", "待处理", "已完成"),
            entry(40, "a", "已完成", "已完成"),
            entry(50, "b", "待处理", "已完成"),
        ];
        let index = StatusChanges::new(&audit);
        let task = |id: &str, status: &str| Task {
            id: id.into(),
            status: status.into(),
            updated: 7,
            ..Default::default()
        };
        assert_eq!(
            index.changed_at(&task("a", "已完成")),
            30,
            "no-op entry ignored"
        );
        assert_eq!(index.changed_at(&task("a", "待处理")), 20);
        assert_eq!(index.changed_at(&task("b", "已完成")), 50);
        assert_eq!(
            index.changed_at(&task("c", "已完成")),
            7,
            "falls back to updated"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn page_sections_are_built_only_on_request_and_full_snapshot_keeps_all() {
        let root = std::env::temp_dir().join(format!("shishi-sections-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let heavy = ["handling", "audit", "evolution", "reports", "messages"];
        let full = app.snapshot();
        for key in heavy {
            assert!(full.get(key).is_some(), "full snapshot keeps {key}");
        }
        let core = app.snapshot_sections(Some(&json!([])));
        for key in heavy {
            assert!(core.get(key).is_none(), "core omits {key}");
        }
        assert!(core["tasks"].is_array() && core["config"].is_object());
        let reports = app.snapshot_sections(Some(&json!(["reports"])));
        assert!(reports["reports"].is_array());
        assert!(reports.get("audit").is_none());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn ui_pulse_is_stable_when_idle_and_changes_on_save_or_activity() {
        let root = std::env::temp_dir().join(format!("shishi-pulse-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let idle = app.ui_pulse().unwrap();
        assert_eq!(app.ui_pulse(), Some(idle));
        let state = app.state.lock().unwrap().clone();
        app.save(&state).unwrap();
        let saved = app.ui_pulse().unwrap();
        assert_ne!(saved, idle);
        app.log("scan", json!({"rows":0}));
        assert_ne!(app.ui_pulse().unwrap(), saved);
        let _held = app.state.lock().unwrap();
        assert_eq!(app.ui_pulse(), None, "a busy state skips the tick");
        drop(_held);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    fn fixture() -> (App, PathBuf, i64) {
        let root = std::env::temp_dir().join(format!("shishi-schedule-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let at = deadline_ms("2026-10-15T10:00:00+08:00").unwrap();
        app.state.lock().unwrap().schedules.push(Schedule {
            id: "inspection".into(),
            title: "巡检".into(),
            description: "核对运行状态".into(),
            priority: "普通".into(),
            environment: "未指定".into(),
            category: "日常事务".into(),
            frequency: "daily".into(),
            time: "09:00".into(),
            enabled: true,
            active_from: at - 86_400_000,
            ..Default::default()
        });
        (app, root, at)
    }
    #[test]
    fn generates_once_across_restarts_and_independent_daily_tasks() {
        let (app, root, at) = fixture();
        assert!(!app.state.lock().unwrap().config.enabled);
        app.ai_busy.store(true, Ordering::SeqCst);
        assert_eq!(app.generate_scheduled(at).unwrap(), 1);
        let restored = App::load(root.clone()).unwrap();
        assert_eq!(restored.generate_scheduled(at + 1000).unwrap(), 0);
        let first = restored.snapshot()["tasks"][0].clone();
        restored
            .task_action(&json!({"id":first["id"],"action":"done"}))
            .unwrap();
        assert_eq!(restored.generate_scheduled(at + 86_400_000).unwrap(), 1);
        let s = restored.state.lock().unwrap();
        assert_eq!(s.tasks.len(), 2);
        assert_eq!(s.tasks[0].status, "待处理");
        assert_eq!(s.tasks[1].status, "已完成");
        assert_ne!(s.tasks[0].id, s.tasks[1].id);
        assert_eq!(s.tasks[0].scheduled_for, "2026-10-16");
        assert_eq!(s.calls, 0);
        drop(s);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn missed_days_are_not_backfilled_and_save_failure_is_retryable() {
        let (app, root, at) = fixture();
        fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app.generate_scheduled(at + 5 * 86_400_000).is_err());
        assert!(app.state.lock().unwrap().tasks.is_empty());
        assert!(app.state.lock().unwrap().schedules[0]
            .last_generated_on
            .is_empty());
        fs::remove_dir(root.join("data/state.next.json")).unwrap();
        assert_eq!(app.generate_scheduled(at + 5 * 86_400_000).unwrap(), 1);
        assert_eq!(app.snapshot()["tasks"][0]["scheduled_for"], "2026-10-20");
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn pause_edit_delete_and_stale_edit_preserve_generated_tasks() {
        let (app, root, at) = fixture();
        app.generate_scheduled(at).unwrap();
        let mut rule = app.snapshot()["schedules"][0].clone();
        rule["enabled"] = json!(false);
        let paused = app.schedule_edit(&json!({"schedule":rule})).unwrap();
        assert!(app.schedule_edit(&json!({"schedule":rule})).is_err());
        assert_eq!(app.generate_scheduled(at + 86_400_000).unwrap(), 0);
        assert!(app
            .schedule_delete(&json!({"id":rule["id"],"updated":rule["updated"]}))
            .is_err());
        app.schedule_delete(&json!({"id":paused["id"],"updated":paused["updated"]}))
            .unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.schedules.is_empty());
        assert_eq!(s.tasks.len(), 1);
        drop(s);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn ai_draft_requires_explicit_valid_rule() {
        assert!(parse_schedule_draft(json!({"error":"请补充生成时间","schedule":{}})).is_err());
        assert!(parse_schedule_draft(
            json!({"error":"","schedule":{"title":"发布","frequency":"weekly","time":"15:00"}})
        )
        .is_err());
        let draft = parse_schedule_draft(json!({"error":"","schedule":{"title":"发布","frequency":"weekly","weekdays":[5],"time":"15:00"}})).unwrap();
        assert!(draft.enabled);
        assert!(draft.id.is_empty());
    }
}

#[cfg(test)]
mod ai_edit_tests {
    use super::*;
    fn fixture() -> (App, PathBuf, Task) {
        let root = std::env::temp_dir().join(format!("shishi-edit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let t = Task {
            id: id(),
            title: "确认会议安排".into(),
            description: "核对议程与参加方式".into(),
            category: "问题排查".into(),
            priority: "普通".into(),
            environment: "未指定".into(),
            status: "待处理".into(),
            evidence: TaskEvidence {
                direct: vec!["source-1".into()],
                ..Default::default()
            },
            due: "2026-10-16T17:00:00+08:00".into(),
            created: now() - 1000,
            updated: now(),
            deferred_until: schedule::tomorrow_start(now()),
            schedule_id: "rule".into(),
            scheduled_for: date(),
            ..Default::default()
        };
        app.state.lock().unwrap().tasks.push(t.clone());
        (app, root, t)
    }
    #[test]
    fn updates_only_requested_fields_and_can_undo() {
        let (app, root, old) = fixture();
        let saved = app.complete_ai_edit(&old, old.clone(), "已经修复，现在等测试验证", json!({"reason":"根据人工进展调整阶段","changes":[{"field":"title","value":"确认更新后的会议安排"},{"field":"waiting_for","value":"测试同事"}]})).unwrap();
        let current: Task = serde_json::from_value(saved).unwrap();
        assert_eq!(current.id, old.id);
        assert_eq!(current.evidence, old.evidence);
        assert_eq!(current.created, old.created);
        assert_eq!(current.due, old.due);
        assert_eq!(current.status, old.status);
        assert_eq!(current.deferred_until, old.deferred_until);
        assert_eq!(current.schedule_id, old.schedule_id);
        assert_eq!(current.title, "确认更新后的会议安排");
        let reopened = App::load(root.clone()).unwrap();
        assert_eq!(
            reopened.state.lock().unwrap().tasks[0].title,
            "确认更新后的会议安排"
        );
        reopened.undo(0).unwrap();
        assert_eq!(reopened.state.lock().unwrap().tasks[0].title, old.title);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn rejects_stale_invalid_and_immutable_changes() {
        let (app, root, old) = fixture();
        for result in [
            json!({"changes":[{"field":"id","value":"new"}]}),
            json!({"changes":[{"field":"due","value":"明天"}]}),
            json!({"changes":[{"field":"status","value":"进行中"}]}),
            json!({"changes":[{"field":"title","value":"甲"},{"field":"title","value":"乙"}]}),
        ] {
            assert!(app
                .complete_ai_edit(&old, old.clone(), "修改", result)
                .is_err());
            assert_eq!(json!(app.state.lock().unwrap().tasks[0]), json!(old));
        }
        app.task_action(&json!({"id":old.id,"action":"done"}))
            .unwrap();
        assert!(app
            .complete_ai_edit(
                &old,
                old.clone(),
                "改标题",
                json!({"changes":[{"field":"title","value":"过期结果"}]})
            )
            .is_err());
        assert_eq!(app.state.lock().unwrap().tasks[0].status, "已完成");
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn failed_save_does_not_apply_ai_edit() {
        let (app, root, old) = fixture();
        fs::create_dir(root.join("data/state.next.json")).unwrap();
        assert!(app
            .complete_ai_edit(
                &old,
                old.clone(),
                "改为高优先级",
                json!({"changes":[{"field":"priority","value":"高"}]})
            )
            .is_err());
        assert_eq!(json!(app.state.lock().unwrap().tasks[0]), json!(old));
        let _ = fs::remove_dir_all(root);
    }
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

#[cfg(test)]
mod archive_tests {
    use super::*;
    #[test]
    fn notes_and_suggestions_survive_incremental_reports() {
        let root = std::env::temp_dir().join(format!("shishi-archive-{}", id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.config.daily_calls = 0;
            s.tasks.push(Task {
                id: "x".into(),
                title: "验证修复".into(),
                status: "待处理".into(),
                created: now(),
                updated: 42,
                ..Default::default()
            });
            s.suggestions.push(Suggestion {
                id: "keep".into(),
                title: "之前的建议".into(),
                rule: "保留未处理建议".into(),
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        assert!(app
            .archive_task(
                &json!({"id":"x","updated":41,"status":"已完成","note":"不要只凭收到关闭事项"})
            )
            .is_err());
        app.archive_task(
            &json!({"id":"x","updated":42,"status":"已完成","note":"不要只凭收到关闭事项"}),
        )
        .unwrap();
        let first = app.report("早报", AiMode::Manual).unwrap();
        let second = app.report("晚报", AiMode::Manual).unwrap();
        assert_eq!(first["suggestions"][0]["id"], "keep");
        assert_eq!(second["suggestions"][0]["id"], "keep");
        assert_eq!(second["previous_report"]["id"], first["id"]);
        assert_eq!(first["delta"].as_array().unwrap().len(), 1);
        assert!(second["delta"].as_array().unwrap().is_empty());
        assert_eq!(first["tasks"][0]["archive_note"], "不要只凭收到关闭事项");
        assert_eq!(app.state.lock().unwrap().learning_cursor, 0);
        drop(app);
        let app = App::load(root.clone()).unwrap();
        assert_eq!(
            app.state.lock().unwrap().tasks[0].archive_note,
            "不要只凭收到关闭事项"
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn outgoing_messages_are_candidates_but_withdrawals_are_not() {
        let mut m = Message {
            mine: true,
            text: "请协助核对".into(),
            ..Default::default()
        };
        assert!(m.relevant());
        m.withdrawn = true;
        assert!(!m.relevant());
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn fixed_reduction_order_blocks_automatic_retries_without_cursor_or_analyzed_changes() {
        let root = std::env::temp_dir().join(format!("shishi-budget-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.test_force_working();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let base = Config::default();
        let fixed = base.max_input_chars - ai::message_budget(&base, "classify");
        {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = (local - chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.work_end = (local + chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.max_input_chars = fixed + 300;
            for i in 0..12 {
                let mid = format!("m{i}");
                s.messages.insert(
                    mid.clone(),
                    Message {
                        id: mid.clone(),
                        text: "消息内容".repeat(5),
                        time: now() - i * 1000,
                        private: true,
                        ..Default::default()
                    },
                );
                s.tasks.push(Task {
                    id: format!("t{i}"),
                    title: "需要核对的事项标题".repeat(6),
                    description: "需要核对的内容描述".repeat(12),
                    status: "待处理".into(),
                    environment: "未指定".into(),
                    evidence: TaskEvidence {
                        direct: vec![mid],
                        ..Default::default()
                    },
                    updated: now(),
                    ..Default::default()
                });
            }
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR), "{error}");
        {
            let s = app.state.lock().unwrap();
            assert!(s.auto_analysis_blocked);
            assert!(s
                .auto_analysis_block_reason
                .starts_with(ai::INPUT_LIMIT_ERROR));
            assert_eq!(s.calls, 0);
            assert!(s.analyzed.is_empty());
            assert_eq!(s.archive_ai_cursor, Some(0));
            assert_eq!(s.last_analysis_at, 0);
            assert_eq!(s.last_analysis_success_at, 0);
            assert!(s.pending_analysis);
            assert_eq!(s.messages.len(), 12);
        }
        assert_eq!(
            app.analyze_after_scan(AiMode::Automatic).unwrap()["skipped"],
            "自动分析已暂停：本批仍超过输入上限，请手动精简后重试"
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn zero_message_budget_pauses_automatic_without_consuming_messages() {
        let root = std::env::temp_dir().join(format!("shishi-budget-zero-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.test_force_working();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = (local - chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.work_end = (local + chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.max_input_chars = 1;
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "材料".into(),
                    time: now(),
                    private: true,
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR));
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.archive_ai_cursor, Some(0));
        assert!(s.pending_analysis);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn single_message_that_cannot_fit_pauses_automatic_instead_of_reporting_empty() {
        let root = std::env::temp_dir().join(format!("shishi-budget-first-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.test_force_working();
        let local = chrono::Utc::now() + chrono::Duration::hours(8);
        let base = Config::default();
        let fixed = base.max_input_chars - ai::message_budget(&base, "classify");
        {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = (local - chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.work_end = (local + chrono::Duration::hours(2))
                .format("%H:%M")
                .to_string();
            s.config.max_input_chars = fixed + 10;
            s.messages.insert(
                "m".into(),
                Message {
                    id: "m".into(),
                    text: "很长".repeat(1_000),
                    time: now(),
                    private: true,
                    ..Default::default()
                },
            );
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.contains("单条消息"), "{error}");
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.archive_ai_cursor, Some(0));
        assert!(s.pending_analysis);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod evidence_identity_tests {
    use super::*;
    #[test]
    fn manual_feedback_keeps_the_frozen_versions_of_the_ai_decision() {
        let root = std::env::temp_dir().join(format!("shishi-decision-context-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let context = json!({"prompt_version":"p1","rule_version":2,"rule_ids":["r1"],"archived_memory_version":3});
        {
            let mut s = app.state.lock().unwrap();
            let task = Task {
                id: "t".into(),
                title: "待核对".into(),
                updated: 10,
                ..Default::default()
            };
            App::audit(
                &mut s,
                "ai_create",
                "t",
                "合成判断",
                Value::Null,
                json!(task),
            );
            s.tasks.push(task);
            App::stamp_decisions(&mut s, 0, &context);
            s.rule_version = 9; // Later adoption must not rewrite past provenance.
            app.save(&s).unwrap();
        }
        app.task_edit(&json!({"task":{"id":"t","updated":10,"title":"人工修正"}}))
            .unwrap();
        let s = app.state.lock().unwrap();
        let feedback = s.audit.last().unwrap();
        assert_eq!(feedback.before["decision_context"], context);
        assert_eq!(
            crate::context_budget::audit(feedback)["prior_decision"],
            context
        );
        assert_eq!(s.tasks[0].decision_context, context);
        drop(s);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejected_updates_stay_unanalyzed_and_manual_retry_survives_the_cap() {
        let at = now();
        let mut s = State::default();
        for (id, time) in [("a", at), ("b", at + 1), ("c", at + 2)] {
            s.messages
                .insert(id.into(), message(id, time, "请核对结果"));
        }
        s.tasks.push(Task {
            id: "t".into(),
            title: "核对".into(),
            updated: 10,
            ..Default::default()
        });
        s.tasks.push(Task {
            id: "conflict".into(),
            title: "人工已改".into(),
            updated: 30,
            ..Default::default()
        });
        let allowed = s.messages.clone();
        let action = |tid: &str, source: &str, title: &str| json!({"action":"update","task_id":tid,"sources":[source],"fields":[{"field":"title","value":title}],"reason":"新进展","confidence":0.99,"tags":[]});
        let actions = vec![
            action("t", "a", "第一步"),
            action("t", "b", "第二步"),
            action("conflict", "c", "过期修改"),
        ];
        let (applied, retry) = App::apply_actions_detailed(
            &mut s,
            &actions,
            &allowed,
            &BTreeMap::from([("t".into(), 10), ("conflict".into(), 20)]),
            &catalog(),
        )
        .unwrap();
        assert_eq!(applied, 2);
        assert_eq!(s.tasks[0].title, "第二步");
        assert_eq!(s.tasks[1].title, "人工已改");
        assert_eq!(retry, BTreeSet::from(["c".into()]));
        let fingerprints = allowed
            .iter()
            .map(|(id, m)| (id.clone(), fingerprint(m)))
            .collect::<BTreeMap<_, _>>();
        for _ in 0..3 {
            App::finish_message_batch(&mut s, fingerprints.clone(), &retry);
        }
        assert!(s.analyzed.contains_key("a") && s.analyzed.contains_key("b"));
        assert!(!s.analyzed.contains_key("c"));
        assert!(crate::context_budget::pending(&s).is_empty());
        assert_eq!(crate::context_budget::pending_for(&s, true).len(), 1);
        App::finish_message_batch(&mut s, fingerprints, &BTreeSet::new());
        assert!(s.analyzed.contains_key("c"));
        assert!(s.analysis_retry_counts.is_empty());
    }

    #[test]
    fn duplicate_create_cannot_bypass_manual_completion_and_step_guards() {
        let mut s = State::default();
        for (id, time) in [("old", 10), ("new", 200)] {
            s.messages.insert(id.into(), message(id, time, "核对发布"));
        }
        let confirmed = TaskStep {
            id: "confirmed".into(),
            title: "人工确认".into(),
            status: "completed".into(),
            confirmed: true,
            ..Default::default()
        };
        s.tasks.push(Task {
            id: "t".into(),
            title: "人工标题".into(),
            status: "待处理".into(),
            created: 20,
            updated: 100,
            manual: true,
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            steps: vec![confirmed.clone()],
            ..Default::default()
        });
        let allowed = s.messages.clone();
        let mut action = json!({"action":"create","sources":["old","new"],"completion_evidence":["new"],"fields":[{"field":"title","value":"模型标题"},{"field":"status","value":"已完成"}],"reason":"回复证据","confidence":0.8,"tags":[],"steps_changed":true,"steps":[{"id":"next","title":"下一步","status":"pending","depends_on":["confirmed"],"waiting_for":"","next_step":""}]});
        let versions = BTreeMap::from([("t".into(), 100)]);
        let (applied, retry) =
            App::apply_actions_detailed(&mut s, &[action.clone()], &allowed, &versions, &catalog())
                .unwrap();
        assert_eq!(applied, 0);
        assert!(retry.contains("new"));
        assert_eq!(s.tasks[0].title, "人工标题");
        action["confidence"] = json!(0.92);
        let (_, retry) =
            App::apply_actions_detailed(&mut s, &[action.clone()], &allowed, &versions, &catalog())
                .unwrap();
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.tasks[0].status, "待处理");
        assert!(retry.contains("new"));
        assert!(
            s.tasks[0]
                .steps
                .iter()
                .find(|x| x.id == "confirmed")
                .unwrap()
                == &confirmed
        );
        assert!(!s.tasks[0].steps.iter().any(|x| x.id == "next"));
        action["confidence"] = json!(0.99);
        let versions = BTreeMap::from([("t".into(), s.tasks[0].updated)]);
        let (_, retry) =
            App::apply_actions_detailed(&mut s, &[action], &allowed, &versions, &catalog())
                .unwrap();
        assert!(retry.is_empty());
        assert_eq!(s.tasks[0].status, "已完成");
    }
    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }
    fn catalog() -> Vec<crate::tags::Group> {
        vec![
            crate::tags::Group {
                display_priority: None,
                name: "事项类型".into(),
                values: vec!["日常事务".into(), "问题排查".into()],
            },
            crate::tags::Group {
                display_priority: None,
                name: "适用环境".into(),
                values: vec!["未指定".into(), "不适用".into()],
            },
        ]
    }
    #[test]
    fn unarchived_completed_task_is_matched_by_identity_and_receives_evidence() {
        let mut s = State::default();
        let old = Task {
            id: "old".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "已完成".into(),
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["old-msg".into()],
                ..Default::default()
            },
            updated: 100,
            created: 90,
            manual: true,
            ..Default::default()
        };
        s.messages
            .insert("old-msg".into(), message("old-msg", 50, "旧依据"));
        s.messages.insert(
            "new-msg".into(),
            message("new-msg", 200, "再次核对发布结果"),
        );
        s.messages.insert(
            "archived-msg".into(),
            message("archived-msg", 200, "归档依据"),
        );
        s.tasks.push(old);
        s.tasks.push(Task {
            id: "archived".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "已完成".into(),
            archived: true,
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["archived-msg".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let allowed = ["old-msg", "new-msg", "archived-msg"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let action = json!({
            "action":"create",
            "sources":["new-msg","old-msg"],
            "fields":[
                {"field":"title","value":"发布结果再次确认"},
                {"field":"description","value":"核对发布结果"}
            ],
            "reason":"延续发布验证",
            "confidence":0.9,
            "tags":[]
        });
        let applied = App::apply_actions(
            &mut s,
            &[action],
            &allowed,
            &BTreeMap::from([("old".into(), 100)]),
            &catalog(),
        )
        .unwrap();
        assert_eq!(applied, 1);
        let unarchived = s.tasks.iter().find(|t| t.id == "old").unwrap();
        assert_eq!(unarchived.status, "已完成");
        assert!(unarchived.evidence.direct.contains(&"new-msg".into()));
        assert_eq!(s.tasks.len(), 2);
        // The archived task is intentionally not used as a dedup identity: a
        // new create that matches the archived title would not be merged into it.
        let archived = s.tasks.iter().find(|t| t.id == "archived").unwrap();
        assert!(archived.archived);
    }
    #[test]
    fn direct_background_and_completion_evidence_stay_distinct_and_old_evidence_is_appended() {
        let mut s = State::default();
        s.messages
            .insert("old".into(), message("old", 300, "当前依据"));
        s.messages
            .insert("new".into(), message("new", 400, "新回复"));
        s.messages.insert("bg".into(), message("bg", 100, "背景"));
        s.messages
            .insert("done".into(), message("done", 500, "已完成"));
        s.messages
            .insert("older".into(), message("older", 50, "更早补充"));
        s.tasks.push(Task {
            id: "t".into(),
            title: "发布验证".into(),
            description: "核对发布结果".into(),
            status: "待处理".into(),
            environment: "未指定".into(),
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            updated: 200,
            ..Default::default()
        });
        let allowed = ["old", "new", "bg", "done", "older"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let versions = BTreeMap::from([("t".to_string(), 200i64)]);
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["new"],"background_evidence":["bg"],"completion_evidence":["done"],
            "fields":[{"field":"status","value":"已完成"}],
            "reason":"明确完成","confidence":0.99,"tags":[]
        });
        App::apply_actions(&mut s, &[action], &allowed, &versions, &catalog()).unwrap();
        let task = s.tasks.iter().find(|t| t.id == "t").unwrap();
        assert_eq!(task.status, "已完成");
        assert_eq!(task.evidence.direct, vec!["new", "old"]);
        assert_eq!(task.evidence.background, vec!["bg"]);
        assert_eq!(task.evidence.completion, vec!["done"]);
        assert!(task.evidence.all().contains("done"));
        // An older message is appended as evidence without rolling the state back.
        let versions = BTreeMap::from([("t".to_string(), task.updated)]);
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["older"],
            "fields":[{"field":"title","value":"不应回退的标题"}],
            "reason":"补充更早证据","confidence":0.99,"tags":[]
        });
        App::apply_actions(&mut s, &[action], &allowed, &versions, &catalog()).unwrap();
        let task = s.tasks.iter().find(|t| t.id == "t").unwrap();
        assert_eq!(task.title, "发布验证");
        assert!(task.evidence.all().contains("older"));
        assert!(task.evidence.direct.contains(&"older".into()));
        assert_eq!(task.status, "已完成");
    }
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

#[cfg(test)]
mod correction_tests {
    use super::*;

    #[test]
    fn temporary_memory_is_writable_only_in_message_analysis_requests() {
        let (app, root) = app();
        for kind in ["classify", "report", "archive", "learning", "prompt_review"] {
            let state = app.state.lock().unwrap().clone();
            let mut context = app
                .capture_prepare_context_from(&state, kind, false, json!([]))
                .unwrap();
            let request = app
                .prepare_ai_request_with(kind, &json!({}), AiMode::Manual, &mut context)
                .unwrap();
            assert_eq!(request.payload["memory"]["writable"], kind == "classify");
            assert_eq!(
                request.payload["_local_context"]["memory"]["writable"],
                kind == "classify"
            );
        }
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_memory_write_and_inline_continuation_survive_preparation_and_reload() {
        let (mut app, root) = app();
        for (index, text) in ["合成记录：等待核对", "合成记录：已核对，等待确认"]
            .iter()
            .enumerate()
        {
            let state = app.state.lock().unwrap().clone();
            let mut context = app
                .capture_prepare_context_from(&state, "classify", false, json!([]))
                .unwrap();
            let request = app
                .prepare_ai_request_with(
                    "classify",
                    &json!({"changes":[]}),
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            let snapshot = &request.payload["_local_context"];
            assert_eq!(
                snapshot["memory"]["before"],
                state.analysis_memory.temporary
            );
            let meter = crate::analysis_context::Meter::default();
            if index > 0 {
                assert_eq!(
                    snapshot["memory"]["inline_text"],
                    state.analysis_memory.temporary
                );
                assert!(request
                    .prepared
                    .prompt
                    .contains(&state.analysis_memory.temporary));
            }
            let update = crate::analysis_context::memory_update(
                snapshot,
                &meter,
                &json!({"memory_update":[text]}),
            )
            .unwrap()
            .unwrap();
            app.save_temporary_memory(&update).unwrap();
            assert!(
                app.save_temporary_memory(&update).is_err(),
                "stale baseline cannot overwrite newer memory"
            );
            drop(app);
            app = App::load(root.clone()).unwrap();
            let state = app.state.lock().unwrap();
            assert_eq!(state.analysis_memory.temporary, *text);
            assert_eq!(state.analysis_memory.temporary_version, (index + 1) as u64);
        }
        assert!(app
            .save_temporary_memory(&json!({"text":"invalid"}))
            .is_err());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handling_learning_prepares_only_scoped_evidence_without_unavailable_retrieval() {
        let (app, root) = app();
        let state = app.state.lock().unwrap().clone();
        let mut context = app
            .capture_prepare_context_from(&state, "handling_learning", false, json!([]))
            .unwrap();
        let request = app
            .prepare_ai_request_with(
                "handling_learning",
                &json!({"current":{"prompt":"合成策略","routes":[]},"evidence":[]}),
                AiMode::Manual,
                &mut context,
            )
            .unwrap();
        assert!(request.payload.get("_local_context").is_none());
        assert!(request.prepared.tools.is_none());
        assert!(request.prepared.prompt.contains("只读补查：不可用"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn app() -> (App, PathBuf) {
        let root = std::env::temp_dir().join(format!("shishi-correction-{}", id()));
        (App::load(root.clone()).unwrap(), root)
    }

    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            source_id: "source".into(),
            chat: "chat".into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }

    #[test]
    fn partial_manual_edit_preserves_evidence_summary_schedule_and_archive_fields() {
        let (app, root) = app();
        {
            let mut s = app.state.lock().unwrap();
            s.tasks.push(Task {
                id: "t".into(),
                title: "原标题".into(),
                description: "原描述".into(),
                status: "待处理".into(),
                priority: "普通".into(),
                environment: "未指定".into(),
                summary: "模型生成的摘要".into(),
                summary_updated_at: 120,
                content_updated_at: 100,
                evidence: TaskEvidence {
                    direct: vec!["m1".into()],
                    background: vec!["m2".into()],
                    completion: vec!["m3".into()],
                },
                created: 50,
                updated: 100,
                schedule_id: "schedule".into(),
                scheduled_for: "2026-10-18".into(),
                deferred_until: 999,
                archive_note: "保留的归档备注".into(),
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        // An old/partial UI payload that omits new fields must not reset them.
        let saved = app
            .task_edit(&json!({"task":{
                "id":"t","updated":100,"status":"已完成","priority":"高"
            }}))
            .unwrap();
        let task: Task = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(task.title, "原标题");
        assert_eq!(task.description, "原描述");
        assert_eq!(task.evidence.direct, vec!["m1"]);
        assert_eq!(task.evidence.background, vec!["m2"]);
        assert_eq!(task.evidence.completion, vec!["m3"]);
        assert_eq!(task.summary, "模型生成的摘要");
        assert_eq!(task.summary_updated_at, 120);
        assert_eq!(task.content_updated_at, 100);
        assert_eq!(task.schedule_id, "schedule");
        assert_eq!(task.scheduled_for, "2026-10-18");
        assert_eq!(task.deferred_until, 999);
        assert_eq!(task.archive_note, "保留的归档备注");

        // Only a content edit invalidates the summary timestamp.
        let saved = app
            .task_edit(&json!({"task":{
                "id":"t","updated":saved["updated"],"title":"新的标题"
            }}))
            .unwrap();
        let task: Task = serde_json::from_value(saved).unwrap();
        assert_eq!(task.evidence.direct, vec!["m1"]);
        assert!(task.content_updated_at > 100);
        assert_eq!(task.summary, "原描述");
        assert_eq!(task.summary_updated_at, task.content_updated_at);
        assert_eq!(task.archive_note, "保留的归档备注");
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn generic_lexical_overlap_never_merges_independent_tasks_without_shared_evidence() {
        let mut s = State::default();
        s.messages
            .insert("old".into(), message("old", 100, "旧事项"));
        s.messages
            .insert("new".into(), message("new", 200, "另一个对象"));
        s.tasks.push(Task {
            id: "old-task".into(),
            title: "处理问题".into(),
            description: "处理问题".into(),
            environment: "生产".into(),
            status: "待处理".into(),
            evidence: TaskEvidence {
                direct: vec!["old".into()],
                ..Default::default()
            },
            updated: 100,
            ..Default::default()
        });
        let allowed = ["old", "new"]
            .into_iter()
            .map(|id| (id.to_string(), s.messages[id].clone()))
            .collect::<BTreeMap<_, _>>();
        let action = json!({
            "action":"create","sources":["new"],
            "fields":[
                {"field":"title","value":"处理问题"},
                {"field":"description","value":"处理问题"}
            ],
            "reason":"不同对象","confidence":0.9,"tags":[]
        });
        let catalog = crate::tags::groups(&s);
        App::apply_actions(&mut s, &[action], &allowed, &BTreeMap::new(), &catalog).unwrap();
        assert_eq!(s.tasks.len(), 2);
        assert!(s.tasks[1].evidence.direct.contains(&"new".into()));
        assert!(s.tasks[0].evidence.direct == vec!["old"]);
    }

    #[test]
    fn frozen_retrieval_survives_snapshot_rebuild_and_cross_chat_omitted_task_update() {
        let (app, root) = app();
        {
            let mut s = app.state.lock().unwrap();
            s.messages
                .insert("old".into(), message("old", 100, "最初问题"));
            let mut done = message("done", 300, "原始完成回复");
            done.chat = "other-chat".into();
            s.messages.insert("done".into(), done);
            s.tasks.push(Task {
                id: "t".into(),
                title: "核对".into(),
                description: "核对".into(),
                status: "待处理".into(),
                evidence: TaskEvidence {
                    direct: vec!["old".into()],
                    ..Default::default()
                },
                updated: 100,
                ..Default::default()
            });
            app.save(&s).unwrap();
        }
        let (batch, context, request) = {
            let s = app.state.lock().unwrap();
            let batch = crate::context_budget::classify_with_task_limit(
                &s,
                &[s.messages["old"].clone()],
                0,
            )
            .unwrap();
            let mut context = app
                .capture_prepare_context_from(&s, "classify", false, json!([]))
                .unwrap();
            let mut request = app
                .prepare_ai_request_with("classify", &batch.payload, AiMode::Manual, &mut context)
                .unwrap();
            request.frozen = Some(FrozenView::new(
                batch.allowed.clone(),
                batch.versions.clone(),
                &request.records,
            ));
            (batch, context, request)
        };
        assert!(!request.initial_tasks.contains("t"));
        // A cross-chat completion message and an omitted task are both present
        // in the frozen view, so the request may retrieve and apply them.
        let frozen = request.frozen.as_ref().unwrap();
        let mut allowed = frozen.initial_messages.clone();
        admit_retrieved(&mut allowed, &[json!("done"), json!("t")], frozen);
        assert_eq!(allowed["done"].text, "原始完成回复");
        let mut versions = frozen.task_versions.clone();
        versions.retain(|id, _| request.initial_tasks.contains(id) || id == "t");
        assert!(allowed.contains_key("done"));
        assert_eq!(versions.get("t"), Some(&100));
        let mut state = app.state.lock().unwrap();
        retain_live_frozen(&mut allowed, &mut versions, frozen, &state);
        assert!(allowed.contains_key("done"));
        let action = json!({
            "action":"update","task_id":"t",
            "sources":["done"],"completion_evidence":["done"],
            "fields":[{"field":"status","value":"已完成"}],
            "reason":"跨会话完成证据","confidence":0.99,"tags":[]
        });
        let catalog = crate::tags::groups(&state);
        App::apply_actions(&mut state, &[action], &allowed, &versions, &catalog).unwrap();
        let task = state.tasks.iter().find(|task| task.id == "t").unwrap();
        assert_eq!(task.status, "已完成");
        assert_eq!(task.evidence.completion, vec!["done"]);
        drop(state);

        // Rebuild the shared snapshot while the request is conceptually in
        // flight, then mutate live state and add a new message. The old request
        // still sees its frozen body and can never admit the new message.
        {
            let mut s = app.state.lock().unwrap();
            s.messages.get_mut("done").unwrap().text = "并发改写后的正文".into();
            s.messages
                .insert("late".into(), message("late", 400, "请求开始后才出现"));
            app.save(&s).unwrap();
            crate::snapshots::ensure(&root, &s, now()).unwrap();
        }
        let shared = crate::snapshots::records(&root).unwrap();
        assert!(shared.iter().any(|row| row["id"] == "late"));
        assert!(shared
            .iter()
            .any(|row| row["id"] == "done" && row["data"]["text"] == "并发改写后的正文"));
        let frozen = request.frozen.as_ref().unwrap();
        let mut allowed = frozen.initial_messages.clone();
        admit_retrieved(&mut allowed, &[json!("done"), json!("late")], frozen);
        assert_eq!(allowed["done"].text, "原始完成回复");
        assert!(!allowed.contains_key("late"));
        let live = app.state.lock().unwrap();
        let mut versions = frozen.task_versions.clone();
        retain_live_frozen(&mut allowed, &mut versions, frozen, &live);
        assert!(!allowed.contains_key("done"));
        drop(live);
        drop(batch);
        drop(context);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn accepted_prepared_request_is_sent_with_frozen_runtime_and_measured_prompt() {
        let (app, root) = app();
        app.test_force_working();
        let mut messages = (0..12)
            .map(|i| Message {
                id: format!("m{}", i + 1),
                text: "需要核对的消息内容".repeat(20),
                time: now() + i,
                private: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        messages[0].images = vec![ImageRef {
            id: "synthetic-image".into(),
            path: root
                .join("data/media/synthetic.png")
                .to_string_lossy()
                .into(),
        }];
        let (mut context, limit, full_total, full_runtime, reduced_total) = {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = "00:00".into();
            s.config.work_end = "23:59".into();
            s.config.codex = "nonexistent-must-not-launch.exe".into();
            s.config.retain_ai_traces = true;
            s.config.max_input_chars = 60_000;
            for message in &messages {
                s.messages.insert(message.id.clone(), message.clone());
            }
            // Hidden snapshot ids participate in alias allocation but are not
            // part of this batch. Some collide with the m1.. alias space so
            // visible aliases are longer than their original ids.
            for i in 0..200 {
                let mut hidden = message(
                    &format!("hidden-{i}"),
                    now() - 1_000 - i,
                    "隐藏快照材料，不进入本批提示词",
                );
                hidden.private = true;
                s.analyzed.insert(hidden.id.clone(), fingerprint(&hidden));
                s.messages.insert(hidden.id.clone(), hidden);
            }
            for i in 13..40 {
                let mut hidden = message(&format!("m{i}"), now() - 2_000 - i, "别名冲突材料");
                hidden.private = true;
                s.analyzed.insert(hidden.id.clone(), fingerprint(&hidden));
                s.messages.insert(hidden.id.clone(), hidden);
            }
            for i in 0..12 {
                s.tasks.push(Task {
                    id: format!("t{i}"),
                    title: format!("事项{i}"),
                    description: "事项摘要内容".repeat(10),
                    status: "待处理".into(),
                    updated: 100 + i,
                    ..Default::default()
                });
            }
            s.tag_state.versions.push(crate::tags::Version {
                priority_kind: String::new(),
                assignments: vec![],
                id: "v1".into(),
                groups: vec![crate::tags::Group {
                    display_priority: None,
                    name: "事项类型".into(),
                    values: (0..40).map(|i| format!("类型{i}")).collect(),
                }],
                ..Default::default()
            });
            app.save(&s).unwrap();
            let mut context = app
                .capture_prepare_context_from(&s, "classify", false, json!([]))
                .unwrap();
            let full_batch =
                crate::context_budget::classify_with_task_limit(&s, &messages, usize::MAX).unwrap();
            let full = app
                .prepare_ai_request_with(
                    "classify",
                    &full_batch.payload,
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            let full_total = full.prepared.total;
            let full_runtime = full.runtime.clone();
            assert!(full.payload["_local_context"]["images"]
                .as_array()
                .is_some_and(|images| !images.is_empty()));
            assert!(full.payload["tag_catalog"]
                .as_array()
                .is_some_and(|g| !g.is_empty()));
            assert!(full
                .records
                .iter()
                .any(|row| row["kind"] == "message" && row["id"] == "hidden-77"));
            assert!(!full.payload["_local_context"]["records"]
                .to_string()
                .contains("hidden-77"));
            let encoded_alias_lengths = full.payload["_local_context"]["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["kind"] == "message")
                .filter_map(|row| row["id"].as_str())
                .filter(|id| id.starts_with('m') && id.chars().skip(1).all(|c| c.is_ascii_digit()))
                .map(|id| id.chars().count())
                .collect::<Vec<_>>();
            assert!(
                encoded_alias_lengths.iter().any(|length| *length >= 3),
                "expected hidden ids to lengthen aliases: {encoded_alias_lengths:?}"
            );
            let reduced_batch =
                crate::context_budget::classify_with_task_limit(&s, &messages[..10], 10).unwrap();
            let reduced = app
                .prepare_ai_request_with(
                    "classify",
                    &reduced_batch.payload,
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap();
            assert_eq!(reduced.runtime, full_runtime);
            let reduced_total = reduced.prepared.total;
            let limit = reduced_total + 20;
            assert!(full_total > limit);
            assert!(reduced_total <= limit);
            context.config.max_input_chars = limit;
            s.config.max_input_chars = limit;
            app.save(&s).unwrap();
            (context, limit, full_total, full_runtime, reduced_total)
        };
        let (accepted_batch, accepted) = {
            let s = app.state.lock().unwrap();
            let mut changes = messages.clone();
            let mut task_limit = usize::MAX;
            loop {
                let batch =
                    crate::context_budget::classify_with_task_limit(&s, &changes, task_limit)
                        .unwrap();
                let mut request = app
                    .prepare_ai_request_with(
                        "classify",
                        &batch.payload,
                        AiMode::Manual,
                        &mut context,
                    )
                    .unwrap();
                assert_eq!(request.runtime, full_runtime);
                let total = request.prepared.total;
                if total <= limit {
                    request.frozen = Some(FrozenView::new(
                        batch.allowed.clone(),
                        batch.versions.clone(),
                        &request.records,
                    ));
                    break (batch, request);
                }
                if changes.len() > 10 {
                    changes.truncate(10);
                    continue;
                }
                if task_limit > 10 {
                    task_limit = 10;
                    continue;
                }
                panic!("reduction did not fit: {total} > {limit}");
            }
        };
        assert_eq!(accepted_batch.allowed.len(), 10);
        assert_eq!(
            accepted_batch.fingerprints.len(),
            accepted_batch.allowed.len()
        );
        assert_eq!(accepted.prepared.total, reduced_total);
        assert_eq!(
            accepted.prepared.prompt.chars().count(),
            accepted.prepared.prompt_chars
        );
        assert_eq!(
            accepted.prepared.prompt_chars
                + accepted.prepared.schema_chars
                + accepted.prepared.tool_chars,
            accepted.prepared.total
        );
        assert!(accepted.prepared.tools.is_some());
        drop(context);
        // The adapter is deliberately unreachable (fixture path), but the
        // exact-send trace is written before the CLI is validated.
        let result = app.invoke_ai_request(&accepted, AiMode::Manual);
        assert!(result.is_err());
        let traces = std::fs::read_dir(root.join("data/ai-traces"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let trace = traces
            .into_iter()
            .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
            .expect("trace dir");
        let sent = std::fs::read(trace.join("input.txt")).unwrap();
        assert_eq!(sent, accepted.prepared.prompt.as_bytes());
        assert_eq!(
            String::from_utf8(sent.clone()).unwrap().chars().count(),
            accepted.prepared.prompt_chars
        );
        assert_eq!(
            std::fs::read_to_string(trace.join("schema.json")).unwrap(),
            accepted.prepared.schema.to_string()
        );
        assert_eq!(
            std::fs::read_to_string(trace.join("tools.json")).unwrap(),
            accepted.prepared.tools.as_ref().unwrap().to_string()
        );
        assert!(full_total > limit);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn final_enriched_budget_failure_pauses_automatic_analysis_without_consuming_messages() {
        let (app, root) = app();
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.work_start = "00:00".into();
            s.config.work_end = "23:59".into();
            let fixed = s.config.max_input_chars - ai::message_budget(&s.config, "classify");
            s.config.max_input_chars = fixed + 3_000;
            s.analysis_memory.temporary = "临".repeat(4_000);
            s.analysis_memory.archived_entries.push(MemoryEntry {
                id: "mem".into(),
                key: "长期".into(),
                text: "记".repeat(4_000),
                version: 1,
                ..Default::default()
            });
            s.messages
                .insert("m".into(), message("m", now(), "请核对这个事项"));
            app.save(&s).unwrap();
        }
        let error = app.analyze(AiMode::Automatic).unwrap_err();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR), "{error}");
        let s = app.state.lock().unwrap();
        assert!(s.auto_analysis_blocked);
        assert!(s
            .auto_analysis_block_reason
            .starts_with(ai::INPUT_LIMIT_ERROR));
        assert_eq!(s.calls, 0);
        assert!(s.analyzed.is_empty());
        assert_eq!(s.last_analysis_at, 0);
        assert_eq!(s.last_analysis_success_at, 0);
        assert!(s.pending_analysis);
        assert!(s.analysis_memory.temporary.chars().count() == 4_000);
        assert_eq!(s.messages.len(), 1);
        drop(s);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod editable_memory_concurrency_tests {
    use super::*;
    #[test]
    fn manual_save_blocks_an_older_ai_result_even_when_body_is_unchanged() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-memory-race-{}", id()))).unwrap();
        let stale = json!({"before":"","text":"旧 AI 结果","expected":0});
        app.memory_save(&json!({"kind":"temporary","expected":0,"text":""}))
            .unwrap();
        assert!(app.save_temporary_memory(&stale).is_err());
        assert_eq!(app.memory_read()["temporary"]["text"], "");
    }
}

#[cfg(test)]
mod response_finalization_tests {
    use super::*;
    #[test]
    fn received_response_with_invalid_reference_has_a_correlated_terminal_failure() {
        let root = std::env::temp_dir().join(format!("shishi-finalization-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let mut state = app.state.lock().unwrap().clone();
        state.messages.insert(
            "message-real".into(),
            Message {
                id: "message-real".into(),
                text: "Synthetic evidence".into(),
                time: now(),
                ..Default::default()
            },
        );
        state.tasks.push(Task {
            id: "task-real".into(),
            title: "Synthetic task".into(),
            status: "pending".into(),
            ..Default::default()
        });
        let mut context = app
            .capture_prepare_context_from(&state, "classify", false, json!([]))
            .unwrap();
        let request = app.prepare_ai_request_with("classify", &json!({"changes":[{"id":"message-real","text":"Synthetic evidence"}],"tasks":[{"id":"task-real"}]}), AiMode::Manual, &mut context).unwrap();
        let request_id = "synthetic-finalization";
        app.log(
            "ai_start",
            json!({"request_id":request_id,"kind":"classify"}),
        );
        let result = json!({"actions":[{"action":"update","task_id":"t999","sources":["m1"]}]});
        let detail = json!({"request_id":request_id,"kind":"classify","response_received":true});
        let error = app
            .finalize_ai_response(&request, request_id, detail, Ok(result))
            .unwrap_err();
        assert!(error.contains("未知证据短编号"), "{error}");
        let rows = app.activity.lock().unwrap();
        let failed = rows.iter().find(|r| r["kind"] == "ai_failed").unwrap();
        assert_eq!(failed["detail"]["response_received"], true);
        assert_eq!(failed["detail"]["failure_stage"], "response_validation");
        assert!(rows.iter().any(|r| r["kind"] == "ai_start"
            && r["detail"]["request_id"] == failed["detail"]["request_id"]));
        assert!(!rows.iter().any(|r| r["kind"] == "ai_success"));
        drop(rows);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
