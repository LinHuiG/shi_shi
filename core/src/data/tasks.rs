//! Task rows (`tasks`, `task_tags`, `task_evidence`) and the manual writes:
//! edit, status actions, batches, merge, undo, archive and deferred resume,
//! plus the validation and commit halves of the AI-assisted edit and
//! natural-language generation. Each write checks the task's `updated`
//! version first.

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension, Params};
use serde_json::{json, Value};

use super::{audit, kv, rows, schedules, Db};
use crate::tasks::model::{self, deadline_ms, from_json, Task, ACTIVE, CLOSED};
use crate::tasks::schedule;
use crate::tasks::tags;
use crate::{exec, time, Error, Result};

/// Task ids the tool domain has calls for; replaced when it lands.
pub const TOOL_TASK_IDS: &str = "SELECT task_id FROM tool_call_tasks";

pub const EDIT_FIELDS: [&str; 6] = ["title", "description", "status", "blocker", "waiting_for", "due"];
pub const PENDING_LEARNING: &str = "pending_learning";

impl Db {
    /// The last audit `seq`, where [`Db::task_changes_since`] starts.
    pub async fn audit_seq(&self) -> Result<i64> {
        self.read(|c| Ok(c.query_row("SELECT COALESCE(MAX(seq), 0) FROM task_audit", [], |r| r.get(0))?)).await
    }

    /// `(task id, before, after, summary)` of the task rows written after
    /// `seq`, in order; AI rows are left out, their round notifies itself.
    pub async fn task_changes_since(&self, seq: i64) -> Result<Vec<(String, Value, Value, String)>> {
        self.read(move |c| {
            let tail = format!("seq > {seq} AND task_id <> '' AND substr(kind, 1, 3) <> 'ai_' ORDER BY seq");
            let rows = super::archive::audits(c, &tail)?;
            Ok(rows
                .into_iter()
                .map(|a| (a.task_id, a.before, a.after, a.change_summary.as_str().unwrap_or("").to_owned()))
                .collect())
        })
        .await
    }

    pub async fn edit_task(&self, p: Value) -> Result<Value> {
        self.write(move |tx| edit(tx, &p)).await
    }

    pub async fn task_action(&self, p: Value, work_start: String) -> Result<Value> {
        self.write(move |tx| action(tx, &p, &work_start)).await
    }

    pub async fn batch_task_action(&self, p: Value, work_start: String) -> Result<Value> {
        self.write(move |tx| batch(tx, &p, &work_start)).await
    }

    pub async fn merge_tasks(&self, p: Value) -> Result<Value> {
        self.write(move |tx| merge(tx, &p)).await
    }

    pub async fn undo_task_change(&self, p: Value) -> Result<Value> {
        self.write(move |tx| undo(tx, &p)).await
    }

    pub async fn archive_task(&self, p: Value) -> Result<Value> {
        self.write(move |tx| archive(tx, &p)).await
    }

    pub async fn prepare_ai_edit(&self, p: Value) -> Result<AiEdit> {
        self.read(move |c| ai_edit_prepare(c, &p)).await
    }

    pub async fn complete_ai_edit(&self, edit: AiEdit, result: Value) -> Result<Value> {
        self.write(move |tx| ai_edit_complete(tx, edit, &result)).await
    }

    pub async fn start_generation(&self, p: Value) -> Result<Task> {
        self.write(move |tx| generation_start(tx, &p)).await
    }

    /// Whether `draft` is still pending at its version.
    pub async fn generation_pending(&self, draft: &Task) -> Result<bool> {
        let (id, updated) = (draft.id.clone(), draft.updated);
        self.read(move |c| Ok(get(c, &id)?.is_some_and(|t| t.updated == updated && t.generation == "pending"))).await
    }

    pub async fn complete_generation(&self, draft: Task, result: Result<Value>) -> Result<usize> {
        self.write(move |tx| generation_complete(tx, &draft, result)).await
    }

    pub async fn interrupt_generations(&self) -> Result<usize> {
        self.write(|tx| interrupt_generations(tx)).await
    }

    /// Whether a schedule or a deferred task is due at `at`, and when the
    /// next one falls due.
    pub async fn task_wake(&self, at: i64) -> Result<(bool, Option<i64>)> {
        self.read(move |c| {
            let resume = next_resume(c)?;
            let due = schedules::has_due(c, at)? || resume.is_some_and(|t| t <= at);
            Ok((due, [schedules::next_wake(c, at)?, resume].into_iter().flatten().min()))
        })
        .await
    }

    /// Generate today's scheduled tasks and resume due deferred ones.
    pub async fn run_due_tasks(&self, at: i64) -> Result<usize> {
        self.write(move |tx| Ok(schedules::generate(tx, at)? + resume_deferred(tx, at)?)).await
    }
}

fn bump(old: i64) -> i64 {
    time::now().max(old + 1)
}

fn need(conn: &Connection, id: &str, missing: &str) -> Result<Task> {
    get(conn, id)?.ok_or_else(|| Error::new(missing))
}

/// The listed message ids that exist and are not withdrawn, in order.
fn live_messages(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    rows::column(
        conn,
        "SELECT j.value FROM json_each(?1) j JOIN messages m ON m.id = j.value WHERE m.withdrawn = 0 ORDER BY j.key",
        [json!(ids).to_string()],
    )
}

/// `edit`: create or patch a task from the edit form.
pub fn edit(conn: &Connection, v: &Value) -> Result<Value> {
    let submitted = &v["task"];
    let mut incoming: Task = from_json(submitted)?;
    incoming.generation.clear();
    incoming.generation_error.clear();
    model::normalize(&mut incoming);
    if incoming.tags.len() > 40 || incoming.tags.iter().any(|t| !tags::valid(t)) {
        crate::bail!("事项标签无效");
    }
    if !incoming.due.is_empty() && model::deadline_ms(&incoming.due).is_none() {
        crate::bail!("截止时间需要完整日期和时间");
    }
    let (before, next) = match get(conn, &incoming.id)? {
        Some(old) => {
            if incoming.updated != old.updated {
                crate::bail!("事项已被更新，请刷新后重新编辑");
            }
            let mut next = old.clone();
            for field in EDIT_FIELDS {
                if submitted.get(field).is_some() {
                    *next.text_field_mut(field).expect("edit field") =
                        std::mem::take(incoming.text_field_mut(field).expect("edit field"));
                }
            }
            if submitted.get("tags").is_some_and(Value::is_array) {
                next.tags = std::mem::take(&mut incoming.tags);
            }
            if let Some(follow) = submitted.get("follow_deadline").and_then(Value::as_bool) {
                next.follow_deadline = follow;
            }
            if next.due != old.due {
                next.deadline_changed = time::now();
            }
            next.updated = bump(old.updated);
            next.manual = true;
            if next.title != old.title || next.description != old.description {
                let at = next.updated;
                model::mark_content_changed(&mut next, at);
            }
            if next.title.trim().is_empty() {
                crate::bail!("标题不能为空");
            }
            next.deferred_until = if next.status == "待处理" { old.deferred_until } else { 0 };
            next.resume_processing = old.resume_processing && next.status == old.status && next.deferred_until > 0;
            model::normalize(&mut next);
            (old.payload(), next)
        }
        None => {
            if incoming.title.trim().is_empty() {
                crate::bail!("标题不能为空");
            }
            let at = time::now();
            incoming.id = model::new_id();
            incoming.created = at;
            incoming.updated = at;
            incoming.deadline_changed = at;
            incoming.manual = true;
            incoming.archived = false;
            incoming.archived_at = 0;
            incoming.merged_into.clear();
            incoming.evidence.direct = live_messages(conn, &incoming.evidence.direct)?;
            model::normalize_evidence(&mut incoming.evidence);
            model::mark_content_changed(&mut incoming, at);
            (Value::Null, incoming)
        }
    };
    let task = store(conn, &next)?;
    audit::write(
        conn,
        "manual_edit",
        &task.id,
        "人工修改，后续AI需结合修改记录和新证据判断",
        &before,
        &task.payload(),
    )?;
    Ok(task.payload())
}

/// `action`: one status action on one task. `work_start` is the configured
/// start of the working day used by `defer`.
pub fn action(conn: &Connection, v: &Value, work_start: &str) -> Result<Value> {
    let mut t = need(conn, v["id"].as_str().unwrap_or(""), "事项不存在")?;
    let before = t.payload();
    let action = v["action"].as_str().unwrap_or("");
    if v["updated"].as_i64() != Some(t.updated) {
        crate::bail!("事项已更新，请刷新后重试");
    }
    let ready = !t.archived && t.merged_into.is_empty() && t.generation.is_empty();
    match action {
        "start" | "defer" => {
            if !t.active() || !ready {
                crate::bail!("只有未归档、已生成的未完成事项可以开始或延后处理");
            }
            if action == "start" {
                t.status = "处理中".into();
                t.deferred_until = 0;
            } else {
                let choice =
                    v.get("choice").map(|c| c.as_str().ok_or("延后时间无效")).transpose()?.unwrap_or("tomorrow");
                let until = schedule::defer_until(choice, work_start, time::now())?;
                t.resume_processing = t.status == "处理中" || t.resume_processing;
                t.status = "待处理".into();
                t.deferred_until = until;
            }
        }
        "tomorrow" => {
            if t.status != "待处理" || !ready {
                crate::bail!("只有未归档、已生成的待处理事项可以移至明日");
            }
            t.deferred_until = schedule::tomorrow_start(time::now());
        }
        "resume_today" => t.deferred_until = 0,
        "done" => t.status = "已完成".into(),
        "reopen" => {
            t.status = "待处理".into();
            t.archived = false;
            t.deferred_until = 0;
        }
        "dismiss" => t.status = "无需处理".into(),
        _ => crate::bail!("未知操作"),
    }
    if t.status != "待处理" {
        t.deferred_until = 0;
    }
    if t.deferred_until == 0 {
        t.resume_processing = false;
    }
    t.manual = true;
    t.generation.clear();
    t.generation_error.clear();
    t.updated = bump(t.updated);
    let detail = match action {
        "start" => "开始处理",
        "defer" => "延后处理",
        "tomorrow" => "明日处理",
        "resume_today" => "恢复今日处理",
        other => other,
    };
    let task = store(conn, &t)?;
    audit::write(conn, "manual_status", &task.id, detail, &before, &task.payload())?;
    Ok(json!(true))
}

/// `task_batch_action`: every item is checked before any is changed, so one
/// bad item leaves the whole batch untouched.
pub fn batch(conn: &Connection, v: &Value, work_start: &str) -> Result<Value> {
    let items = v["items"].as_array().filter(|i| !i.is_empty()).ok_or("请先选择事项")?;
    let action = v["action"].as_str().unwrap_or("");
    if !["start", "done", "defer", "dismiss", "reopen", "archive"].contains(&action) {
        crate::bail!("未知批量操作");
    }
    let mut seen = BTreeSet::new();
    let mut notes = Vec::new();
    for item in items {
        let id = item["id"].as_str().ok_or("缺少事项编号")?;
        if !seen.insert(id) {
            crate::bail!("不能重复选择同一事项");
        }
        let task = need(conn, id, "所选事项已不存在，请重新选择")?;
        let fail = |message: &str| Error(format!("{}：{message}，本批未修改", task.title));
        if item["updated"].as_i64() != Some(task.updated) {
            return Err(fail("事项已更新，请刷新后重试"));
        }
        if !task.merged_into.is_empty() || !task.generation.is_empty() {
            return Err(fail("事项尚未生成完成或已合并"));
        }
        let active = task.active() && !task.archived;
        let allowed = match action {
            "reopen" => !active,
            "archive" => !task.archived,
            _ => active,
        };
        if !allowed {
            return Err(fail("当前状态不支持此操作"));
        }
        notes.push(task.archive_note);
    }
    if action == "archive" && !CLOSED.contains(&v["status"].as_str().unwrap_or("")) {
        crate::bail!("请选择本批归档结果");
    }
    for (item, note) in items.iter().zip(notes) {
        let mut request = item.clone();
        request["action"] = json!(action);
        if action == "archive" {
            request["status"] = v["status"].clone();
            request["note"] = json!(note);
            archive(conn, &request)?;
        } else {
            if let Some(choice) = v.get("choice") {
                request["choice"] = choice.clone();
            }
            self::action(conn, &request, work_start)?;
        }
    }
    Ok(json!({"updated": items.len()}))
}

const RESUMABLE: &str = "resume_processing = 1 AND status = '待处理' AND archived = 0 AND merged_into = '' \
    AND deferred_until > 0";

/// The earliest moment a deferred task returns to 处理中.
pub fn next_resume(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row(&format!("SELECT min(deferred_until) FROM tasks WHERE {RESUMABLE}"), [], |r| r.get(0))?)
}

/// Return tasks deferred from 处理中 to it once their time has come.
pub fn resume_deferred(conn: &Connection, at: i64) -> Result<usize> {
    let due = list(conn, &format!("{RESUMABLE} AND deferred_until <= {at}"))?;
    for mut t in due.iter().cloned() {
        let before = t.payload();
        t.status = "处理中".into();
        t.deferred_until = 0;
        t.resume_processing = false;
        t.updated = at.max(t.updated + 1);
        let task = store(conn, &t)?;
        audit::write(conn, "defer_resumed", &task.id, "延后到期，恢复处理中", &before, &task.payload())?;
    }
    Ok(due.len())
}

/// `merge`: the first id is kept; the others point at it.
pub fn merge(conn: &Connection, v: &Value) -> Result<Value> {
    let ids: Vec<String> = from_json(&v["ids"])?;
    if ids.len() < 2 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        crate::bail!("请选择至少两个不同事项");
    }
    let mut tasks = Vec::new();
    for id in &ids {
        let task = need(conn, id, "所选事项不存在或已清理")?;
        if task.archived || !task.merged_into.is_empty() || !task.generation.is_empty() {
            crate::bail!("归档、已合并或正在生成的事项不能合并");
        }
        if !v["versions"].is_null() && v["versions"][id].as_i64() != Some(task.updated) {
            crate::bail!("所选事项已有修改，请重新打开合并窗口核对");
        }
        tasks.push(task);
    }
    let before = json!(tasks);
    let at = tasks.iter().map(|t| t.updated + 1).fold(time::now(), i64::max);
    let (main, others) = tasks.split_first_mut().expect("two tasks");
    for other in others.iter_mut() {
        for tag in &other.tags {
            if !main.tags.contains(tag) {
                main.tags.push(tag.clone());
            }
        }
        model::merge_evidence(main, &other.evidence);
        main.description.push_str(&format!("\n\n合并事项：{}\n{}", other.title, other.description));
        other.merged_into = main.id.clone();
        other.updated = at;
    }
    main.manual = true;
    main.updated = at;
    model::mark_content_changed(main, at);
    model::normalize(main);
    let mut after = Vec::new();
    for t in &tasks {
        after.push(store(conn, t)?);
    }
    audit::write(conn, "merge", &ids[0], "人工合并", &before, &json!(after))?;
    Ok(json!(true))
}

/// The audit kinds `undo` reverses.
pub const UNDOABLE: [&str; 4] = ["merge", "manual_edit", "manual_status", "manual_ai_edit"];

/// `undo {audit_id}`: the audit row is addressed by its token only.
pub fn undo(conn: &Connection, v: &Value) -> Result<Value> {
    let token = v["audit_id"].as_str().filter(|s| !s.is_empty()).ok_or("缺少记录索引")?;
    let row: Option<(String, String, Option<String>, Option<String>)> = conn
        .query_row("SELECT kind, task_id, before, after FROM task_audit WHERE token = ?1", [token], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let (kind, task_id, before, after) = row.ok_or("记录不存在或已清理")?;
    if !UNDOABLE.contains(&kind.as_str()) {
        crate::bail!("此记录不支持撤销");
    }
    let parse = |text: Option<String>| -> Result<Value> {
        Ok(text.map(|t| serde_json::from_str(&t)).transpose()?.unwrap_or(Value::Null))
    };
    let (before, after) = (parse(before)?, parse(after)?);
    let list = |v: &Value| -> Result<Vec<Task>> {
        Ok(if kind == "merge" {
            from_json(v)?
        } else if v.is_null() {
            Vec::new()
        } else {
            vec![from_json(v)?]
        })
    };
    let olds = list(&before)?;
    let expected = list(&after)?;
    if expected.is_empty() {
        crate::bail!("事项此后已有修改，不能直接撤销，请手动编辑");
    }
    let mut current = Vec::new();
    for e in &expected {
        match get(conn, &e.id)? {
            Some(t) if t.updated == e.updated => current.push(t),
            _ => crate::bail!("事项此后已有修改，不能直接撤销，请手动编辑"),
        }
    }
    if olds.is_empty() {
        if let Some(mut t) = current.into_iter().find(|t| t.id == task_id) {
            t.status = "无需处理".into();
            t.updated = bump(t.updated);
            save(conn, &t)?;
        }
    } else {
        for mut old in olds {
            let Some(now) = current.iter().find(|t| t.id == old.id) else { continue };
            old.updated = bump(now.updated);
            old.manual = true;
            save(conn, &old)?;
        }
    }
    audit::write(conn, "undo", &task_id, "撤销人工操作", &after, &before)?;
    Ok(json!(true))
}

/// `archive_task`: close and archive one task with an optional note.
pub fn archive(conn: &Connection, v: &Value) -> Result<Value> {
    let id = v["id"].as_str().ok_or("缺少事项ID")?;
    let note = v["note"].as_str().unwrap_or("").trim();
    if note.chars().count() > 4000 {
        crate::bail!("归档备注最多4000字");
    }
    let mut t = need(conn, id, "事项不存在")?;
    if v["updated"].as_i64() != Some(t.updated) {
        crate::bail!("事项已更新，请重新打开归档窗口");
    }
    let status = v["status"].as_str().unwrap_or(&t.status).to_string();
    if !CLOSED.contains(&status.as_str()) {
        crate::bail!("请先选择归档结果");
    }
    let before = t.payload();
    t.status = status;
    t.archived = true;
    t.archived_at = time::now();
    t.archive_note = note.into();
    t.deferred_until = 0;
    t.resume_processing = false;
    t.updated = bump(t.updated);
    t.manual = true;
    t.generation.clear();
    t.generation_error.clear();
    let task = store(conn, &t)?;
    audit::write(conn, "manual_archive", &task.id, "人工归档并记录说明", &before, &task.payload())?;
    if !note.is_empty() {
        kv::set(conn, PENDING_LEARNING, &true)?;
    }
    Ok(task.payload())
}

fn validate_edit(t: &Task) -> Result<()> {
    if t.title.trim().is_empty() || t.title.chars().count() > 200 {
        crate::bail!("标题需要1–200字");
    }
    if [&t.description, &t.blocker, &t.waiting_for].iter().any(|s| s.chars().count() > 6000) {
        crate::bail!("修改字段最多6000字");
    }
    if t.tags.len() > 40 || t.tags.iter().any(|tag| !tags::valid(tag)) {
        crate::bail!("事项标签无效或超过40个");
    }
    if !model::STATUSES.contains(&t.status.as_str()) {
        crate::bail!("修改后的状态无效");
    }
    if !t.due.is_empty() && model::deadline_ms(&t.due).is_none() {
        crate::bail!("修改后的截止时间无效，原事项未改动");
    }
    Ok(())
}

/// What an AI-assisted edit sends to the model and later commits.
pub struct AiEdit {
    pub original: Task,
    pub draft: Task,
    pub text: String,
    /// The `edit` prompt input.
    pub request: Value,
}

/// `edit_ai {task, text}` before the AI call: the submitted form fields are
/// the draft the instruction applies to.
pub fn ai_edit_prepare(conn: &Connection, v: &Value) -> Result<AiEdit> {
    let text = v["text"].as_str().unwrap_or("").trim();
    if text.is_empty() || text.chars().count() > 6000 {
        crate::bail!("请输入1–6000字的修改说明");
    }
    let mut submitted: Task = from_json(&v["task"])?;
    let original = need(conn, &submitted.id, "事项不存在，请刷新后重试")?;
    if original.updated != submitted.updated {
        crate::bail!("事项已被更新，请重新打开编辑");
    }
    if !original.merged_into.is_empty() || !original.generation.is_empty() {
        crate::bail!("此事项暂不能用一句话修改");
    }
    let mut draft = original.clone();
    let mut fields = serde_json::Map::new();
    for field in EDIT_FIELDS {
        let value = std::mem::take(submitted.text_field_mut(field).expect("edit field"));
        fields.insert(field.into(), json!(value));
        *draft.text_field_mut(field).expect("edit field") = value;
    }
    validate_edit(&draft)?;
    draft.tags = submitted.tags;
    let request = json!({
        "current_task": fields, "current_tags": draft.tags, "user_instruction": text, "_tool_task_id": draft.id
    });
    Ok(AiEdit { original, draft, text: text.into(), request })
}

/// Commit the AI's answer to an `edit_ai` request, unless the task changed
/// while the model was working.
pub fn ai_edit_complete(conn: &Connection, edit: AiEdit, result: &Value) -> Result<Value> {
    let AiEdit { original, mut draft, text, .. } = edit;
    let changes = result["changes"].as_array().ok_or("AI结果缺少修改内容")?;
    let mut fields = BTreeSet::new();
    for change in changes {
        let field = change["field"].as_str().ok_or("AI修改字段无效")?;
        let value = change["value"].as_str().ok_or("AI修改内容无效")?;
        if !EDIT_FIELDS.contains(&field) || !fields.insert(field) {
            crate::bail!("AI返回了不允许或重复的修改字段，原事项未改动");
        }
        *draft.text_field_mut(field).expect("edit field") = value.into();
    }
    validate_edit(&draft)?;
    let current = need(conn, &original.id, "事项已不存在")?;
    if current != original {
        crate::bail!("分析期间事项已有新修改，本次结果未覆盖。请重新打开编辑后再试");
    }
    let (groups, _) = super::tags::catalog(conn)?;
    let catalog = tags::result_catalog(&groups, result);
    draft.tags = tags::apply_changes(&draft.tags, &result["tag_changes"], &catalog, true)?;
    tags::sanitize(&mut draft, Some(&original), &catalog);
    draft.updated = bump(original.updated);
    if draft.title != original.title || draft.description != original.description {
        let at = draft.updated;
        model::mark_content_changed(&mut draft, at);
    }
    if draft.due != original.due {
        draft.deadline_changed = draft.updated;
    }
    draft.resume_processing = original.resume_processing && draft.status == original.status && draft.deferred_until > 0;
    if draft.status != "待处理" {
        draft.deferred_until = 0;
    }
    if ACTIVE.contains(&draft.status.as_str()) && draft.status != original.status {
        draft.archived = false;
    }
    draft.suggested_project_id = result["suggested_project_id"].as_str().unwrap_or("").into();
    draft.suggested_project_reason = result["suggested_project_reason"].as_str().unwrap_or("").into();
    draft.manual = true;
    draft.reason = result["reason"].as_str().unwrap_or("根据你的修改说明更新").chars().take(1000).collect();
    let task = store(conn, &draft)?;
    super::tools::link_tool_calls(conn, result, std::slice::from_ref(&task.id))?;
    audit::write(
        conn,
        "manual_ai_edit",
        &task.id,
        &format!("一句话修改：{text}"),
        &original.payload(),
        &task.payload(),
    )?;
    Ok(task.payload())
}

/// `generate {text, sources}` or `generate {id}` (retry): the pending draft
/// the analysis domain then formats with the `format` prompt.
pub fn generation_start(conn: &Connection, v: &Value) -> Result<Task> {
    if let Some(id) = v["id"].as_str() {
        let mut t = need(conn, id, "事项不存在")?;
        if !["failed", "interrupted"].contains(&t.generation.as_str()) {
            crate::bail!("该事项不需要重试");
        }
        t.generation = "pending".into();
        t.generation_error.clear();
        t.updated = bump(t.updated);
        return store(conn, &t);
    }
    let text = generation_text(v)?;
    let at = time::now();
    let mut t = Task {
        id: model::new_id(),
        title: text.chars().take(32).collect(),
        description: text.into(),
        status: "待处理".into(),
        generation: "pending".into(),
        manual: true,
        follow_deadline: true,
        created: at,
        updated: at,
        ..Default::default()
    };
    t.evidence.direct = live_messages(conn, &crate::params::Params::strings(v, "sources"))?;
    model::normalize(&mut t);
    let task = store(conn, &t)?;
    audit::write(conn, "generation_start", &task.id, "自然语言新建，后台整理", &Value::Null, &task.payload())?;
    Ok(task)
}

/// The description check of `generate`, before any write.
pub fn generation_text(v: &Value) -> Result<&str> {
    let text = v["text"].as_str().unwrap_or("").trim();
    if text.is_empty() || text.chars().count() > 6000 {
        crate::bail!("请输入1–6000字的事项描述");
    }
    Ok(text)
}

/// Replace a pending draft with the tasks the AI formatted, or mark it
/// failed. Nothing happens when the draft changed meanwhile.
pub fn generation_complete(conn: &Connection, draft: &Task, result: Result<Value>) -> Result<usize> {
    let current = get(conn, &draft.id)?;
    if !current.is_some_and(|t| t.updated == draft.updated && t.generation == "pending") {
        return Ok(0);
    }
    let mut links = Vec::new();
    let parsed = result.and_then(|value| -> Result<Vec<Task>> {
        let (groups, _) = super::tags::catalog(conn)?;
        let catalog = tags::result_catalog(&groups, &value);
        let rows = value["tasks"].as_array().ok_or("AI结果缺少事项列表")?;
        if rows.is_empty() {
            crate::bail!("AI未生成事项，请重试");
        }
        let mut tasks = Vec::new();
        for row in rows {
            let mut t: Task = from_json(row)?;
            if t.title.trim().is_empty() {
                crate::bail!("AI生成了空标题，请重试");
            }
            if !t.due.is_empty() && model::deadline_ms(&t.due).is_none() {
                t.due.clear();
            }
            t.id = if tasks.is_empty() { draft.id.clone() } else { model::new_id() };
            t.created = draft.created;
            t.updated = bump(draft.updated);
            t.manual = true;
            t.status = "待处理".into();
            t.generation.clear();
            t.generation_error.clear();
            t.archived = false;
            t.merged_into.clear();
            t.evidence = draft.evidence.clone();
            t.reason = "根据你的描述整理".into();
            let at = t.updated;
            model::mark_content_changed(&mut t, at);
            model::normalize(&mut t);
            tags::sanitize(&mut t, None, &catalog);
            let mut link = json!({"_tool_run_id": value.get("_tool_run_id")});
            if let Some(ids) = row.get("tool_call_ids") {
                link["tool_call_ids"] = ids.clone();
            } else if rows.len() > 1 {
                link["tool_call_ids"] = json!([]);
            }
            links.push(link);
            tasks.push(t);
        }
        Ok(tasks)
    });
    match parsed {
        Ok(tasks) => {
            for (t, link) in tasks.iter().zip(&links) {
                let task = store(conn, t)?;
                super::tools::link_tool_calls(conn, link, std::slice::from_ref(&task.id))?;
                audit::write(
                    conn,
                    "generation_done",
                    &task.id,
                    "自然语言事项已生成",
                    &draft.payload(),
                    &task.payload(),
                )?;
            }
            super::activity::log(conn, "generation_result", &json!({"count": tasks.len()}))?;
            Ok(tasks.len())
        }
        Err(e) => {
            let cancelled = e.0 == exec::CANCELLED;
            let mut t = draft.clone();
            t.generation = if cancelled { "interrupted" } else { "failed" }.into();
            t.generation_error = e.0.clone();
            t.updated = bump(draft.updated);
            save(conn, &t)?;
            let kind = if cancelled { "generation_interrupted" } else { "generation_failed" };
            audit::write(conn, kind, &draft.id, &e.0, &draft.payload(), &Value::Null)?;
            Ok(0)
        }
    }
}

/// A restart ends every pending generation; the user can retry it.
pub fn interrupt_generations(conn: &Connection) -> Result<usize> {
    let pending = list(conn, "generation = 'pending'")?;
    for t in &pending {
        generation_complete(conn, t, Err(Error::new("软件重启，任务已中断")))?;
    }
    Ok(pending.len())
}

/// Tasks of the `task_rows` view (aliased `tasks`) matching `tail`, the SQL
/// after `WHERE` (with any ORDER BY / LIMIT).
pub fn select(conn: &Connection, tail: &str, p: impl Params) -> Result<Vec<Task>> {
    let rows =
        rows::objects(conn, &format!("SELECT * FROM task_rows AS tasks WHERE {tail}"), p, &json!(Task::default()))?;
    rows.iter()
        .map(|v| {
            let mut t: Task = from_json(v)?;
            (t.number, t.status_changed_at) =
                (v["number"].as_i64().unwrap_or(0), v["status_changed_at"].as_i64().unwrap_or(0));
            Ok(t)
        })
        .collect()
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Task>> {
    Ok(select(conn, "id = ?1", [id])?.pop())
}

/// Tasks matching a `WHERE` clause (or all with `"1"`), in number order.
pub fn list(conn: &Connection, filter: &str) -> Result<Vec<Task>> {
    select(conn, &format!("{filter} ORDER BY number"), [])
}

/// Insert or replace a task with its tags and evidence. A new task takes the
/// next number; `status_changed_at` moves only when the status changes.
pub fn save(conn: &Connection, t: &Task) -> Result<()> {
    let (number, changed): (i64, i64) = conn.query_row(
        "SELECT coalesce((SELECT number FROM tasks WHERE id = ?1), (SELECT coalesce(max(number), 0) + 1 FROM tasks)),
                coalesce((SELECT status_changed_at FROM tasks WHERE id = ?1 AND status = ?2), ?3)",
        params![t.id, t.status, time::now()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut row = json!(t);
    (row["number"], row["status_changed_at"], row["due_ms"]) =
        (json!(number), json!(changed), json!(deadline_ms(&t.due)));
    rows::put(conn, "tasks", "id", &row)?;
    conn.execute("DELETE FROM task_tags WHERE task_id = ?1", [&t.id])?;
    conn.execute(
        "INSERT OR IGNORE INTO task_tags(task_id, kind, value) SELECT ?1, value ->> 'kind', value ->> 'value' FROM json_each(?2)",
        params![t.id, json!(t.tags).to_string()],
    )?;
    conn.execute("DELETE FROM task_evidence WHERE task_id = ?1", [&t.id])?;
    for (role, ids) in
        [("direct", &t.evidence.direct), ("background", &t.evidence.background), ("completion", &t.evidence.completion)]
    {
        conn.execute(
            "INSERT OR IGNORE INTO task_evidence(task_id, message_id, role, position) SELECT ?1, value, ?2, key FROM json_each(?3)",
            params![t.id, role, json!(ids).to_string()],
        )?;
    }
    Ok(())
}

/// Save and return the stored payload (with its number and status time).
pub fn store(conn: &Connection, t: &Task) -> Result<Task> {
    save(conn, t)?;
    get(conn, &t.id)?.ok_or_else(|| Error::new("事项不存在"))
}
