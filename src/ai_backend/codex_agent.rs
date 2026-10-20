//! Codex session protocol translation. No task state or scheduling lives here.
use super::{AgentSession, ContextTool};
use crate::{codex_bridge::Client, model::Config};
use serde_json::{json, Value};
use std::{path::Path, sync::atomic::Ordering};
fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn context_config(tool: &ContextTool) -> Value {
    json!({"mcp_servers.shishi_context":stable_context(tool)})
}
fn stable_context(tool: &ContextTool) -> Value {
    let root = tool
        .context_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent);
    json!({"command":tool.executable,"args":["--context-mcp-root",root],"enabled":true,"startup_timeout_sec":20,"tool_timeout_sec":20})
}
impl AgentSession for Client {
    fn close(&self) -> Result<(), String> {
        Client::close(self)
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn projects(&self) -> Result<Vec<Value>, String> {
        let mut data = vec![];
        let mut cursor = Value::Null;
        for _ in 0..20 {
            let v = match self.call("project/list", json!({"limit":100,"cursor":cursor})) {
                Ok(v) => v,
                Err(e) if unsupported(&e) => break,
                Err(e) => return Err(e),
            };
            for row in v["data"].as_array().into_iter().flatten() {
                for root in row["roots"].as_array().into_iter().flatten() {
                    if let Some(path) = root["path"].as_str() {
                        if Path::new(path).is_dir() {
                            data.push(json!({"id":row["id"],"name":row["name"],"cwd":path}));
                        }
                    }
                }
            }
            cursor = v["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        Ok(data)
    }
    fn start(
        &self,
        cwd: &str,
        project: &str,
        context: &ContextTool,
        config: &Config,
    ) -> Result<Value, String> {
        self.prepare_handoff("", context, "")?;
        let mut p = json!({"cwd":cwd,"projectId":project,"ephemeral":false,"config":context_config(context)});
        if project.starts_with("local:") {
            p.as_object_mut().unwrap().remove("projectId");
        }
        if !config.model.is_empty() {
            p["model"] = json!(config.model);
        }
        self.call("thread/start", p)
    }
    fn rename(&self, thread: &str, name: &str) -> Result<(), String> {
        self.call("thread/name/set", json!({"threadId":thread,"name":name}))
            .map(|_| ())
    }
    fn submit(
        &self,
        thread: &str,
        text: &str,
        config: &Config,
        message_id: &str,
    ) -> Result<Value, String> {
        self.call("turn/start", turn_params(thread, text, config, message_id))
    }
    fn steer(&self, thread: &str, turn: &str, text: &str, message_id: &str) -> Result<(), String> {
        self.call("turn/steer",json!({"threadId":thread,"expectedTurnId":turn,"clientUserMessageId":message_id,"input":[{"type":"text","text":text,"text_elements":[]}]})).map(|_| ())
    }
    fn read(&self, thread: &str) -> Result<Value, String> {
        self.call("thread/read", json!({"threadId":thread}))
    }
    fn history(&self, thread: &str) -> Result<Value, String> {
        read_turns(self, thread)
    }
    fn resume(&self, thread: &str, context: &ContextTool) -> Result<Value, String> {
        self.call(
            "thread/resume",
            json!({"threadId":thread,"excludeTurns":true,"config":context_config(context)}),
        )
    }
    fn prepare_handoff(
        &self,
        thread: &str,
        context: &ContextTool,
        note: &str,
    ) -> Result<(), String> {
        let config = self.call("config/read", json!({"includeLayers":false}))?;
        let existing = &config["config"]["mcp_servers"]["shishi_context"];
        if existing.is_object()
            && !existing["args"].as_array().is_some_and(|args| {
                args.first()
                    .and_then(Value::as_str)
                    .is_some_and(|s| ["--context-mcp", "--context-mcp-root"].contains(&s))
            })
        {
            return Err("Codex 已有同名 shishi_context 配置，请先处理名称冲突；未覆盖配置".into());
        }
        self.call("config/value/write",json!({"keyPath":"mcp_servers.shishi_context","value":stable_context(context),"mergeStrategy":"upsert"}))?;
        if !thread.is_empty() {
            let inventory=self.call("mcpServerStatus/list",json!({"threadId":thread,"serverName":"shishi_context","detail":"toolsAndAuthOnly"}))?;
            if !inventory["data"].as_array().is_some_and(|rows| {
                rows.iter().any(|row| {
                    row["name"] == "shishi_context"
                        && row["tools"]
                            .as_object()
                            .is_some_and(|tools| tools.contains_key("get_task_context"))
                })
            }) {
                return Err(
                    "事项上下文 MCP 未就绪，尚未交接；请检查 Codex 的 MCP 配置后重试".into(),
                );
            }
        }
        if !note.is_empty() {
            self.call("thread/inject_items",json!({"threadId":thread,"items":[{"type":"message","role":"user","content":[{"type":"input_text","text":note}]}]}))
                .map_err(|e|format!("无法保存上下文交接说明，请更新 Codex CLI 后重试：{e}"))?;
        }
        Ok(())
    }
    fn unsubscribe(&self, thread: &str) -> Result<(), String> {
        self.call("thread/unsubscribe", json!({"threadId":thread}))
            .map(|_| ())
    }
    fn interrupt(&self, thread: &str, turn: &str) -> Result<(), String> {
        self.call("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            .map(|_| ())
    }
    fn events(&self) -> Vec<super::UsageEvent> {
        Client::events(self)
            .into_iter()
            .filter_map(|event| {
                Some(super::UsageEvent {
                    notification: event.clone(),
                    thread_id: event["params"]["threadId"].as_str()?.into(),
                    turn_id: event["params"]["turnId"].as_str().map(String::from),
                    usage: event["params"]["tokenUsage"].clone(),
                })
            })
            .collect()
    }
    fn requests(&self, thread: &str) -> Vec<Value> {
        Client::requests(self, thread)
    }
    fn respond(
        &self,
        thread: &str,
        key: &str,
        request: &Value,
        answer: &Value,
    ) -> Result<(), String> {
        self.reply(key, thread, approval_result(request, answer)?)
    }
}
fn unsupported(error: &str) -> bool {
    let e = error.to_lowercase();
    e.contains("-32601")
        || e.contains("method not found")
        || e.contains("unknown variant")
        || e.contains("unknown method")
}
fn read_turns(c: &Client, thread: &str) -> Result<Value, String> {
    match c.call(
        "thread/turns/list",
        json!({"threadId":thread,"limit":20,"sortDirection":"desc","itemsView":"full"}),
    ) {
        Ok(v) => Ok(v),
        Err(e) if unsupported(&e) => {
            let v = c.call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            )?;
            let turns = v["thread"]["turns"]
                .as_array()
                .ok_or("Codex CLI 未返回轮次历史，请更新 CLI")?;
            Ok(json!({"data":turns.iter().rev().take(20).collect::<Vec<_>>()}))
        }
        Err(e) => Err(e),
    }
}
pub(crate) fn turn_params(thread: &str, text: &str, c: &Config, message_id: &str) -> Value {
    let mut p = json!({"threadId":thread,"input":[{"type":"text","text":text,"text_elements":[]}],"clientUserMessageId":message_id});
    if !c.model.trim().is_empty() {
        p["model"] = json!(c.model);
    }
    if !c.reasoning_effort.is_empty() {
        p["effort"] = json!(c.reasoning_effort);
    }
    p
}
pub(crate) fn approval_result(request: &Value, p: &Value) -> Result<Value, String> {
    let action = str_at(p, "action");
    let method = str_at(request, "method");
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision =
                if action == "acceptSimilar" && method == "item/commandExecution/requestApproval" {
                    let rule = request["params"]["proposedExecpolicyAmendment"]
                        .as_array()
                        .filter(|v| {
                            !v.is_empty()
                                && v.iter()
                                    .all(|x| x.as_str().is_some_and(|s| !s.trim().is_empty()))
                        })
                        .ok_or("本次请求未提供类似命令规则")?;
                    json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":rule}})
                } else if ["accept", "acceptForSession", "decline", "cancel"].contains(&action) {
                    json!(action)
                } else {
                    return Err("无效审批选项".into());
                };
            if let Some(options) = request["params"]["availableDecisions"].as_array() {
                if !options.contains(&decision) {
                    return Err("Codex 未允许此审批选项".into());
                }
            }
            Ok(json!({"decision":decision}))
        }
        "item/permissions/requestApproval" => {
            if !["accept", "acceptForSession", "decline"].contains(&action) {
                return Err("请选择允许或拒绝".into());
            }
            Ok(
                json!({"permissions":if action!="decline"{request["params"]["permissions"].clone()}else{json!({})},"scope":if action=="acceptForSession"{"session"}else{"turn"}}),
            )
        }
        "item/tool/requestUserInput" => {
            let questions = request["params"]["questions"]
                .as_array()
                .ok_or("问题格式无效")?;
            let mut answers = serde_json::Map::new();
            for q in questions {
                let id = str_at(q, "id");
                let answer = p["answers"][id].as_str().unwrap_or("").trim();
                if answer.is_empty() {
                    return Err("请回答每个问题")?;
                }
                answers.insert(id.into(), json!({"answers":[answer]}));
            }
            Ok(json!({"answers":answers}))
        }
        "mcpServer/elicitation/request" => {
            if !["decline", "cancel"].contains(&action) {
                return Err("此类外部服务请求请在 Codex 中处理，或拒绝本次请求".into());
            }
            Ok(json!({"action":action,"content":null}))
        }
        _ => Err("该 Codex 请求类型暂不支持，请停止本轮后在 Codex 会话继续处理".into()),
    }
}
