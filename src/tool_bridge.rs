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
                                stream.set_nonblocking(false).map_err(|e| e.to_string())?;
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
    let request = request.to_string();
    if request.len() as u64 > REQUEST_LIMIT {
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

    #[test]
    fn service_accepts_delayed_fragments_and_keeps_authentication() {
        let (root, app) = crate::test_support::scoped_app("tool-service-frames");
        let app = Arc::new(app);
        start(&app).unwrap();
        let route: Value = serde_json::from_slice(&fs::read(service_path(&root)).unwrap()).unwrap();
        for (token, expected) in [
            (json!("invalid"), "工具请求已失效"),
            (route["token"].clone(), "缺少事项凭证"),
        ] {
            let response = crate::test_support::fragmented_json(
                route["address"].as_str().unwrap().parse().unwrap(),
                &json!({"token":token}),
            );
            assert_eq!(response["isError"], true);
            assert_eq!(response["content"][0]["text"], expected);
            assert!(app.state.lock().unwrap().tool_runs.is_empty());
        }
        assert!(call_request(&json!({"address":"192.0.2.1:9"}), json!({}))
            .unwrap_err()
            .contains("本机"));
    }
}
