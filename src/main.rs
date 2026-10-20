#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod activity;
mod ai;
mod ai_backend;
mod ai_connections;
mod ai_control;
mod ai_jobs;
mod ai_progress;
mod analysis_context;
mod archive;
mod archive_storage;
mod backdrop;
mod backlog;
mod cache_diagnostics;
mod calendar_service;
mod codex_bridge;
mod context_budget;
mod context_mcp;
mod engine;
mod evolution;
mod handling;
mod holiday;
mod images;
#[cfg(test)]
mod improvement_tests;
mod managed_data;
mod message_progress;
mod messages;
mod model;
mod plugins;
mod process;
mod prompt_config;
mod prompts;
mod retention;
mod schedule;
mod snapshots;
mod sources;
mod tags;
mod tray;
mod ui_bundle;
mod wire_context;
mod work_hours;
mod work_scheduler;
mod work_state;
use shishi_api::http_client;
mod credentials;
mod desktop_watch;
mod live_ai_regression;
mod memory_editor;
mod message_filter;
#[cfg(test)]
mod test_support;
use engine::App;
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Theme, WindowBuilder},
};
use work_hours::AiMode;
use wry::{WebContext, WebViewBuilder, WebViewBuilderExtWindows};
#[derive(Debug)]
enum UiEvent {
    Reply(Value),
    Refresh,
    Ready(Value),
    Tray(tray::Action),
}
fn fatal(s: &str) {
    rfd::MessageDialog::new()
        .set_title("拾事")
        .set_description(s)
        .set_level(rfd::MessageLevel::Error)
        .show();
}
fn dispatch(app: &Arc<App>, v: &Value) -> Result<Value, String> {
    process::check_running()?;
    let p = &v["params"];
    match v["method"].as_str().unwrap_or("") {
        "state" => Ok(if p.get("sections").is_some() {
            app.snapshot_sections(p.get("sections"))
        } else {
            app.snapshot_page(p["page"].as_str())
        }),
        "state_version" => Ok(app.state_version()),
        "work_state" => app.set_work_state(p),
        "tag_filters" => app.tag_filters(p),
        "tag_decide" => app.tag_decide(p),
        "advice_decide" => app.advice_decide(p),
        "advice_feedback" => app.advice_feedback(p),
        "messages_delete" => app.delete_messages(p),
        "messages_query" => app.query_messages(p),
        "plugin_toggle" => app.plugin_toggle(p),
        "plugin_uninstall" => app.plugin_uninstall(p),
        "handling_prepare" => app.handling_prepare(p["task_id"].as_str().unwrap_or("")),
        "handling_projects" => app.handling_projects(),
        "handling_add_project" => app.handling_add_project(p),
        "handling_start" => app.handling_start(p),
        "handling_handoff" => app.handling_handoff(p["task_id"].as_str().unwrap_or("")),
        "handling_context_note" => app.handling_context_note(p["task_id"].as_str().unwrap_or("")),
        "handling_followup" => app.handling_followup(p),
        "handling_sync" => app.handling_sync(p["task_id"].as_str()),
        "handling_request" => app.handling_request(p),
        "handling_reconnect" => app.handling_reconnect(p),
        "handling_reset_setup" => app.handling_reset_setup(p),
        "handling_interrupt" => app.handling_interrupt(p["task_id"].as_str().unwrap_or("")),
        "handling_history" => Ok(app.handling_history()),
        "handling_policy_save" => app.handling_policy_save(p),
        "handling_learn" => app.handling_learn(),
        "handling_audit" => app.handling_audit(p["task_id"].as_str().unwrap_or("")),
        "handling_open" => {
            let thread = p["thread_id"].as_str().ok_or("缺少会话编号")?;
            app.handling_open(thread)?;
            Ok(json!(true))
        }
        "detect" => {
            if p["driver"] == "dsh" {
                Ok(ai_connections::detect_dsh(
                    p["saved"].as_str().unwrap_or(""),
                ))
            } else {
                Ok(json!({"codex":ai::detect_cli()}))
            }
        }
        "connection_save" => app.save_connection(p),
        "connection_delete" => app.delete_connection(p),
        "activity_query" => app.activity_query(p),
        "dsh_initialize_acp" => {
            let connection =
                serde_json::from_value(p["connection"].clone()).map_err(|e| e.to_string())?;
            ai_backend::dsh::initialize_acp(&connection, &app.root, p["confirmed"] == true)
        }
        "dsh_workspace_plugin" => {
            let connection =
                serde_json::from_value(p["connection"].clone()).map_err(|e| e.to_string())?;
            ai_backend::dsh::workspace_plugin(
                &connection,
                p["install"] == true,
                p["confirmed"] == true,
            )
        }
        "prompt_save" => app.save_prompt(p),
        "plugins" => app.plugin_catalog(),
        "plugin_upload" => app.install_plugin_upload(p),
        "plugin_install" => app.install_plugin(
            p["path"]
                .as_str()
                .ok_or("请选择插件 ZIP 包或 plugin.json")?,
        ),
        "source_discover" => app.source_discover(p),
        "source_save" => app.source_save(p),
        "source_remove" => app.source_remove(p),
        "source_range" => app.source_range(p),
        "prompt_history" => Ok(app.prompt_history()),
        "memory_read" => Ok(app.memory_read()),
        "memory_save" => app.memory_save(p),
        "plugin_development_guide" => Ok(
            json!({"text":format!("{}\n\n{}\n\n{}\n\n{}",include_str!("../docs/PLUGIN_ADAPTATION.md"),include_str!("../PLUGIN_API.txt"),include_str!("../examples/json-source/plugin.json"),include_str!("../examples/json-source/src/lib.rs"))}),
        ),
        "prompt_adopt" => app.adopt_prompt(p["id"].as_str().unwrap_or("")),
        "prompt_rollback" => app.rollback_prompt(
            p["id"].as_str().unwrap_or(""),
            p["expected"].as_str().unwrap_or(""),
        ),
        "prompt_retry" => app.retry_prompt_review(p["id"].as_str().unwrap_or("")),
        "prompts" => Ok(
            json!({"classify":ai::default_policy("classify"),"format":ai::default_policy("format"),"learning":ai::default_policy("learning"),"report":ai::default_policy("report"),"handling":ai::default_policy("handling"),"prompt_review":ai::default_policy("prompt_review"),"archive":ai::default_policy("archive")}),
        ),
        "ai_trace" => {
            let id = p["id"].as_str().ok_or("缺少调用编号")?;
            uuid::Uuid::parse_str(id).map_err(|_| "无效的调用编号")?;
            let dir = app.root.join("data/ai-traces").join(id);
            if !dir.is_dir() {
                let retain = app.state.lock().unwrap().config.retain_ai_traces;
                return Err(if retain {
                    "这次调用没有保存详细输入输出（旧版记录）".into()
                } else {
                    "完整跟踪已关闭，本次未保存输入输出".into()
                });
            }
            let read = |name| fs::read_to_string(dir.join(name)).unwrap_or_default();
            if p["previous"] == true {
                let identity: Value = serde_json::from_str(&read("request.json"))
                    .map_err(|_| "当前记录缺少同类请求标识，请使用更新后的调用记录比较")?;
                let current_time = fs::metadata(dir.join("input.txt"))
                    .and_then(|m| m.modified())
                    .map_err(|_| "当前请求输入不可读")?;
                let mut candidates = fs::read_dir(app.root.join("data/ai-traces"))
                    .map_err(|_| "跟踪目录不可读")?
                    .filter_map(Result::ok)
                    .take(10000)
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name == id || uuid::Uuid::parse_str(&name).is_err() {
                            return None;
                        }
                        let path = entry.path().join("input.txt");
                        let other: Value = serde_json::from_slice(
                            &fs::read(entry.path().join("request.json")).ok()?,
                        )
                        .ok()?;
                        if other != identity {
                            return None;
                        }
                        let time = fs::metadata(&path).ok()?.modified().ok()?;
                        (time <= current_time).then_some((time, name, path))
                    })
                    .collect::<Vec<_>>();
                candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
                let (_, previous_id, path) =
                    candidates.first().ok_or("没有更早的已保留请求可供比较")?;
                let previous = fs::read_to_string(path).map_err(|_| "上一份请求输入不可读")?;
                return Ok(
                    json!({"input":read("input.txt"),"previous_input":previous,"previous_request_id":previous_id}),
                );
            }
            Ok(
                json!({"input":read("input.txt"),"output":read("output.txt"),"events":format!("{}\n{}",read("tool-failures.jsonl"),read("events.jsonl")),"schema":read("schema.json"),"error":read("error.txt")}),
            )
        }
        "analysis_session_open" => {
            let driver = p["driver"].as_str().unwrap_or("");
            let thread = p["thread_id"].as_str().ok_or("缺少会话编号")?;
            uuid::Uuid::parse_str(thread).map_err(|_| "无效会话编号")?;
            if driver != "codex" {
                return Err("此接入不支持自动打开会话，请在对应客户端按会话编号查看".into());
            }
            crate::ai_backend::backend(&crate::ai_backend::Connection::default())?
                .open_session(thread)?;
            Ok(json!(true))
        }
        "models" => {
            if p.get("connection").is_some() {
                app.connection_models(p)
            } else {
                ai::list_models(p["codex"].as_str().unwrap_or(""))
            }
        }
        "config" => app.configure(p.clone()),
        "settings_patch" => app.configure_patch(p),
        "scan" => app.scan(AiMode::Manual),
        "scan_only" => app.refresh_messages(),
        "scan_control" => app.scan_control(p),
        "ai_job_start" => ai_jobs::start(app, p),
        "ai_job_result" => ai_jobs::result(p),
        "analyze" => app.analyze(AiMode::Manual),
        "analysis_backlog" => app.analysis_backlog(p),
        "image_preview" => app.image_preview(p),
        "ai_interrupt" => app.interrupt_ai(p["request_id"].as_str().unwrap_or("")),
        "generate" => {
            let draft = app.create_generation(p)?;
            let reply = json!(draft);
            let worker = app.clone();
            std::thread::spawn(move || worker.generate_background(draft, AiMode::Manual));
            Ok(reply)
        }
        "format" => app.format(p["text"].as_str().unwrap_or("")),
        "schedule_edit" => app.schedule_edit(p),
        "schedule_delete" => app.schedule_delete(p),
        "schedule_format" => app.schedule_format(p["text"].as_str().unwrap_or("")),
        "edit" => app.task_edit(p),
        "edit_ai" => app.task_ai_edit(p),
        "action" => app.task_action(p),
        "merge" => app.merge(p),
        "undo" => app.undo(p["index"].as_u64().ok_or("缺少记录索引")? as usize),
        "evidence" => Ok(app.evidence(p["id"].as_str().unwrap_or(""))),
        "report" => app.report("手动报表", AiMode::Manual),
        "archive" => app.archive(AiMode::Manual),
        "archive_history" => Ok(app.archive_history()),
        "archive_version" => app.archive_version(p["id"].as_str().unwrap_or("")),
        "archive_rollback" => app.archive_rollback(
            p["id"].as_str().unwrap_or(""),
            p["expected"].as_str().unwrap_or(""),
        ),
        "archive_task" => app.archive_task(p),
        "learn" => app.learn(AiMode::Manual),
        "rule" => app.rule_toggle(p["id"].as_str().unwrap_or("")),
        "pick" => {
            let title = p["kind"].as_str().unwrap_or("程序");
            let dialog = rfd::FileDialog::new().set_title(format!("选择{title}"));
            let selected = match p["type"].as_str().unwrap_or("file") {
                "directory" => dialog.pick_folder(),
                "plugin" => dialog
                    .add_filter("插件包或描述", &["zip", "json"])
                    .pick_file(),
                "manifest" => dialog.add_filter("插件描述", &["json"]).pick_file(),
                _ => dialog.pick_file(),
            };
            Ok(json!(selected.map(|p| p.to_string_lossy().to_string())))
        }
        "open_data" => {
            std::process::Command::new("explorer.exe")
                .arg(app.root.join("data"))
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "codex_status" | "ai_connection_status" => {
            let c = app.state.lock().unwrap().config.clone();
            let connection = match p.get("connection") {
                Some(value) => serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                None => ai_backend::connection(&c)?,
            };
            ai_backend::backend(&connection)?.status(&connection, &app.root)
        }
        _ => Err("未知请求".into()),
    }
}
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.iter().any(|a| a == "--live-ai-regression") {
        if let Err(error) = live_ai_regression::run(&args) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--analysis-submit-mcp") {
        if let Err(e) = args
            .get(i + 1)
            .ok_or("缺少提交凭据".to_string())
            .and_then(|p| {
                shishi_acp::submit::serve(std::path::Path::new(p)).map_err(|e| e.to_string())
            })
        {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--analysis-mcp") {
        let result = args
            .get(i + 1)
            .ok_or("缺少分析资料文件".to_string())
            .and_then(|p| analysis_context::serve(std::path::Path::new(p)));
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--context-mcp-root") {
        let result = args
            .get(i + 1)
            .ok_or("缺少上下文数据目录".to_string())
            .and_then(|p| context_mcp::serve_root(std::path::Path::new(p)));
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--context-mcp") {
        let result = args
            .get(i + 1)
            .ok_or("缺少上下文授权文件".to_string())
            .and_then(|p| context_mcp::serve(std::path::Path::new(p)));
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = start() {
        let args = std::env::args().collect::<Vec<_>>();
        if e.starts_with(managed_data::CANCELLED) {
            // The user already answered the single native dialog. Do not show a
            // second modal or pretend the app failed.
            if let Some(i) = args.iter().position(|a| a == "--root") {
                if let Some(path) = args.get(i + 1) {
                    let _ = fs::write(PathBuf::from(path).join("data/startup-cancelled.txt"), &e);
                }
            }
            return;
        }
        if args.iter().any(|a| {
            a == "--read-once"
                || matches!(
                    a.as_str(),
                    "--ui-smoke" | "--ui-smoke-mica" | "--ui-smoke-mica-fallback"
                )
        }) {
            if let Some(i) = args.iter().position(|a| a == "--root") {
                if let Some(path) = args.get(i + 1) {
                    let _ = fs::write(PathBuf::from(path).join("smoke-error.txt"), &e);
                }
            }
            std::process::exit(1)
        }
        let root = args
            .iter()
            .position(|a| a == "--root")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(PathBuf::from))
            });
        if let Some(root) = root {
            let _ = fs::write(root.join("data/startup-error.txt"), &e);
        }
        fatal(&e)
    }
}
/// The embedded UI bundle, or with `--ui-dir <path to assets/ui>` the same
/// bundle assembled from disk at startup, so UI changes need a restart only,
/// not a Rust rebuild.
fn load_ui(args: &[String]) -> Result<String, String> {
    let Some(i) = args.iter().position(|a| a == "--ui-dir") else {
        return Ok(include_str!(concat!(env!("OUT_DIR"), "/ui.html")).to_string());
    };
    let dir = PathBuf::from(args.get(i + 1).ok_or("--ui-dir 缺少路径")?);
    ui_bundle::assemble(&|path| fs::read_to_string(dir.join(path)))
        .map_err(|e| format!("界面目录无法组装（{}）：{e}", dir.display()))
}
fn start() -> Result<(), String> {
    let args = std::env::args().collect::<Vec<_>>();
    let mica_smoke = args
        .iter()
        .any(|a| matches!(a.as_str(), "--ui-smoke-mica" | "--ui-smoke-mica-fallback"));
    let mica_failure = args.iter().any(|a| a == "--ui-smoke-mica-fallback");
    let smoke = mica_smoke || args.iter().any(|a| a == "--ui-smoke");
    if smoke && !args.iter().any(|a| a == "--root") {
        return Err("启动自检必须通过 --root 指定隔离测试目录".into());
    }
    let root = if let Some(i) = args.iter().position(|a| a == "--root") {
        PathBuf::from(args.get(i + 1).ok_or("--root 缺少路径")?)
    } else {
        std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .unwrap()
            .to_path_buf()
    };
    fs::create_dir_all(root.join("data")).map_err(|e| e.to_string())?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(root.join("data/app.lock"))
        .map_err(|e| e.to_string())?;
    if lock.try_lock_exclusive().is_err() {
        if !smoke && tray::show_existing(&root) {
            return Ok(());
        }
        return Err("拾事已经在运行，请从托盘打开已有窗口。".into());
    }
    crate::prompt_config::initialize(&root)?;
    // Incompatible managed state must be surfaced before App initialization can
    // fail. The dialog lists exact files; cancel stops loading and keeps them.
    let headless = smoke || args.iter().any(|a| a == "--read-once");
    managed_data::startup_with(&root, headless, managed_data::native_choice)?;
    let app = Arc::new(App::load(root.clone())?);
    if args.iter().any(|a| a == "--read-once") {
        {
            let mut s = app.state.lock().unwrap();
            s.config.auto_ai = false;
        }
        let result = app.scan(AiMode::Automatic);
        fs::write(
            root.join("data/smoke-result.json"),
            json!({"result":result,"state":app.snapshot()}).to_string(),
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    // Use DWM's full-client frame, not Tao's legacy transparent-window blur.
    let backdrop = (!smoke || mica_smoke) && app.state.lock().unwrap().config.window_backdrop;
    let window = WindowBuilder::new()
        .with_title("拾事")
        .with_transparent(false)
        .with_background_color(if backdrop {
            (0, 0, 0, 0)
        } else {
            (32, 32, 32, 255)
        })
        // Reveal only after the page has rendered and completed an IPC round trip.
        .with_visible(false)
        .with_focused(false)
        .with_window_icon(
            tao::window::Icon::from_rgba(include_bytes!("../assets/shishi.rgba").to_vec(), 64, 64)
                .ok(),
        )
        .with_taskbar_icon(
            tao::window::Icon::from_rgba(include_bytes!("../assets/shishi.rgba").to_vec(), 64, 64)
                .ok(),
        )
        .with_theme(Some(Theme::Dark))
        .with_inner_size(LogicalSize::new(1240., 820.))
        .with_min_inner_size(LogicalSize::new(900., 620.))
        .build(&event_loop)
        .map_err(|e| e.to_string())?;
    app.log("native_window_created", json!({}));
    let mut mica = backdrop
        && !mica_failure
        && match backdrop::apply_mica(window.hwnd() as isize) {
            Ok(()) => true,
            Err(error) => {
                app.log("backdrop_unavailable", json!({"error":error}));
                false
            }
        };
    if !mica {
        window.set_background_color(Some((32, 32, 32, 255)));
    }
    let initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watch = initialized.clone();
    let watch_root = root.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        if !watch.load(Ordering::SeqCst) {
            let message="界面初始化超过60秒，页面未完成加载。请保留 data 中的 startup-error.txt 和日志，供排查启动阶段。";
            let _ = fs::write(watch_root.join("data/startup-error.txt"), message);
            if !smoke {
                fatal(message);
            }
            std::process::exit(1);
        }
    });
    let ui_html = load_ui(&args)?;
    let mut context = WebContext::new(Some(root.join("data/webview")));
    let ipc_app = app.clone();
    let ipc_proxy = proxy.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(ui_html)
        // A hidden/not-yet-active WebView cannot reliably accept MoveFocus at startup.
        .with_focused(false)
        .with_theme(wry::Theme::Dark)
        .with_transparent(mica)
        .with_background_color(if mica {
            (0, 0, 0, 0)
        } else {
            (32, 32, 32, 255)
        })
        .with_devtools(false)
        // Keep WebView2's native external-file support enabled. The page accepts
        // DOM drops and sends ZIP bytes; HWND replacement must not disable drops.
        .with_navigation_handler({
            let a = app.clone();
            let first = std::sync::atomic::AtomicBool::new(true);
            move |u| {
                let allow = u == "about:blank"
                    || (u.starts_with("data:text/html;") && first.swap(false, Ordering::SeqCst));
                a.log(
                    "navigation",
                    json!({"scheme":u.split(':').next(),"allowed":allow}),
                );
                allow
            }
        })
        .with_ipc_handler(move |request| {
            let value = match serde_json::from_str::<Value>(request.body()) {
                Ok(v) => v,
                Err(_) => return,
            };
            if value["method"] == "ui_ready" {
                let _ = ipc_proxy.send_event(UiEvent::Ready(value["params"].clone()));
                let _ = ipc_proxy.send_event(UiEvent::Reply(
                    json!({"id":value["id"],"ok":true,"data":true}),
                ));
                return;
            }
            let app = ipc_app.clone();
            let proxy = ipc_proxy.clone();
            std::thread::spawn(move || {
                let result = dispatch(&app, &value);
                if let Err(ref e) = result {
                    if e != ai_control::CANCELLED {
                        app.log("request_error", json!({"method":value["method"],"error":e}));
                    }
                }
                let reply = match result {
                    Ok(data) => json!({"id":value["id"],"ok":true,"data":data}),
                    Err(e) => json!({"id":value["id"],"ok":false,"error":e}),
                };
                let _ = proxy.send_event(UiEvent::Reply(reply));
            });
        })
        .build(&window)
        .map_err(|e| {
            initialized.store(true, Ordering::SeqCst);
            format!("界面启动失败：{e}\n详细错误已记录在 data/startup-error.txt。")
        })?;
    app.log("webview_created", json!({}));
    let tray_proxy = proxy.clone();
    let mut tray = Some(
        tray::Tray::new(&root, move |action| {
            let _ = tray_proxy.send_event(UiEvent::Tray(action));
        })
        .map_err(|e| {
            initialized.store(true, Ordering::SeqCst);
            e
        })?,
    );
    let smoke_proxy = proxy.clone();
    let mut smoke_report: Option<Value> = None;
    if !smoke {
        let bg = app.clone();
        let scheduler = app.clone();
        let handling = app.clone();
        let handling_proxy = proxy.clone();
        std::thread::spawn(move || {
            let mut seen = 0;
            let mut last_reconcile = std::time::Instant::now();
            while !process::stopping() {
                if codex_bridge::wait_for_activity(&mut seen) {
                    // Coalesce chunks before persistence and WebView refresh.
                    std::thread::sleep(Duration::from_millis(50));
                }
                if handling.handling_stream().unwrap_or(false) {
                    let _ = handling_proxy.send_event(UiEvent::Refresh);
                }
                if handling.handling_observe_desktop() {
                    let _ = handling_proxy.send_event(UiEvent::Refresh);
                }
                let desktop = handling
                    .state
                    .lock()
                    .unwrap()
                    .handling
                    .runs
                    .iter()
                    .any(|r| {
                        r.desktop_control
                            && ["inProgress", "syncPending"].contains(&r.desktop_status.as_str())
                    });
                if last_reconcile.elapsed() >= Duration::from_secs(if desktop { 5 } else { 30 }) {
                    let _ = handling.handling_sync(None);
                    last_reconcile = std::time::Instant::now();
                }
            }
        });
        std::thread::spawn(move || {
            while !process::stopping() {
                if let Err(e) = scheduler.generate_scheduled(model::now()) {
                    scheduler.error(&e);
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let boundary = app.clone();
        std::thread::spawn(move || {
            let mut previous = None;
            let mut failed_since: Option<i64> = None;
            while !process::stopping() {
                let at = model::now();
                if failed_since
                    .is_some_and(|since| at.saturating_sub(since) > work_scheduler::MAX_TICK_GAP_MS)
                {
                    boundary
                        .error("上下班边界保存失败超过120秒，已跳过本次边界，请手动生成早报或归档");
                    previous = Some(at);
                    failed_since = None;
                }
                // Advance only after a committed tick: a failed save keeps the
                // old observation so the crossed boundary is retried, bounded
                // by the scheduler's maximum tick gap.
                match boundary.work_boundary_tick(previous, at) {
                    Ok(()) => {
                        previous = Some(at);
                        failed_since = None;
                    }
                    Err(e) => {
                        failed_since.get_or_insert(previous.unwrap_or(at));
                        boundary.error(&e);
                        // Past the retry window the scheduler would treat the
                        // gap as missed anyway; say so and resume normal ticks
                        // instead of retrying an old boundary forever.
                        if previous.is_some_and(|p| at - p > work_scheduler::MAX_TICK_GAP_MS) {
                            boundary.error("上下班边界检查连续失败超过 120 秒，期间若跨越上班或下班时间，早报或下班归档可能未生成；请检查后手动生成早报或手动归档");
                            previous = Some(at);
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let calendar = app.clone();
        std::thread::spawn(move || {
            while !process::stopping() {
                let delay = match calendar.refresh_calendar() {
                    Ok(()) => 3600,
                    Err(e) => {
                        calendar.error(&e);
                        900
                    }
                };
                for _ in 0..delay / 5 {
                    if process::stopping() {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        });
        let scanner = app.clone();
        std::thread::spawn(move || {
            while !process::stopping() {
                scanner.scan_tick();
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        std::thread::spawn(move || {
            bg.resume_pending();
            loop {
                if process::stopping() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(15));
                if let Err(e) = bg.prune_messages() {
                    bg.error(&e);
                }
                bg.tick();
            }
        });
        // The only periodic UI refresh: push when the visible state changed.
        let notifier = app.clone();
        std::thread::spawn(move || {
            let mut last = None;
            while !process::stopping() {
                if let Some(pulse) = notifier.ui_pulse() {
                    if last != Some(pulse) {
                        last = Some(pulse);
                        let _ = proxy.send_event(UiEvent::Refresh);
                    }
                }
                std::thread::sleep(Duration::from_millis(
                    if notifier.ai_busy.load(Ordering::SeqCst) {
                        100
                    } else {
                        1000
                    },
                ));
            }
        });
    }
    let mut push_generation = 0u64;
    event_loop.run(move |event, _, control_flow| {
        // Own all native/UI resources for the entire event loop lifetime.
        let _ = (&lock, &window, &context);
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UiEvent::Tray(tray::Action::Show)) => {
                if smoke {
                    if let Some(report) = smoke_report.as_mut() {
                        report["tray_message"] = json!(true);
                        let _ = smoke_proxy.send_event(UiEvent::Tray(tray::Action::Exit));
                    }
                } else if initialized.load(Ordering::SeqCst) {
                    window.set_visible(true);
                    window.set_minimized(false);
                    window.set_focus();
                    let _ = webview.evaluate_script("window.refreshState(true)");
                }
            }
            Event::UserEvent(UiEvent::Tray(tray::Action::Exit)) => {
                app.log("exit_from_tray", json!({}));
                process::shutdown();
                drop(tray.take());
                if smoke {
                    let mut report = smoke_report.take().unwrap_or_default();
                    report["tray_removed"] = json!(!tray::show_existing(&root));
                    let ok = ["ok", "closed_to_tray", "tray_message", "tray_removed"]
                        .iter()
                        .all(|key| report[*key] == true);
                    let saved =
                        fs::write(root.join("data/ui-smoke.json"), report.to_string()).is_ok();
                    std::process::exit(if ok && saved { 0 } else { 1 });
                }
                std::process::exit(0);
            }
            Event::UserEvent(UiEvent::Ready(metrics)) => {
                if !initialized.swap(true, Ordering::SeqCst) {
                    app.log("ui_ready", metrics.clone());
                    if smoke {
                        let ok =
                            metrics["rendered"] == true && metrics["tag_catalog_ready"] == true;
                        smoke_report = Some(json!({"ok":ok,"ipc":true,"page":metrics,"mica":mica,"mica_requested":backdrop,"forced_fallback":mica_failure}));
                        if mica_smoke {
                            if mica {
                                let _ = webview.evaluate_script("document.documentElement.dataset.backdrop='mica'");
                            }
                            window.set_visible(true);
                            // Allow an isolated native-window capture/resize before
                            // running the usual close/tray checks. No background jobs.
                            let hwnd = window.hwnd() as isize;
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_secs(12));
                                unsafe {
                                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                                        hwnd as _, windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE, 0, 0);
                                }
                            });
                            return;
                        }
                        // Exercise a real native close message without showing or focusing the window.
                        let sent = unsafe {
                            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                                window.hwnd() as _,
                                windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                                0,
                                0,
                            )
                        };
                        if sent == 0 {
                            std::process::exit(1);
                        }
                    } else {
                        // The page retains a protective tint even with native Mica.
                        if mica {
                            let _ = webview.evaluate_script(
                                "document.documentElement.dataset.backdrop='mica'",
                            );
                        }
                        window.set_visible(true);
                    }
                }
            }
            Event::UserEvent(UiEvent::Reply(v)) => {
                let _ = webview.evaluate_script(&format!("window.receive({})", v));
            }
            Event::UserEvent(UiEvent::Refresh) => {
                push_generation += 1;
                let _ = webview.evaluate_script(&format!("window.stateChanged({push_generation})"));
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(_) | WindowEvent::Resized(_),
                ..
            } if mica => {
                if let Err(error) = backdrop::refresh(window.hwnd() as isize) {
                    mica = false;
                    // Restore every layer together if native composition stops working.
                    let _ = webview.evaluate_script("delete document.documentElement.dataset.backdrop");
                    let _ = webview.set_background_color((32, 32, 32, 255));
                    window.set_background_color(Some((32, 32, 32, 255)));
                    backdrop::clear(window.hwnd() as isize);
                    app.log("backdrop_unavailable", json!({"error":error,"stage":"refresh"}));
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                window.set_visible(false);
                app.log("hide_to_tray", json!({}));
                if smoke {
                    if let Some(report) = smoke_report.as_mut() {
                        report["closed_to_tray"] = json!(!window.is_visible());
                        if !tray::show_existing(&root) {
                            std::process::exit(1);
                        }
                    }
                }
            }
            _ => {}
        }
    });
}
