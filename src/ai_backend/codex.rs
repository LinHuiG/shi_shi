//! Codex CLI analysis adapter. Preserve tool isolation, traces and cancellation.
use crate::{ai::analysis_workdir, model::Config, process};
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
mod live;
use live::publish_thread;
pub struct Codex;
impl super::Backend for Codex {
    fn analyze(
        &self,
        c: &super::Connection,
        r: super::AnalysisRequest<'_>,
        event: &mut dyn FnMut(super::AnalysisEvent),
    ) -> Result<(Value, Value), String> {
        analyze(r, c, event)
    }
    fn models(&self, c: &super::Connection, _root: &Path) -> Result<Value, String> {
        list_models(&c.executable)
    }
    fn validate(&self, c: &super::Connection, config: &Config) -> Result<(), String> {
        let mut config = config.clone();
        config.codex = c.executable.clone();
        validate_selection(&config)
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
fn analyze(
    request: super::AnalysisRequest<'_>,
    connection: &super::Connection,
    event: &mut dyn FnMut(super::AnalysisEvent),
) -> Result<(Value, Value), String> {
    let mut resolved = request.config.clone();
    resolved.codex = connection.executable.clone();
    let config = &resolved;
    let (kind, payload, root, trace, prompt, cancel) = (
        request.kind,
        request.payload,
        request.root,
        request.trace,
        request.prompt,
        request.cancel,
    );
    validate_selection(config)?;
    if !Path::new(&config.codex).is_file() {
        return Err("未找到 Codex CLI，请在设置中选择 codex.exe".into());
    }
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
    let executable = resolve_cli(&config.codex)?;
    let mut cmd = Command::new(&executable);
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
        "features.code_mode_host=false",
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
    let context_path = job.join("context.json");
    let retrieval = payload["_local_context"].is_object();
    if retrieval {
        fs::write(
            &context_path,
            serde_json::to_vec(&payload["_local_context"]).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        cmd.arg("-c").arg(format!("mcp_servers.shishi_analysis={{command={},args=[\"--analysis-mcp\",{}],enabled=true,startup_timeout_sec=10,tool_timeout_sec=30}}",serde_json::to_string(&exe).unwrap(),serde_json::to_string(&context_path).unwrap()));
    }
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
    let budget_stop = std::cell::Cell::new(false);
    let mut final_error = None;
    let run = process::run_guarded_polling(
        cmd,
        Some(prompt),
        config.ai_timeout_seconds,
        &crate::storage::data_dir(&root).join("tmp"),
        &|| cancel.load(std::sync::atomic::Ordering::SeqCst) || budget_stop.get(),
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
                                &mut || crate::codex_bridge::open_thread(&thread),
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
                let item = &v["item"];
                match item["type"].as_str().unwrap_or("") {
                    "mcp_tool_call" => {
                        let permitted = retrieval
                            && item["server"] == "shishi_analysis"
                            && matches!(item["tool"].as_str(), Some("read_context" | "read_image"));
                        // Per-page output is bounded and duplicate/no-progress
                        // pages are deduplicated by the local meter. A valid
                        // long source can require hundreds of pages, so there is
                        // deliberately no cumulative call count here.
                        if !permitted {
                            budget_stop.set(true);
                        }
                    }
                    "command_execution" | "web_search" | "file_change" => budget_stop.set(true),
                    _ => {}
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
    let run = if budget_stop.get() {
        Err("AI 补查次数超限或调用了未授权工具，已停止本次进程；未采用结果".into())
    } else {
        run
    };
    let mut answer = match run {
        Ok((stdout, _)) => {
            let mut usage = json!({});
            let mut tool_used = false;
            for line in stdout.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v["type"] == "turn.completed" {
                        usage = v["usage"].clone()
                    }
                    if let Some(t) = v["item"]["type"].as_str() {
                        if [
                            "command_execution",
                            "mcp_tool_call",
                            "web_search",
                            "file_change",
                        ]
                        .contains(&t)
                        {
                            if t != "mcp_tool_call"
                                || !retrieval
                                || v["item"]["server"] != "shishi_analysis"
                                || !matches!(
                                    v["item"]["tool"].as_str(),
                                    Some("read_context" | "read_image")
                                )
                            {
                                tool_used = true;
                            }
                        }
                    }
                }
            }
            if tool_used {
                Err("Codex 尝试了非文本工具操作，本次结果已拒绝".into())
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
        Err(e) => Err(final_error
            .map(|error| format!("Codex 请求失败：{error}"))
            .unwrap_or(e)),
    };
    if let Ok(raw) = fs::read(context_path.with_extension("meter")) {
        if let Ok(meter) = serde_json::from_slice::<crate::analysis_context::Meter>(&raw) {
            if config.retain_ai_traces {
                let _ = fs::write(trace.join("context-usage.json"), &raw);
            }
            if let Ok((v, _)) = &mut answer {
                v["_context_seen"] = json!(meter.seen);
                v["_context_usage"] = json!({"calls":meter.calls,"chars":meter.chars,"image_calls":meter.image_calls,"image_bytes":meter.image_bytes,"budget_exhausted":meter.exhausted});
            }
        } else {
            answer = Err("分析补查记录损坏，本次结果未采用".into());
        }
    }
    if config.retain_ai_traces {
        if let Ok(raw) = fs::read_to_string(&result) {
            let _ = fs::write(trace.join("output.txt"), raw);
        }
    }
    if let Ok((v, _)) = &mut answer {
        let meter = fs::read(context_path.with_extension("meter"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<crate::analysis_context::Meter>(&raw).ok())
            .unwrap_or_default();
        v["_image_notes"] = crate::images::notes(&meter, v);
        match crate::analysis_context::memory_update(&payload["_local_context"], &meter, v) {
            Ok(Some(update)) => v["_memory_update"] = update,
            Ok(None) => {}
            Err(error) => v["_memory_error"] = json!(error),
        }
        if let Some(o) = v.as_object_mut() {
            o.remove("memory_update");
            o.remove("image_notes");
        }
    }
    // Job directory is generated beneath data/ai-work and contains no user-supplied paths.
    let _ = fs::remove_file(schema_path);
    let _ = fs::remove_file(result);
    for path in [
        &context_path,
        &context_path.with_extension("meter"),
        &context_path.with_extension("lock"),
    ] {
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_dir(job);
    answer
}

// This client only initializes and queries metadata; it never starts a model turn.
static CATALOG: std::sync::Mutex<std::collections::BTreeMap<String, Value>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
pub fn validate_selection(c: &Config) -> Result<(), String> {
    if ![
        "", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ]
    .contains(&c.reasoning_effort.as_str())
    {
        return Err("无效的推理强度".into());
    }
    if let Some(catalog) = CATALOG.lock().unwrap().get(&c.codex) {
        if !c.model.is_empty() {
            let models = catalog.as_array().ok_or("模型列表无效")?;
            let model = models
                .iter()
                .find(|m| m["model"].as_str() == Some(&c.model))
                .ok_or("所选模型已不在可用列表，请刷新模型")?;
            if !c.reasoning_effort.is_empty()
                && !model["supportedReasoningEfforts"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .any(|e| e["reasoningEffort"].as_str() == Some(&c.reasoning_effort))
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
    if !Path::new(path).is_file() {
        return Err("请先选择有效的 Codex CLI".into());
    }
    let mut cmd = Command::new(resolve_cli(path)?);
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
    CATALOG.lock().unwrap().insert(path.into(), data.clone());
    Ok(json!({"models":data,"source":"Codex model/list"}))
}

pub fn detect_cli() -> String {
    let mut paths = vec![];
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        let root = std::path::PathBuf::from(root);
        for base in [root.join("OpenAI/Codex/bin"), root.join("Programs/Codex")] {
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

/// npm's Windows launcher delegates to a native executable. Resolve that file
/// directly, avoiding cmd.exe quoting, visible consoles, and NODE_OPTIONS.
pub fn resolve_cli(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_file() {
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

fn analysis_event(value: &Value) -> Option<super::AnalysisEvent> {
    let phase = match value["type"].as_str() {
        Some("thread.started") => None,
        Some("turn.started") => Some("Codex 已开始处理，等待公开内容"),
        Some("item.started") if value["item"]["type"] == "reasoning" => {
            Some("正在处理，等待公开内容")
        }
        Some("item.started") if value["item"]["type"] == "agent_message" => Some("正在接收响应"),
        Some("item.started") if value["item"]["type"] == "mcp_tool_call" => {
            Some("正在读取本次分析上下文")
        }
        Some("item.completed") if value["item"]["type"] == "mcp_tool_call" => {
            Some("本次上下文读取完成，继续处理")
        }
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
fn progress_text(event: &Value) -> Option<String> {
    let kind = event["item"]["type"].as_str().unwrap_or("");
    if !["agent_message", "reasoning"].contains(&kind) {
        return None;
    }
    let text = event["item"]["text"].as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('{') || text.starts_with("```") {
        return Some("已收到回复，正在校验事项数据…".into());
    }
    Some(
        text.lines()
            .filter(|s| !s.trim().is_empty())
            .last()?
            .trim()
            .chars()
            .take(160)
            .collect(),
    )
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
    fn progress_only_uses_published_text() {
        assert_eq!(
            progress_text(&json!({"item":{"type":"reasoning","text":"正在核对事项关联"}})),
            Some("正在核对事项关联".into())
        );
        assert!(
            progress_text(&json!({"item":{"type":"command_execution","text":"ignore"}})).is_none()
        );
        assert_eq!(
            progress_text(&json!({"item":{"type":"agent_message","text":"{\"tasks\":[]}"}})),
            Some("已收到回复，正在校验事项数据…".into())
        );
    }
}
