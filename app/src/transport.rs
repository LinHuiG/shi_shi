//! Versioned loopback transport between the console client and the owner.
//! Also the single-instance channel: a second launch forwards to the owner.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};

pub const PROTOCOL: u32 = 1;
pub const REQUEST_LIMIT: u64 = 8 * 1024 * 1024;
pub const RESPONSE_LIMIT: u64 = 64 * 1024 * 1024;
pub const ROUTE_FILE: &str = "local-service.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub protocol: u32,
    pub address: SocketAddr,
    pub token: String,
    pub instance: String,
    pub pid: u32,
}

/// Data directory of an installation: `data-location.json` or `root/data`.
/// A configured but unavailable directory is an error, never a fallback.
pub use shishi_core::data::directory as data_directory;

/// The owner's published route; `None` when no owner ever published one.
pub fn route(root: &Path) -> Result<Option<Route>, String> {
    route_in(&data_directory(root)?)
}

/// The route published in a data directory.
pub fn route_in(data: &Path) -> Result<Option<Route>, String> {
    let path = data.join(ROUTE_FILE);
    let raw = match fs::File::open(path) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(4097).read_to_end(&mut raw).map_err(|e| format!("后台服务信息不可读：{e}"))?;
            if raw.len() > 4096 {
                return Err("后台服务信息过长".into());
            }
            raw
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("后台服务信息不可读：{e}")),
    };
    let value: Route = serde_json::from_slice(&raw).map_err(|_| "后台服务信息无效")?;
    if value.protocol != PROTOCOL || !value.address.ip().is_loopback() || value.token.len() < 32 {
        return Err("后台协议不兼容或本地地址无效".into());
    }
    Ok(Some(value))
}

/// A route that answers `backend.status`; `None` when nobody listens.
pub fn probe(root: &Path) -> Result<Option<Route>, String> {
    let Some(route) = route(root)? else {
        return Ok(None);
    };
    let stream = match TcpStream::connect_timeout(&route.address, Duration::from_secs(2)) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(format!("无法探测后台：{e}")),
    };
    success(exchange(&route, stream, "backend.status", json!({}))?)?;
    Ok(Some(route))
}

pub fn call(route: &Route, method: &str, params: Value) -> Result<Value, String> {
    let stream =
        TcpStream::connect_timeout(&route.address, Duration::from_secs(2)).map_err(|e| format!("无法连接后台：{e}"))?;
    exchange(route, stream, method, params)
}

pub fn exchange(route: &Route, mut stream: TcpStream, method: &str, params: Value) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let request = json!({"protocol":PROTOCOL,"token":route.token,"id":id,"method":method,"params":params});
    let mut bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > REQUEST_LIMIT {
        return Err("请求超过 8 MiB，请缩小输入".into());
    }
    stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let timeout = if method == "backend.status" { 5 } else { 1900 };
    stream.set_read_timeout(Some(Duration::from_secs(timeout))).map_err(|e| e.to_string())?;
    // Once any request bytes have been sent, never reconnect/replay a mutation.
    bytes.push(b'\n');
    stream.write_all(&bytes).map_err(|e| format!("请求 {id} 发送中断，结果可能未知；请查询状态，不要直接重发：{e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(RESPONSE_LIMIT + 1)
        .read_line(&mut line)
        .map_err(|e| format!("请求 {id} 未取得结果，请查询状态，不要直接重发：{e}"))?;
    if line.len() as u64 > RESPONSE_LIMIT {
        return Err(format!("请求 {id} 的结果过大，请改用分页查询"));
    }
    let response: Value =
        serde_json::from_str(&line).map_err(|_| format!("请求 {id} 的响应不完整，结果可能未知；请查询状态"))?;
    if response["id"] != id || response["protocol"] != PROTOCOL {
        return Err("后台响应编号或协议不匹配，未重试".into());
    }
    Ok(response)
}

pub fn success(mut response: Value) -> Result<Value, String> {
    if response["ok"] == true {
        Ok(response["data"].take())
    } else {
        Err(response["error"].as_str().unwrap_or("后台操作失败").into())
    }
}

/// An isolated test directory beside the build output, never the live data.
#[cfg(test)]
pub fn test_dir(label: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target/test-data")
        .join(format!("{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Remove a test directory. Dropped owners close their database and
/// service on background threads, and Windows refuses to delete a file
/// that is still open, so wait up to 10 s for them to finish.
#[cfg(test)]
pub fn remove_test_dir(dir: &std::path::Path) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Err(e) = fs::remove_dir_all(dir) {
        assert!(std::time::Instant::now() < until, "{}: {e}", dir.display());
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests;
