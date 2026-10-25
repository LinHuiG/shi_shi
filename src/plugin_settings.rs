//! Plugin-owned HTML is rendered in a sandboxed frame in the existing source
//! editor. Native calls run in a bounded child process, never the UI process.
use crate::{
    engine::App,
    plugins::{Library, Plugin},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub struct Session {
    plugin: Plugin,
    config: Value,
    created: Instant,
    cancelled: Arc<AtomicBool>,
    phase: Mutex<Phase>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Calling,
    Saving,
    Closed,
}
pub type Sessions = BTreeMap<String, Arc<Session>>;
#[derive(Serialize, Deserialize)]
struct Ticket {
    manifest: PathBuf,
    config: Value,
    method: String,
    params: Value,
}

pub fn begin(app: &App, plugin: &Plugin, config: &Value) -> Result<Value, String> {
    let settings = plugin
        .manifest
        .settings
        .as_ref()
        .ok_or("插件未提供配置页面")?;
    let html =
        fs::read_to_string(plugin.dir.join(&settings.entrypoint)).map_err(|e| e.to_string())?;
    let mut sessions = app.plugin_settings.lock().unwrap();
    sessions.retain(|_, s| {
        let mut phase = s.phase.lock().unwrap();
        let retain = s.created.elapsed() < Duration::from_secs(3600) || *phase == Phase::Saving;
        if !retain {
            *phase = Phase::Closed;
            s.cancelled.store(true, Ordering::Release);
        }
        retain
    });
    if sessions.len() >= 8 {
        return Err("打开的插件配置过多，请先关闭已有配置".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    sessions.insert(
        id.clone(),
        Arc::new(Session {
            plugin: plugin.clone(),
            config: config.clone(),
            created: Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
            phase: Mutex::new(Phase::Idle),
        }),
    );
    Ok(json!({"session":id,"html":html}))
}

pub fn dispatch(app: &App, value: &Value) -> Result<Value, String> {
    let id = value["session"].as_str().ok_or("缺少插件配置会话")?;
    let action = value["action"].as_str().unwrap_or("");
    if action == "cancel" {
        let mut sessions = app.plugin_settings.lock().unwrap();
        if let Some(session) = sessions.get(id) {
            let mut phase = session.phase.lock().unwrap();
            if *phase == Phase::Saving {
                return Err("正在保存来源，请等待保存结果".into());
            }
            *phase = Phase::Closed;
            session.cancelled.store(true, Ordering::Release);
        }
        sessions.remove(id);
        return Ok(json!(true));
    }
    let session = app
        .plugin_settings
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or("插件配置已关闭，请重新打开")?;
    {
        let mut phase = session.phase.lock().unwrap();
        match *phase {
            Phase::Closed => return Err("插件配置已取消".into()),
            Phase::Calling | Phase::Saving => return Err("请等待当前配置操作完成".into()),
            Phase::Idle => {
                *phase = if action == "complete" {
                    Phase::Saving
                } else {
                    Phase::Calling
                };
            }
        }
    }
    struct Busy<'a>(&'a Mutex<Phase>);
    impl Drop for Busy<'_> {
        fn drop(&mut self) {
            let mut phase = self.0.lock().unwrap();
            if *phase != Phase::Closed {
                *phase = Phase::Idle;
            }
        }
    }
    let _busy = Busy(&session.phase);
    let _access = app.plugin_access.read().map_err(|_| "插件状态不可用")?;
    let current = crate::plugins::find(&app.root, &session.plugin.manifest.id)?;
    if serde_json::to_value(&current.manifest).map_err(|e| e.to_string())?
        != serde_json::to_value(&session.plugin.manifest).map_err(|e| e.to_string())?
    {
        return Err("插件已更新，请重新打开配置页面".into());
    }
    if action == "complete" {
        let config = value.get("config").unwrap_or(&session.config);
        if !config.is_object()
            || serde_json::to_vec(config).map_err(|e| e.to_string())?.len() > 2 * 1024 * 1024
        {
            return Err("插件配置格式无效或过大".into());
        }
        current.validate_fields(config)?;
        let mut source = value["source"].clone();
        if !source.is_object() || source["plugin_id"] != session.plugin.manifest.id {
            return Err("来源与当前插件配置不匹配".into());
        }
        source["config"] = config.clone();
        // source_save owns validation and the durable commit. Do not reacquire
        // the plugin read lock while a writer may already be waiting for it.
        drop(_access);
        app.source_save(&json!({"source":source}))?;
        *session.phase.lock().unwrap() = Phase::Closed;
        app.plugin_settings.lock().unwrap().remove(id);
        return Ok(config.clone());
    }
    if action != "call" {
        return Err("不支持的插件配置操作".into());
    }
    let method = value["method"].as_str().ok_or("缺少配置方法")?;
    if !current
        .manifest
        .settings
        .as_ref()
        .is_some_and(|s| s.methods.iter().any(|m| m == method))
    {
        return Err("插件未声明此配置方法".into());
    }
    let directory = crate::storage::data_dir(&app.root)
        .join("tmp/plugin-settings")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    // Request parameters may contain a manually entered secret. Keep neither
    // request nor response after the bounded helper has exited or been killed.
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _temporary = Temporary(directory.clone());
    let ticket = directory.join("request.json");
    let bytes = serde_json::to_vec(&Ticket {
        manifest: current.dir.join("plugin.json"),
        config: session.config.clone(),
        method: method.into(),
        params: value["params"].clone(),
    })
    .map_err(|e| e.to_string())?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("插件配置请求过大".into());
    }
    fs::write(&ticket, bytes).map_err(|e| e.to_string())?;
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command.arg("--plugin-settings-call").arg(&ticket);
    let mut child = crate::process::spawn_owned(&mut command)?;
    let started = Instant::now();
    loop {
        if session.cancelled.load(Ordering::Acquire) {
            return Err("插件配置已取消".into());
        }
        if started.elapsed() > Duration::from_secs(95) {
            return Err("插件配置操作超时".into());
        }
        crate::process::check_running()?;
        if let Some(status) = child.child.try_wait().map_err(|e| e.to_string())? {
            let path = directory.join("result.json");
            if fs::metadata(&path)
                .map_err(|_| "插件配置调用未正常完成")?
                .len()
                > 32 * 1024 * 1024
            {
                return Err("插件配置响应过大".into());
            }
            let response: Value =
                serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if !status.success() || response["ok"] != true {
                return Err(response["error"]
                    .as_str()
                    .unwrap_or("插件配置调用失败")
                    .into());
            }
            return Ok(response["result"].clone());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn run(path: &Path) -> Result<(), String> {
    let ticket: Ticket = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let plugin = Plugin::read(&ticket.manifest)?;
    let result = call(
        &plugin,
        &ticket.config,
        &ticket.method,
        &ticket.params,
        Arc::new(AtomicBool::new(false)),
    );
    let response = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    fs::write(path.with_file_name("result.json"), response.to_string()).map_err(|e| e.to_string())
}
struct Cancellation {
    closed: Arc<AtomicBool>,
    deadline: Instant,
}
unsafe extern "C" fn cancelled(context: usize) -> u32 {
    let state = &*(context as *const Cancellation);
    u32::from(state.closed.load(Ordering::Acquire) || Instant::now() >= state.deadline)
}

fn call(
    plugin: &Plugin,
    config: &Value,
    method: &str,
    params: &Value,
    closed: Arc<AtomicBool>,
) -> Result<Value, String> {
    let settings = plugin
        .manifest
        .settings
        .as_ref()
        .ok_or("插件未提供配置页面")?;
    if !settings.methods.iter().any(|allowed| allowed == method) {
        return Err("插件未声明此配置方法".into());
    }
    let input = serde_json::to_vec(&json!({"protocol":1,"method":method,"config":config,"params":params,"context":{"plugin_dir":plugin.dir,"timeout_ms":90000}})).map_err(|e| e.to_string())?;
    if input.len() > 2 * 1024 * 1024 {
        return Err("插件配置请求过大".into());
    }
    let library = Library::open(&plugin.dir.join(&plugin.manifest.entrypoint))?;
    let cancel = Cancellation {
        closed,
        deadline: Instant::now() + Duration::from_secs(90),
    };
    unsafe {
        let abi: unsafe extern "C" fn() -> u32 =
            std::mem::transmute(library.symbol(b"shishi_plugin_abi\0")?);
        if abi() != 1 {
            return Err("插件 ABI 不兼容".into());
        }
        type Call = unsafe extern "C" fn(
            *const u8,
            usize,
            *mut usize,
            Option<unsafe extern "C" fn(usize) -> u32>,
            usize,
        ) -> *mut u8;
        let invoke: Call = std::mem::transmute(library.symbol(b"shishi_plugin_settings_call\0")?);
        let free: unsafe extern "C" fn(*mut u8, usize) =
            std::mem::transmute(library.symbol(b"shishi_plugin_free\0")?);
        let mut length = 0;
        let pointer = invoke(
            input.as_ptr(),
            input.len(),
            &mut length,
            Some(cancelled),
            &cancel as *const _ as usize,
        );
        if pointer.is_null() {
            return Err("插件返回空响应".into());
        }
        if length > 32 * 1024 * 1024 {
            free(pointer, length);
            return Err("插件配置响应过大".into());
        }
        let result = serde_json::from_slice::<Value>(std::slice::from_raw_parts(pointer, length));
        free(pointer, length);
        if cancelled(&cancel as *const _ as usize) != 0 {
            return Err("插件配置操作已取消或超时".into());
        }
        let value = result.map_err(|e| e.to_string())?;
        if value["ok"] != true {
            return Err(value["error"].as_str().unwrap_or("插件配置操作失败").into());
        }
        Ok(value["result"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_saves_source_once_and_keeps_session_on_failure() {
        let root = std::env::temp_dir().join(format!("shishi-settings-{}", uuid::Uuid::new_v4()));
        let app = Arc::new(App::load(root.clone()).unwrap());
        let directory = crate::storage::data_dir(&root).join("plugins/synthetic");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("fixture.dll"), b"not loaded in this test").unwrap();
        fs::write(directory.join("settings.html"), "<p>Synthetic</p>").unwrap();
        let manifest = json!({"id":"synthetic","name":"Synthetic","version":"1","api_version":1,
            "entrypoint":"fixture.dll","files":["fixture.dll","settings.html"],
            "capabilities":["discover","validate","fetch_incremental","fetch_range","lookup_messages"],
            "settings":{"api_version":1,"entrypoint":"settings.html","methods":["ui_load"]}});
        fs::write(directory.join("plugin.json"), manifest.to_string()).unwrap();
        // Opening settings must not load or discover through the fake DLL.
        let opened = app
            .source_discover(&json!({"plugin_id":"synthetic","config":{}}))
            .unwrap();
        let id = opened["settings"]["session"].as_str().unwrap();
        let request = json!({"session":id,"action":"complete","config":{"profile":"synthetic"},
            "source":{"id":"","plugin_id":"synthetic","name":"Synthetic source","enabled":false}});
        crate::test_support::persistence_failure(&root, true).unwrap();
        assert!(dispatch(&app, &request).is_err());
        assert!(app.plugin_settings.lock().unwrap().contains_key(id));
        assert!(app.state.lock().unwrap().chat_sources.is_empty());
        crate::test_support::persistence_failure(&root, false).unwrap();
        assert_eq!(dispatch(&app, &request).unwrap(), request["config"]);
        assert!(dispatch(&app, &request).is_err());
        let saved = crate::state_store::load(&crate::storage::data_dir(&root)).unwrap();
        assert_eq!(saved.chat_sources.len(), 1);
        assert_eq!(saved.chat_sources[0].config, request["config"]);
        let opened = app
            .source_discover(&json!({"plugin_id":"synthetic","config":{}}))
            .unwrap();
        let id = opened["settings"]["session"].as_str().unwrap();
        let session = app.plugin_settings.lock().unwrap()[id].clone();
        let request = json!({"session":id,"action":"complete","config":{"profile":"synthetic"},
            "source":saved.chat_sources[0]});
        let blocker = rusqlite::Connection::open(
            crate::storage::data_dir(&root).join(crate::state_store::FILE),
        )
        .unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let worker = app.clone();
        let save = std::thread::spawn(move || dispatch(&worker, &request));
        let deadline = Instant::now() + Duration::from_secs(2);
        while *session.phase.lock().unwrap() != Phase::Saving && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(dispatch(&app, &json!({"session":id,"action":"cancel"}))
            .unwrap_err()
            .contains("正在保存"));
        assert!(app.plugin_settings.lock().unwrap().contains_key(id));
        blocker.execute_batch("ROLLBACK").unwrap();
        save.join().unwrap().unwrap();
        drop(blocker);
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
}
