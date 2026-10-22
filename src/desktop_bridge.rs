//! Windows desktop message bridge. Target conversations are never resumed or
//! executed by ShiShi. The temporary metadata process only registers our own
//! empty source, projects and the bounded context MCP configuration.
use crate::{ai_backend::ContextTool, codex_bridge::Client, model::id};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const UNCERTAIN: &str = "提交结果待核实：";
const MAX_FRAME: usize = 8 * 1024 * 1024;
pub trait DesktopApi: Send + Sync {
    fn projects(&self) -> Result<Vec<Value>, String>;
    fn create(
        &self,
        cwd: &str,
        title: &str,
        prompt: &str,
        model: &str,
        effort: &str,
        context: &ContextTool,
    ) -> Result<Value, String>;
    fn send(
        &self,
        thread: &str,
        text: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Value, String>;
    fn read(&self, thread: &str) -> Result<Value, String>;
}

/// Unit tests cannot discover a real desktop accidentally. A handling fixture
/// must explicitly inject its synthetic bridge into App.
#[cfg(test)]
pub struct OfflineDesktop;
#[cfg(test)]
impl DesktopApi for OfflineDesktop {
    fn projects(&self) -> Result<Vec<Value>, String> {
        Err("未注入离线桌面夹具".into())
    }
    fn create(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &ContextTool,
    ) -> Result<Value, String> {
        Err("未注入离线桌面夹具".into())
    }
    fn send(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Value, String> {
        Err("未注入离线桌面夹具".into())
    }
    fn read(&self, _: &str) -> Result<Value, String> {
        Err("未注入离线桌面夹具".into())
    }
}

pub struct Desktop {
    root: PathBuf,
    executable: String,
    source: Mutex<Option<String>>,
    endpoint: Mutex<Option<PathBuf>>,
    settings: Mutex<BTreeMap<String, (String, Value, Instant)>>,
    history_cursors: Mutex<BTreeMap<String, Value>>,
    latest_turns: Mutex<BTreeMap<String, Vec<String>>>,
}
impl Desktop {
    pub fn new(root: PathBuf, executable: String) -> Self {
        Self {
            root,
            executable,
            source: Mutex::new(None),
            endpoint: Mutex::new(None),
            settings: Mutex::new(BTreeMap::new()),
            history_cursors: Mutex::new(BTreeMap::new()),
            latest_turns: Mutex::new(BTreeMap::new()),
        }
    }
    fn metadata(&self) -> Result<Arc<Client>, String> {
        // Prefer the CLI shipped with this desktop. Do not inherit another
        // agent's caller identity or run a target conversation here.
        let cli = desktop_cli().unwrap_or_else(|| PathBuf::from(&self.executable));
        Client::connect_metadata(&cli.to_string_lossy())
    }
    fn source(&self) -> Result<String, String> {
        let mut source = self.source.lock().unwrap();
        if let Some(value) = source.as_ref() {
            return Ok(value.clone());
        }
        let dir = crate::storage::data_dir(&self.root).join("desktop-bridge");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join("source.json");
        if file.is_file() {
            let value: Value = serde_json::from_slice(&fs::read(&file).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let thread = value["threadId"]
                .as_str()
                .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                .ok_or("桌面接入登记无效，请核对本地 source.json")?;
            *source = Some(thread.into());
            return Ok(thread.into());
        }
        let lock_path = dir.join("registration.lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|_| "接入登记正在进行或上次结果待核实；没有重复登记")?;
        let mut attempted = false;
        let result: Result<String, String> = (|| {
            let c = self.metadata()?;
            let result: Result<String, String> = (|| {
                attempted = true;
                let response = c.call("thread/start", json!({"cwd":dir,"ephemeral":false,"persistExtendedHistory":true,"serviceName":"shishi_desktop_bridge","config":{"mcp_servers":{},"features.plugins":false}}))?;
                let thread = response["thread"]["id"]
                    .as_str()
                    .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                    .ok_or("未返回桌面接入登记编号")?
                    .to_owned();
                fs::write(
                    dir.join("source.next.json"),
                    serde_json::to_vec(&json!({"schemaVersion":1,"threadId":thread})).unwrap(),
                )
                .map_err(|e| e.to_string())?;
                fs::rename(dir.join("source.next.json"), &file).map_err(|e| e.to_string())?;
                let _ = c.call(
                    "thread/name/set",
                    json!({"threadId":thread,"name":"ShiShi 接入记录（不执行任务）"}),
                );
                let _ = c.call("thread/unsubscribe", json!({"threadId":thread}));
                Ok(thread)
            })();
            let closed = c.close();
            match result {
                Ok(thread) => {
                    closed?;
                    Ok(thread)
                }
                Err(error) => Err(error),
            }
        })();
        drop(lock);
        if !attempted || file.is_file() {
            let _ = fs::remove_file(&lock_path);
        }
        let thread = result?;
        *source = Some(thread.clone());
        Ok(thread)
    }
    fn endpoint(&self) -> Result<PathBuf, String> {
        if let Some(path) = self.endpoint.lock().unwrap().as_ref() {
            return Ok(path.clone());
        }
        let paths = fs::read_dir(r"\\.\pipe\").map_err(|e| format!("无法查找 Codex 桌面：{e}"))?;
        let mut matches = vec![];
        for entry in paths.flatten().take(4096) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("codex-app-tools-")
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                continue;
            }
            let endpoint = PathBuf::from(format!(r"\\.\pipe\{name}"));
            if let Ok(catalog) = pipe_rpc(
                &endpoint,
                "tools/list",
                json!({"threadStartKind":"all"}),
                Duration::from_secs(2),
                false,
            ) {
                if [
                    "create_thread",
                    "send_message_to_thread",
                    "read_thread",
                    "list_projects",
                ]
                .iter()
                .all(|name| {
                    catalog["tools"]
                        .as_array()
                        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == *name))
                }) {
                    matches.push(endpoint);
                }
            }
        }
        if matches.len() != 1 {
            return Err(if matches.is_empty() {
                "请先打开并登录 Codex 桌面；未找到可用消息通道"
            } else {
                "发现多个 Codex 桌面实例，请保留一个实例后重试"
            }
            .into());
        }
        let endpoint = matches.pop().unwrap();
        *self.endpoint.lock().unwrap() = Some(endpoint.clone());
        Ok(endpoint)
    }
    fn call(&self, tool: &str, arguments: Value, mutation: bool) -> Result<Value, String> {
        if ![
            "create_thread",
            "send_message_to_thread",
            "read_thread",
            "list_projects",
        ]
        .contains(&tool)
        {
            return Err("桌面桥接禁止此操作".into());
        }
        let endpoint = self.endpoint()?;
        let source = self.source()?;
        let result = pipe_rpc(
            &endpoint,
            "tools/call",
            json!({"namespace":"codex_app","tool":tool,"arguments":arguments,"callerSource":"codex","hostId":"local","threadId":source,"turnId":format!("mcp-turn-shishi-{}",id()),"callId":format!("shishi-{}",id())}),
            Duration::from_secs(if mutation { 30 } else { 8 }),
            mutation,
        );
        if result.is_err() {
            *self.endpoint.lock().unwrap() = None;
        }
        let result = result?;
        let text = result["contentItems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["type"] == "inputText")
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if result["success"] != true {
            return Err(format!("{}{text}", if mutation { UNCERTAIN } else { "" }));
        }
        serde_json::from_str(&text).map_err(|_| {
            format!(
                "{}桌面返回格式不可识别",
                if mutation { UNCERTAIN } else { "" }
            )
        })
    }
    fn project_for(&self, cwd: &str) -> Result<String, String> {
        let find = |rows: Vec<Value>| {
            rows.into_iter()
                .find(|p| same_path(p["cwd"].as_str().unwrap_or(""), cwd))
                .and_then(|p| p["id"].as_str().map(str::to_owned))
        };
        if let Some(project) = find(self.projects()?) {
            return Ok(project);
        }
        let c = self.metadata()?;
        let created =
            c.analysis_project(Path::new(cwd), &std::sync::atomic::AtomicBool::new(false));
        let closed = c.close();
        created?;
        closed?;
        // Only a project visible to the desktop is accepted. Never silently
        // create in a projectless directory different from the user's choice.
        find(self.projects()?)
            .ok_or("所选目录尚未出现在 Codex 桌面项目中，请刷新桌面项目后重试".into())
    }
    fn prepare_context(&self, context: &ContextTool) -> Result<(), String> {
        let root = context
            .context_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or("事项上下文目录无效")?;
        let c = self.metadata()?;
        let result: Result<(), String> = (|| {
            let existing = c.call("config/read", json!({"includeLayers":false}))?;
            let old = &existing["config"]["mcp_servers"]["shishi_context"];
            if old.is_object()
                && !old["args"].as_array().is_some_and(|a| {
                    a.first()
                        .and_then(Value::as_str)
                        .is_some_and(|s| ["--context-mcp", "--context-mcp-root"].contains(&s))
                })
            {
                return Err("Codex 已存在同名上下文配置，未覆盖；请处理冲突后重试".into());
            }
            c.call("config/value/write", json!({"keyPath":"mcp_servers.shishi_context","value":{"command":context.executable,"args":["--context-mcp-root",root],"enabled":true,"startup_timeout_sec":20,"tool_timeout_sec":20},"mergeStrategy":"upsert"}))?;
            Ok(())
        })();
        let closed = c.close();
        result?;
        closed
    }
    fn observed_settings(&self, thread: &str, turn: &str) -> Value {
        if let Some((_, value, _)) =
            self.settings
                .lock()
                .unwrap()
                .get(thread)
                .filter(|(known, value, at)| {
                    known == turn && (!value.is_null() || at.elapsed() < Duration::from_secs(10))
                })
        {
            return value.clone();
        }
        let result = (|| -> Result<Value, String> {
            let c = self.metadata()?;
            let result = c.call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            );
            let closed = c.close();
            let meta = result?;
            closed?;
            if meta["thread"]["id"] != thread {
                return Err("会话元数据不匹配".into());
            }
            let path = meta["thread"]["path"]
                .as_str()
                .ok_or("未提供会话记录路径")?;
            read_settings(Path::new(path), thread, turn).ok_or("未提供当前轮次模型信息".into())
        })();
        let value = result.unwrap_or(Value::Null);
        self.settings
            .lock()
            .unwrap()
            .insert(thread.into(), (turn.into(), value.clone(), Instant::now()));
        value
    }
}
impl DesktopApi for Desktop {
    fn projects(&self) -> Result<Vec<Value>, String> {
        let result = self.call("list_projects", json!({}), false)?;
        Ok(result["projects"].as_array().ok_or("桌面未返回项目列表")?.iter().filter(|p| p["projectKind"] == "local" && p["hostId"] == "local").filter_map(|p| Some(json!({"id":p["projectId"].as_str()?,"name":p["label"].as_str()?,"cwd":p["path"].as_str()?}))).collect())
    }
    fn create(
        &self,
        cwd: &str,
        title: &str,
        prompt: &str,
        model: &str,
        effort: &str,
        context: &ContextTool,
    ) -> Result<Value, String> {
        let project = self.project_for(cwd)?;
        self.prepare_context(context)?;
        let mut args = json!({"title":title,"prompt":prompt,"target":{"type":"project","projectId":project,"environment":{"type":"local"}}});
        overrides(&mut args, Some(model), Some(effort));
        self.call("create_thread", args, true)
    }
    fn send(
        &self,
        thread: &str,
        text: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Value, String> {
        let mut args = json!({"threadId":thread,"hostId":"local","prompt":text});
        overrides(&mut args, model, effort);
        self.call("send_message_to_thread", args, true)
    }
    fn read(&self, thread: &str) -> Result<Value, String> {
        let mut snapshot=self.call("read_thread", json!({"threadId":thread,"hostId":"local","turnLimit":10,"includeOutputs":true,"maxOutputCharsPerItem":20000}), false)?;
        let ids: Vec<String> = snapshot["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["id"].as_str().map(str::to_owned))
            .collect();
        let previous = self
            .latest_turns
            .lock()
            .unwrap()
            .insert(thread.into(), ids.clone())
            .unwrap_or_default();
        if previous.is_empty() || !ids.iter().any(|id| previous.contains(id)) {
            self.history_cursors
                .lock()
                .unwrap()
                .insert(thread.into(), snapshot["page"]["nextCursor"].clone());
        }
        let cursor = self
            .history_cursors
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .unwrap_or(Value::Null);
        if !cursor.is_null() {
            let older=self.call("read_thread",json!({"threadId":thread,"hostId":"local","turnLimit":10,"includeOutputs":true,"maxOutputCharsPerItem":20000,"cursor":cursor}),false)?;
            if let Some(rows) = snapshot["turns"].as_array_mut() {
                for turn in older["turns"].as_array().into_iter().flatten() {
                    if !rows.iter().any(|v| v["id"] == turn["id"]) {
                        rows.push(turn.clone());
                    }
                }
            }
            self.history_cursors
                .lock()
                .unwrap()
                .insert(thread.into(), older["page"]["nextCursor"].clone());
        }
        if let Some(turn) = snapshot["turns"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(|v| v["id"].as_str())
        {
            let settings = self.observed_settings(thread, turn);
            if let Some(model) = settings["model"].as_str() {
                snapshot["thread"]["model"] = json!(model);
            }
            if let Some(effort) = settings["effort"].as_str() {
                snapshot["thread"]["reasoningEffort"] = json!(effort);
            }
        }
        Ok(snapshot)
    }
}

/// Only return settings from the exact thread/turn named by desktop metadata.
/// This bounded fallback never reads or displays reasoning text.
fn read_settings(path: &Path, thread: &str, turn: &str) -> Option<Value> {
    let mut file = fs::File::open(path).ok()?;
    let mut header = String::new();
    BufReader::new((&mut file).take(65536))
        .read_line(&mut header)
        .ok()?;
    let meta: Value = serde_json::from_str(&header).ok()?;
    if meta["type"] != "session_meta" || meta["payload"]["id"] != thread {
        return None;
    }
    let start = file.metadata().ok()?.len().saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![];
    file.take(256 * 1024).read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["type"] == "turn_context" && v["payload"]["turn_id"] == turn)
        .last()
        .map(|v| json!({"model":v["payload"]["model"],"effort":v["payload"]["effort"]}))
}

/// Explicit local diagnostics: no target prompt, turn, resume or interruption.
pub fn check(root: PathBuf, executable: String) -> Result<Value, String> {
    let desktop = Desktop::new(root, executable);
    let projects = desktop.projects()?;
    let source = desktop.source()?;
    let snapshot = desktop.read(&source)?;
    Ok(
        json!({"connected":true,"own_registration":snapshot["thread"]["id"]==source,"local_projects":projects.len(),"model_calls":0}),
    )
}
fn overrides(args: &mut Value, model: Option<&str>, effort: Option<&str>) {
    if let Some(model) = model.filter(|s| !s.trim().is_empty()) {
        args["model"] = json!(model);
    }
    if let Some(effort) = effort.filter(|s| !s.trim().is_empty()) {
        args["thinking"] = json!(effort);
    }
}
pub fn same_path(a: &str, b: &str) -> bool {
    Path::new(a)
        .canonicalize()
        .ok()
        .zip(Path::new(b).canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
}
fn desktop_cli() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("OpenAI/Codex/bin");
    fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|v| v.path().join("codex.exe"))
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max_by_key(|v| v.0)
        .map(|v| v.1)
}

fn pipe_rpc(
    endpoint: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
    mutation: bool,
) -> Result<Value, String> {
    use std::os::windows::io::AsRawHandle;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(endpoint)
        .map_err(|e| format!("Codex 桌面连接失败：{e}"))?;
    let mode = windows_sys::Win32::System::Pipes::PIPE_NOWAIT;
    if unsafe {
        windows_sys::Win32::System::Pipes::SetNamedPipeHandleState(
            file.as_raw_handle() as _,
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(format!(
            "无法设置桌面读取期限：{}",
            std::io::Error::last_os_error()
        ));
    }
    let request_id = id();
    let body = serde_json::to_vec(
        &json!({"jsonrpc":"2.0","id":request_id,"method":method,"params":params}),
    )
    .unwrap();
    if body.len() > MAX_FRAME {
        return Err("桌面请求超过大小限制".into());
    }
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&body);
    let deadline = Instant::now() + timeout;
    let mut attempted = false;
    let result = (|| {
        let mut offset = 0;
        while offset < frame.len() {
            if Instant::now() >= deadline {
                return Err("桌面提交超时".into());
            }
            attempted = true;
            match file.write(&frame[offset..]) {
                Ok(n) => offset += n,
                Err(e) if e.raw_os_error() == Some(232) => {}
                Err(e) => return Err(format!("桌面写入失败：{e}")),
            }
            if offset < frame.len() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let mut bytes = vec![];
        let mut buffer = [0u8; 65536];
        loop {
            if Instant::now() >= deadline {
                return Err("读取 Codex 桌面超时".into());
            }
            match file.read(&mut buffer) {
                Ok(0) => {}
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.raw_os_error() == Some(232) => {}
                Err(e) => return Err(format!("桌面连接已断开：{e}")),
            }
            if bytes.len() >= 4 {
                let size = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
                if size > MAX_FRAME {
                    return Err("桌面响应超过大小限制".into());
                }
                if bytes.len() >= size + 4 {
                    let response: Value =
                        serde_json::from_slice(&bytes[4..size + 4]).map_err(|e| e.to_string())?;
                    if response["id"] != request_id {
                        return Err("桌面回执编号不匹配".into());
                    }
                    if response.get("error").is_some() {
                        return Err(format!(
                            "桌面拒绝请求：{}",
                            response["error"]["message"].as_str().unwrap_or("未知错误")
                        ));
                    }
                    return Ok(response["result"].clone());
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    result.map_err(|e: String| {
        if mutation && attempted {
            format!("{UNCERTAIN}{e}；未自动重发")
        } else {
            e
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_overrides_never_reset_desktop_settings() {
        let mut args = json!({});
        overrides(&mut args, None, None);
        assert_eq!(args, json!({}));
        overrides(&mut args, Some("synthetic"), Some("high"));
        assert_eq!(args, json!({"model":"synthetic","thinking":"high"}));
    }
    #[test]
    fn settings_fallback_requires_exact_thread_and_turn_metadata() {
        let file = std::env::temp_dir().join(format!("shishi-settings-{}.jsonl", id()));
        let text = format!(
            "{}\n{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            json!({"type":"turn_context","payload":{"turn_id":"old","model":"old-model","effort":"low"}}),
            json!({"type":"turn_context","payload":{"turn_id":"current","model":"current-model","effort":"high"}})
        );
        fs::write(&file, text).unwrap();
        assert!(read_settings(&file, "other-thread", "current").is_none());
        assert!(read_settings(&file, "thread", "unknown-turn").is_none());
        assert_eq!(
            read_settings(&file, "thread", "current").unwrap()["model"],
            "current-model"
        );
        fs::remove_file(file).unwrap();
    }
    fn server(
        reply: impl FnOnce(&mut fs::File, Value) + Send + 'static,
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        use std::os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        };
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            Storage::FileSystem::PIPE_ACCESS_DUPLEX,
            System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT},
        };
        let path = PathBuf::from(format!(r"\\.\pipe\shishi-desktop-test-{}", id()));
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                65536,
                65536,
                1000,
                std::ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let mut file = unsafe { fs::File::from_raw_handle(handle as _) };
        let thread = std::thread::spawn(move || {
            let connected =
                unsafe { ConnectNamedPipe(file.as_raw_handle() as _, std::ptr::null_mut()) };
            assert!(connected != 0 || std::io::Error::last_os_error().raw_os_error() == Some(535));
            let mut header = [0u8; 4];
            file.read_exact(&mut header).unwrap();
            let size = u32::from_le_bytes(header) as usize;
            assert!(size < MAX_FRAME);
            let mut body = vec![0; size];
            file.read_exact(&mut body).unwrap();
            reply(&mut file, serde_json::from_slice(&body).unwrap());
        });
        (path, thread)
    }
    #[test]
    fn native_pipe_reassembles_fragmented_utf8_frames() {
        let (path, thread) = server(|file, request| {
            assert_eq!(request["method"], "tools/list");
            let body = serde_json::to_vec(&json!({"id":request["id"],"result":{"text":"中文🚀"}}))
                .unwrap();
            let mut frame = (body.len() as u32).to_le_bytes().to_vec();
            frame.extend_from_slice(&body);
            for chunk in frame.chunks(3) {
                file.write_all(chunk).unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let result = pipe_rpc(
            &path,
            "tools/list",
            json!({}),
            Duration::from_secs(3),
            false,
        )
        .unwrap();
        assert_eq!(result["text"], "中文🚀");
        thread.join().unwrap();
    }
    #[test]
    fn native_mutation_timeout_is_uncertain_without_a_second_request() {
        let (path, thread) = server(|_, _| std::thread::sleep(Duration::from_millis(150)));
        assert!(pipe_rpc(
            &path,
            "tools/call",
            json!({}),
            Duration::from_millis(50),
            true
        )
        .unwrap_err()
        .starts_with(UNCERTAIN));
        thread.join().unwrap();
    }
    #[test]
    fn native_pipe_rejects_oversized_response_headers() {
        let (path, thread) = server(|file, _| {
            file.write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
                .unwrap()
        });
        assert!(pipe_rpc(
            &path,
            "tools/list",
            json!({}),
            Duration::from_secs(3),
            false
        )
        .unwrap_err()
        .contains("大小限制"));
        thread.join().unwrap();
    }
}
