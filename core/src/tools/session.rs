//! A tool session: the commands one AI request (or one local test) may run.
//! Each call is persisted as an intent before its process starts and is
//! never replayed while an earlier identical call has an uncertain outcome.
//! All methods block; call them from a blocking thread, never async code.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use shishi_api::{ToolFailure, ToolFailureKind};

use super::registry::Tool;
use super::runner::{self, CommandResult};
use crate::{time, Ctx, Error, Result};

/// A failure returned to the model; converts into the provider type.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub kind: ToolFailureKind,
    pub message: Option<String>,
}

impl Failure {
    fn new(kind: ToolFailureKind) -> Self {
        Self { kind, message: None }
    }

    fn with(kind: ToolFailureKind, message: impl Into<String>) -> Self {
        Self { kind, message: Some(message.into()) }
    }
}

impl From<Failure> for ToolFailure {
    fn from(f: Failure) -> Self {
        match f.message {
            Some(m) => ToolFailure::with_message(f.kind, m),
            None => ToolFailure::new(f.kind),
        }
    }
}

/// One stored call; `result` is `None` while its outcome is unknown.
#[derive(Clone, Debug)]
pub struct CallRecord {
    pub call_id: String,
    pub tool_id: String,
    pub tool: String,
    pub arguments: Value,
    pub started: i64,
    pub finished: i64,
    pub result: Option<CommandResult>,
}

/// The newest earlier call with the same tool and canonical arguments
/// whose command may have run without a known result.
fn uncertain_prior_call<'a>(calls: &'a [CallRecord], tool: &Tool, arguments: &Value) -> Option<&'a CallRecord> {
    // Omitted string parameters and explicit empty strings render the same.
    let arguments = tool.arguments(arguments).ok()?;
    calls.iter().rev().find(|call| {
        call.tool_id == tool.id
            && call.tool == tool.name
            && call.result.as_ref().is_none_or(|r| {
                !matches!(r.status.as_str(), "rejected" | "start_failed")
                    && (r.exit_code.is_none()
                        || r.timed_out
                        || r.cancelled
                        || !r.error.is_empty()
                        || !matches!(r.status.as_str(), "completed" | "nonzero_exit"))
            })
            && tool.arguments(&call.arguments).ok().as_ref() == Some(&arguments)
    })
}

/// Why `intent` must not run given the run's stored calls: its id is taken,
/// or an identical earlier call has an uncertain outcome.
pub(crate) fn replay_failure(calls: &[CallRecord], tool: &Tool, intent: &CallRecord) -> Option<Failure> {
    if calls.iter().any(|c| c.call_id == intent.call_id) {
        return Some(Failure::new(ToolFailureKind::Failed));
    }
    uncertain_prior_call(calls, tool, &intent.arguments).map(unsafe_replay)
}

fn unsafe_replay(call: &CallRecord) -> Failure {
    Failure::with(
        ToolFailureKind::UnsafeReplay,
        format!(
            "先前调用 {} 的命令可能已执行，但结果尚未确定（{}）；本次未重放。不要重复相同命令，请依据已有资料完成，并说明无法核实的结果。",
            call.call_id,
            call.result.as_ref().map_or("执行中", |r| r.status.as_str())
        ),
    )
}

fn result_input_chars(text: &str, json_encoded: bool) -> usize {
    if json_encoded {
        serde_json::to_string(text).map_or(0, |s| s.chars().count() - 2)
    } else {
        text.chars().count()
    }
}

/// The result as JSON within `max_chars` (escaped when `json_encoded`) and
/// the provider's byte cap. A result that does not fit keeps the longest
/// prefixes of stdout and stderr that do, sharing the room.
pub(crate) fn tool_result_text(mut result: CommandResult, max_chars: usize, json_encoded: bool) -> Option<String> {
    let fits = |text: &str| {
        text.len() <= shishi_api::openai::MAX_TOOL_RESULT_BYTES
            && text.chars().count() <= max_chars.min(60_000)
            && result_input_chars(text, json_encoded) <= max_chars
    };
    let text = serde_json::to_string(&result).ok()?;
    if fits(&text) {
        return Some(text);
    }
    result.truncated = true;
    let (stdout, stderr) = (std::mem::take(&mut result.stdout), std::mem::take(&mut result.stderr));
    let (o, e) = (stdout.chars().count(), stderr.chars().count());
    let mut render = |k: usize| {
        let err = e.min(k - o.min(k / 2));
        result.stdout = stdout.chars().take(o.min(k - err)).collect();
        result.stderr = stderr.chars().take(err).collect();
        serde_json::to_string(&result).ok().filter(|t| fits(t))
    };
    // The largest kept total that fits; both shares grow with it.
    let (mut low, mut high, mut best) = (0, o + e, None);
    while low < high {
        let mid = (low + high) / 2;
        match render(mid) {
            Some(text) => (best, low) = (Some(text), mid + 1),
            None => high = mid,
        }
    }
    best
}

pub struct Session {
    ctx: Arc<Ctx>,
    pub run_id: String,
    purpose: String,
    task_id: String,
    tools: Vec<Tool>,
    source: Value,
    continuation_limit: usize,
    continuation_chars: Mutex<usize>,
}

impl Session {
    /// `request` is the AI request id and becomes the run id; `tools` are
    /// the definitions frozen for this request.
    pub fn new(ctx: &Arc<Ctx>, request: &str, purpose: &str, task: &str, tools: Vec<Tool>, source: Value) -> Self {
        ctx.tools.active.lock().unwrap().insert(request.to_string());
        Self {
            ctx: ctx.clone(),
            run_id: request.into(),
            purpose: purpose.into(),
            task_id: task.into(),
            tools,
            source,
            continuation_limit: if purpose == "tool_test" {
                usize::MAX
            } else {
                ctx.config.get().retrieval_budget_chars
            },
            continuation_chars: Mutex::new(0),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    fn tool(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    fn log_payload(&self, tool_id: &str, tool: &str, call_id: &str, result: Option<&CommandResult>) -> Value {
        json!({
            "kind": self.purpose, "request_id": self.run_id, "run_id": self.run_id, "call_id": call_id,
            "tool_id": tool_id, "tool": tool, "task_id": self.task_id, "source": self.source,
            "status": result.map(|r| &r.status), "elapsed_ms": result.map(|r| r.duration_ms),
            "exit_code": result.and_then(|r| r.exit_code), "error": result.map(|r| &r.error),
        })
    }

    /// Validate a whole model reply before any of its commands starts.
    pub fn check_batch(&self, batch: &[(&str, &str, &Value)]) -> std::result::Result<(), Failure> {
        for (_, name, arguments) in batch {
            let tool = self.tool(name).ok_or(Failure::new(ToolFailureKind::Denied))?;
            if let Err(error) = runner::render(tool, arguments) {
                let message = format!("本批未执行：{}", error.0);
                let rows = batch
                    .iter()
                    .map(|(call_id, name, arguments)| {
                        let (call, log) = self.rejected(call_id, name, (*arguments).clone(), message.clone());
                        (call, self.contains(name).then_some(log))
                    })
                    .collect();
                self.ctx
                    .db
                    .record_rejected_batch_blocking(self.run_meta(), rows)
                    .map_err(|_| Failure::new(ToolFailureKind::Failed))?;
                return Err(Failure::with(ToolFailureKind::InvalidArguments, error.0));
            }
        }
        let records = self.ctx.db.tool_calls_blocking(self.run_id.clone()).map_err(|_| {
            Failure::with(
                ToolFailureKind::UnsafeReplay,
                "既有工具记录无法读取，无法核实先前命令是否执行。不要重复相同命令，请说明无法核实的结果。",
            )
        })?;
        for (_, name, arguments) in batch {
            if let Some(prior) = self.tool(name).and_then(|t| uncertain_prior_call(&records, t, arguments)) {
                return Err(unsafe_replay(prior));
            }
        }
        Ok(())
    }

    /// Record calls the provider refused before they reached us.
    pub fn record_rejection(&self, value: &Value) -> Result<()> {
        let list = value["response"]["tool_calls"].as_array().into_iter().flatten();
        let reason = value["error"].as_str().unwrap_or("工具参数无效").to_string();
        let rows = list
            .map(|call| {
                let raw = &call["function"]["arguments"];
                let arguments = raw.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| raw.clone());
                let (id, name) = (call["id"].as_str().unwrap_or(""), call["function"]["name"].as_str().unwrap_or(""));
                self.rejected(id, name, arguments, reason.clone())
            })
            .collect();
        self.ctx.db.record_provider_rejections_blocking(self.run_meta(), rows)
    }

    /// A call refused before it ran, with its log row.
    fn rejected(&self, call_id: &str, name: &str, arguments: Value, reason: String) -> (CallRecord, Value) {
        let tool_id = self.tool(name).map(|t| t.id.clone()).unwrap_or_default();
        let result = CommandResult::rejected(name, call_id, reason);
        let log = self.log_payload(&tool_id, name, call_id, Some(&result));
        let at = time::now();
        let call = CallRecord {
            call_id: call_id.into(),
            tool_id,
            tool: name.into(),
            arguments,
            started: at,
            finished: at,
            result: Some(result),
        };
        (call, log)
    }

    /// End the run. A test run takes the status of its last call.
    pub fn finish(&self, error: Option<&str>, usage: &Value) -> Result<()> {
        let status = match error {
            None => "completed",
            Some(e) if crate::ai::failure_class(e) == "cancelled" => "cancelled",
            Some(_) => "failed",
        };
        let test = self.purpose == "tool_test";
        let error = error.unwrap_or_default().to_string();
        self.ctx.db.finish_tool_run_blocking(self.run_id.clone(), test, status.into(), error, usage.clone())
    }

    /// Run one call. `result_budget` is the API caller's shared remainder;
    /// the returned text counts its outer JSON when it is set.
    pub fn call(
        &self,
        name: &str,
        call_id: &str,
        arguments: &Value,
        cancel: &AtomicBool,
        deadline: Instant,
        result_budget: Option<usize>,
    ) -> std::result::Result<String, Failure> {
        let fail = || Failure::new(ToolFailureKind::Failed);
        let tool = self.tool(name).ok_or_else(fail)?.clone();
        // A durable intent precedes execution; the check and the insert share
        // one transaction so a concurrent callback cannot replay it.
        let intent = CallRecord {
            call_id: call_id.into(),
            tool_id: tool.id.clone(),
            tool: name.into(),
            arguments: arguments.clone(),
            started: time::now(),
            finished: 0,
            result: None,
        };
        let started_log = self.log_payload(&tool.id, name, call_id, None);
        let replay = self
            .ctx
            .db
            .start_tool_call_blocking(self.run_meta(), tool.clone(), intent, started_log)
            .map_err(|_| fail())?;
        if let Some(failure) = replay {
            return Err(failure);
        }
        let wrapper = result_budget.map_or(0, |_| {
            shishi_api::openai::message_input_chars(&json!({"role": "tool", "tool_call_id": call_id, "content": ""}))
                + 1
        });
        let content_budget = |used: usize| {
            let remaining = self.continuation_limit.saturating_sub(used);
            result_budget.map_or(remaining, |b| b.min(remaining).saturating_sub(wrapper))
        };
        let within_budget = {
            let mut chars = self.continuation_chars.lock().unwrap();
            if result_budget.is_none() {
                *chars = chars.saturating_add(shishi_api::json::chars(arguments) + name.len() + call_id.len() + 100);
            }
            // Even an empty result with the widest status and numbers must fit
            // before a process starts.
            let probe = CommandResult {
                tool: name.into(),
                call_id: call_id.into(),
                status: "nonzero_exit".into(),
                exit_code: Some(i32::MIN),
                duration_ms: u64::MAX,
                ..Default::default()
            };
            tool_result_text(probe, content_budget(*chars), result_budget.is_some()).is_some()
        };
        let result = if !within_budget {
            CommandResult::rejected(name, call_id, "本次工具续轮已达到输入预算，未执行命令".into())
        } else {
            runner::execute_with_start(
                &tool,
                call_id,
                arguments,
                || cancel.load(Ordering::SeqCst),
                deadline,
                |cmd| {
                    if self.purpose != "tool_test" {
                        self.recheck(&tool).map_err(|e| (true, e.0))?;
                    }
                    if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        return Err((true, "运行已取消或到达总期限，未执行命令".into()));
                    }
                    self.ctx.tools.charge(&tool).map_err(|e| (true, e.0))?;
                    runner::spawn(cmd).map_err(|e| (false, e))
                },
            )
        };
        let finished_log = self.log_payload(&tool.id, name, call_id, Some(&result));
        self.ctx
            .db
            .finish_tool_call_blocking(self.run_id.clone(), call_id.into(), result.clone(), finished_log)
            .map_err(|_| fail())?;
        if !within_budget {
            return Err(Failure::with(
                ToolFailureKind::BudgetExhausted,
                "本次续轮额度不足，命令未执行；请依据已有结果完成。",
            ));
        }
        if result_budget.is_some() && matches!(result.status.as_str(), "rejected" | "start_failed") {
            let kind = if result.status == "rejected" { ToolFailureKind::Denied } else { ToolFailureKind::Unavailable };
            return Err(Failure::with(kind, format!("命令未执行：{}", result.error)));
        }
        let mut chars = self.continuation_chars.lock().unwrap();
        let Some(text) = tool_result_text(result, content_budget(*chars), result_budget.is_some()) else {
            return Err(Failure::with(
                ToolFailureKind::BudgetExhausted,
                "命令结果已保存在调用记录中，但续轮额度不足以完整交付；不要重放命令，请依据已收到的资料完成。",
            ));
        };
        *chars = chars.saturating_add(wrapper + result_input_chars(&text, result_budget.is_some()));
        Ok(text)
    }

    /// Live authority check right before the process starts: the tool must
    /// still be enabled with the same command, and the task still open.
    fn recheck(&self, frozen: &Tool) -> Result<()> {
        let current = self.ctx.db.runnable_tool_blocking(frozen.id.clone(), self.task_id.clone())?;
        let Some(current) = current else {
            return Err(Error::new("工具已停用、删除或事项已不可用，未执行命令"));
        };
        if !current.same_command(frozen) {
            return Err(Error::new("工具执行配置已变更，本轮旧命令未执行；请依据已有资料完成。"));
        }
        Ok(())
    }

    fn run_meta(&self) -> RunMeta {
        RunMeta {
            id: self.run_id.clone(),
            purpose: self.purpose.clone(),
            source: self.source.clone(),
            task_id: self.task_id.clone(),
            tools: self.tools.clone(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.ctx.tools.active.lock().unwrap().remove(&self.run_id);
    }
}

/// What the run row is created from on the first call.
pub struct RunMeta {
    pub id: String,
    pub purpose: String,
    pub source: Value,
    pub task_id: String,
    pub tools: Vec<Tool>,
}
