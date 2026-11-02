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
        self.ai_live.lock().unwrap().revision.hash(&mut h);
        self.active_ai
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| (&a.id, a.cancel.load(Ordering::SeqCst), &a.source))
            .hash(&mut h);
        self.analyzing_messages.lock().unwrap().len().hash(&mut h);
        Some(h.finish())
    }
    pub fn state_version(&self) -> Value {
        json!(self.ui_pulse())
    }
    #[cfg(test)]
    pub fn snapshot_page(&self, page: Option<&str>) -> Value {
        self.snapshot_page_scope(page, None, None)
    }
    pub(crate) fn snapshot_page_scope(
        &self,
        page: Option<&str>,
        task: Option<&str>,
        selected: Option<&str>,
    ) -> Value {
        let Some(page) = page else {
            return self.snapshot();
        };
        let mut sections = vec![];
        if page == "tasks" {
            sections.extend(["tasks", "handling"]);
        }
        if task.is_some() || selected.is_some() {
            sections.push("handling");
        }
        if ["prompts", "learning"].contains(&page) {
            sections.push("evolution");
        }
        if page == "schedules" {
            sections.push("schedules");
        }
        if page == "reports" {
            sections.push("reports");
        }
        let mut value = self.snapshot_projection(Some(&json!(sections)), task, selected, true);
        value["_page"] = json!(page);
        value
    }
    /// All sections, for internal callers and offline regression.
    pub fn snapshot(&self) -> Value {
        self.snapshot_sections(None)
    }
    /// Core data is always included. Build the requested large sections only.
    pub fn snapshot_sections(&self, sections: Option<&Value>) -> Value {
        self.snapshot_projection(sections, None, None, false)
    }
    fn snapshot_projection(
        &self,
        sections: Option<&Value>,
        task: Option<&str>,
        selected: Option<&str>,
        scoped: bool,
    ) -> Value {
        let want = |name: &str| {
            sections
                .and_then(Value::as_array)
                .is_none_or(|list| list.iter().any(|v| v == name))
        };
        let mut runtime = self.runtime_fields();
        let s = self.state.lock().unwrap();
        let at = now();
        let core_revision = self.core_revision(&s);
        let full_tasks = !scoped || want("tasks");
        let status_changes = full_tasks.then(|| StatusChanges::new(&s.audit));
        let catalog = crate::tags::groups(&s);
        let priority_kind = crate::tags::priority_kind(&s);
        let (waiting, analyzing, manual_retry) = self.message_queue_counts(&s);
        let tasks = s
            .tasks
            .iter()
            .map(|t| {
                if full_tasks || task == Some(t.id.as_str()) || selected == Some(t.id.as_str()) {
                    let mut value = json!(t);
                    if let Some(changes) = &status_changes {
                        let tags = crate::tags::effective_tags(t, &catalog, &priority_kind, at);
                        value["evidence_count"] = json!(t
                            .evidence
                            .all()
                            .into_iter()
                            .filter_map(|id| s.messages.get(id))
                            .filter(|m| !m.withdrawn)
                            .count());
                        value["priority_rank"] =
                            json!(crate::tags::priority_rank(&tags, &catalog, &priority_kind));
                        value["effective_tags"] = json!(tags);
                        value["status_changed_at"] = json!(changes.changed_at(t));
                        value["today"] = json!(changes.today(t, at));
                        value["deferred"] = json!(t.focus(at) == Some("延后处理"));
                    }
                    value
                } else {
                    // Sidebar counts, palette navigation and log labels need only
                    // identity and focus state. The tasks page reloads full rows.
                    json!({"id":t.id,"title":t.title,"status":t.status,
                    "archived":t.archived,"merged_into":t.merged_into,
                    "deferred_until":t.deferred_until,"blocker":t.blocker,
                    "waiting_for":t.waiting_for,"number":s.task_numbers.get(&t.id)})
                }
            })
            .collect::<Vec<_>>();
        let mut snapshot = json!({"async_ai_requests":true,"messages_revision":[s.messages.len(),s.last_scan,s.revision],"ai_interrupted_at":s.ai_interrupted_at,"tag_state":crate::tags::snapshot(&s),"handling_proposals":s.handling.proposals.len(),"prompt_state":{"versions":s.prompt_versions.len(),"reviews":s.prompt_reviews.iter().map(|r|json!([r.id,r.status,r.error])).collect::<Vec<_>>()},"chat_sources":s.chat_sources,"prompt_pending":s.prompt_reviews.iter().filter(|r|r.archived_at==0 && r.status=="ready" && prompts::has_result(r)).count(),"work_state":self.work_snapshot(&s,now()).ok(),"morning_jobs":s.morning_jobs,"filter_usage":s.message_filter.summary(),"analysis_retries":s.analysis_retries,"in_work_hours":self.work_gate(&s,"classify",AiMode::Automatic,None,now()),"config":s.config,"tasks":tasks,"last_scan":s.last_scan,"last_analysis_success_at":s.last_analysis_success_at,"auto_analysis_blocked":s.auto_analysis_blocked,"auto_analysis_block_reason":s.auto_analysis_block_reason,"archive_attempt":s.archive_attempts.last(),"last_error":s.last_error,"usage":{"day":date(),"calls":if s.usage_day==date(){s.calls}else{0},"input_tokens":if s.usage_day==date(){s.input_tokens}else{0},"output_tokens":if s.usage_day==date(){s.output_tokens}else{0},"cached_input_tokens":if s.usage_day==date(){s.cached_input_tokens}else{0},"cached_reports":if s.usage_day==date(){s.cached_reports}else{0},"unreported_calls":if s.usage_day==date(){s.unreported_calls}else{0}},"retention_digest":s.retention_digest,"root":self.root,"data_directory":crate::storage::data_dir(&self.root),"rule_version":s.rule_version,"pending":waiting,"analyzing":analyzing,"manual_retry_count":manual_retry});
        snapshot["analysis_retry_counts"] = json!(s.analysis_retry_counts);
        snapshot["report_outcome"] = json!(s.report_outcome());
        snapshot["core_revision"] = json!(core_revision);
        snapshot["version"] = json!(env!("CARGO_PKG_VERSION"));
        snapshot["build_time"] = json!(crate::updates::build_time());
        snapshot
            .as_object_mut()
            .unwrap()
            .append(runtime.as_object_mut().unwrap());
        if full_tasks {
            let task_numbers = &s.task_numbers;
            let mut change_counts = std::collections::BTreeMap::<&str, usize>::new();
            for row in s
                .audit
                .iter()
                .filter(|a| !a.kind.starts_with("archive_memory"))
            {
                *change_counts.entry(&row.task_id).or_default() += 1;
            }
            let tool_tasks = s
                .tool_runs
                .iter()
                .flat_map(|run| run.task_ids())
                .filter(|id| !id.is_empty())
                .collect::<BTreeSet<_>>();
            for task in snapshot["tasks"].as_array_mut().unwrap() {
                task["change_count"] = json!(change_counts
                    .get(task["id"].as_str().unwrap_or(""))
                    .copied()
                    .unwrap_or(0));
                task["number"] = json!(task_numbers.get(task["id"].as_str().unwrap_or("")));
                task["has_tool_calls"] =
                    json!(tool_tasks.contains(task["id"].as_str().unwrap_or("")));
            }
            snapshot["task_history_revision"] = json!([s.revision, s.audit.len()]);
        }
        if want("handling") {
            let choice = s.config.model_choice("handling");
            let connection =
                crate::ai_backend::connection_by_id(&s.config, choice.connection_id).ok();
            let unconfigured = choice.connection_id == UNCONFIGURED_CONNECTION;
            snapshot["handling"] = json!(s
                .handling
                .runs
                .iter()
                .filter(|r| !scoped
                    || want("tasks")
                    || task == Some(r.task_id.as_str())
                    || selected == Some(r.task_id.as_str()))
                .map(|r| {
                    let full = !scoped
                        || task == Some(r.task_id.as_str())
                        || selected == Some(r.task_id.as_str());
                    let mut value = self.handling_view(r, full);
                    value["restartable"] = json!(crate::handling::can_restart(r));
                    value["current_source"] = json!(
                        unconfigured
                            || connection
                                .as_ref()
                                .is_some_and(|c| crate::handling::source_matches(r, c))
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
        if !scoped || want("evolution") {
            snapshot["suggestions"] = json!(s.suggestions);
        }
        if want("evolution") {
            snapshot["evolution"] = json!({"prompts":s.prompt_reviews.iter().filter(|r|r.archived_at==0 && prompts::has_result(r)).map(|r|{let mut v=json!(r);v.as_object_mut().unwrap().remove("evidence");v}).collect::<Vec<_>>(),"handling":s.handling.proposals.iter().filter(|r|r.archived_at==0).collect::<Vec<_>>(),"revision_count":s.advice_revisions.len()});
        }
        if !scoped || want("schedules") {
            snapshot["schedules"] = json!(s
                .schedules
                .iter()
                .map(|s| {
                    let mut value = json!(s);
                    value["next_at"] = json!(schedule::next_occurrence(s, at));
                    value
                })
                .collect::<Vec<_>>());
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
                .iter()
                .filter(|(_, m)| m.relevant())
                .collect::<Vec<_>>();
            // The old stable time sort inherited ID order from the BTreeMap.
            // Preserve that tie order while selecting only the newest 200.
            let order = |a: &(&String, &Message), b: &(&String, &Message)| {
                b.1.time.cmp(&a.1.time).then_with(|| a.0.cmp(b.0))
            };
            if messages.len() > 200 {
                messages.select_nth_unstable_by(200, order);
                messages.truncate(200);
            }
            messages.sort_unstable_by(order);
            snapshot["messages"] = json!(messages
                .into_iter()
                .map(|(_, message)| message)
                .collect::<Vec<_>>());
        }
        snapshot
    }
}
