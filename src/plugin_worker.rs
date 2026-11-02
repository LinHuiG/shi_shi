//! One reusable plugin process. Calls are serialized so cancellation and a
//! native crash have an unambiguous owner; queued calls never enter the DLL.
use crate::plugins::{Library, Plugin};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

const MAX: u64 = 32 * 1024 * 1024;
static HOST: Mutex<Host> = Mutex::new(Host {
    worker: None,
    failures: 0,
    retry_at: None,
});
struct Host {
    worker: Option<Worker>,
    failures: u32,
    retry_at: Option<Instant>,
}
struct Worker {
    child: crate::process::OwnedChild,
    directory: PathBuf,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.child.terminate();
        let _ = self.child.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
#[derive(Serialize, Deserialize)]
struct Request {
    id: String,
    manifest: PathBuf,
    settings: bool,
    input: Value,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    if fs::metadata(path).map_err(|e| e.to_string())?.len() > MAX {
        return Err("插件通信超过 32 MiB".into());
    }
    fs::read(path).map_err(|e| e.to_string())
}
fn publish(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX {
        return Err("插件通信超过 32 MiB".into());
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}
impl Worker {
    fn start() -> Result<Self, String> {
        let directory =
            std::env::temp_dir().join(format!("shishi-plugins-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .arg("--plugin-worker")
            .arg(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match crate::process::spawn_owned(&mut command) {
            Ok(child) => Ok(Self { child, directory }),
            Err(error) => {
                let _ = fs::remove_dir_all(directory);
                Err(error)
            }
        }
    }
    fn exchange(
        &mut self,
        request: &Request,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value, String> {
        publish(&self.directory.join("request.json"), request)?;
        let started = Instant::now();
        loop {
            crate::process::check_running()?;
            if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                return Err("插件操作已取消".into());
            }
            if started.elapsed() >= Duration::from_secs(90) {
                return Err("插件工作进程调用超时（90 秒）".into());
            }
            if let Some(status) = self.child.child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!(
                    "插件工作进程退出，退出码 {:?}",
                    status.code().map(|c| format!("0x{:08x}", c as u32))
                ));
            }
            let path = self.directory.join("response.json");
            if path.exists() {
                let response: Value =
                    serde_json::from_slice(&read(&path)?).map_err(|e| e.to_string())?;
                fs::remove_file(path).map_err(|e| e.to_string())?;
                if response["id"] != request.id || !response["ok"].is_boolean() {
                    return Err("插件工作进程响应协议不匹配".into());
                }
                return Ok(response);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
pub fn reset() -> Result<(), String> {
    let mut host = HOST.lock().map_err(|_| "插件工作进程状态不可用")?;
    host.worker = None;
    host.failures = 0;
    host.retry_at = None;
    Ok(())
}
pub fn call(
    plugin: &Plugin,
    settings: bool,
    input: Value,
    cancel: Option<&AtomicBool>,
) -> Result<Value, String> {
    let size = serde_json::to_vec(&input).map_err(|e| e.to_string())?.len();
    if size as u64 > MAX || (settings && size > 2 * 1024 * 1024) {
        return Err("插件请求过大".into());
    }
    let queued = Instant::now();
    let mut host = loop {
        crate::process::check_running()?;
        if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            return Err("插件操作已取消".into());
        }
        match HOST.try_lock() {
            Ok(host) => break host,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("插件工作进程状态不可用".into())
            }
            Err(_) => {}
        }
        if queued.elapsed() >= Duration::from_secs(90) {
            return Err("等待插件工作进程超时".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if host.retry_at.is_some_and(|at| Instant::now() < at) {
        return Err("插件工作进程连续失败，正在退避，请稍后重试".into());
    }
    let request = Request {
        id: uuid::Uuid::new_v4().to_string(),
        manifest: plugin.dir.join("plugin.json"),
        settings,
        input,
    };
    let started = Instant::now();
    let result = (|| {
        if host.worker.is_none() {
            host.worker = Some(Worker::start()?);
        }
        host.worker.as_mut().unwrap().exchange(&request, cancel)
    })();
    match result {
        Ok(response) => {
            host.failures = 0;
            host.retry_at = None;
            if response["ok"] == true {
                Ok(response["result"].clone())
            } else {
                Err(response["error"].as_str().unwrap_or("插件调用失败").into())
            }
        }
        Err(error) => {
            host.worker = None;
            if !cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                host.failures = host.failures.saturating_add(1);
            }
            host.retry_at = Some(
                Instant::now()
                    + Duration::from_secs(if host.failures < 2 {
                        0
                    } else {
                        (1u64 << host.failures.min(6)).min(60)
                    }),
            );
            // No parameters or plugin output: these can contain credentials.
            Err(format!(
                "插件 {}，方法 {}，请求 {}，调用阶段，耗时 {}ms：{}；工作进程已回收",
                plugin.manifest.id,
                request.input["method"].as_str().unwrap_or(""),
                request.id,
                started.elapsed().as_millis(),
                error
            ))
        }
    }
}

pub fn run(directory: &Path) -> Result<(), String> {
    // Libraries stay loaded until this process exits, including business errors.
    let mut libraries = BTreeMap::<PathBuf, Library>::new();
    loop {
        let path = directory.join("request.json");
        if !path.exists() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let request: Request = serde_json::from_slice(&read(&path)?).map_err(|e| e.to_string())?;
        fs::remove_file(path).map_err(|e| e.to_string())?;
        let result = (|| {
            let plugin = Plugin::read(&request.manifest)?;
            let path = plugin.dir.join(&plugin.manifest.entrypoint);
            if !libraries.contains_key(&path) {
                libraries.insert(path.clone(), Library::open(&path)?);
            }
            let library = &libraries[&path];
            let method = request.input["method"].as_str().ok_or("缺少插件方法")?;
            if request.settings {
                crate::plugin_settings::call_loaded(
                    &plugin,
                    library,
                    &request.input["config"],
                    method,
                    &request.input["params"],
                )
            } else {
                if !plugin.manifest.capabilities.iter().any(|m| m == method) {
                    return Err("插件未声明此方法".into());
                }
                library.call(&serde_json::to_vec(&request.input).map_err(|e| e.to_string())?)
            }
        })();
        let response = match result {
            Ok(value) => json!({"id":request.id,"ok":true,"result":value}),
            Err(error) => json!({"id":request.id,"ok":false,"error":error}),
        };
        publish(&directory.join("response.json"), &response)?;
    }
}
