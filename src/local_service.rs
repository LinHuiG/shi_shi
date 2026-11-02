//! One live App owns both UI and CLI operations. Ownership flags are never persisted.
use crate::{cli_transport as wire, engine::App};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::{Ipv4Addr, TcpListener},
    sync::{Arc, Condvar, Mutex},
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
pub struct Runtime {
    state: Mutex<State>,
    changed: Condvar,
}
pub struct Request<'a>(&'a Runtime);
impl Drop for Request<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.requests -= 1;
        self.0.changed.notify_all();
    }
}
impl Runtime {
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
    pub fn gui(&self) -> bool {
        self.state.lock().unwrap().gui
    }
    pub fn cli(&self) -> bool {
        self.state.lock().unwrap().cli
    }
    pub fn snapshot(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"running":true,"gui":s.gui,"cli":s.cli,"closing":s.closing,"active_requests":s.requests,"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION")})
    }
    pub fn begin(&self) -> Result<Request<'_>, String> {
        let mut s = self.state.lock().unwrap();
        if s.closing {
            return Err("后台正在退出，未接受新操作".into());
        }
        s.requests += 1;
        Ok(Request(self))
    }
    pub fn wait_idle(&self, timeout: Duration) {
        let state = self.state.lock().unwrap();
        let _guard = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.requests > 0)
            .unwrap();
    }
    pub fn restart_args(&self) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let mut args = std::env::args()
            .skip(1)
            .filter(|a| a != "--background" && a != "--cli-owner")
            .collect::<Vec<_>>();
        if !s.gui {
            args.push("--background".into());
        } else if s.cli {
            args.push("--cli-owner".into());
        }
        args
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Signal {
    Show,
    Hide,
    Shutdown,
    Restart,
}

pub fn start(
    app: &Arc<App>,
    signal: impl Fn(Signal) + Send + Sync + 'static,
) -> Result<(), String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let route = wire::Route {
        protocol: wire::PROTOCOL,
        address: listener.local_addr().map_err(|e| e.to_string())?,
        token: crate::model::id(),
        instance: crate::model::id(),
        pid: std::process::id(),
    };
    let path = crate::storage::data_dir(&app.root).join(wire::ROUTE_FILE);
    let temporary = path.with_extension("tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(&route).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // The App already holds the exclusive data lock; stale routes have no owner.
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
    let weak = Arc::downgrade(app);
    let signal = Arc::new(signal);
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::spawn(move || {
        while !crate::process::stopping() && weak.strong_count() > 0 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    use std::sync::atomic::Ordering;
                    if connections.fetch_add(1, Ordering::SeqCst) >= 32 {
                        connections.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    let Some(app) = weak.upgrade() else { break };
                    let token = route.token.clone();
                    let signal = signal.clone();
                    let connections = connections.clone();
                    std::thread::spawn(move || {
                        struct Count(Arc<std::sync::atomic::AtomicUsize>);
                        impl Drop for Count {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let _count = Count(connections);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                        let mut id = Value::Null;
                        let mut after = None;
                        let result = (|| -> Result<Value, String> {
                            stream.set_nonblocking(false).map_err(|e| e.to_string())?;
                            let mut line = String::new();
                            BufReader::new(&mut stream)
                                .take(wire::REQUEST_LIMIT + 1)
                                .read_line(&mut line)
                                .map_err(|e| e.to_string())?;
                            if line.len() as u64 > wire::REQUEST_LIMIT {
                                return Err("本地请求过长".into());
                            }
                            let request: Value =
                                serde_json::from_str(&line).map_err(|_| "本地请求格式无效")?;
                            if request["token"] != token || request["protocol"] != wire::PROTOCOL {
                                return Err("本地授权或协议无效".into());
                            }
                            id = request["id"].clone();
                            if id.as_str().is_none_or(|id| id.is_empty() || id.len() > 128) {
                                return Err("请求编号无效".into());
                            }
                            let method = request["method"].as_str().ok_or("缺少操作名")?;
                            let p = &request["params"];
                            if !p.is_object() {
                                return Err("参数必须是 JSON 对象".into());
                            }
                            match method {
                                "backend.status" => Ok(app.local_runtime.snapshot()),
                                "backend.start" => {
                                    app.local_runtime.acquire(false)?;
                                    Ok(app.local_runtime.snapshot())
                                }
                                "ui.open" => {
                                    app.local_runtime.acquire(true)?;
                                    after = Some(Signal::Show);
                                    Ok(app.local_runtime.snapshot())
                                }
                                "backend.stop" | "ui.close" => {
                                    if app.storage_changing.load(Ordering::SeqCst) {
                                        return Err("正在切换数据目录，请等待完成".into());
                                    }
                                    let closing = app
                                        .local_runtime
                                        .release(method == "ui.close", p["force"] == true);
                                    if closing {
                                        after = Some(Signal::Shutdown)
                                    } else if method == "ui.close" {
                                        after = Some(Signal::Hide)
                                    }
                                    Ok(app.local_runtime.snapshot())
                                }
                                "pick" => {
                                    Err("文件选择器需要图形界面；CLI 请直接向对应业务方法传入路径"
                                        .into())
                                }
                                "storage_change" => {
                                    app.local_runtime.acquire(false)?;
                                    let _request = app.local_runtime.begin()?;
                                    crate::storage::begin(&app, p, [100, 100, 900, 700])?;
                                    after = Some(Signal::Restart);
                                    Ok(json!(true))
                                }
                                _ => {
                                    app.local_runtime.acquire(false)?;
                                    let result = crate::dispatch(&app, &request);
                                    if method == "update_install" && result.is_ok() {
                                        after = Some(Signal::Restart);
                                    }
                                    if let Err(ref error) = result {
                                        app.log("request_error",json!({"source":"cli","method":method,"request_id":id,"error":error}));
                                    }
                                    result
                                }
                            }
                        })();
                        let response = match result {
                            Ok(data) => {
                                json!({"protocol":wire::PROTOCOL,"id":id,"ok":true,"data":data})
                            }
                            Err(error) => {
                                json!({"protocol":wire::PROTOCOL,"id":id,"ok":false,"error":error})
                            }
                        };
                        let mut output = BufWriter::new(stream);
                        let _ = writeln!(output, "{response}").and_then(|_| output.flush());
                        // Exit only after the accepted lifecycle result has been returned.
                        if let Some(value) = after {
                            signal(value);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => break,
            }
        }
        if fs::read(&path)
            .ok()
            .and_then(|v| serde_json::from_slice::<wire::Route>(&v).ok())
            .is_some_and(|v| v.instance == route.instance)
        {
            let _ = fs::remove_file(path);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_accepts_delayed_fragments_and_keeps_authentication() {
        let (root, app) = crate::test_support::scoped_app("local-service-frames");
        let app = Arc::new(app);
        start(&app, |_| panic!("status queries must not change lifecycle")).unwrap();
        let route = wire::route(&root).unwrap().unwrap();
        for authorized in [false, true] {
            let token = if authorized {
                route.token.as_str()
            } else {
                "invalid"
            };
            let response = crate::test_support::fragmented_json(
                route.address,
                &json!({"protocol":wire::PROTOCOL,"token":token,"id":"fragmented-status","method":"backend.status","params":{}}),
            );
            assert_eq!(response["ok"], authorized);
            if authorized {
                assert_eq!(response["id"], "fragmented-status");
                assert_eq!(wire::success(response).unwrap()["running"], true);
            } else {
                assert!(response["error"].as_str().unwrap().contains("授权"));
            }
        }
        assert!(!app.local_runtime.cli());
        assert!(app.state.lock().unwrap().tool_runs.is_empty());
    }

    #[test]
    fn owners_are_idempotent_and_default_exit_releases_only_self() {
        let r = Runtime::default();
        r.acquire(true).unwrap();
        r.acquire(false).unwrap();
        r.acquire(false).unwrap();
        assert!(!r.release(true, false));
        assert!(!r.gui());
        assert!(r.cli());
        assert!(r.release(false, false));
        assert!(r.acquire(true).is_err());
        assert!(r.begin().is_err());
    }
    #[test]
    fn force_releases_both_and_drain_waits_for_inflight_save() {
        let r = Arc::new(Runtime::default());
        r.acquire(true).unwrap();
        r.acquire(false).unwrap();
        let guard = r.begin().unwrap();
        assert!(r.release(false, true));
        assert_eq!(r.snapshot()["active_requests"], 1);
        let worker = r.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            worker.wait_idle(Duration::from_secs(2));
            sent.send(()).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(40)).is_err());
        drop(guard);
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
        assert_eq!(r.snapshot()["active_requests"], 0);
    }
}
