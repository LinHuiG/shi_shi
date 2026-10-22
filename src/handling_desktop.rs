//! Desktop-owned handling. Reads and local completion never take execution
//! ownership. All remote mutations have a durable local pending input first.
use super::*;

pub(super) fn is_desktop(run: &Run) -> bool {
    run.connection.as_ref().is_some_and(|c| c.driver == "codex")
}
impl App {
    pub(super) fn desktop(
        &self,
        connection: &Connection,
    ) -> Arc<dyn crate::desktop_bridge::DesktopApi> {
        let mut slot = self.desktop_bridge.lock().unwrap();
        slot.get_or_insert_with(|| {
            #[cfg(test)]
            {
                let _ = connection;
                Arc::new(crate::desktop_bridge::OfflineDesktop)
            }
            #[cfg(not(test))]
            Arc::new(crate::desktop_bridge::Desktop::new(
                self.root.clone(),
                connection.executable.clone(),
            ))
        })
        .clone()
    }
    pub(super) fn desktop_start(
        &self,
        run: &Run,
        title: &str,
        prompt: &str,
        context: &ContextTool,
    ) -> Result<Value, String> {
        let round_id = id();
        self.update_run(&run.id, |r| {
            r.transport = "desktop".into();
            r.status = "submitting".into();
            r.rounds.push(Round {
                id: round_id.clone(),
                question: prompt.into(),
                status: "submitting".into(),
                created: now(),
                reserved: true,
                ..Default::default()
            });
        })?;
        let bridge = self.desktop(&self.run_connection(run)?);
        self.ensure_run_open(&run.id)?;
        let result = bridge.create(&run.cwd, title, prompt, &run.model, &run.effort, context).and_then(|result| {
            if result["threadId"].as_str().is_some_and(|s| uuid::Uuid::parse_str(s).is_ok()) { Ok(result) }
            else { Err("提交结果待核实：桌面没有返回正式会话编号；请核对 Codex 后恢复同步，勿重复创建".into()) }
        });
        match result {
            Ok(result) => {
                let thread = result["threadId"].as_str().filter(|s| uuid::Uuid::parse_str(s).is_ok()).ok_or("提交结果待核实：桌面没有返回正式会话编号；请核对 Codex 后恢复同步，勿重复创建")?;
                // Persist an acknowledged ID even if local completion happened
                // while create was in flight. Never resurrect its watching flag.
                let mut state = self.state.lock().unwrap();
                let mut next = state.clone();
                let current = next
                    .handling
                    .runs
                    .iter_mut()
                    .find(|r| r.id == run.id)
                    .ok_or("处置记录不存在")?;
                current.thread_id = thread.into();
                if !current.stop_requested {
                    current.status = "inProgress".into();
                    current.error.clear();
                }
                self.save(&next).map_err(|e| {
                    format!(
                        "提交结果待核实：桌面会话 {thread} 已创建，本地保存失败：{e}；不要重复创建"
                    )
                })?;
                *state = next;
                Ok(json!({"thread_id":thread,"run_id":run.id}))
            }
            Err(error) => {
                let uncertain = error.starts_with(crate::desktop_bridge::UNCERTAIN);
                let _ = self.update_run(&run.id, |r| {
                    r.status = if uncertain { "uncertain" } else { "failed" }.into();
                    r.error = error.clone();
                    r.watching = uncertain;
                    if let Some(round) = r.rounds.iter_mut().find(|r| r.id == round_id) {
                        round.not_submitted = !uncertain;
                        round.status = r.status.clone();
                    }
                });
                Err(error)
            }
        }
    }
    pub(super) fn desktop_followup(&self, run: &Run, text: &str) -> Result<Value, String> {
        if run.rounds.iter().any(|r| {
            r.status == "uncertain"
                || r.supplements
                    .iter()
                    .any(|s| !s.accepted && !s.not_submitted)
        }) {
            return Err("上次消息提交结果待核实，请先同步并在 Codex 核对；不会自动重发".into());
        }
        let _context = context_config(self, run)?;
        let snapshot = self
            .desktop(&self.run_connection(run)?)
            .read(&run.thread_id)?;
        self.followup_target(run)?;
        let busy = snapshot["thread"]["status"]["type"] == "active"
            || snapshot["turns"]
                .as_array()
                .is_some_and(|v| v.first().is_some_and(|t| t["status"] == "inProgress"));
        let cfg = if busy {
            self.state.lock().unwrap().config.for_kind("handling")
        } else {
            self.reserve_ai(AiMode::Manual, None)?.for_kind("handling")
        };
        let same_connection = ai_backend::connection(&cfg).ok().map(|c| c.cache_key())
            == Some(self.run_connection(run)?.cache_key());
        // Only an explicit change in ShiShi overrides subsequent desktop
        // settings. An unchanged profile must not reset a desktop-side choice.
        let model =
            (same_connection && cfg.model != run.requested_model).then_some(cfg.model.as_str());
        let effort = (same_connection && cfg.reasoning_effort != run.requested_effort)
            .then_some(cfg.reasoning_effort.as_str());
        let input_id = id();
        self.update_run(&run.id, |r| {
            apply_desktop_snapshot(r, &snapshot);
            r.input_revision = r.input_revision.wrapping_add(1);
            if busy {
                if let Some(round) = r.rounds.last_mut() {
                    round.supplements.push(Supplement {
                        id: input_id.clone(),
                        text: text.into(),
                        accepted: false,
                        not_submitted: false,
                    });
                }
            } else {
                r.rounds.push(Round {
                    id: input_id.clone(),
                    question: text.into(),
                    status: "submitting".into(),
                    reserved: true,
                    created: now(),
                    ..Default::default()
                });
            }
            r.status = "submitting".into();
            r.error.clear();
            r.watching = true;
        })?;
        self.followup_target(run)?;
        let result =
            self.desktop(&self.run_connection(run)?)
                .send(&run.thread_id, text, model, effort);
        let uncertain = result
            .as_ref()
            .is_err_and(|e| e.starts_with(crate::desktop_bridge::UNCERTAIN));
        let accepted = result.is_ok();
        let persisted = self.update_run(&run.id, |r| {
            r.input_revision = r.input_revision.wrapping_add(1);
            r.status = if accepted {
                "inProgress"
            } else if uncertain {
                "uncertain"
            } else {
                "failed"
            }
            .into();
            if let Err(error) = &result {
                r.error = error.clone();
            }
            if accepted && same_connection {
                r.requested_model = cfg.model.clone();
                r.requested_effort = cfg.reasoning_effort.clone();
            }
            for round in &mut r.rounds {
                if round.id == input_id {
                    round.status = r.status.clone();
                    round.not_submitted = !accepted && !uncertain;
                }
                for input in &mut round.supplements {
                    if input.id == input_id {
                        input.accepted = accepted;
                        input.not_submitted = !accepted && !uncertain;
                    }
                }
            }
        });
        if let Err(error) = persisted {
            if accepted {
                return Err(format!(
                    "提交结果待核实：桌面已接收，本地未完成记录；请勿重发：{error}"
                ));
            }
            return Err(error);
        }
        result?;
        Ok(json!({"steered":busy}))
    }
    /// Called independently of the submission lock. A blocked read cannot keep
    /// the composer or local finish action locked.
    pub fn handling_sync_desktop(&self, task: Option<&str>) -> bool {
        let runs = self
            .state
            .lock()
            .unwrap()
            .handling
            .runs
            .iter()
            .filter(|r| {
                is_desktop(r)
                    && !r.stop_requested
                    && r.watching
                    && !r.thread_id.is_empty()
                    && task.is_none_or(|t| t == r.task_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut changed = false;
        for run in runs {
            let mut reading = self.desktop_reading.lock().unwrap();
            if !reading.insert(run.id.clone()) {
                continue;
            }
            drop(reading);
            let result = self
                .run_connection(&run)
                .and_then(|c| self.desktop(&c).read(&run.thread_id));
            // Read before write; update_run rechecks the durable local end flag.
            match result {
                Ok(snapshot) => {
                    let mut candidate = run.clone();
                    candidate.error.clear();
                    apply_desktop_snapshot(&mut candidate, &snapshot);
                    if json!([
                        candidate.status,
                        candidate.model,
                        candidate.model_reported,
                        candidate.effort,
                        candidate.progress,
                        candidate.rounds,
                        candidate.waiting_on
                    ]) != json!([
                        run.status,
                        run.model,
                        run.model_reported,
                        run.effort,
                        run.progress,
                        run.rounds,
                        run.waiting_on
                    ]) || !run.error.is_empty()
                    {
                        let saved = self.update_run(&run.id, |r| {
                            if r.input_revision == run.input_revision {
                                r.error.clear();
                                apply_desktop_snapshot(r, &snapshot);
                                r.synced = now();
                            }
                        });
                        changed |= saved.is_ok();
                    }
                }
                Err(error) => {
                    if run.error != error {
                        changed |= self
                            .update_run(&run.id, |r| {
                                r.error = error;
                                r.waiting_on = "sync".into();
                            })
                            .is_ok();
                    }
                }
            }
            self.desktop_reading.lock().unwrap().remove(&run.id);
        }
        changed
    }
    pub(super) fn finish_desktop(&self, p: &Value) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let run = next
            .handling
            .runs
            .iter_mut()
            .find(|r| r.id == str_at(p, "run_id") && r.task_id == str_at(p, "task_id"))
            .ok_or("处置目标已变化，请重新核对")?;
        if !is_desktop(run) {
            return Err("不是 Codex 桌面处置".into());
        }
        run.stop_requested = true;
        run.watching = false;
        run.status = "ended".into();
        run.requests.clear();
        run.error.clear();
        run.waiting_on.clear();
        run.updated = now();
        self.save(&next)?;
        *state = next;
        Ok(json!(true))
    }
    pub(super) fn desktop_reconnect(&self, p: &Value) -> Result<Value, String> {
        let run = self.run(str_at(p, "task_id"))?;
        self.ensure_run_open(&run.id)?;
        let thread = if run.thread_id.is_empty() {
            str_at(p, "thread_id").trim()
        } else {
            &run.thread_id
        };
        uuid::Uuid::parse_str(thread)
            .map_err(|_| "请核对 Codex 中已创建的会话编号；不会重复创建")?;
        let snapshot = self.desktop(&self.run_connection(&run)?).read(thread)?;
        if !crate::desktop_bridge::same_path(str_at(&snapshot["thread"], "cwd"), &run.cwd) {
            return Err("此会话不属于本次所选目录".into());
        }
        context_config(self, &run)?;
        self.update_run(&run.id, |r| {
            r.thread_id = thread.into();
            r.watching = true;
            r.error.clear();
            apply_desktop_snapshot(r, &snapshot);
        })?;
        Ok(json!(true))
    }
}

fn apply_desktop_snapshot(run: &mut Run, snapshot: &Value) {
    if run.stop_requested {
        return;
    }
    let mut turns = snapshot["turns"].clone();
    // Preserve the wire distinction in storage while projecting delegated text
    // through the same bounded visible-input history as native desktop input.
    for turn in turns.as_array_mut().into_iter().flatten() {
        for item in turn["items"].as_array_mut().into_iter().flatten() {
            if item["type"] != "functionCallOutput" {
                continue;
            }
            let raw = item["output"]
                .as_str()
                .or_else(|| item["output"]["text"].as_str())
                .unwrap_or("");
            if !raw.starts_with("<codex_delegation>") {
                continue;
            }
            if let Some((start, end)) = raw.find("<input>").zip(raw.rfind("</input>")) {
                if end >= start + 7 {
                    let text = raw[start + 7..end].to_owned();
                    *item = json!({"type":"userMessage","id":item["id"],"origin":"delegation","content":[{"type":"text","text":text}]});
                }
            }
        }
    }
    apply_history(run, &turns, &snapshot["thread"]);
    run.rounds
        .sort_by_key(|r| r.started_at.unwrap_or(r.created));
    run.model_reported = Some(snapshot["thread"]["model"].is_string());
    run.waiting_on = if snapshot["thread"]["status"]["activeFlags"]
        .as_array()
        .is_some_and(|v| v.iter().any(|f| f == "waitingOnApproval"))
    {
        "approval"
    } else if snapshot["thread"]["status"]["activeFlags"]
        .as_array()
        .is_some_and(|v| v.iter().any(|f| f == "waitingOnUserInput"))
    {
        "input"
    } else {
        ""
    }
    .into();
    if !run.waiting_on.is_empty() {
        run.status = "waiting".into();
    }
    if let Some(effort) = snapshot["thread"]["reasoningEffort"]
        .as_str()
        .or_else(|| snapshot["thread"]["effort"].as_str())
    {
        run.effort = effort.into();
    }
    run.desktop_status = run.status.clone();
}

#[cfg(test)]
#[path = "handling_desktop_tests.rs"]
mod tests;
