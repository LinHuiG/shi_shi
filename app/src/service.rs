//! The owner's loopback service: single instance hand-off and the CLI channel.
//! One process owns the data directory; the GUI and the CLI are its two
//! possible owners. Ownership is never persisted.
use crate::transport as wire;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct State {
    gui: bool,
    cli: bool,
    closing: bool,
    requests: usize,
}

#[derive(Default)]
pub struct Owners {
    state: Mutex<State>,
    changed: Condvar,
}

/// One running request; `Owners::wait_idle` waits for all of them.
pub struct Active(Arc<Owners>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().requests -= 1;
        self.0.changed.notify_all();
    }
}

impl Owners {
    pub fn acquire(&self, gui: bool) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        if s.closing {
            return Err("后台正在退出，请等待退出完成后重试".into());
        }
        if gui {
            s.gui = true
        } else {
            s.cli = true
        }
        Ok(())
    }

    /// Release one owner (or both with `force`); true once nobody owns it.
    pub fn release(&self, gui: bool, force: bool) -> bool {
        let mut s = self.state.lock().unwrap();
        if force {
            s.gui = false;
            s.cli = false;
        } else if gui {
            s.gui = false
        } else {
            s.cli = false
        }
        if !s.gui && !s.cli {
            s.closing = true;
        }
        s.closing
    }

    /// Stop accepting work regardless of owners (exit or restart).
    pub fn close(&self) {
        self.state.lock().unwrap().closing = true;
    }

    pub fn gui(&self) -> bool {
        self.state.lock().unwrap().gui
    }

    pub fn cli(&self) -> bool {
        self.state.lock().unwrap().cli
    }

    pub fn snapshot(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"running": true, "gui": s.gui, "cli": s.cli, "closing": s.closing,
            "active_requests": s.requests, "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION")})
    }

    pub fn begin(self: &Arc<Self>) -> Result<Active, String> {
        let mut s = self.state.lock().unwrap();
        if s.closing {
            return Err("后台正在退出，未接受新操作".into());
        }
        s.requests += 1;
        Ok(Active(self.clone()))
    }

    pub fn wait_idle(&self, timeout: Duration) {
        let state = self.state.lock().unwrap();
        let _ = self.changed.wait_timeout_while(state, timeout, |s| s.requests > 0).unwrap();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    Show,
    Hide,
    Shutdown,
    /// A helper (update or data transfer) takes over: exit without relaunching.
    Handoff,
}

/// Runs one business or host method for the CLI; blocks until it ends.
pub type Handler = dyn Fn(&str, Value) -> Result<Value, String> + Send + Sync;

/// A running listener; dropping it stops accepting and removes the route.
pub struct Service {
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
    }
}

/// Methods whose helper replaces the running process once they succeed.
pub const HANDING_OFF: [&str; 2] = ["update_install", "storage_change"];

pub fn start(
    data: PathBuf,
    owners: Arc<Owners>,
    handler: Arc<Handler>,
    signal: Arc<dyn Fn(Signal) + Send + Sync>,
) -> Result<Service, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let route = wire::Route {
        protocol: wire::PROTOCOL,
        address,
        token: uuid::Uuid::new_v4().simple().to_string(),
        instance: uuid::Uuid::new_v4().to_string(),
        pid: std::process::id(),
    };
    let path = data.join(wire::ROUTE_FILE);
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(&route).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    // The caller holds the exclusive data lock, so an old route has no owner.
    fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let connections = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("shishi-service".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                if connections.fetch_add(1, Ordering::SeqCst) >= 32 {
                    connections.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let (token, owners, handler, signal, connections) =
                    (route.token.clone(), owners.clone(), handler.clone(), signal.clone(), connections.clone());
                std::thread::spawn(move || {
                    serve(stream, &token, &owners, &*handler, &*signal);
                    connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            let ours = fs::read(&path)
                .ok()
                .and_then(|v| serde_json::from_slice::<wire::Route>(&v).ok())
                .is_some_and(|v| v.instance == route.instance);
            if ours {
                let _ = fs::remove_file(path);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Service { stop, address })
}

fn serve(mut stream: TcpStream, token: &str, owners: &Arc<Owners>, handler: &Handler, signal: &dyn Fn(Signal)) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut id = Value::Null;
    let mut after = None;
    let result = (|| -> Result<Value, String> {
        let mut line = String::new();
        BufReader::new(&mut stream).take(wire::REQUEST_LIMIT + 1).read_line(&mut line).map_err(|e| e.to_string())?;
        if line.len() as u64 > wire::REQUEST_LIMIT {
            return Err("本地请求过长".into());
        }
        let request: Value = serde_json::from_str(&line).map_err(|_| "本地请求格式无效")?;
        if request["token"] != token || request["protocol"] != wire::PROTOCOL {
            return Err("本地授权或协议无效".into());
        }
        id = request["id"].clone();
        if id.as_str().is_none_or(|id| id.is_empty() || id.len() > 128) {
            return Err("请求编号无效".into());
        }
        let method = request["method"].as_str().ok_or("缺少操作名")?;
        let params = request["params"].clone();
        if !params.is_object() {
            return Err("参数必须是 JSON 对象".into());
        }
        match method {
            "backend.status" => Ok(owners.snapshot()),
            "backend.start" => {
                owners.acquire(false)?;
                Ok(owners.snapshot())
            }
            "ui.open" => {
                owners.acquire(true)?;
                after = Some(Signal::Show);
                Ok(owners.snapshot())
            }
            "backend.stop" | "ui.close" => {
                if owners.release(method == "ui.close", params["force"] == true) {
                    after = Some(Signal::Shutdown);
                } else if method == "ui.close" {
                    after = Some(Signal::Hide);
                }
                Ok(owners.snapshot())
            }
            "pick" => Err("文件选择器需要图形界面；CLI 请直接向对应业务方法传入路径".into()),
            _ => {
                owners.acquire(false)?;
                let _active = owners.begin()?;
                let result = handler(method, params);
                if result.is_ok() && HANDING_OFF.contains(&method) {
                    after = Some(Signal::Handoff);
                }
                result
            }
        }
    })();
    let response = match result {
        Ok(data) => json!({"protocol": wire::PROTOCOL, "id": id, "ok": true, "data": data}),
        Err(error) => json!({"protocol": wire::PROTOCOL, "id": id, "ok": false, "error": error}),
    };
    let mut output = BufWriter::new(stream);
    let _ = writeln!(output, "{response}").and_then(|_| output.flush());
    // Change the lifecycle only after the caller has its answer.
    if let Some(value) = after {
        signal(value);
    }
}

#[cfg(test)]
mod tests;
