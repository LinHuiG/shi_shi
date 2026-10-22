//! Synthetic local ACP agent for transport/session tests. It never calls a
//! model and never reads a DSH home, credential, or real transcript.

use serde_json::{json, Value};
use shishi_acp::jsonrpc::{
    decode_line, encode_error, encode_notification, encode_request, encode_response, Incoming,
    RequestId,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static SESSION_SEQ: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Default)]
struct Session {
    model: String,
    effort: String,
    mcp_servers: Value,
}

struct State {
    sessions: HashMap<String, Session>,
}

fn main() {
    let scenario = std::env::var("MOCK_SCENARIO").unwrap_or_else(|_| "happy".into());
    if scenario == "stderr" {
        for index in 0..200 {
            let _ = writeln!(
                std::io::stderr(),
                "STDERR_NOISE_LINE_{index}_ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            );
        }
    }
    if scenario == "stderr-long" {
        let _ = writeln!(std::io::stderr(), "{}", "S".repeat(100_000));
        let _ = std::io::stderr().flush();
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout.lock();
    let mut state = State {
        sessions: HashMap::new(),
    };
    let mut pending: HashMap<RequestId, Result<Value, Value>> = HashMap::new();
    loop {
        let Some(line) = read_line(&mut reader) else {
            return;
        };
        if line.is_empty() {
            continue;
        }
        let incoming = match decode_line(&line, 8 * 1024 * 1024) {
            Ok(incoming) => incoming,
            Err(_) => {
                let _ = writeln!(out, "not-json");
                let _ = out.flush();
                return;
            }
        };
        match incoming {
            Incoming::Response { id, result } => {
                pending.insert(id, result.map_err(|error| json!(error.message)));
            }
            Incoming::Notification { method, params } => {
                if method == "$/cancel_request" {
                    continue;
                }
                if method == "session/cancel" {
                    if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                        let _ = session_id;
                    }
                }
            }
            Incoming::Request { id, method, params } => {
                handle_request(
                    &mut reader,
                    &mut out,
                    &mut state,
                    &mut pending,
                    &scenario,
                    id,
                    &method,
                    params,
                );
            }
        }
    }
}

fn handle_request(
    reader: &mut BufReader<std::io::StdinLock<'_>>,
    out: &mut std::io::StdoutLock<'_>,
    state: &mut State,
    pending: &mut HashMap<RequestId, Result<Value, Value>>,
    scenario: &str,
    id: RequestId,
    method: &str,
    params: Value,
) {
    let result = match method {
        "initialize" => {
            if scenario == "malformed" {
                let _ = writeln!(out, "not-json");
                let _ = out.flush();
                return;
            }
            if scenario == "oversized" {
                let _ = writeln!(out, "{}", "X".repeat(300 * 1024));
                let _ = out.flush();
                return;
            }
            if scenario == "bad-version" {
                json!({"protocolVersion": 2, "agentInfo": {"name": "mock", "version": "0"}, "agentCapabilities": {}})
            } else {
                let mut session_caps = json!({"close": {}, "list": {}});
                if scenario != "no-resume" {
                    session_caps["resume"] = json!({});
                }
                json!({
                    "protocolVersion": 1,
                    "agentInfo": {"name": "mock-acp", "version": "0.1"},
                    "agentCapabilities": {
                        "mcpCapabilities": {"http": true},
                        "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                        "sessionCapabilities": session_caps
                    },
                    "authMethods": []
                })
            }
        }
        "session/new" => {
            let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
            if !Path::new(cwd).is_absolute() {
                send_error(out, &id, -32602, "cwd must be absolute");
                return;
            }
            if !params.get("mcpServers").is_some_and(Value::is_array) {
                send_error(out, &id, -32602, "mcpServers is required");
                return;
            }
            let session_id = format!(
                "mock-session-{}",
                SESSION_SEQ.fetch_add(1, Ordering::SeqCst)
            );
            let session = Session {
                model: "[\"mock\",\"mock-model\"]".into(),
                effort: "low".into(),
                mcp_servers: params.get("mcpServers").cloned().unwrap_or(json!([])),
            };
            state.sessions.insert(session_id.clone(), session);
            json!({"sessionId": session_id, "configOptions": config_options(state, &session_id)})
        }
        "session/resume" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("mock-session-1")
                .to_string();
            if !state.sessions.contains_key(&session_id) {
                state.sessions.insert(
                    session_id.clone(),
                    Session {
                        model: "[\"mock\",\"mock-model\"]".into(),
                        effort: "low".into(),
                        mcp_servers: json!([]),
                    },
                );
            }
            json!({"configOptions": config_options(state, &session_id)})
        }
        "session/list" => {
            let sessions = state
                .sessions
                .keys()
                .map(|session_id| json!({"sessionId": session_id, "cwd": "C:\\mock"}))
                .collect::<Vec<_>>();
            json!({"sessions": sessions})
        }
        "session/close" => {
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                state.sessions.remove(session_id);
            }
            json!({})
        }
        "session/set_config_option" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let config_id = params.get("configId").and_then(Value::as_str).unwrap_or("");
            let value = params.get("value").and_then(Value::as_str).unwrap_or("");
            let Some(session) = state.sessions.get_mut(&session_id) else {
                send_error(out, &id, -32602, "unknown session");
                return;
            };
            if config_id == "model" {
                if value != "[\"mock\",\"mock-model\"]" && value != "[\"mock\",\"other-model\"]" {
                    send_error(out, &id, -32602, "unknown model option");
                    return;
                }
                session.model = value.to_string();
            } else if config_id == "reasoning_effort" {
                if !["", "low", "high"].contains(&value) {
                    send_error(out, &id, -32602, "unknown reasoning effort");
                    return;
                }
                session.effort = value.to_string();
            } else {
                send_error(out, &id, -32602, "unknown config option");
                return;
            }
            json!({"configOptions": config_options(state, &session_id)})
        }
        "session/prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let Some(session) = state.sessions.get(&session_id).cloned() else {
                send_error(out, &id, -32602, "unknown session");
                return;
            };
            let prompt = params
                .get("prompt")
                .and_then(Value::as_array)
                .and_then(|blocks| blocks.first())
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            record_prompt(prompt);
            if scenario == "exit" {
                let _ = out.flush();
                std::process::exit(7);
            }
            run_prompt(
                reader,
                out,
                state,
                pending,
                scenario,
                &id,
                &session_id,
                &session,
                prompt,
            );
            return;
        }
        _ => {
            send_error(out, &id, -32601, "method not found");
            return;
        }
    };
    send_result(out, &id, result);
}

fn run_prompt(
    reader: &mut BufReader<std::io::StdinLock<'_>>,
    out: &mut std::io::StdoutLock<'_>,
    state: &mut State,
    pending: &mut HashMap<RequestId, Result<Value, Value>>,
    scenario: &str,
    id: &RequestId,
    session_id: &str,
    session: &Session,
    prompt: &str,
) {
    if scenario == "job" {
        spawn_grandchild();
    }
    if scenario == "submit" || scenario == "submit-cancel" {
        call_submit_mcp(session);
    }
    if scenario == "permission" {
        let permission_id = RequestId::String(format!("perm-{}", std::process::id()));
        send_value(
            out,
            &json!({
                "jsonrpc": "2.0",
                "id": permission_id.to_value(),
                "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "toolCall": {"toolCallId": "call-1", "title": "mock tool"},
                    "options": [
                        {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                        {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}
                    ]
                }
            }),
        );
        loop {
            let Some(line) = read_line(reader) else {
                return;
            };
            let incoming = match decode_line(&line, 8 * 1024 * 1024) {
                Ok(incoming) => incoming,
                Err(_) => continue,
            };
            match incoming {
                Incoming::Response {
                    id: response_id,
                    result,
                } if response_id == permission_id => {
                    let outcome = result.unwrap_or(Value::Null);
                    record_permission(&outcome);
                    send_session_update(
                        out,
                        session_id,
                        json!({"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "mock tool", "status": "completed"}),
                    );
                    send_result(
                        out,
                        id,
                        json!({"stopReason": "end_turn", "usage": {"totalTokens": 3, "inputTokens": 2, "outputTokens": 1}}),
                    );
                    return;
                }
                Incoming::Notification { method, params } if method == "session/cancel" => {
                    let _ = params;
                    record_cancel();
                    send_result(out, id, json!({"stopReason": "cancelled"}));
                    return;
                }
                Incoming::Request {
                    id: request_id,
                    method,
                    ..
                } if method == "session/close" => {
                    send_result(out, &request_id, json!({}));
                    send_result(out, id, json!({"stopReason": "cancelled"}));
                    return;
                }
                _ => {}
            }
        }
    }
    if scenario == "hang" {
        send_session_update(
            out,
            session_id,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "working"}}),
        );
        loop {
            let Some(line) = read_line(reader) else {
                return;
            };
            let incoming = match decode_line(&line, 8 * 1024 * 1024) {
                Ok(incoming) => incoming,
                Err(_) => continue,
            };
            match incoming {
                Incoming::Notification { method, params } if method == "session/cancel" => {
                    let _ = params;
                    record_cancel();
                    send_result(out, id, json!({"stopReason": "cancelled"}));
                    return;
                }
                Incoming::Response {
                    id: response_id, ..
                } => {
                    pending.insert(response_id, Ok(Value::Null));
                }
                Incoming::Request {
                    id: request_id,
                    method,
                    ..
                } if method == "session/close" => {
                    send_result(out, &request_id, json!({}));
                    send_result(out, id, json!({"stopReason": "cancelled"}));
                    return;
                }
                _ => {}
            }
        }
    }
    if scenario == "server-request" {
        let request_id = RequestId::String("unknown-server-request".into());
        send_value(
            out,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id.to_value(),
                "method": "fs/read_text_file",
                "params": {"sessionId": session_id, "path": "C:\\fixture.txt"}
            }),
        );
        loop {
            let Some(line) = read_line(reader) else {
                return;
            };
            let incoming = match decode_line(&line, 8 * 1024 * 1024) {
                Ok(incoming) => incoming,
                Err(_) => continue,
            };
            match incoming {
                Incoming::Response {
                    id: response_id,
                    result,
                } if response_id == request_id => {
                    if let Ok(path) = std::env::var("MOCK_SERVER_REQUEST_FILE") {
                        let recorded = match result {
                            Ok(value) => value,
                            Err(error) => json!({
                                "error": {"code": error.code, "message": error.message}
                            }),
                        };
                        let _ = std::fs::write(path, recorded.to_string());
                    }
                    break;
                }
                Incoming::Notification { method, params } if method == "session/cancel" => {
                    let _ = params;
                    record_cancel();
                    send_result(out, id, json!({"stopReason": "cancelled"}));
                    return;
                }
                _ => {}
            }
        }
    }
    if scenario == "tool" || scenario == "submit" {
        send_session_update(
            out,
            session_id,
            json!({"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "mcp__shishi_submit__submit_analysis", "status": "in_progress", "rawInput": {"reason": "mock"}}),
        );
        send_session_update(
            out,
            session_id,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "accepted"}}]}),
        );
        send_session_update(
            out,
            session_id,
            json!({"sessionUpdate": "usage_update", "used": 10, "size": 1000}),
        );
    }
    send_session_update(
        out,
        session_id,
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("echo:{}", prompt.chars().take(20).collect::<String>())}}),
    );
    let usage = if scenario == "no-usage" {
        Value::Null
    } else {
        json!({"totalTokens": 3, "inputTokens": 2, "outputTokens": 1})
    };
    let stop_reason = if scenario == "submit-cancel" {
        "cancelled"
    } else {
        "end_turn"
    };
    let mut result = json!({"stopReason": stop_reason});
    if !usage.is_null() {
        result["usage"] = usage;
    }
    send_result(out, id, result);
    let _ = (state, session);
}

fn config_options(state: &State, session_id: &str) -> Value {
    let session = state.sessions.get(session_id);
    let model = session
        .map(|session| session.model.clone())
        .unwrap_or_else(|| "[\"mock\",\"mock-model\"]".into());
    let effort = session
        .map(|session| session.effort.clone())
        .unwrap_or_else(|| "low".into());
    json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": model,
            "options": [{
                "group": "mock",
                "name": "Mock",
                "options": [
                    {"value": "[\"mock\",\"mock-model\"]", "name": "Mock Model"},
                    {"value": "[\"mock\",\"other-model\"]", "name": "Other Model"}
                ]
            }]
        },
        {
            "id": "reasoning_effort",
            "name": "Reasoning effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": effort,
            "options": [
                {"value": "", "name": "Provider default"},
                {"value": "low", "name": "Low"},
                {"value": "high", "name": "High"}
            ]
        }
    ])
}

fn send_session_update(out: &mut std::io::StdoutLock<'_>, session_id: &str, update: Value) {
    send_value(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update}
        }),
    );
}

fn send_result(out: &mut std::io::StdoutLock<'_>, id: &RequestId, result: Value) {
    let line = encode_response(id, &result);
    let _ = out.write_all(&line);
    let _ = out.flush();
}

fn send_error(out: &mut std::io::StdoutLock<'_>, id: &RequestId, code: i64, message: &str) {
    let line = encode_error(id, code, message);
    let _ = out.write_all(&line);
    let _ = out.flush();
}

fn send_value(out: &mut std::io::StdoutLock<'_>, value: &Value) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = out.write_all(&bytes);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    match reader.read_until(b'\n', &mut line) {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

fn record_prompt(prompt: &str) {
    if let Ok(path) = std::env::var("MOCK_PROMPT_FILE") {
        let _ = std::fs::write(path, prompt.as_bytes());
    }
    if let Ok(count_path) = std::env::var("MOCK_PROMPT_COUNT_FILE") {
        let count = std::fs::read_to_string(&count_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        let _ = std::fs::write(count_path, count.to_string());
    }
}

fn record_cancel() {
    if let Ok(path) = std::env::var("MOCK_CANCEL_FILE") {
        let _ = std::fs::write(path, b"cancelled");
    }
}

fn record_permission(outcome: &Value) {
    if let Ok(path) = std::env::var("MOCK_PERMISSION_FILE") {
        let inner = outcome
            .get("outcome")
            .cloned()
            .unwrap_or_else(|| outcome.clone());
        let _ = std::fs::write(path, inner.to_string());
    }
}

fn spawn_grandchild() {
    let Ok(path) = std::env::var("MOCK_CHILD_PID_FILE") else {
        return;
    };
    let mut command = Command::new("cmd.exe");
    command.args(["/D", "/S", "/C", "ping -n 30 127.0.0.1 >NUL"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return,
    };
    let _ = std::fs::write(path, child.id().to_string());
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = child.try_wait();
}

fn call_submit_mcp(session: &Session) {
    let Some(server) = session.mcp_servers.as_array().and_then(|servers| {
        servers
            .iter()
            .find(|server| server.get("name").and_then(Value::as_str) == Some("shishi_submit"))
    }) else {
        return;
    };
    let command = server.get("command").and_then(Value::as_str).unwrap_or("");
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut child = match Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => return,
    };
    let mut stdout = BufReader::new(match child.stdout.take() {
        Some(stdout) => stdout,
        None => return,
    });
    let arguments: Value = std::env::var("MOCK_SUBMIT_ARGS")
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_else(|| json!({"reason": "mock", "changes": []}));
    let initialize = encode_request(
        &RequestId::Number(1),
        "initialize",
        &json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "mock", "version": "0"}}),
    );
    if stdin.write_all(&initialize).is_err() {
        return;
    }
    let _ = stdin.write_all(&encode_notification(
        "notifications/initialized",
        &json!({}),
    ));
    let list = encode_request(&RequestId::Number(2), "tools/list", &json!({}));
    let call = encode_request(
        &RequestId::Number(3),
        "tools/call",
        &json!({"name": "submit_analysis", "arguments": arguments}),
    );
    let _ = stdin.write_all(&list);
    let _ = stdin.write_all(&call);
    let _ = stdin.flush();
    let mut lines = 0;
    while lines < 6 {
        let Some(line) = read_line(&mut stdout) else {
            break;
        };
        lines += 1;
        if let Ok(Incoming::Response {
            id: response_id,
            result,
        }) = decode_line(&line, 4 * 1024 * 1024)
        {
            if response_id == RequestId::Number(3) {
                let _ = result;
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
