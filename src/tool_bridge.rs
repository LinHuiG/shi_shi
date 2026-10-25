//! Request-owned loopback callback for stdio MCP children. Only SS executes commands.
use crate::tool_calling::ToolSession;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const REQUEST_LIMIT: u64 = 128 * 1024;
const RESPONSE_LIMIT: u64 = 1024 * 1024;

fn service_path(root: &std::path::Path) -> std::path::PathBuf {
    crate::storage::data_dir(root).join("handling-context/tool-service.json")
}

/// Keep execution in the live SS owner; MCP children only forward requests.
pub(crate) fn start(app: &Arc<crate::engine::App>) -> Result<(), String> {
    if app.tool_runtime.mcp_started.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let result = (|| {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let token = crate::model::id();
        let route = json!({"address":listener.local_addr().map_err(|e|e.to_string())?.to_string(),"token":token});
        let path = service_path(&app.root);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&path, route.to_string()).map_err(|e| e.to_string())?;
        let weak = Arc::downgrade(app);
        std::thread::spawn(move || {
            while weak.strong_count() > 0 && !crate::process::stopping() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(app) = weak.upgrade() else { break };
                        let token = token.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            let result = (|| -> Result<Value, String> {
                                let mut line = String::new();
                                BufReader::new(&mut stream)
                                    .take(REQUEST_LIMIT + 1)
                                    .read_line(&mut line)
                                    .map_err(|e| e.to_string())?;
                                if line.len() as u64 > REQUEST_LIMIT {
                                    return Err("工具请求过长".into());
                                }
                                let value: Value =
                                    serde_json::from_str(&line).map_err(|e| e.to_string())?;
                                if value["token"] != token {
                                    return Err("工具请求已失效".into());
                                }
                                execute_handling(&app, &value)
                            })();
                            let response=result.unwrap_or_else(|error|json!({"isError":true,"content":[{"type":"text","text":error}]}));
                            let _ = writeln!(stream, "{response}");
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(15))
                    }
                    Err(_) => break,
                }
            }
            if fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .is_some_and(|value| value["token"] == token)
            {
                let _ = fs::remove_file(path);
            }
        });
        Ok(())
    })();
    if result.is_err() {
        app.tool_runtime.mcp_started.store(false, Ordering::SeqCst);
    }
    result
}

fn execute_handling(app: &crate::engine::App, value: &Value) -> Result<Value, String> {
    let context = value["context_token"].as_str().ok_or("缺少事项凭证")?;
    let (_, ticket) = crate::context_mcp::load_ticket(&app.root, context)?;
    if ticket.expires < crate::model::now() {
        return Err("当前事项上下文授权已过期".into());
    }
    let (config, tools) = {
        let state = app.state.lock().unwrap();
        if !state.tasks.iter().any(|task| task.id == ticket.task) {
            return Err("事项不存在".into());
        }
        if state
            .handling
            .runs
            .iter()
            .any(|run| run.id == ticket.run && run.stop_requested)
        {
            return Err("处置已终止，上下文授权已失效".into());
        }
        (
            state.config.for_kind("handling"),
            crate::tool_registry::effective(&state),
        )
    };
    let name = value["name"].as_str().ok_or("缺少工具名")?;
    let id = value["call_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 200)
        .ok_or("缺少调用编号")?;
    let session = ToolSession::handling(app, &ticket, tools, &config);
    let cancel = AtomicBool::new(false);
    let result = session
        .execute(
            name,
            id,
            &value["arguments"],
            &cancel,
            Instant::now() + Duration::from_secs(60),
        )
        .map_err(|_| "工具执行或记录失败，未自动重试");
    session.finish(result.as_ref().err().copied(), &Value::Null)?;
    match result? {
        shishi_api::ToolResult::Text(text) => Ok(json!({"content":[{"type":"text","text":text}]})),
        _ => Err("工具返回类型无效".into()),
    }
}

pub(crate) fn call_handling(
    root: &std::path::Path,
    context: &str,
    name: &str,
    arguments: &Value,
    call_id: &str,
) -> Result<Value, String> {
    let route: Value = serde_json::from_slice(
        &fs::read(service_path(root)).map_err(|_| "拾事工具执行服务未就绪，请确认拾事正在运行")?,
    )
    .map_err(|_| "工具执行服务信息无效")?;
    call_request(
        &route,
        json!({"token":route["token"],"context_token":context,"name":name,"arguments":arguments,"call_id":call_id}),
    )
}

#[cfg(test)]
pub(crate) fn with_session<T>(
    session: &ToolSession<'_>,
    cancel: &AtomicBool,
    deadline: Instant,
    invoke: impl FnOnce(Value) -> Result<T, String>,
) -> Result<T, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let token = crate::model::id();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !session.stopped.load(Ordering::SeqCst) && !cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream,_)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let result = (|| -> Result<Value,String> {
                            let mut line=String::new();
                            BufReader::new(&mut stream).take(REQUEST_LIMIT+1).read_line(&mut line).map_err(|e|e.to_string())?;
                            if line.len() as u64 > REQUEST_LIMIT { return Err("工具请求过长".into()); }
                            let request:Value=serde_json::from_str(&line).map_err(|e|e.to_string())?;
                            if request["token"].as_str() != Some(token.as_str()) { return Err("工具请求已失效".into()); }
                            let name=request["name"].as_str().ok_or("缺少工具名")?;
                            let call=request["call_id"].as_str().filter(|id|!id.is_empty() && id.len() <= 200).ok_or("缺少调用编号")?;
                            let output=session.execute(name,call,&request["arguments"],cancel,deadline).map_err(|_|"工具执行或记录失败，未自动重试")?;
                            match output {
                                shishi_api::ToolResult::Text(text) => Ok(json!({"content":[{"type":"text","text":text}]})),
                                _ => Err("注册命令返回类型无效".into()),
                            }
                        })();
                        let value=result.unwrap_or_else(|error|json!({"isError":true,"content":[{"type":"text","text":error}]}));
                        let _ = writeln!(stream,"{value}");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(15)),
                    Err(_) => break,
                }
            }
        });
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _stop = Stop(&session.stopped);
        invoke(json!({"address":address.to_string(),"token":token}))
    })
}

/// The child knows declarations and this one-use route, not command templates.
#[cfg(test)]
pub fn call(route: &Value, name: &str, arguments: &Value, call_id: &str) -> Result<Value, String> {
    call_request(
        route,
        json!({"token":route["token"],"name":name,"arguments":arguments,"call_id":call_id}),
    )
}
fn call_request(route: &Value, request: Value) -> Result<Value, String> {
    let address: SocketAddr = route["address"]
        .as_str()
        .ok_or("工具回调不可用")?
        .parse()
        .map_err(|_| "工具回调地址无效")?;
    if !address.ip().is_loopback() {
        return Err("工具回调必须位于本机".into());
    }
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(65)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    if request.to_string().len() as u64 > REQUEST_LIMIT {
        return Err("工具参数过长".into());
    }
    writeln!(stream, "{request}").map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(RESPONSE_LIMIT + 1)
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    if response.len() as u64 > RESPONSE_LIMIT {
        return Err("工具响应过长".into());
    }
    serde_json::from_str(&response).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::App, model::*, tool_registry::ToolDefinition};

    #[test]
    fn only_authenticated_declared_calls_execute_in_the_parent_and_ids_are_not_replayed() {
        let root = std::env::temp_dir().join(format!("shishi-tool-bridge-{}", id()));
        let app = App::load(root.clone()).unwrap();
        let tool = ToolDefinition {
            id: "synthetic".into(),
            name: "synthetic_echo".into(),
            description: "Synthetic bridge fixture".into(),
            command_template: "echo synthetic_bridge_result".into(),
            enabled: true,
            ..Default::default()
        };
        app.state.lock().unwrap().config.tools.push(tool.clone());
        let config = app.state.lock().unwrap().config.clone();
        let session = ToolSession::new(
            &app,
            "request",
            "format",
            "",
            vec![tool.clone()],
            &config,
            0,
        );
        let cancel = AtomicBool::new(false);
        with_session(
            &session,
            &cancel,
            Instant::now() + Duration::from_secs(10),
            |route| {
                let mut invalid = route.clone();
                invalid["token"] = json!("invalid");
                assert_eq!(
                    call(&invalid, &tool.name, &json!({}), "unauthorized")?["isError"],
                    true
                );
                assert!(app.state.lock().unwrap().tool_runs.is_empty());
                let response = call(&route, &tool.name, &json!({}), "first")?;
                assert!(response["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("synthetic_bridge_result"));
                assert_eq!(
                    call(&route, &tool.name, &json!({}), "first")?["isError"],
                    true
                );
                app.state.lock().unwrap().config.tools[0].enabled = false;
                let response = call(&route, &tool.name, &json!({}), "disabled")?;
                let result: Value =
                    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(result["status"], "rejected");
                Ok(())
            },
        )
        .unwrap();
        assert!(session.stopped.load(Ordering::SeqCst));
        session.finish(None, &Value::Null).unwrap();
        let history = app
            .tool_run_history(&json!({"tool_id":"synthetic","purpose":"format"}))
            .unwrap();
        assert_eq!(history["rows"][0]["call_count"], 2);
        assert_eq!(app.state.lock().unwrap().calls, 0);
        app.log_writer.flush().unwrap();
        let logs = app
            .activity_query(&json!({"purpose":"tool_call","filter":"ai"}))
            .unwrap();
        let text = logs.to_string();
        assert!(text.contains("tool_call_finished"));
        assert!(!text.contains("synthetic_bridge_result"));
        assert!(call(
            &json!({"address":"192.0.2.1:9"}),
            "unknown",
            &json!({}),
            "blocked"
        )
        .is_err());
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}
