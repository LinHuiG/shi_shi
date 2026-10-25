//! Explicit user edits and configuration validation.
use super::*;

impl App {
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
        if !enabled {
            state.message_filter.early = false;
            state.message_filter.trigger.clear();
        }
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
            "auto_update",
            "ai_timeout_seconds",
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
        let mut c: Config = serde_json::from_value(v).map_err(|e| e.to_string())?;
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
            &c.tool_call_prompt,
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
        if c.daily_calls > 500 {
            return Err("每日调用上限不能超过500".into());
        }
        crate::work_state::WorkSchedule::parse(&c.work_start, &c.work_end)
            .map_err(|error| format!("上下班时间无效：{error}"))?;
        let mut locked = self.state.lock().unwrap();
        if expected.is_some_and(|before| *before != json!(locked.config)) {
            return Err("settings_concurrent_change".into());
        }
        let mut s = locked.clone();
        // Tool CRUD has its own revision checks. Old/stale settings payloads
        // must never clear the registry or overwrite a simultaneous tool edit.
        c.tools = s.config.tools.clone();
        prompts::record_config_changes(&mut s, &c);
        let old = json!(s.config);
        let filter_disabled = s.config.message_filter_enabled && !c.message_filter_enabled;
        let scope_changed = s.config.analysis_related_only != c.analysis_related_only;
        s.config = c;
        if !s.config.early_analysis_enabled() {
            s.message_filter.early = false;
            s.message_filter.trigger.clear();
        }
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
        self.commit_state(|s| {
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
                            tags.extend(
                                incoming.tags.iter().filter(|tag| tag.kind == kind).cloned(),
                            );
                        }
                    }
                    next_value["tags"] = json!(tags);
                }
                if let Some(follow) = submitted.get("follow_deadline").and_then(Value::as_bool) {
                    next_value["follow_deadline"] = json!(follow);
                }
                let mut next: Task =
                    serde_json::from_value(next_value).map_err(|e| e.to_string())?;
                let content_changed =
                    next.title != old.title || next.description != old.description;
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
                s,
                "manual_edit",
                &t.id,
                "人工修改，后续AI需结合修改记录和新证据判断",
                before,
                json!(t),
            );
            Ok(json!(t))
        })
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
            &json!({"current_task":fields,"current_tags":draft.tags,"user_instruction":text,"_tool_task_id":draft.id}),
        )?;
        self.complete_ai_edit(&original, draft, text, result)
    }
    pub(super) fn complete_ai_edit(
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
        draft.suggested_project_id = result["suggested_project_id"].as_str().unwrap_or("").into();
        draft.suggested_project_reason = result["suggested_project_reason"]
            .as_str()
            .unwrap_or("")
            .into();
        draft.manual = true;
        draft.locked = false;
        draft.reason = result["reason"]
            .as_str()
            .unwrap_or("根据你的修改说明更新")
            .chars()
            .take(1000)
            .collect();
        s.tasks[pos] = draft.clone();
        crate::tool_calling::link_tasks(&mut s, &result, &[draft.id.clone()]);
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
        self.commit_state(|s| {
            let a = s.audit.get(index).cloned().ok_or("记录不存在")?;
            if !["merge", "manual_edit", "manual_status", "manual_ai_edit"]
                .contains(&a.kind.as_str())
            {
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
            Self::audit(s, "undo", &a.task_id, "撤销人工操作", a.after, a.before);
            Ok(json!(true))
        })
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
        self.commit_state(|s| {
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
                s,
                "rule_change",
                "",
                "规则启用/撤销，仅影响后续判断",
                before,
                after,
            );
            Ok(json!(true))
        })
    }
}
