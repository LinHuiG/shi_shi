//! Codex CLI analysis adapter. Preserve tool isolation, traces and cancellation.
use crate::{ai::analysis_workdir, process};
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
mod live;
use live::publish_thread;
pub struct Codex;
impl super::Backend for Codex {
    fn models(&self, c: &super::Connection, _root: &Path) -> Result<Value, String> {
        list_models(&c.executable)
    }
    fn validate(&self, c: &super::Connection, model: &str, effort: &str) -> Result<(), String> {
        validate_selection(&c.executable, model, effort)
    }
    fn status(&self, c: &super::Connection, root: &Path) -> Result<Value, String> {
        let mut cmd = Command::new(resolve_cli(&c.executable)?);
        cmd.args(["login", "status"]);
        let (o, e) = process::run(cmd, None, 15, &crate::storage::data_dir(&root).join("tmp"))?;
        Ok(json!({"status":format!("{o}{e}")}))
    }
    fn agent(
        &self,
        c: &super::Connection,
    ) -> Result<std::sync::Arc<dyn super::AgentSession>, String> {
        let _ = c;
        Err("Codex 处置使用桌面消息桥接，不提供独立执行连接".into())
    }
    fn open_session(&self, thread: &str) -> Result<(), String> {
        crate::codex_bridge::open_thread(thread)
    }
}
pub(super) fn analyze_round(
    request: super::AnalysisRequest<'_>,
    connection: &super::Connection,
    event: &mut dyn FnMut(super::AnalysisEvent),
) -> Result<(Value, Value), String> {
    let executable = resolve_cli(&connection.executable)?;
    let config = request.config;
    let (kind, payload, root, trace, prompt, cancel) = (
        request.kind,
        request.payload,
        request.root,
        request.trace,
        request.prompt,
        request.cancel,
    );
    validate_selection(&executable, &config.model, &config.reasoning_effort)?;
    let job = crate::storage::data_dir(&root)
        .join("ai-work")
        .join(crate::model::id());
    fs::create_dir_all(&job).map_err(|e| e.to_string())?;
    let workdir = analysis_workdir(root, config)?;
    fs::create_dir_all(&workdir).map_err(|e| format!("无法创建分析工作目录：{e}"))?;
    let schema_path = job.join("schema.json");
    let result = job.join("result.json");
    fs::write(&schema_path, serde_json::to_vec(request.schema).unwrap())
        .map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&executable);
    // A desktop-agent shell can lend its own tool pipe to children. This
    // analysis owns its MCP server and must not borrow the launching chat.
    cmd.env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_APP_TOOLS_PIPE_PATH");
    let help = exec_help(&executable, root, cancel)?;
    cmd.arg("exec");
    for option in ["--ignore-user-config", "--ignore-rules"] {
        if !help.contains(option) {
            return Err(format!(
                "此 Codex CLI 缺少 {option}，无法隔离全局工具和规则；请更新 CLI 后重试"
            ));
        }
        cmd.arg(option);
    }
    // Persist a fresh conversation per invocation: visible history is never reused as input.
    let visible = if request.show_session {
        match crate::codex_bridge::Client::connect_guarded(&executable, Some(cancel)).and_then(
            |desktop| {
                let project = desktop.analysis_project(&workdir, cancel)?;
                Ok((desktop, project))
            },
        ) {
            Ok(value) => Some(value),
            Err(error) => {
                event(super::AnalysisEvent::VisibilityIssue {
                    thread_id: String::new(),
                    error,
                });
                None
            }
        }
    } else {
        None
    };
    cmd.args([
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--json",
        "--color",
        "never",
        "-c",
        "features.shell_tool=false",
        "-c",
        "features.unified_exec=false",
        "-c",
        "features.apps=false",
        "-c",
        "features.browser_use=false",
        "-c",
        "features.computer_use=false",
        "-c",
        "features.image_generation=false",
        "-c",
        "features.hooks=false",
        "-c",
        "features.code_mode=false",
        "-c",
        "features.plugins=false",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "mcp_servers={}",
        "-c",
        "approval_policy=\"never\"",
    ])
    .arg("--output-schema")
    .arg(&schema_path)
    .arg("--output-last-message")
    .arg(&result)
    .arg("-C")
    .arg(&workdir);
    if !config.model.trim().is_empty() {
        cmd.arg("--model").arg(&config.model);
    }
    if !config.reasoning_effort.is_empty() {
        cmd.arg("-c").arg(format!(
            "model_reasoning_effort={}",
            serde_json::to_string(&config.reasoning_effort).unwrap()
        ));
    }
    cmd.arg("-");
    let model_images = crate::images::model_files(&job, &payload["_model_images"])?;
    for image in &model_images.0 {
        cmd.arg("--image").arg(image);
    }
    if config.retain_ai_traces {
        let diagnostic = crate::cache_diagnostics::prepare(&request, connection, &cmd);
        let _ = fs::write(trace.join("cache-diagnostics.json"), diagnostic.to_string());
        event(super::AnalysisEvent::CacheDiagnostics(diagnostic));
    }
    let visibility_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct VisibilityGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for VisibilityGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _visibility_guard = VisibilityGuard(visibility_stop.clone());
    let (visibility_tx, visibility_rx) = std::sync::mpsc::sync_channel(64);
    let isolation_violation = std::cell::Cell::new(false);
    let mut final_error = None;
    let run = process::run_guarded_polling(
        cmd,
        Some(prompt),
        config.ai_timeout_seconds,
        &crate::storage::data_dir(&root).join("tmp"),
        &|| cancel.load(std::sync::atomic::Ordering::SeqCst) || isolation_violation.get(),
        &mut |line| {
            use std::io::Write;
            if config.retain_ai_traces && !line.is_empty() {
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(trace.join("events.jsonl"))
                {
                    if let Ok(mut record) = serde_json::from_str::<Value>(line) {
                        crate::images::redact(&mut record);
                        let _ = writeln!(f, "{record}");
                    } else {
                        let _ = writeln!(f, "{}", line);
                    }
                }
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v["type"] == "turn.failed" {
                    final_error = v["error"]["message"]
                        .as_str()
                        .map(|s| crate::context_budget::clip(s, 1500));
                }
                if v["type"] == "thread.started" {
                    if let Some(thread) = v["thread_id"].as_str().filter(|_| request.show_session) {
                        let title = format!(
                            "拾事 · {} · {}",
                            kind,
                            chrono::Local::now().format("%m-%d %H:%M")
                        );
                        if config.retain_ai_traces {
                            let _ = fs::write(
                                trace.join("codex-thread.json"),
                                json!({"thread_id":thread,"project_id":visible.as_ref().map(|(_,p)|p),"title":title})
                                    .to_string(),
                            );
                        }
                        // A thread id precedes its durable rollout. Do metadata
                        // work off the stdout reader, after persisted read succeeds.
                        let (desktop, project, thread, stop, tx) = (
                            visible.as_ref().map(|(d, _)| d.clone()),
                            visible.as_ref().map(|(_, p)| p.clone()).unwrap_or_default(),
                            thread.to_string(),
                            visibility_stop.clone(),
                            visibility_tx.clone(),
                        );
                        std::thread::spawn(move || {
                            let visible = publish_thread(
                                &thread,
                                &project,
                                &title,
                                &stop,
                                &mut |method, params| {
                                    desktop
                                        .as_ref()
                                        .ok_or_else(|| "Codex 前台关联服务暂不可用".to_string())?
                                        .call_guarded(method, params, Some(&stop))
                                },
                                std::time::Duration::from_secs(20),
                            );
                            match visible {
                                Ok(published) => {
                                    if let Some(error) = published.error {
                                        if !stop.load(std::sync::atomic::Ordering::SeqCst) {
                                            let _ = tx.try_send(
                                                super::AnalysisEvent::VisibilityIssue {
                                                    thread_id: thread.clone(),
                                                    error,
                                                },
                                            );
                                        }
                                    }
                                    live::watch_public_rollout(
                                        &published.stored,
                                        &thread,
                                        &stop,
                                        &tx,
                                    )
                                }
                                Err(error) if !stop.load(std::sync::atomic::Ordering::SeqCst) => {
                                    let _ = tx.try_send(super::AnalysisEvent::VisibilityIssue {
                                        thread_id: thread,
                                        error,
                                    });
                                }
                                Err(_) => {}
                            }
                        });
                    }
                }
                if is_native_tool_event(&v) {
                    isolation_violation.set(true);
                }
                if let Some(update) = analysis_event(&v) {
                    event(update);
                }
            }
            for update in visibility_rx.try_iter() {
                event(update);
            }
        },
    );
    for update in visibility_rx.try_iter() {
        event(update);
    }
    let run = if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        Err(crate::ai_control::CANCELLED.into())
    } else if isolation_violation.get() {
        Err(NATIVE_TOOL_VIOLATION.into())
    } else {
        run
    };
    let answer = match run {
        Ok((stdout, _)) => {
            let mut usage = json!({});
            let mut tool_used = false;
            for line in stdout.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v["type"] == "turn.completed" {
                        usage = v["usage"].clone()
                    }
                    tool_used |= is_native_tool_event(&v);
                }
            }
            if tool_used {
                Err(NATIVE_TOOL_VIOLATION.into())
            } else {
                fs::read_to_string(&result)
                    .map_err(|e| format!("Codex 未生成结果：{e}"))
                    .and_then(|s| {
                        serde_json::from_str::<Value>(&s)
                            .map(|v| (v, usage))
                            .map_err(|e| format!("Codex JSON 无效：{e}"))
                    })
            }
        }
        Err(e) if isolation_violation.get() || cancel.load(std::sync::atomic::Ordering::SeqCst) => {
            Err(e)
        }
        Err(e) => Err(final_error
            .map(|error| format!("Codex 请求失败：{error}"))
            .unwrap_or(e)),
    };
    if config.retain_ai_traces {
        if let Ok(raw) = fs::read_to_string(&result) {
            if let Err(error) = fs::write(trace.join("output.txt"), raw) {
                event(super::AnalysisEvent::Stream {
                    kind: "diagnostic".into(),
                    text: format!("分析结果跟踪保存失败：{error}"),
                    append: false,
                });
            }
        }
    }
    // Job directory is generated beneath data/ai-work and contains no user-supplied paths.
    let _ = fs::remove_file(schema_path);
    let _ = fs::remove_file(result);
    drop(model_images);
    let _ = fs::remove_dir(job);
    answer
}

// This client only initializes and queries metadata; it never starts a model turn.
static CATALOG: std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, Value>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
pub fn validate_selection(launcher: &str, model_id: &str, effort: &str) -> Result<(), String> {
    if ![
        "", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ]
    .contains(&effort)
    {
        return Err("无效的推理强度".into());
    }
    // An unavailable saved launcher must not prevent editing its configuration.
    let executable = resolve_cli(launcher)
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    let catalogs = CATALOG.lock().unwrap();
    if let Some(catalog) = executable.as_ref().and_then(|path| catalogs.get(path)) {
        if !model_id.is_empty() {
            let models = catalog.as_array().ok_or("模型列表无效")?;
            let model = models
                .iter()
                .find(|m| m["model"].as_str() == Some(model_id))
                .ok_or("所选模型已不在可用列表，请刷新模型")?;
            if !effort.is_empty()
                && !model["supportedReasoningEfforts"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .any(|e| e["reasoningEffort"].as_str() == Some(effort))
                    })
                    .unwrap_or(false)
            {
                return Err("该模型不支持所选推理强度，请重新选择".into());
            }
        }
    }
    Ok(())
}
static MODEL_QUERY: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub fn list_models(path: &str) -> Result<Value, String> {
    let _query = MODEL_QUERY.lock().unwrap();
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let executable = resolve_cli(path)?;
    let catalog_path = fs::canonicalize(&executable)
        .map_err(|error| format!("无法读取 Codex CLI 路径：{error}"))?;
    let mut cmd = Command::new(&executable);
    cmd.args(["app-server"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut owned = process::spawn_owned(&mut cmd)?;
    let diagnostic = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(stderr) = owned.child.stderr.take() {
        let sink = diagnostic.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                *sink.lock().unwrap() = line.chars().take(800).collect();
            }
        });
    }
    let mut input = owned.child.stdin.take().ok_or("无法打开 Codex 输入")?;
    let output = owned.child.stdout.take().ok_or("无法读取 Codex 输出")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(25);
    let receive = |id: u64| -> Result<Value, String> {
        loop {
            process::check_running()?;
            if Instant::now() >= deadline {
                return Err("读取模型列表超时，请检查 Codex 登录和网络后刷新".into());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(line)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if v["id"].as_u64() == Some(id) {
                            if !v["error"].is_null() {
                                return Err(format!("Codex：{}", v["error"]["message"]));
                            }
                            return Ok(v["result"].clone());
                        }
                    }
                }
                Ok(Err(e)) => return Err(e.to_string()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    return Err(format!(
                        "Codex 模型服务已退出：{}",
                        diagnostic.lock().unwrap()
                    ))
                }
            }
        }
    };
    writeln!(input,"{}",json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"shishi","version":"0.1.3"},"capabilities":{"experimentalApi":true}}})).map_err(|e|e.to_string())?;
    receive(1)?;
    writeln!(input, "{}", json!({"method":"initialized"})).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    let mut cursor = Value::Null;
    let mut id = 2;
    loop {
        writeln!(input,"{}",json!({"id":id,"method":"model/list","params":{"limit":100,"includeHidden":false,"cursor":cursor}})).map_err(|e|e.to_string())?;
        let page = receive(id)?;
        data.extend(
            page["data"]
                .as_array()
                .ok_or("Codex 返回的模型列表格式不兼容")?
                .iter()
                .filter(|m| m["hidden"] != true)
                .cloned(),
        );
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        id += 1;
        if id > 30 {
            return Err("模型分页过多，请更新 Codex 后重试".into());
        }
    }
    if data.is_empty() {
        return Err("Codex 未返回可用模型，请检查登录状态".into());
    }
    let data = json!(data);
    CATALOG.lock().unwrap().insert(catalog_path, data.clone());
    Ok(json!({"models":data,"source":"Codex model/list"}))
}

pub fn detect_cli() -> String {
    let mut paths = vec![];
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        let root = std::path::PathBuf::from(root);
        let bundled = root.join("OpenAI/Codex/bin");
        if let Some(path) = latest_bundled_cli(&bundled) {
            paths.push(path);
        }
        paths.push(bundled.join("codex.exe"));
        let base = root.join("Programs/Codex");
        if let Ok(entries) = fs::read_dir(&base) {
            let mut children = entries
                .flatten()
                .map(|e| e.path().join("codex.exe"))
                .collect::<Vec<_>>();
            children.sort();
            children.reverse();
            paths.extend(children);
        }
        paths.push(base.join("codex.exe"));
    }
    if let Some(root) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&root) {
            paths.push(dir.join("codex.exe"));
            paths.push(dir.join("codex.cmd"));
        }
    }
    if let Some(root) = std::env::var_os("USERPROFILE") {
        paths.push(std::path::PathBuf::from(root).join(".codex/bin/codex.exe"));
    }
    if let Some(root) = std::env::var_os("APPDATA") {
        paths.push(std::path::PathBuf::from(root).join("npm/codex.cmd"));
    }
    paths
        .into_iter()
        .filter(|p| p.is_file())
        .find_map(|p| resolve_cli(&p.to_string_lossy()).ok())
        .unwrap_or_default()
}

fn bundled_version(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        (8..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

fn bundled_cli_root(path: &Path) -> Option<&Path> {
    if !path.is_absolute()
        || !path.file_name()?.eq_ignore_ascii_case("codex.exe")
        || !bundled_version(path.parent()?.file_name()?)
    {
        return None;
    }
    let bin = path.parent()?.parent()?;
    (bin.file_name()?.eq_ignore_ascii_case("bin")
        && bin.parent()?.file_name()?.eq_ignore_ascii_case("Codex")
        && bin
            .parent()?
            .parent()?
            .file_name()?
            .eq_ignore_ascii_case("OpenAI"))
    .then_some(bin)
}

fn latest_bundled_cli(bin: &Path) -> Option<std::path::PathBuf> {
    use std::os::windows::fs::MetadataExt;
    let root = fs::symlink_metadata(bin).ok()?;
    if !root.is_dir() || root.file_attributes() & 0x400 != 0 {
        return None;
    }
    fs::read_dir(bin)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !bundled_version(&entry.file_name()) {
                return None;
            }
            let dir = fs::symlink_metadata(entry.path()).ok()?;
            if !dir.is_dir() || dir.file_attributes() & 0x400 != 0 {
                return None;
            }
            let path = entry.path().join("codex.exe");
            let file = fs::symlink_metadata(&path).ok()?;
            (file.is_file() && file.file_attributes() & 0x400 == 0)
                .then(|| (file.modified().ok(), file.created().ok(), path))
        })
        .max()
        .map(|(_, _, path)| path)
}

/// Recover removed desktop versions within the saved installation only. npm
/// launchers resolve directly to their native binary without invoking a shell.
pub fn resolve_cli(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_file() {
        if fs::metadata(p).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
            if let Some(replacement) = bundled_cli_root(p).and_then(latest_bundled_cli) {
                return Ok(replacement.to_string_lossy().into());
            }
        }
        return Err("未找到 Codex CLI，请选择 codex.exe 或 npm 安装的 codex.cmd".into());
    }
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return Ok(path.into());
    }
    if !p
        .file_name()
        .is_some_and(|s| s.eq_ignore_ascii_case("codex.cmd") || s.eq_ignore_ascii_case("codex.ps1"))
    {
        return Err("请选择 Codex 原生程序或 npm 的 Codex 启动文件".into());
    }
    let mut pending = vec![(p.parent().unwrap().join("node_modules/@openai"), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = pending.pop() {
        if depth > 9 || seen > 4000 {
            continue;
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                seen += 1;
                let path = e.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    continue;
                }
                if meta.is_file() && e.file_name().eq_ignore_ascii_case("codex.exe") {
                    let arch = path.to_string_lossy().to_ascii_lowercase();
                    if arch.contains("x86_64") || arch.contains("win32-x64") {
                        return Ok(path.to_string_lossy().into());
                    }
                } else if meta.is_dir() {
                    pending.push((path, depth + 1));
                }
            }
        }
    }
    Err(
        "未找到 npm Codex 的 Windows x64 原生程序。请完整安装 Codex CLI，或直接选择其 codex.exe"
            .into(),
    )
}

#[cfg(test)]
#[path = "codex/launcher_tests.rs"]
mod launcher_tests;

fn exec_help(
    executable: &str,
    root: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    static HELP: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);
    let mut cache = HELP.lock().unwrap();
    if let Some((path, text)) = cache.as_ref() {
        if path == executable {
            return Ok(text.clone());
        }
    }
    let mut cmd = Command::new(executable);
    cmd.args(["exec", "--help"]);
    let (out, err) = process::run_cancellable(
        cmd,
        None,
        15,
        &crate::storage::data_dir(&root).join("tmp"),
        Some(cancel),
        &mut |_| {},
    )?;
    let help = format!("{out}\n{err}");
    *cache = Some((executable.into(), help.clone()));
    Ok(help)
}

const NATIVE_TOOL_VIOLATION: &str =
    "Codex 纯模型进程尝试调用未授权的原生工具，已停止本次进程；未采用结果";

fn is_native_tool_event(value: &Value) -> bool {
    matches!(
        value["item"]["type"].as_str(),
        Some("mcp_tool_call" | "command_execution" | "web_search" | "file_change")
    )
}

fn analysis_event(value: &Value) -> Option<super::AnalysisEvent> {
    if is_native_tool_event(value) {
        return Some(super::AnalysisEvent::Stream {
            kind: "phase".into(),
            text: "检测到未授权的原生工具操作，正在停止".into(),
            append: false,
        });
    }
    let phase = match value["type"].as_str() {
        Some("thread.started") => None,
        Some("turn.started") => Some("Codex 已开始处理，等待公开内容"),
        Some("item.started") if value["item"]["type"] == "reasoning" => {
            Some("正在处理，等待公开内容")
        }
        Some("item.started") if value["item"]["type"] == "agent_message" => Some("正在接收响应"),
        _ => None,
    };
    if let Some(text) = phase {
        return Some(super::AnalysisEvent::Stream {
            kind: "phase".into(),
            text: text.into(),
            append: false,
        });
    }
    match value["type"].as_str() {
        Some("thread.started") => Some(super::AnalysisEvent::SessionStarted(
            value["thread_id"].as_str().unwrap_or("").into(),
        )),
        Some("turn.completed") => Some(super::AnalysisEvent::Usage(value["usage"].clone())),
        _ => {
            let kind = value["item"]["type"].as_str().unwrap_or("");
            if matches!(kind, "reasoning" | "agent_message") {
                value["item"]["text"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|text| super::AnalysisEvent::Stream {
                        kind: if kind == "reasoning" {
                            "reasoning"
                        } else {
                            "content"
                        }
                        .into(),
                        text: text.into(),
                        append: false,
                    })
            } else {
                None
            }
        }
    }
}
#[cfg(test)]
mod progress_output_tests {
    use super::*;
    #[test]
    fn events_translate_to_application_types_without_inventing_usage() {
        use crate::ai_backend::AnalysisEvent;
        assert_eq!(
            analysis_event(&json!({"type":"thread.started","thread_id":"fixture"})),
            Some(AnalysisEvent::SessionStarted("fixture".into()))
        );
        assert_eq!(
            analysis_event(&json!({"type":"turn.completed"})),
            Some(AnalysisEvent::Usage(Value::Null))
        );
        assert_eq!(
            analysis_event(
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"done"}})
            ),
            Some(AnalysisEvent::Stream {
                kind: "content".into(),
                text: "done".into(),
                append: false
            })
        );
        assert_eq!(analysis_event(&json!({"type":"unknown"})), None);
    }
    #[test]
    fn native_tool_events_are_isolation_failures_but_text_tool_requests_are_not() {
        for kind in [
            "mcp_tool_call",
            "command_execution",
            "web_search",
            "file_change",
        ] {
            for phase in ["item.started", "item.completed"] {
                let event = json!({"type":phase,"item":{"type":kind}});
                assert!(is_native_tool_event(&event));
                assert!(matches!(analysis_event(&event),
                    Some(super::super::AnalysisEvent::Stream { kind, text, .. })
                    if kind == "phase" && text.contains("未授权的原生工具") && !text.contains("超限")));
            }
        }
        let text_request = json!({"type":"item.completed","item":{"type":"agent_message","text":"{\"tool_call\":{\"name\":\"read_context\",\"arguments\":\"{}\"},\"result\":null}"}});
        assert!(!is_native_tool_event(&text_request));
        assert!(matches!(analysis_event(&text_request),
            Some(super::super::AnalysisEvent::Stream { kind, .. }) if kind == "content"));
    }
}
