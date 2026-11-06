//! The tray panel: a small borderless window by the tray icon, created on
//! the first click and kept hidden between uses. It closes when it loses
//! focus or on Esc, and reads state only while it is shown.
use super::{window::SOLID, UiEvent};
use crate::{owner::Owner, placement};
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{MonitorHandleExtWindows, WindowBuilderExtWindows},
    window::{Theme, Window, WindowBuilder, WindowId},
};
use wry::{MemoryUsageLevel, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

const SIZE: (f64, f64) = (360., 520.);
/// A click on the icon that closed the panel by taking its focus must not
/// open it again at once.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// What the panel page asks the loop for.
pub enum Command {
    Ready,
    Hide,
    /// Open the main window, at this task when given.
    Open(Option<String>),
}

pub struct Panel {
    window: Window,
    pub webview: WebView,
    ready: bool,
    /// Show once the page answered.
    pending: bool,
    hidden_at: Option<Instant>,
}

impl Panel {
    pub fn create(
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        owner: &Arc<Owner>,
        proxy: &EventLoopProxy<UiEvent>,
    ) -> Result<Self, String> {
        let window = WindowBuilder::new()
            .with_title("拾事")
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_always_on_top(true)
            .with_decorations(false)
            .with_undecorated_shadow(false)
            .with_resizable(false)
            .with_background_color(SOLID)
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(SIZE.0, SIZE.1))
            .build(target)
            .map_err(|e| e.to_string())?;
        let html = include_str!("../../../assets/tray-panel.html")
            .replace("/* THEME */", include_str!("../../../assets/ui/styles/tokens.css"));
        let (handler, proxy) = (owner.clone(), proxy.clone());
        let first = std::sync::atomic::AtomicBool::new(true);
        let webview = WebViewBuilder::new_with_web_context(context)
            .with_html(html)
            .with_theme(wry::Theme::Dark)
            .with_background_color(SOLID)
            .with_devtools(false)
            .with_navigation_handler(move |_| first.swap(false, std::sync::atomic::Ordering::SeqCst))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                let Ok(value) = serde_json::from_str::<Value>(request.body()) else { return };
                let command = match value["method"].as_str().unwrap_or("") {
                    "panel_ready" => Command::Ready,
                    "panel_hide" => Command::Hide,
                    "panel_open" => {
                        Command::Open(value["params"]["id"].as_str().filter(|id| id.len() <= 128).map(String::from))
                    }
                    _ => return super::window::dispatch(&handler, &proxy, value, UiEvent::PanelReply),
                };
                let _ = proxy.send_event(UiEvent::Panel(command));
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        Ok(Self { window, webview, ready: false, pending: true, hidden_at: None })
    }

    pub fn contains(&self, id: WindowId) -> bool {
        self.window.id() == id
    }

    pub fn visible(&self) -> bool {
        self.window.is_visible()
    }

    /// The tray icon was clicked: open by it, or close an open panel.
    pub fn toggle(&mut self, target: &EventLoopWindowTarget<UiEvent>) {
        if self.visible() {
            return self.hide();
        }
        if self.hidden_at.is_some_and(|at| at.elapsed() < REOPEN_GUARD) {
            return;
        }
        self.pending = true;
        self.show(target);
    }

    pub fn ready(&mut self, target: &EventLoopWindowTarget<UiEvent>) {
        self.ready = true;
        self.show(target);
    }

    fn show(&mut self, target: &EventLoopWindowTarget<UiEvent>) {
        if !self.ready || !std::mem::take(&mut self.pending) {
            return;
        }
        let Some(r) = target.cursor_position().ok().and_then(|at| place(target, at)) else { return };
        self.window.set_outer_position(PhysicalPosition::new(r.left, r.top));
        self.window.set_inner_size(PhysicalSize::new((r.right - r.left) as u32, (r.bottom - r.top) as u32));
        super::window::round_corners(&self.window);
        let _ = self.webview.set_memory_usage_level(MemoryUsageLevel::Normal);
        let _ = self.webview.set_visible(true);
        self.window.set_visible(true);
        self.window.set_focus();
        let _ = self.webview.focus();
        let _ = self.webview.evaluate_script("window.panelShown()");
    }

    pub fn hide(&mut self) {
        if !self.visible() {
            return;
        }
        self.hidden_at = Some(Instant::now());
        self.window.set_visible(false);
        let _ = self.webview.set_visible(false);
        let _ = self.webview.set_memory_usage_level(MemoryUsageLevel::Low);
    }
}

/// The panel rectangle for a click at `at`, on that point's monitor.
fn place(target: &EventLoopWindowTarget<UiEvent>, at: PhysicalPosition<f64>) -> Option<placement::Rect> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    let monitor = target.monitor_from_point(at.x, at.y)?;
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor.hmonitor() as _, &mut info) } == 0 {
        return None;
    }
    let rect = |r: windows_sys::Win32::Foundation::RECT| placement::Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };
    let (x, y) = (at.x as i32, at.y as i32);
    let anchor = placement::Rect { left: x, top: y, right: x + 1, bottom: y + 1 };
    Some(placement::place(rect(info.rcMonitor), rect(info.rcWork), anchor, SIZE, monitor.scale_factor()))
}
