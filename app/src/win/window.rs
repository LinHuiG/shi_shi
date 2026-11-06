//! The main window loop. Everything is event driven: store revisions arrive
//! through `Runtime::watch`, and the loop sleeps until the next real deadline
//! (page load limit, toast expiry or update schedule). A background start
//! runs only the tray; the window and its WebView are created the first time
//! the page is needed and are then kept (hidden) so the page state survives.
use super::{backdrop, notify::Surface, panel, tray, UiEvent};
use crate::{notices::Queue, owner::Owner, service::Signal};
use serde_json::{json, Value};
use shishi_core::{time::now, HostEvent};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tao::{
    dpi::LogicalSize,
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Icon, Theme, Window, WindowBuilder},
};
use wry::{MemoryUsageLevel, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

pub(super) const SOLID: (u8, u8, u8, u8) = (32, 32, 32, 255);

/// Clip a borderless window to rounded corners at its current size; native
/// clipping also covers the WebView child on Windows 10 and 11.
pub(super) fn round_corners(window: &Window) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn};
    let size = window.outer_size();
    let diameter = (16. * window.scale_factor()).round() as i32;
    unsafe {
        let region = CreateRoundRectRgn(0, 0, size.width as i32 + 1, size.height as i32 + 1, diameter, diameter);
        if region.is_null() {
            return false;
        }
        if SetWindowRgn(window.hwnd() as _, region, 1) == 0 {
            DeleteObject(region as _);
            return false;
        }
    }
    true
}
const STARTUP_LIMIT: Duration = Duration::from_secs(60);
const STARTUP_STALLED: &str = "界面初始化超过60秒，页面未完成加载。";

/// The main window with its page, created on first use.
struct View {
    window: Window,
    webview: WebView,
    hwnd: isize,
    mica: bool,
    /// The page answered `ui_ready`.
    ready: bool,
    /// The page must be ready by then.
    deadline: Instant,
}

impl View {
    fn create(
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        owner: &Arc<Owner>,
        proxy: &EventLoopProxy<UiEvent>,
    ) -> Result<Self, String> {
        let icon = Icon::from_rgba(include_bytes!("../../../assets/shishi.rgba").to_vec(), 64, 64).ok();
        let window = WindowBuilder::new()
            .with_title("拾事")
            .with_transparent(false)
            .with_background_color((0, 0, 0, 0))
            // Shown only after the page has rendered and answered over IPC.
            .with_visible(false)
            .with_focused(false)
            .with_window_icon(icon.clone())
            .with_taskbar_icon(icon)
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(1360., 820.))
            .with_min_inner_size(LogicalSize::new(900., 620.))
            .build(target)
            .map_err(|e| e.to_string())?;
        let hwnd = window.hwnd() as isize;
        let mica = backdrop::apply_mica(hwnd).is_ok();
        if !mica {
            window.set_background_color(Some(SOLID));
        }
        let handler = owner.clone();
        let proxy = proxy.clone();
        let first = std::sync::atomic::AtomicBool::new(true);
        let webview = WebViewBuilder::new_with_web_context(context)
            .with_html(super::page(owner.args.ui_dir.as_deref())?)
            .with_focused(false)
            .with_theme(wry::Theme::Dark)
            .with_transparent(mica)
            .with_background_color(if mica { (0, 0, 0, 0) } else { SOLID })
            .with_devtools(false)
            .with_navigation_handler(move |url| {
                url == "about:blank"
                    || url.starts_with("data:text/html;") && first.swap(false, std::sync::atomic::Ordering::SeqCst)
            })
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                let Ok(value) = serde_json::from_str::<Value>(request.body()) else { return };
                match value["method"].as_str().unwrap_or("") {
                    "ui_ready" => {
                        let _ = proxy.send_event(UiEvent::Ready);
                        let _ = proxy.send_event(UiEvent::Reply(json!({"id": value["id"], "ok": true, "data": true})));
                    }
                    "notification_opened" => {
                        if let Some(id) = value["params"]["id"].as_str().filter(|id| id.len() <= 128) {
                            let _ = proxy.send_event(UiEvent::NoticeOpened(id.into()));
                        }
                    }
                    _ => dispatch(&handler, &proxy, value, UiEvent::Reply),
                }
            })
            .build(&window)
            .map_err(|e| format!("界面启动失败：{e}"))?;
        Ok(Self { window, webview, hwnd, mica, ready: false, deadline: Instant::now() + STARTUP_LIMIT })
    }

    fn reveal(&self) {
        let _ = self.webview.set_memory_usage_level(MemoryUsageLevel::Normal);
        // The page's `document.hidden` follows the controller, not the window.
        let _ = self.webview.set_visible(true);
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
        let _ = self.webview.evaluate_script("window.refreshState(true)");
    }

    fn conceal(&self) {
        self.window.set_visible(false);
        let _ = self.webview.set_visible(false);
        let _ = self.webview.set_memory_usage_level(MemoryUsageLevel::Low);
    }
}

/// One page request: host and CLI methods on a short-lived thread, core methods on
/// the core executor; either way the answer returns through the proxy as `answer`.
pub(super) fn dispatch(
    owner: &Arc<Owner>,
    proxy: &EventLoopProxy<UiEvent>,
    request: Value,
    answer: fn(Value) -> UiEvent,
) {
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or("").to_owned();
    let params = request["params"].clone();
    let reply = {
        let (proxy, method) = (proxy.clone(), method.clone());
        move |result: Result<Value, String>| {
            let handoff = result.is_ok() && crate::service::HANDING_OFF.contains(&method.as_str());
            let _ = proxy.send_event(answer(match result {
                Ok(data) => json!({"id": id, "ok": true, "data": data}),
                Err(error) => json!({"id": id, "ok": false, "error": error}),
            }));
            if handoff {
                let _ = proxy.send_event(UiEvent::Signal(Signal::Handoff));
            }
        }
    };
    let active = match owner.owners.begin() {
        Ok(active) => active,
        Err(error) => return reply(Err(error)),
    };
    if !crate::HOST_METHODS.contains(&method.as_str()) && !crate::cli::METHODS.contains(&method.as_str()) {
        return owner.rt.request(method, params, move |result| {
            let _active = active;
            reply(result.map_err(|e| e.0));
        });
    }
    let owner = owner.clone();
    let spawned = std::thread::Builder::new().name("shishi-host-request".into()).spawn(move || {
        let _active = active;
        reply(owner.handle(&method, params));
    });
    if spawned.is_err() {
        let _ = proxy.send_event(answer(json!({"id": request["id"], "ok": false, "error": "无法启动请求"})));
    }
}

/// Select `id` in the page and tell the host once it is open.
fn open_task(v: &View, id: &str) {
    let _ = v.webview.evaluate_script(&format!(
        "window.openNotifiedTask({id}).then(opened=>{{if(opened)window.ipc.postMessage(JSON.stringify({{method:'notification_opened',params:{{id:{id}}}}}));}}).catch(()=>{{}});",
        id = json!(id)
    ));
}

fn instant_at(epoch_ms: i64) -> Instant {
    Instant::now() + Duration::from_millis(epoch_ms.saturating_sub(now()).max(0) as u64)
}

/// Open the main window: create it on first use, reveal it once the page
/// has answered. A page that cannot be created ends the program.
fn show(
    view: &mut Option<View>,
    target: &EventLoopWindowTarget<UiEvent>,
    context: &mut WebContext,
    owner: &Arc<Owner>,
    proxy: &EventLoopProxy<UiEvent>,
) {
    match view {
        Some(v) if v.ready => v.reveal(),
        Some(_) => {}
        None => match View::create(target, context, owner, proxy) {
            Ok(v) => *view = Some(v),
            Err(error) => crate::fail(&error, Some(&owner.data), true),
        },
    }
}

pub fn run(owner: Arc<Owner>) -> Result<(), String> {
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let proxy = Mutex::new(proxy.clone());
        owner.host.connect(move |event| {
            let _ = proxy.lock().unwrap().send_event(UiEvent::Host(event));
        });
    }
    let service = {
        let proxy = Mutex::new(proxy.clone());
        owner.serve(move |signal| {
            let _ = proxy.lock().unwrap().send_event(UiEvent::Signal(signal));
        })?
    };
    {
        let proxy = Mutex::new(proxy.clone());
        owner.rt.watch(move |revision| {
            let _ = proxy.lock().unwrap().send_event(UiEvent::Refresh(revision));
        });
    }
    let mut tray = Some({
        let proxy = proxy.clone();
        tray::Tray::new(&owner.root, move |action| {
            let _ = proxy.send_event(UiEvent::Tray(action));
        })?
    });

    let mut context = WebContext::new(Some(owner.data.join("webview")));
    let mut view: Option<View> = None;
    let updates = super::updates(&owner).clone();
    let mut ticking = false;
    let mut stopping = false;
    let mut service = Some(service);
    let mut queue = Queue::default();
    let mut surface: Option<Surface> = None;
    let mut panel: Option<panel::Panel> = None;
    // A task the panel asked for before the main page was ready.
    let mut opening: Option<String> = None;

    // Ask the owner to stop everything, then leave from the loop.
    let stop = {
        let owner = owner.clone();
        let proxy = proxy.clone();
        move |restart: bool| {
            let owner = owner.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                owner.shutdown();
                let _ = proxy.send_event(UiEvent::Stopped { restart });
            });
        }
    };

    let gui = {
        let owner = owner.clone();
        move || owner.owners.gui()
    };
    let enabled = {
        let owner = owner.clone();
        move || super::updates_enabled(&owner)
    };
    event_loop.run(move |event, target, control_flow| {
        match event {
            Event::NewEvents(cause) => {
                if cause == StartCause::Init && gui() {
                    show(&mut view, target, &mut context, &owner, &proxy);
                }
                if view.as_ref().is_some_and(|v| !v.ready && Instant::now() >= v.deadline) {
                    crate::fail(STARTUP_STALLED, Some(&owner.data), gui());
                }
                if let Some(s) = surface.as_mut() {
                    s.refresh(&mut queue, view.as_ref().map(|v| &v.window));
                }
                if !ticking && !stopping && now() >= updates.next_wake(now(), enabled()) {
                    ticking = true;
                    let (updates, proxy, on) = (updates.clone(), proxy.clone(), enabled());
                    let args = owner.args.restart(owner.owners.gui(), owner.owners.cli());
                    std::thread::spawn(move || {
                        let due = updates.tick(on);
                        // The 4:00 install needs no page: hand off like "update now".
                        if due.install {
                            match updates.install(args) {
                                Ok(_) => {
                                    let _ = proxy.send_event(UiEvent::Signal(Signal::Handoff));
                                }
                                Err(error) => updates.fail(format!("定时更新安装失败：{error}")),
                            }
                        }
                        let _ = proxy.send_event(UiEvent::Ticked(due.remind));
                    });
                }
            }
            Event::UserEvent(UiEvent::Ticked(remind)) => {
                ticking = false;
                if remind && gui() {
                    show(&mut view, target, &mut context, &owner, &proxy);
                }
            }
            Event::UserEvent(UiEvent::Ready) => {
                if let Some(v) = view.as_mut().filter(|v| !v.ready) {
                    v.ready = true;
                    // The page keeps a protective tint even over native Mica.
                    if v.mica {
                        let _ = v.webview.evaluate_script("document.documentElement.dataset.backdrop='mica'");
                    }
                    if gui() {
                        v.reveal();
                        if let Some(id) = opening.take() {
                            open_task(v, &id);
                        }
                    } else {
                        v.conceal();
                    }
                }
            }
            Event::UserEvent(UiEvent::PanelReply(value)) => {
                if let Some(p) = panel.as_ref() {
                    let _ = p.webview.post_web_message_as_json(&value.to_string());
                }
            }
            Event::UserEvent(UiEvent::Panel(command)) => {
                // A command can still arrive after the panel was dropped.
                if let Some(p) = panel.as_mut() {
                    match command {
                        panel::Command::Ready => p.ready(target),
                        panel::Command::Hide => p.hide(),
                        panel::Command::Open(task) => {
                            p.hide();
                            if owner.owners.acquire(true).is_ok() {
                                show(&mut view, target, &mut context, &owner, &proxy);
                                match (view.as_ref().filter(|v| v.ready), task) {
                                    (Some(v), Some(id)) => open_task(v, &id),
                                    (None, task) => opening = task,
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            Event::UserEvent(UiEvent::Reply(value)) => {
                if let Some(v) = view.as_ref() {
                    let _ = v.webview.post_web_message_as_json(&value.to_string());
                }
            }
            Event::UserEvent(UiEvent::Refresh(revision)) => {
                if let Some(v) = view.as_ref().filter(|v| v.window.is_visible()) {
                    let _ = v.webview.evaluate_script(&format!("window.stateChanged({revision})"));
                }
                if let Some(p) = panel.as_ref().filter(|p| p.visible()) {
                    let _ = p.webview.evaluate_script("window.stateChanged()");
                }
            }
            Event::UserEvent(UiEvent::Signal(Signal::Show)) => {
                if gui() {
                    show(&mut view, target, &mut context, &owner, &proxy);
                }
            }
            Event::UserEvent(UiEvent::Signal(Signal::Hide)) => {
                if !gui() {
                    if let Some(v) = view.as_ref() {
                        v.conceal();
                    }
                    queue.dismiss();
                    surface = None;
                    panel = None;
                }
            }
            Event::UserEvent(UiEvent::Signal(Signal::Shutdown | Signal::Handoff)) => {
                // After a handoff the helper restarts the program; we only leave.
                if !std::mem::replace(&mut stopping, true) {
                    stop(false);
                }
            }
            Event::UserEvent(UiEvent::Host(HostEvent::Exit { restart })) => {
                if !std::mem::replace(&mut stopping, true) {
                    stop(restart);
                }
            }
            Event::UserEvent(UiEvent::Host(HostEvent::Open(target))) => {
                let _ = super::shell_open(&target);
            }
            Event::UserEvent(UiEvent::Host(event @ HostEvent::Notify { .. })) => {
                // The tray stays while only the console owns the backend, so
                // its changes still notify.
                if !stopping {
                    if let Some(notice) = crate::notices::Notice::from_event(&event) {
                        queue.push(notice);
                    }
                    let s = surface.get_or_insert_with(Surface::default);
                    if s.prepare(queue.count(), target, &mut context, &proxy).is_err() {
                        queue.dismiss();
                    }
                    s.refresh(&mut queue, view.as_ref().map(|v| &v.window));
                }
            }
            Event::UserEvent(UiEvent::Stopped { restart }) => {
                drop(service.take());
                drop(tray.take());
                if restart {
                    if let Err(error) = owner.relaunch() {
                        super::fatal(&error);
                    }
                }
                std::process::exit(0);
            }
            Event::UserEvent(UiEvent::Notice(command)) => {
                if let Some(s) = surface.as_mut() {
                    if let Some(notice) = s.command(&command, &mut queue) {
                        if owner.owners.acquire(true).is_ok() {
                            show(&mut view, target, &mut context, &owner, &proxy);
                            match (view.as_ref().filter(|v| v.ready), notice.task) {
                                (Some(v), Some(id)) => open_task(v, &id),
                                (None, Some(id)) => opening = Some(id),
                                _ => {}
                            }
                        }
                    }
                    s.refresh(&mut queue, view.as_ref().map(|v| &v.window));
                }
            }
            Event::UserEvent(UiEvent::NoticeOpened(id)) => {
                queue.dismiss_task(&id);
                if let Some(s) = surface.as_mut() {
                    s.refresh(&mut queue, view.as_ref().map(|v| &v.window));
                }
            }
            Event::UserEvent(UiEvent::Tray(tray::Action::Panel)) => {
                if panel.is_none() {
                    match panel::Panel::create(target, &mut context, &owner, &proxy) {
                        Ok(p) => panel = Some(p),
                        Err(error) => super::fatal(&format!("面板启动失败：{error}")),
                    }
                }
                if let Some(p) = panel.as_mut() {
                    p.toggle(target);
                }
            }
            Event::UserEvent(UiEvent::Tray(tray::Action::Show)) => {
                if let Some(p) = panel.as_mut() {
                    p.hide();
                }
                if owner.owners.acquire(true).is_ok() {
                    show(&mut view, target, &mut context, &owner, &proxy);
                }
            }
            Event::UserEvent(UiEvent::Tray(tray::Action::Exit)) => {
                if !stopping {
                    let force = owner.owners.cli() && super::force_exit(view.as_ref().map_or(0, |v| v.hwnd));
                    let closing = owner.owners.release(true, force);
                    if let Some(v) = view.as_ref() {
                        v.conceal();
                    }
                    queue.dismiss();
                    surface = None;
                    panel = None;
                    if closing {
                        stopping = true;
                        stop(false);
                    }
                }
            }
            Event::WindowEvent {
                window_id, event: WindowEvent::Focused(false) | WindowEvent::CloseRequested, ..
            } if panel.as_ref().is_some_and(|p| p.contains(window_id)) => {
                if let Some(p) = panel.as_mut() {
                    p.hide();
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. }
                if surface.as_ref().is_some_and(|s| s.contains(window_id)) =>
            {
                if let Some(s) = surface.as_mut() {
                    s.close(window_id, &mut queue);
                    s.refresh(&mut queue, view.as_ref().map(|v| &v.window));
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if let Some(v) = view.as_ref().filter(|v| v.window.id() == window_id) {
                    v.conceal();
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::Focused(_) | WindowEvent::Resized(_), .. } => {
                if let Some(v) = view.as_mut().filter(|v| v.mica && v.window.id() == window_id) {
                    if backdrop::refresh(v.hwnd).is_err() {
                        // Restore every layer together once composition stops working.
                        v.mica = false;
                        let _ = v.webview.evaluate_script("delete document.documentElement.dataset.backdrop");
                        let _ = v.webview.set_background_color(SOLID);
                        v.window.set_background_color(Some(SOLID));
                        backdrop::clear(v.hwnd);
                    }
                }
            }
            _ => {}
        }
        // Sleep until the earliest real deadline.
        let mut wake = if ticking || stopping { None } else { Some(instant_at(updates.next_wake(now(), enabled()))) };
        if let Some(v) = view.as_ref().filter(|v| !v.ready) {
            wake = Some(wake.map_or(v.deadline, |w| w.min(v.deadline)));
        }
        if let Some(expiry) = queue.deadline() {
            wake = Some(wake.map_or(expiry, |w| w.min(expiry)));
        }
        *control_flow = match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        };
    })
}
