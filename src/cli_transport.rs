//! Versioned local transport shared by the console client and the desktop owner.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
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

pub fn data_directory(root: &Path) -> Result<PathBuf, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Location {
        version: u32,
        directory: PathBuf,
    }
    match fs::read(root.join("data-location.json")) {
        Ok(bytes) => {
            let value: Location =
                serde_json::from_slice(&bytes).map_err(|_| "数据目录设置损坏，未启动新后台")?;
            if value.version != 1 || !value.directory.is_absolute() || !value.directory.is_dir() {
                return Err("配置的数据目录不可用，未创建新数据".into());
            }
            Ok(value.directory)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(root.join("data")),
        Err(e) => Err(format!("无法读取数据目录设置：{e}")),
    }
}

pub fn route(root: &Path) -> Result<Option<Route>, String> {
    let path = data_directory(root)?.join(ROUTE_FILE);
    let raw = match fs::File::open(path) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(4097)
                .read_to_end(&mut raw)
                .map_err(|e| format!("后台服务信息不可读：{e}"))?;
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
    let stream = TcpStream::connect_timeout(&route.address, Duration::from_secs(2))
        .map_err(|e| format!("无法连接后台：{e}"))?;
    exchange(route, stream, method, params)
}

fn exchange(
    route: &Route,
    mut stream: TcpStream,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let request =
        json!({"protocol":PROTOCOL,"token":route.token,"id":id,"method":method,"params":params});
    let mut bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > REQUEST_LIMIT {
        return Err("请求超过 8 MiB，请缩小输入".into());
    }
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let timeout = if method == "backend.status" { 5 } else { 1900 };
    stream
        .set_read_timeout(Some(Duration::from_secs(timeout)))
        .map_err(|e| e.to_string())?;
    // Once any request bytes have been sent, never reconnect/replay a mutation.
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|e| format!("请求 {id} 发送中断，结果可能未知；请查询状态，不要直接重发：{e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(RESPONSE_LIMIT + 1)
        .read_line(&mut line)
        .map_err(|e| format!("请求 {id} 未取得结果，请查询状态，不要直接重发：{e}"))?;
    if line.len() as u64 > RESPONSE_LIMIT {
        return Err(format!("请求 {id} 的结果过大，请改用分页查询"));
    }
    let response: Value = serde_json::from_str(&line)
        .map_err(|_| format!("请求 {id} 的响应不完整，结果可能未知；请查询状态"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_explicit_directory_never_falls_back() {
        let root = std::env::temp_dir().join(format!("ss-cli-location-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("data-location.json"),
            json!({"version":1,"directory":root.join("missing")}).to_string(),
        )
        .unwrap();
        assert!(data_directory(&root).is_err());
        assert!(!root.join("data").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
