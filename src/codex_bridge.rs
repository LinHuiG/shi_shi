//! Persistent Codex app-server transport. Uses the user's normal configuration.
use crate::process;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    process::{ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

static ACTIVITY: (Mutex<u64>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());
pub(crate) fn signal_activity() {
    let mut generation = ACTIVITY.0.lock().unwrap();
    *generation = generation.wrapping_add(1);
    ACTIVITY.1.notify_all();
}
pub fn wait_for_activity(seen: &mut u64) -> bool {
    let generation = ACTIVITY.0.lock().unwrap();
    let (generation, _) = ACTIVITY
        .1
        .wait_timeout_while(generation, Duration::from_secs(2), |g| *g == *seen)
        .unwrap();
    let changed = *generation != *seen;
    *seen = *generation;
    changed
}

pub struct Client {
    child: Mutex<Option<process::OwnedChild>>,
    input: Mutex<ChildStdin>,
    pending: Mutex<BTreeMap<u64, mpsc::Sender<Result<Value, String>>>>,
    events: Mutex<VecDeque<Value>>,
    requests: Mutex<BTreeMap<String, Value>>,
    approval_items: Mutex<BTreeMap<String, Value>>,
    diagnostic: Mutex<String>,
    next: AtomicU64,
    pub alive: AtomicBool,
}
impl Client {
    pub fn close(&self) -> Result<(), String> {
        let mut slot = self.child.lock().unwrap();
        if let Some(child) = slot.as_mut() {
            if child.child.try_wait().map_err(|e| e.to_string())?.is_none() {
                child.child.kill().map_err(|e| e.to_string())?;
            }
            child.child.wait().map_err(|e| e.to_string())?;
        }
        slot.take();
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
    pub fn analysis_project(
        &self,
        cwd: &std::path::Path,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let normalized = cwd.canonicalize().map_err(|e| e.to_string())?;
        let path = normalized
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        let mut cursor = Value::Null;
        for _ in 0..100 {
            let page = self.call_guarded(
                "project/list",
                json!({"limit":100,"cursor":cursor}),
                Some(cancel),
            )?;
            for project in page["data"].as_array().into_iter().flatten() {
                if project["roots"].as_array().into_iter().flatten().any(|r| {
                    r["path"].as_str().is_some_and(|p| {
                        std::path::Path::new(p).canonicalize().ok().as_ref() == Some(&normalized)
                    })
                }) {
                    return project["id"]
                        .as_str()
                        .map(String::from)
                        .ok_or("Codex 项目缺少编号".into());
                }
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        // The server idempotency key is stable for this normalized path, including retries.
        let key = format!("shishi:{}", path.to_lowercase());
        let project = self.call_guarded(
            "project/create",
            json!({"idempotencyKey":key,"name":"ShiShi","roots":[{"path":path}]}),
            Some(cancel),
        )?;
        project["project"]["id"]
            .as_str()
            .map(String::from)
            .ok_or("Codex 未返回项目编号".into())
    }
    pub fn connect(executable: &str) -> Result<Arc<Self>, String> {
        Self::connect_guarded(executable, None)
    }
    pub fn connect_guarded(
        executable: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<Arc<Self>, String> {
        if !std::path::Path::new(executable).is_file() {
            return Err("请先在设置中选择 Codex CLI".into());
        }
        let mut command = Command::new(crate::ai_backend::codex::resolve_cli(executable)?);
        command.args(["app-server"]);
        Self::connect_command_guarded(command, cancel)
    }
    #[cfg(test)]
    pub(crate) fn connect_command(command: Command) -> Result<Arc<Self>, String> {
        Self::connect_command_guarded(command, None)
    }
    fn connect_command_guarded(
        mut command: Command,
        cancel: Option<&AtomicBool>,
    ) -> Result<Arc<Self>, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process::spawn_owned(&mut command)?;
        let input = child.child.stdin.take().ok_or("无法打开 Codex 输入")?;
        let output = child.child.stdout.take().ok_or("无法读取 Codex 输出")?;
        let error = child.child.stderr.take();
        let client = Arc::new(Self {
            child: Mutex::new(Some(child)),
            input: Mutex::new(input),
            pending: Mutex::new(BTreeMap::new()),
            events: Mutex::new(VecDeque::new()),
            requests: Mutex::new(BTreeMap::new()),
            approval_items: Mutex::new(BTreeMap::new()),
            diagnostic: Mutex::new(String::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        });
        let weak = Arc::downgrade(&client);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                match reader
                    .by_ref()
                    .take(32 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
                if line.len() > 32 * 1024 * 1024 {
                    break;
                }
                let Some(c) = weak.upgrade() else { break };
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    c.receive(value);
                }
            }
            if let Some(c) = weak.upgrade() {
                c.alive.store(false, Ordering::SeqCst);
                signal_activity();
                for (_, tx) in std::mem::take(&mut *c.pending.lock().unwrap()) {
                    let _ = tx.send(Err(
                        "Codex 连接已断开，请重新同步；不会自动重复提交处置".into()
                    ));
                }
            }
        });
        if let Some(error) = error {
            let weak = Arc::downgrade(&client);
            std::thread::spawn(move || {
                for line in BufReader::new(error).lines().map_while(Result::ok) {
                    let Some(c) = weak.upgrade() else { break };
                    *c.diagnostic.lock().unwrap() = line.chars().take(1000).collect();
                }
            });
        }
        client.call_guarded("initialize",json!({"clientInfo":{"name":"shishi","title":"拾事","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),cancel)?;
        client.write(json!({"method":"initialized"}))?;
        Ok(client)
    }
    fn receive(&self, mut value: Value) {
        if value.get("method").is_none() {
            if let Some(id) = value["id"].as_u64() {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let result = if value.get("error").is_some() {
                        Err(format!(
                            "{}: {}",
                            value["error"]["code"],
                            value["error"]["message"]
                                .as_str()
                                .unwrap_or("Codex 请求失败")
                        ))
                    } else {
                        Ok(value["result"].clone())
                    };
                    let _ = tx.send(result);
                }
            }
            return;
        }
        if value.get("id").is_some() {
            if value["method"] == "currentTime/read" {
                let _=self.write(json!({"id":value["id"],"result":{"currentTimeAt":chrono::Utc::now().timestamp()}}));
                return;
            }
            if value["params"]["threadId"].as_str().is_none() {
                // Authentication/client-level requests cannot be approved as a task action.
                let _=self.write(json!({"id":value["id"],"error":{"code":-32601,"message":"拾事不支持此客户端级请求。请在 Codex 完成登录或相关配置后重试。"}}));
                return;
            }
            let p = &mut value["params"];
            let item_key = json!([p["threadId"], p["turnId"], p["itemId"]]).to_string();
            if let Some(item) = self.approval_items.lock().unwrap().get(&item_key) {
                for key in ["command", "cwd", "changes"] {
                    if p[key].is_null() && !item[key].is_null() {
                        p[key] = item[key].clone();
                    }
                }
            }
            self.requests
                .lock()
                .unwrap()
                .insert(value["id"].to_string(), value.clone());
        }
        if value["method"] == "serverRequest/resolved" {
            self.requests
                .lock()
                .unwrap()
                .remove(&value["params"]["requestId"].to_string());
        }
        if ["item/started", "item/completed"]
            .iter()
            .any(|m| value["method"] == *m)
        {
            let p = &value["params"];
            let item = &p["item"];
            if ["commandExecution", "fileChange"]
                .iter()
                .any(|kind| item["type"] == *kind)
            {
                let key = json!([p["threadId"], p["turnId"], item["id"]]).to_string();
                let mut items = self.approval_items.lock().unwrap();
                items.insert(
                    key,
                    json!({"command":item["command"],"cwd":item["cwd"],"changes":item["changes"]}),
                );
                while items.len() > 2000 {
                    items.pop_first();
                }
            }
        }
        let mut events = self.events.lock().unwrap();
        events.push_back(value);
        while events.len() > 2000 {
            events.pop_front();
        }
        drop(events);
        signal_activity();
    }
    fn write(&self, value: Value) -> Result<(), String> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err("Codex 连接已断开".into());
        }
        let mut input = self.input.lock().unwrap();
        writeln!(input, "{value}")
            .and_then(|_| input.flush())
            .map_err(|e| e.to_string())
    }
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_guarded(method, params, None)
    }
    pub fn call_guarded(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value, String> {
        process::check_running()?;
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return Err(crate::ai_control::CANCELLED.into());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(json!({"id":id,"method":method,"params":params})) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let result = loop {
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                self.pending.lock().unwrap().remove(&id);
                return Err(crate::ai_control::CANCELLED.into());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {
                    continue
                }
                other => break other,
            }
        };
        self.pending.lock().unwrap().remove(&id);
        result.map_err(|_| {
            format!(
                "Codex 请求超时（{method}）。结果可能尚未返回，请先同步状态，避免重复创建会话。{}",
                self.diagnostic.lock().unwrap()
            )
        })?
    }
    pub fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().drain(..).collect()
    }
    pub fn requests(&self, thread: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .values()
            .filter(|r| r["params"]["threadId"] == thread)
            .cloned()
            .collect()
    }
    pub fn reply(&self, key: &str, thread: &str, result: Value) -> Result<(), String> {
        let mut requests = self.requests.lock().unwrap();
        let request = requests.get(key).ok_or("该请求已失效，请同步状态")?;
        if request["params"]["threadId"] != thread {
            return Err("请求不属于当前事项会话".into());
        }
        self.write(json!({"id":request["id"],"result":result}))?;
        requests.remove(key);
        Ok(())
    }
}
pub fn open_thread(thread: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(thread).map_err(|_| "无效会话编号")?;
    #[cfg(not(test))]
    {
        let target = format!("codex://threads/{thread}")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let result = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if result as isize <= 32 {
            return Err("无法打开 Codex 前台；会话已保存，可按标题查找".into());
        }
    }
    Ok(())
}
impl Drop for Client {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.child.kill();
            drop(child);
        }
    }
}
