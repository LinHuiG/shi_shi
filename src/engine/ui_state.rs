//! UI projections and notification fingerprints.
use super::*;

impl App {
    /// The notifier compares a cheap fingerprint; the minute bucket covers
    /// clock-derived fields. Skip this pulse while business state is locked.
    pub fn ui_pulse(&self) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        {
            let s = self.state.try_lock().ok()?;
            (s.revision, s.last_scan, s.tasks.len(), s.messages.len()).hash(&mut h);
        }
        self.saves.load(Ordering::SeqCst).hash(&mut h);
        self.notifications.lock().unwrap().revision.hash(&mut h);
        self.updates.revision.load(Ordering::SeqCst).hash(&mut h);
        self.handling_preview_revision
            .load(Ordering::SeqCst)
            .hash(&mut h);
        self.activity_revision.load(Ordering::SeqCst).hash(&mut h);
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
        if page == "logs" {
            value["core_revision"] = value["logs_core_revision"].clone();
        }
        value.as_object_mut().unwrap().remove("logs_core_revision");
        value["_page"] = json!(page);
        value
    }
    /// All sections, for internal callers and offline regression.
    pub fn snapshot(&self) -> Value {
        self.snapshot_sections(None)
    }
    /// Core data is always included. Build the requested large sections only.
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
        let s = self.state.lock().unwrap();
        let at = now();
        let core_revision = self.core_revision(&s, None);
        let logs_revision = self.core_revision(&s, Some("logs"));
        let status_changes = StatusChanges::new(&s.audit);
        let catalog = crate::tags::groups(&s);
        let priority_kind = crate::tags::priority_kind(&s);
        let (waiting, analyzing) = self.message_queue(&s);
        let mut snapshot = json!({"async_ai_requests":true,"messages_revision":[s.messages.len(),s.last_scan,s.revision],"ai_request_id":ai_request_id,"ai_source":ai_source,"ai_cancelling":ai_cancelling,"ai_interrupted_at":s.ai_interrupted_at,"tag_state":crate::tags::snapshot(&s),"handling_proposals":s.handling.proposals.len(),"prompt_state":{"versions":s.prompt_versions.len(),"reviews":s.prompt_reviews.iter().map(|r|json!([r.id,r.status,r.error])).collect::<Vec<_>>()},"chat_sources":s.chat_sources,"prompt_pending":s.prompt_reviews.iter().filter(|r|r.archived_at==0 && r.status=="ready" && prompts::has_result(r)).count(),"work_state":self.work_snapshot(&s,now()).ok(),"morning_jobs":s.morning_jobs,"filter_usage":s.message_filter.summary(),"analysis_retries":s.analysis_retries,"in_work_hours":self.work_gate(&s,"classify",AiMode::Automatic,None,now()),"activity":*self.activity.lock().unwrap(),"ai_started":self.ai_started.load(Ordering::SeqCst),"ai_progress":*self.ai_progress.lock().unwrap(),"ai_live":*self.ai_live.lock().unwrap(),"config":s.config,"schedules":s.schedules.iter().map(|s|{let mut v=json!(s);v["next_at"]=json!(schedule::next_occurrence(s,now()));v}).collect::<Vec<_>>(),"tasks":s.tasks.iter().map(|t|{let mut v=json!(t);let effective_tags=crate::tags::effective_tags(t,&catalog,&priority_kind,at);v["evidence_count"]=json!(t.evidence.all().iter().filter_map(|id|s.messages.get(id)).filter(|m|!m.withdrawn).count());v["effective_tags"]=json!(effective_tags);v["priority_rank"]=json!(crate::tags::priority_rank(&effective_tags,&catalog,&priority_kind));v["status_changed_at"]=json!(status_changes.changed_at(t));v["today"]=json!(status_changes.today(t,at));v["deferred"]=json!(t.deferred_until>at);v}).collect::<Vec<_>>(),"suggestions":s.suggestions,"last_scan":s.last_scan,"last_analysis_success_at":s.last_analysis_success_at,"auto_analysis_blocked":s.auto_analysis_blocked,"auto_analysis_block_reason":s.auto_analysis_block_reason,"archive_attempt":s.archive_attempts.last(),"last_error":s.last_error,"usage":{"day":date(),"calls":if s.usage_day==date(){s.calls}else{0},"input_tokens":if s.usage_day==date(){s.input_tokens}else{0},"output_tokens":if s.usage_day==date(){s.output_tokens}else{0},"cached_input_tokens":if s.usage_day==date(){s.cached_input_tokens}else{0},"cached_reports":if s.usage_day==date(){s.cached_reports}else{0},"unreported_calls":if s.usage_day==date(){s.unreported_calls}else{0}},"retention_digest":s.retention_digest,"busy":self.busy.load(Ordering::SeqCst),"ai_busy":self.ai_busy.load(Ordering::SeqCst),"root":self.root,"data_directory":crate::storage::data_dir(&self.root),"rule_version":s.rule_version,"pending":waiting.len(),"analyzing":analyzing});
        snapshot["analysis_retry_counts"] = json!(s.analysis_retry_counts);
        snapshot["core_revision"] = json!(core_revision);
        snapshot["logs_core_revision"] = json!(logs_revision);
        snapshot["version"] = json!(env!("CARGO_PKG_VERSION"));
        snapshot["build_time"] = json!(crate::updates::build_time());
        snapshot["updates"] = self.updates.snapshot();
        let task_numbers = &s.task_numbers;
        let tool_tasks = s
            .tool_runs
            .iter()
            .filter(|run| !run.calls.is_empty())
            .flat_map(|run| run.task_ids())
            .filter(|id| !id.is_empty())
            .collect::<BTreeSet<_>>();
        for task in snapshot["tasks"].as_array_mut().unwrap() {
            task["number"] = json!(task_numbers.get(task["id"].as_str().unwrap_or("")));
            task["has_tool_calls"] = json!(tool_tasks.contains(task["id"].as_str().unwrap_or("")));
        }
        snapshot["task_history_revision"] = json!([s.revision, s.audit.len()]);
        if want("handling") {
            let connection = crate::ai_backend::connection(&s.config.for_kind("handling")).ok();
            let unconfigured =
                s.config.for_kind("handling").selected_connection == UNCONFIGURED_CONNECTION;
            snapshot["handling"] = json!(s
                .handling
                .runs
                .iter()
                .map(|r| {
                    let r = self.preview_run(r);
                    let mut value = json!(r);
                    value["restartable"] = json!(crate::handling::can_restart(&r));
                    value["current_source"] = json!(
                        unconfigured
                            || connection
                                .as_ref()
                                .is_some_and(|c| crate::handling::source_matches(&r, c))
                    );
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
                .collect::<Vec<_>>();
            messages.sort_by_key(|m| std::cmp::Reverse(m.time));
            messages.truncate(200);
            snapshot["messages"] = json!(messages);
        }
        snapshot
    }
}
