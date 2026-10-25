//! Shared command sessions and local tool management. SS owns every process.
use crate::{
    engine::App,
    model::*,
    tool_registry::{self, ToolDefinition},
    tool_runner::{self, CommandResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shishi_api::{ToolFailure, ToolFailureKind, ToolInvocation, ToolResult};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Runtime {
    rates: Mutex<BTreeMap<String, VecDeque<Instant>>>,
    tests: Mutex<TestState>,
    pub(crate) mcp_started: AtomicBool,
}
#[derive(Default)]
struct TestState {
    running: Option<(String, Arc<AtomicBool>)>,
    cancelled: VecDeque<String>,
}
impl Runtime {
    pub(crate) fn testing(&self) -> bool {
        self.tests.lock().unwrap().running.is_some()
    }
    fn charge(&self, tool: &ToolDefinition) -> Result<(), String> {
        let mut rates = self.rates.lock().unwrap();
        let at = Instant::now();
        let queue = rates
            .entry(
                if tool.id.is_empty() {
                    "__draft__"
                } else {
                    &tool.id
                }
                .into(),
            )
            .or_default();
        while queue
            .front()
            .is_some_and(|t| at.duration_since(*t) >= Duration::from_secs(60))
        {
            queue.pop_front();
        }
        if queue.len() >= tool_registry::CALLS_PER_MINUTE {
            return Err("已达到此工具每分钟调用上限，未执行命令".into());
        }
        queue.push_back(at);
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallRecord {
    /// Explicit per-call attribution; legacy batch links are not guessed.
    pub task_ids: Vec<String>,
    pub call_id: String,
    pub tool_id: String,
    pub finished: i64,
    pub tool: String,
    pub arguments: Value,
    pub started: i64,
    pub result: Option<CommandResult>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolRun {
    pub id: String,
    pub request_id: String,
    pub purpose: String,
    pub source: Value,
    pub task_id: String,
    pub related_task_ids: Vec<String>,
    pub prompt_version: String,
    pub tools: Vec<ToolDefinition>,
    pub context: Value,
    pub instruction: String,
    pub started: i64,
    pub finished: i64,
    pub status: String,
    pub calls: Vec<CallRecord>,
    pub answer: String,
    pub error: String,
    pub usage: Value,
}
impl ToolRun {
    pub fn relates_to_task(&self, id: &str) -> bool {
        self.task_id == id
            || self
                .calls
                .iter()
                .any(|call| self.call_relates_to_task(call, id))
    }
    pub fn call_relates_to_task(&self, call: &CallRecord, id: &str) -> bool {
        if !self.task_id.is_empty() {
            return self.task_id == id;
        }
        if !call.task_ids.is_empty() {
            return call.task_ids.iter().any(|task| task == id);
        }
        self.related_task_ids.len() == 1 && self.related_task_ids[0] == id
    }
    pub fn task_ids(&self) -> BTreeSet<&str> {
        self.calls
            .iter()
            .flat_map(|call| {
                if !self.task_id.is_empty() {
                    vec![self.task_id.as_str()]
                } else if !call.task_ids.is_empty() {
                    call.task_ids.iter().map(String::as_str).collect()
                } else if self.related_task_ids.len() == 1 {
                    vec![self.related_task_ids[0].as_str()]
                } else {
                    vec![]
                }
            })
            .collect()
    }
}
pub(crate) fn link_tasks(state: &mut State, result: &Value, task_ids: &[String]) {
    let Some(id) = result["_tool_run_id"].as_str() else {
        return;
    };
    let Some(run) = state
        .tool_runs
        .iter_mut()
        .find(|run| run.id == id && !run.calls.is_empty())
    else {
        return;
    };
    let call_ids = result.get("tool_call_ids");
    for call in &mut run.calls {
        let selected = match call_ids {
            Some(value) => value.as_array().is_some_and(|ids| {
                ids.iter()
                    .any(|id| id.as_str() == Some(call.call_id.as_str()))
            }),
            None => task_ids.len() == 1,
        };
        if selected {
            for id in task_ids {
                if !id.is_empty() && !call.task_ids.contains(id) {
                    call.task_ids.push(id.clone());
                }
            }
        }
    }
}
pub fn recover(state: &mut State) {
    for run in state.tool_runs.iter_mut().filter(|r| r.status == "running") {
        run.status = "interrupted".into();
        run.finished = now();
        run.error = "软件上次退出前运行未完成；已执行操作的结果可能不确定，不会自动重放".into();
    }
}
fn history_tool_id<'a>(run: &'a ToolRun, call: &'a CallRecord) -> &'a str {
    if !call.tool_id.is_empty() {
        return &call.tool_id;
    }
    run.tools
        .iter()
        .find(|tool| tool.name == call.tool)
        .map(|tool| tool.id.as_str())
        .unwrap_or("")
}
fn history_call_matches(run: &ToolRun, call: &CallRecord, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let purpose = match run.purpose.as_str() {
        "classify" => "事项分析",
        "format" => "事项生成",
        "edit" => "一句话修改",
        "archive" => "归档",
        "tool_test" => "测试运行",
        "tool_call" => "手动工具调用",
        "handling" => "AI 一键处置",
        _ => "历史记录",
    };
    let status = match call.result.as_ref().map(|r| r.status.as_str()) {
        Some("completed") => "执行完成",
        Some("nonzero_exit") => "非零退出",
        Some("start_failed") => "启动失败",
        Some("output_error") => "输出解码/读取失败",
        Some("timed_out") => "超时",
        Some("cancelled") => "已取消",
        Some("rejected") => "执行前拒绝",
        Some("running") => "运行中",
        Some("failed") => "运行失败",
        Some("interrupted") => "已中断",
        _ => "结果未确定",
    };
    let content = json!({"tool":call.tool,"tool_id":history_tool_id(run,call),"arguments":call.arguments,"result":call.result,"purpose":run.purpose,"request_id":run.request_id,"call_id":call.call_id});
    let text = format!("{content} {purpose} {status}").to_lowercase();
    terms.iter().all(|term| text.contains(term))
}
impl App {
    pub fn tool_run_history(&self, p: &Value) -> Result<Value, String> {
        let calls_view = p["view"] == "calls";
        let terms = crate::task_history::terms(p)?;
        let task_id = p["task_id"].as_str().filter(|s| !s.is_empty());
        let tool_id = p["tool_id"].as_str().filter(|s| !s.is_empty());
        let purpose = p["purpose"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "all");
        let day = p["day"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|day| {
                let date =
                    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| "日期无效")?;
                if date.to_string() != day {
                    return Err("日期无效");
                }
                let start = date
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis()
                    - 8 * 3_600_000;
                Ok((start, start + 86_400_000))
            })
            .transpose()?;
        let state = self.state.lock().unwrap();
        let matching = state
            .tool_runs
            .iter()
            .rev()
            .filter(|r| {
                task_id.is_none_or(|id| r.relates_to_task(id))
                    && purpose.is_none_or(|purpose| r.purpose == purpose)
                    && (calls_view
                        || day.is_none_or(|(start, end)| r.started >= start && r.started < end))
            })
            .collect::<Vec<_>>();
        let mut tools = BTreeMap::new();
        for run in &matching {
            for call in &run.calls {
                if task_id.is_some_and(|id| !run.call_relates_to_task(call, id)) {
                    continue;
                }
                let id = if call.tool_id.is_empty() {
                    run.tools
                        .iter()
                        .find(|t| t.name == call.tool)
                        .map(|t| t.id.as_str())
                        .unwrap_or("")
                } else {
                    &call.tool_id
                };
                if !id.is_empty() {
                    tools
                        .entry(id.to_owned())
                        .or_insert_with(|| call.tool.clone());
                }
            }
        }
        let tools = tools
            .into_iter()
            .map(|(id, name)| json!({"id":id,"name":name}))
            .collect::<Vec<_>>();
        if calls_view {
            let mut calls = matching
                .into_iter()
                .flat_map(|run| {
                    run.calls
                        .iter()
                        .enumerate()
                        .map(move |(index, call)| (run, index, call))
                })
                .filter(|(run, _, call)| {
                    task_id.is_none_or(|id| run.call_relates_to_task(call, id))
                        && tool_id.is_none_or(|id| history_tool_id(run, call) == id)
                        && day
                            .is_none_or(|(start, end)| call.started >= start && call.started < end)
                        && history_call_matches(run, call, &terms)
                })
                .collect::<Vec<_>>();
            calls.sort_by_key(|(_, _, call)| std::cmp::Reverse(call.started));
            let total = calls.len();
            let pages = total.max(1).div_ceil(20);
            let page = p["page"].as_u64().unwrap_or(1).max(1).min(pages as u64) as usize;
            let rows = calls.into_iter().skip((page - 1) * 20).take(20).map(|(run, index, call)| {
                let result = call.result.as_ref();
                json!({"run_id":run.id,"call_index":index,"call_id":call.call_id,"tool_id":history_tool_id(run,call),"tool":call.tool,"arguments":call.arguments,"started":call.started,"finished":call.finished,"purpose":run.purpose,"status":result.map(|r| r.status.as_str()).unwrap_or("unknown"),"exit_code":result.and_then(|r| r.exit_code),"duration_ms":result.map(|r| r.duration_ms)})
            }).collect::<Vec<_>>();
            return Ok(json!({"rows":rows,"total":total,"page":page,"pages":pages,"tools":tools}));
        }
        let rows = matching
            .into_iter()
            .filter(|r| {
                (terms.is_empty()
                    || r.calls.iter().any(|call| {
                        task_id.is_none_or(|id| r.call_relates_to_task(call, id))
                            && history_call_matches(r, call, &terms)
                    }))
                    && tool_id.is_none_or(|id| {
                        r.calls.iter().any(|call| {
                            task_id.is_none_or(|task| r.call_relates_to_task(call, task))
                                && history_tool_id(r, call) == id
                        })
                    })
            })
            .collect::<Vec<_>>();
        let total = rows.len();
        let pages = total.max(1).div_ceil(20);
        let page = p["page"].as_u64().unwrap_or(1).max(1).min(pages as u64) as usize;
        let rows=rows.into_iter().skip((page-1)*20).take(20).map(|r|json!({"id":r.id,"started":r.started,"status":r.status,"call_count":r.calls.iter().filter(|call|task_id.is_none_or(|id|r.call_relates_to_task(call,id))).count(),"purpose":r.purpose,"task_id":r.task_id})).collect::<Vec<_>>();
        Ok(json!({"rows":rows,"total":total,"page":page,"pages":pages,"tools":tools}))
    }
    pub fn tool_command_preview(&self, p: &Value) -> Result<Value, String> {
        let tool: ToolDefinition =
            serde_json::from_value(p["tool"].clone()).map_err(|e| e.to_string())?;
        Ok(json!({"command":tool_runner::render(&tool,&p["arguments"])?}))
    }
    pub fn tool_test(&self, p: &Value) -> Result<Value, String> {
        let tool: ToolDefinition =
            serde_json::from_value(p["tool"].clone()).map_err(|e| e.to_string())?;
        let test_id = p["test_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 100)
            .ok_or("缺少测试编号")?;
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut tests = self.tool_runtime.tests.lock().unwrap();
            if tests.running.is_some() {
                return Err("已有工具测试运行中，请等待或取消".into());
            }
            if let Some(i) = tests.cancelled.iter().position(|id| id == test_id) {
                tests.cancelled.remove(i);
                cancel.store(true, Ordering::SeqCst);
            }
            tests.running = Some((test_id.into(), cancel.clone()));
        }
        let config = self.state.lock().unwrap().config.clone();
        let mut session = ToolSession::new(
            self,
            test_id,
            "tool_test",
            "",
            vec![tool.clone()],
            &config,
            0,
        );
        session.source = Value::Null;
        let result = (|| {
            session
                .execute(
                    &tool.name,
                    test_id,
                    &p["arguments"],
                    &cancel,
                    Instant::now() + Duration::from_secs(config.ai_timeout_seconds),
                )
                .map_err(|_| "工具测试失败，请查看调用记录".to_string())?;
            let state = self.state.lock().unwrap();
            Ok(json!(state
                .tool_runs
                .iter()
                .find(|r| r.id == test_id)
                .and_then(|r| r.calls.last())
                .and_then(|c| c.result.as_ref())
                .ok_or("调用结果未保存")?))
        })();
        let finished = session.finish(result.as_ref().err().map(String::as_str), &Value::Null);
        self.tool_runtime.tests.lock().unwrap().running = None;
        finished?;
        result
    }
    pub fn tool_test_cancel(&self, p: &Value) -> Value {
        if let Some(id) = p["test_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 100)
        {
            let mut tests = self.tool_runtime.tests.lock().unwrap();
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
    pub fn tool_run_get(&self, p: &Value) -> Result<Value, String> {
        let active = self
            .active_ai
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.id.clone());
        let state = self.state.lock().unwrap();
        let run = state.tool_runs.iter().rev().find(|r| {
            if let Some(id) = p["run_id"].as_str() {
                r.id == id
            } else {
                p["task_id"]
                    .as_str()
                    .is_some_and(|id| r.relates_to_task(id))
            }
        });
        let Some(run) = run else {
            return Ok(Value::Null);
        };
        let task_id = p["task_id"].as_str().filter(|id| !id.is_empty());
        if task_id.is_some_and(|id| !run.relates_to_task(id)) {
            return Err("该调用不属于当前事项".into());
        }
        let mut value = json!({"id":run.id,"request_id":run.request_id,"purpose":run.purpose,"source":run.source,"task_id":run.task_id,"prompt_version":run.prompt_version,"started":run.started,"finished":run.finished,"status":run.status,"answer":run.answer,"error":run.error,"usage":run.usage});
        if run.status == "running"
            && active.as_deref()
                != Some(if run.request_id.is_empty() {
                    run.id.as_str()
                } else {
                    run.request_id.as_str()
                })
            && !(run.purpose == "tool_test" && self.tool_runtime.testing())
        {
            value["status"] = json!("interrupted");
            value["error"] =
                json!("运行已结束但终态未能保存，请查看日志。已执行命令不会自动重放。");
        }
        // Execution templates are local configuration; the reading pane needs
        // only the advertised subset, never a second editable authority.
        value["tools"] = json!(run
            .tools
            .iter()
            .map(ToolDefinition::declaration)
            .collect::<Vec<_>>());
        if let Some(index) = p.get("call_index") {
            let index = index.as_u64().ok_or("调用序号无效")?;
            let call = run.calls.get(index as usize).ok_or("调用记录已清理")?;
            if task_id.is_some_and(|id| !run.call_relates_to_task(call, id)) {
                return Err("该调用不属于当前事项".into());
            }
            value["calls"] = json!([call]);
            value["call_index"] = json!(index);
            value["call_count"] = json!(1);
            value["page"] = json!(1);
            value["pages"] = json!(1);
            return Ok(value);
        }
        let calls = run
            .calls
            .iter()
            .filter(|call| task_id.is_none_or(|id| run.call_relates_to_task(call, id)))
            .collect::<Vec<_>>();
        let page = p["page"]
            .as_u64()
            .unwrap_or(1)
            .max(1)
            .min((calls.len().max(1).div_ceil(20)) as u64) as usize;
        value["calls"] = json!(calls
            .iter()
            .skip((page - 1) * 20)
            .take(20)
            .collect::<Vec<_>>());
        value["call_count"] = json!(calls.len());
        value["page"] = json!(page);
        value["pages"] = json!(calls.len().max(1).div_ceil(20));
        Ok(value)
    }
}
/// One execution authority shared by manual tests, API callbacks and local MCP bridges.
pub(crate) struct ToolSession<'a> {
    app: &'a App,
    pub run_id: String,
    request_id: String,
    purpose: String,
    task_id: String,
    tools: Vec<ToolDefinition>,
    source: Value,
    max_input_chars: usize,
    continuation_chars: Mutex<usize>,
    pub budget_exhausted: AtomicBool,
    pub stopped: AtomicBool,
}
impl<'a> ToolSession<'a> {
    pub(crate) fn handling(
        app: &'a App,
        ticket: &crate::context_mcp::Ticket,
        tools: Vec<ToolDefinition>,
        config: &Config,
    ) -> Self {
        let mut session = Self::new(app, &ticket.run, "handling", &ticket.task, tools, config, 0);
        session.source = json!({"driver":"mcp","label":"MCP"});
        session
    }
    pub fn new(
        app: &'a App,
        request: &str,
        purpose: &str,
        task: &str,
        tools: Vec<ToolDefinition>,
        config: &Config,
        prompt_chars: usize,
    ) -> Self {
        Self {
            app,
            run_id: request.into(),
            request_id: request.into(),
            purpose: purpose.into(),
            task_id: task.into(),
            tools,
            source: crate::ai_backend::source_info(config),
            max_input_chars: if purpose == "tool_test" {
                usize::MAX
            } else {
                config.max_input_chars
            },
            continuation_chars: Mutex::new(prompt_chars),
            budget_exhausted: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }
    fn run_mut<'s>(&self, state: &'s mut State) -> &'s mut ToolRun {
        let index = match state.tool_runs.iter().position(|run| run.id == self.run_id) {
            Some(index) => index,
            None => {
                state.tool_runs.push(ToolRun {
                    id: self.run_id.clone(),
                    request_id: self.request_id.clone(),
                    purpose: self.purpose.clone(),
                    source: self.source.clone(),
                    task_id: self.task_id.clone(),
                    tools: self.tools.clone(),
                    started: now(),
                    status: "running".into(),
                    ..Default::default()
                });
                state.tool_runs.len() - 1
            }
        };
        &mut state.tool_runs[index]
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.name == name)
    }
    pub fn check_batch(&self, calls: &[ToolInvocation<'_>]) -> Result<(), ToolFailure> {
        for call in calls {
            let tool = self
                .tools
                .iter()
                .find(|t| t.name == call.name())
                .ok_or(ToolFailure::new(ToolFailureKind::Denied))?;
            // Validate every render before the first side effect in this reply.
            if let Err(error) = tool_runner::render(tool, call.arguments()) {
                self.app
                    .commit_state(|state| {
                        let run = self.run_mut(state);
                        for rejected in calls {
                            run.calls.push(CallRecord {
                                task_ids: Vec::new(),
                                call_id: rejected.call_id().into(),
                                tool_id: self
                                    .tools
                                    .iter()
                                    .find(|t| t.name == rejected.name())
                                    .map(|t| t.id.clone())
                                    .unwrap_or_default(),
                                finished: now(),
                                tool: rejected.name().into(),
                                arguments: rejected.arguments().clone(),
                                started: now(),
                                result: Some(CommandResult::rejected(
                                    rejected.name(),
                                    rejected.call_id(),
                                    format!("本批未执行：{error}"),
                                )),
                            });
                        }
                        Ok(())
                    })
                    .map_err(|_| ToolFailure::new(ToolFailureKind::Failed))?;
                for call in calls {
                    if let Some(tool) = self.tools.iter().find(|t| t.name == call.name()) {
                        self.log_call(
                            tool,
                            call.call_id(),
                            "tool_call_finished",
                            Some(&CommandResult::rejected(
                                call.name(),
                                call.call_id(),
                                format!("本批未执行：{error}"),
                            )),
                        );
                    }
                }
                return Err(ToolFailure::new(ToolFailureKind::Failed));
            }
        }
        Ok(())
    }
    pub(crate) fn record_rejection(&self, value: &Value) -> Result<(), String> {
        let calls = value["response"]["tool_calls"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut recorded = Vec::new();
        self.app.commit_state(|state| {
            let run = self.run_mut(state);
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let id = call["id"].as_str().unwrap_or("");
                if run.calls.iter().any(|c| c.call_id == id) {
                    continue;
                }
                let tool = self.tools.iter().find(|t| t.name == name);
                let result = CommandResult::rejected(
                    name,
                    id,
                    value["error"].as_str().unwrap_or("工具参数无效").into(),
                );
                let arguments = call["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| call["function"]["arguments"].clone());
                run.calls.push(CallRecord {
                    task_ids: Vec::new(),
                    call_id: id.into(),
                    tool_id: tool.map(|t| t.id.clone()).unwrap_or_default(),
                    tool: name.into(),
                    arguments,
                    started: now(),
                    finished: now(),
                    result: Some(result.clone()),
                });
                recorded.push((
                    tool.cloned().unwrap_or_else(|| ToolDefinition {
                        name: name.into(),
                        ..Default::default()
                    }),
                    id.to_owned(),
                    result,
                ));
            }
            Ok(())
        })?;
        for (tool, id, result) in recorded {
            self.log_call(&tool, &id, "tool_call_finished", Some(&result));
        }
        Ok(())
    }
    fn log_call(
        &self,
        tool: &ToolDefinition,
        call: &str,
        event: &str,
        result: Option<&CommandResult>,
    ) {
        self.app.log(event,json!({"kind":self.purpose,"request_id":self.request_id,"run_id":self.run_id,"call_id":call,"tool_id":tool.id,"tool":tool.name,"task_id":self.task_id,"source":self.source,"status":result.map(|r|&r.status),"elapsed_ms":result.map(|r|r.duration_ms),"exit_code":result.and_then(|r|r.exit_code),"error":result.map(|r|&r.error)}));
    }
    pub fn finish(&self, error: Option<&str>, usage: &Value) -> Result<(), String> {
        // Most AI calls use no command. Do not create or save empty execution records.
        if !self
            .app
            .state
            .lock()
            .unwrap()
            .tool_runs
            .iter()
            .any(|r| r.id == self.run_id)
        {
            return Ok(());
        }
        self.app.commit_state(|state| {
            let run = self.run_mut(state);
            run.finished = now();
            run.usage = usage.clone();
            run.status = if error.is_none() {
                "completed"
            } else if error.is_some_and(|e| crate::ai_control::failure_class(e) == "cancelled") {
                "cancelled"
            } else {
                "failed"
            }
            .into();
            run.error = error.unwrap_or_default().into();
            if self.purpose == "tool_test" {
                if let Some(result) = run.calls.last().and_then(|c| c.result.as_ref()) {
                    run.status = result.status.clone();
                    run.error = result.error.clone();
                }
            }
            Ok(())
        })
    }
    pub fn execute(
        &self,
        name: &str,
        call_id: &str,
        arguments: &Value,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<ToolResult, ToolFailure> {
        let fail = || ToolFailure::new(ToolFailureKind::Failed);
        let tool = self
            .tools
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(fail)?;
        // A durable intent precedes execution. If its commit fails, do not run.
        self.app
            .commit_state(|state| {
                let run = self.run_mut(state);
                if run.calls.iter().any(|c| c.call_id == call_id) {
                    return Err("重复调用编号，未重放".into());
                }
                run.calls.push(CallRecord {
                    task_ids: Vec::new(),
                    call_id: call_id.into(),
                    tool_id: tool.id.clone(),
                    finished: 0,
                    tool: name.into(),
                    arguments: arguments.clone(),
                    started: now(),
                    result: None,
                });
                Ok(())
            })
            .map_err(|_| fail())?;
        self.log_call(tool, call_id, "tool_call_started", None);
        let within_budget = {
            let mut chars = self.continuation_chars.lock().unwrap();
            *chars = chars.saturating_add(
                arguments.to_string().chars().count() + name.len() + call_id.len() + 100,
            );
            *chars < self.max_input_chars
        };
        let result = if !within_budget {
            self.budget_exhausted.store(true, Ordering::SeqCst);
            CommandResult::rejected(
                name,
                call_id,
                "本次工具续轮已达到输入预算，未执行命令".into(),
            )
        } else {
            tool_runner::execute_with_start(
                tool,
                call_id,
                arguments,
                || cancel.load(Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst),
                deadline,
                |cmd| {
                    // Hold the authority lock through process creation, so a saved
                    // disable and a command start have a well-defined ordering.
                    let state = self.app.state.lock().unwrap();
                    if self.purpose != "tool_test"
                        && (!state
                            .config
                            .tools
                            .iter()
                            .any(|t| t.id == tool.id && t.enabled)
                            || (!self.task_id.is_empty()
                                && !state
                                    .tasks
                                    .iter()
                                    .any(|t| t.id == self.task_id && t.merged_into.is_empty())))
                    {
                        return Err((true, "工具已停用、删除或事项已不可用，未执行命令".into()));
                    }
                    if cancel.load(Ordering::SeqCst)
                        || self.stopped.load(Ordering::SeqCst)
                        || Instant::now() >= deadline
                    {
                        return Err((true, "运行已取消或到达总期限，未执行命令".into()));
                    }
                    self.app.tool_runtime.charge(tool).map_err(|e| (true, e))?;
                    crate::process::spawn_owned(cmd).map_err(|e| (false, e))
                },
            )
        };
        self.app
            .commit_state(|state| {
                let record = state
                    .tool_runs
                    .iter_mut()
                    .find(|r| r.id == self.run_id)
                    .and_then(|r| r.calls.iter_mut().find(|c| c.call_id == call_id))
                    .ok_or("调用记录丢失")?;
                record.finished = now();
                record.result = Some(result.clone());
                Ok(())
            })
            .map_err(|_| fail())?;
        self.log_call(tool, call_id, "tool_call_finished", Some(&result));
        // Retain the full bounded result locally. Bound the copy returned to
        // the model as well, including JSON escaping; never resend on failure.
        let mut returned = result;
        let target = self
            .max_input_chars
            .saturating_sub(*self.continuation_chars.lock().unwrap())
            .min(60_000)
            / 2;
        if returned.stdout.chars().count() + returned.stderr.chars().count() > target {
            returned.stdout = returned.stdout.chars().take(target / 2).collect();
            returned.stderr = returned.stderr.chars().take(target / 2).collect();
            returned.truncated = true;
        }
        let mut text = serde_json::to_string(&returned).map_err(|_| fail())?;
        while text.len() > shishi_api::openai::MAX_TOOL_RESULT_BYTES {
            returned.stdout = returned
                .stdout
                .chars()
                .take(returned.stdout.chars().count() / 2)
                .collect();
            returned.stderr = returned
                .stderr
                .chars()
                .take(returned.stderr.chars().count() / 2)
                .collect();
            returned.truncated = true;
            text = serde_json::to_string(&returned).map_err(|_| fail())?;
        }
        let mut chars = self.continuation_chars.lock().unwrap();
        *chars = chars.saturating_add(text.chars().count());
        if *chars > self.max_input_chars {
            self.budget_exhausted.store(true, Ordering::SeqCst);
            self.stopped.store(true, Ordering::SeqCst);
            return Err(fail());
        }
        Ok(ToolResult::Text(text))
    }
}
#[cfg(test)]
#[path = "tool_calling_tests.rs"]
mod tests;
