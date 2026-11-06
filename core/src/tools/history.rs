//! Reading runs and calls back: the run detail pane and the history pages.

use serde_json::{json, Value};

use super::registry::Tool;
use super::session::CallRecord;
use crate::{time, Ctx, Error, Result};

pub struct Run {
    pub id: String,
    pub request_id: String,
    pub purpose: String,
    pub source: Value,
    pub task_id: String,
    pub tools: Vec<Tool>,
    pub started: i64,
    pub finished: i64,
    pub status: String,
    pub answer: String,
    pub error: String,
    pub usage: Value,
    /// Calls in order, each with its linked tasks.
    pub calls: Vec<(CallRecord, Vec<String>)>,
}

impl Run {
    /// A run with its own task attributes every call to it; otherwise only
    /// explicit links count.
    pub fn call_relates(&self, links: &[String], id: &str) -> bool {
        if self.task_id.is_empty() {
            links.iter().any(|t| t == id)
        } else {
            self.task_id == id
        }
    }

    pub fn relates(&self, id: &str) -> bool {
        self.task_id == id || self.calls.iter().any(|(_, links)| self.call_relates(links, id))
    }
}

/// A `tool_run_history` request: the lowercased query terms joined by
/// spaces, an optional purpose, task, tool and day, and the wanted page.
pub struct HistoryQuery {
    pub terms: String,
    pub purpose: String,
    pub task: String,
    pub tool: String,
    pub day: Option<(i64, i64)>,
    pub page: Option<u64>,
}

fn call_json(call: &CallRecord, links: &[String]) -> Value {
    json!({
        "task_ids": links, "call_id": call.call_id, "tool_id": call.tool_id, "finished": call.finished,
        "tool": call.tool, "arguments": call.arguments, "started": call.started, "result": call.result,
    })
}

fn terms(p: &Value) -> Result<Vec<String>> {
    let query = p.get("query").and_then(Value::as_str).unwrap_or("");
    if query.chars().count() > 200 {
        crate::bail!("搜索内容不能超过 200 字");
    }
    Ok(query.to_lowercase().split_whitespace().map(str::to_owned).collect())
}

fn day_range(p: &Value) -> Result<Option<(i64, i64)>> {
    let Some(day) = p.get("day").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let start = time::day_start(day).ok_or_else(|| Error::new("日期无效"))?;
    Ok(Some((start, start + time::DAY)))
}

pub(crate) fn page_of(page: Option<u64>, total: usize) -> (usize, usize) {
    let pages = total.max(1).div_ceil(20);
    let page = page.unwrap_or(1).clamp(1, pages as u64) as usize;
    (page, pages)
}

/// `tool_run_history`: the calls page of the tool log.
pub async fn history(ctx: &Ctx, p: &Value) -> Result<Value> {
    let text = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let q = HistoryQuery {
        terms: terms(p)?.join(" "),
        purpose: Some(text("purpose")).filter(|s| s != "all").unwrap_or_default(),
        task: text("task_id"),
        tool: text("tool_id"),
        day: day_range(p)?,
        page: p.get("page").and_then(Value::as_u64),
    };
    ctx.db.tool_run_history(q).await
}

/// `tool_run_get`: one run with its calls; a running run without a live
/// session that can finish it reads as interrupted.
pub async fn get(ctx: &Ctx, p: &Value) -> Result<Value> {
    let run = if let Some(id) = p.get("run_id").and_then(Value::as_str) {
        ctx.db.tool_run(id.into(), false).await?
    } else if let Some(id) = p.get("task_id").and_then(Value::as_str) {
        ctx.db.tool_run(id.into(), true).await?
    } else {
        None
    };
    let Some(run) = run else {
        return Ok(Value::Null);
    };
    let task_id = p.get("task_id").and_then(Value::as_str).filter(|id| !id.is_empty());
    if task_id.is_some_and(|id| !run.relates(id)) {
        crate::bail!("该调用不属于当前事项");
    }
    let mut value = json!({
        "id": run.id, "request_id": run.request_id, "purpose": run.purpose, "source": run.source,
        "task_id": run.task_id, "started": run.started, "finished": run.finished,
        "status": run.status, "answer": run.answer, "error": run.error, "usage": run.usage,
    });
    if run.status == "running" && !ctx.tools.live(&run) {
        value["status"] = json!("interrupted");
        value["error"] = json!("运行已结束但终态未能保存，请查看日志。已执行命令不会自动重放。");
    }
    // The pane shows the advertised subset, never the command templates.
    value["tools"] = run.tools.iter().map(Tool::declaration).collect::<Vec<_>>().into();
    let related = |links: &[String]| task_id.is_none_or(|id| run.call_relates(links, id));
    if let Some(index) = p.get("call_index") {
        let index = index.as_u64().ok_or_else(|| Error::new("调用序号无效"))?;
        let (call, links) = run.calls.get(index as usize).ok_or_else(|| Error::new("调用记录已清理"))?;
        if !related(links) {
            crate::bail!("该调用不属于当前事项");
        }
        value["calls"] = json!([call_json(call, links)]);
        value["call_index"] = index.into();
        value["call_count"] = 1.into();
        value["page"] = 1.into();
        value["pages"] = 1.into();
        return Ok(value);
    }
    let calls: Vec<Value> =
        run.calls.iter().filter(|(_, links)| related(links)).map(|(call, links)| call_json(call, links)).collect();
    let (page, pages) = page_of(p.get("page").and_then(Value::as_u64), calls.len());
    value["call_count"] = calls.len().into();
    value["calls"] = calls.into_iter().skip((page - 1) * 20).take(20).collect::<Vec<_>>().into();
    value["page"] = page.into();
    value["pages"] = pages.into();
    Ok(value)
}
