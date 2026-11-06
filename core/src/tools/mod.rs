//! Local tools: the registry, command execution, per-request sessions and
//! the call records. Approval is the tool's enabled switch plus a live
//! recheck right before each process starts; content the model reads never
//! grants a call.

mod history;
mod registry;
mod runner;
pub mod session;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{exec, Ctx, Error, Result};

pub(crate) use history::page_of;
pub use history::{HistoryQuery, Run};
pub use registry::Tool;
pub use runner::CommandResult;
pub use session::{CallRecord, Failure, RunMeta, Session};

#[derive(Default)]
struct TestState {
    running: Option<(String, Arc<AtomicBool>)>,
    /// Tests cancelled before they started.
    cancelled: VecDeque<String>,
}

/// In-memory state of the tools domain.
#[derive(Default)]
pub struct Runtime {
    rates: Mutex<BTreeMap<String, VecDeque<Instant>>>,
    tests: Mutex<TestState>,
    /// Runs whose session is alive and can still record a final state.
    pub(crate) active: Mutex<BTreeSet<String>>,
}

impl Runtime {
    /// Count one process start against the tool's per-minute limit.
    pub(crate) fn charge(&self, tool: &Tool) -> Result<()> {
        let at = Instant::now();
        let mut rates = self.rates.lock().unwrap();
        rates.retain(|_, queue| {
            while queue.front().is_some_and(|t| at.duration_since(*t) >= Duration::from_secs(60)) {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        let key = if tool.id.is_empty() { "__draft__" } else { tool.id.as_str() };
        let queue = rates.entry(key.into()).or_default();
        if queue.len() >= registry::CALLS_PER_MINUTE {
            crate::bail!("已达到此工具每分钟调用上限，未执行命令");
        }
        queue.push_back(at);
        Ok(())
    }

    fn live(&self, run: &history::Run) -> bool {
        let id = if run.request_id.is_empty() { &run.id } else { &run.request_id };
        self.active.lock().unwrap().contains(id)
    }
}

pub const METHODS: &[&str] = &[
    "tool_list",
    "tool_making_prompt",
    "tool_save",
    "tool_toggle",
    "tool_delete",
    "tool_command_preview",
    "tool_test",
    "tool_test_cancel",
    "tool_run_get",
    "tool_run_history",
];

pub async fn handle(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    match method {
        "tool_list" => Ok(json!(ctx.db.tools().await?)),
        "tool_making_prompt" => registry::making_prompt(&p),
        "tool_save" => {
            let tool = parse_tool(&p)?;
            tool.validate(tool.enabled)?;
            Ok(json!(ctx.db.save_tool(tool).await?))
        }
        "tool_toggle" => {
            let enabled = p.get("enabled").and_then(Value::as_bool).ok_or_else(|| Error::new("缺少开关状态"))?;
            Ok(json!(ctx.db.toggle_tool(edited_id(&p), p["updated"].as_i64(), enabled).await?))
        }
        "tool_delete" => {
            ctx.db.delete_tool(edited_id(&p), p["updated"].as_i64()).await?;
            Ok(json!(true))
        }
        "tool_command_preview" => {
            let tool = parse_tool(&p)?;
            Ok(json!({"command": runner::render(&tool, &p["arguments"])?}))
        }
        "tool_test" => {
            let ctx = ctx.clone();
            exec::blocking(move || test(&ctx, &p)).await
        }
        "tool_test_cancel" => Ok(test_cancel(ctx, &p)),
        "tool_run_get" => history::get(ctx, &p).await,
        "tool_run_history" => history::history(ctx, &p).await,
        _ => Err(Error::new("未知请求")),
    }
}

fn parse_tool(p: &Value) -> Result<Tool> {
    Ok(serde_json::from_value(p["tool"].clone())?)
}

fn edited_id(p: &Value) -> String {
    p["id"].as_str().unwrap_or("").into()
}

fn test_id(p: &Value) -> Option<&str> {
    p["test_id"].as_str().filter(|s| !s.is_empty() && s.len() <= 100)
}

/// Run a draft tool once. Blocks; runs on a blocking thread.
fn test(ctx: &Arc<Ctx>, p: &Value) -> Result<Value> {
    let tool = parse_tool(p)?;
    let id = test_id(p).ok_or_else(|| Error::new("缺少测试编号"))?;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut tests = ctx.tools.tests.lock().unwrap();
        if tests.running.is_some() {
            crate::bail!("已有工具测试运行中，请等待或取消");
        }
        if let Some(i) = tests.cancelled.iter().position(|c| c == id) {
            tests.cancelled.remove(i);
            cancel.store(true, Ordering::SeqCst);
        }
        tests.running = Some((id.into(), cancel.clone()));
    }
    let timeout = Duration::from_secs(ctx.config.get().ai_timeout_seconds);
    let session = Session::new(ctx, id, "tool_test", "", vec![tool.clone()], Value::Null);
    let result: Result<Value> = (|| {
        session
            .call(&tool.name, id, &p["arguments"], &cancel, Instant::now() + timeout, None)
            .map_err(|_| Error::new("工具测试失败，请查看调用记录"))?;
        let last = ctx.db.tool_calls_blocking(id.into())?.pop().and_then(|c| c.result);
        Ok(json!(last.ok_or_else(|| Error::new("调用结果未保存"))?))
    })();
    let finished = session.finish(result.as_ref().err().map(|e| e.0.as_str()), &Value::Null);
    ctx.tools.tests.lock().unwrap().running = None;
    finished?;
    result
}

fn test_cancel(ctx: &Ctx, p: &Value) -> Value {
    if let Some(id) = test_id(p) {
        let mut tests = ctx.tools.tests.lock().unwrap();
        if let Some((_, cancel)) = tests.running.as_ref().filter(|(current, _)| current == id) {
            cancel.store(true, Ordering::SeqCst);
        } else {
            tests.cancelled.push_back(id.into());
            while tests.cancelled.len() > 32 {
                tests.cancelled.pop_front();
            }
        }
    }
    json!(true)
}
