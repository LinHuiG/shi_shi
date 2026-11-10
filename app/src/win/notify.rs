//! Up to three borderless, topmost toast windows stacked in the bottom right
//! of the work area. They show without taking focus.
use super::{window::SOLID, UiEvent};
use crate::notices::{Notice, Queue};
use serde_json::{json, Value};
use std::time::Instant;
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Theme, Window, WindowBuilder, WindowId},
};
use wry::{WebContext, WebView, WebViewBuilderExtWindows};

const WIDTH: f64 = 320.;
const HEIGHT: f64 = 140.;
const MAX_VISIBLE: usize = 3;

fn html() -> String {
    include_str!("../../../assets/task-notification.html")
        .replace("/* THEME */", include_str!("../../../assets/ui/styles/tokens.css"))
}

#[derive(Debug)]
pub struct Command {
    window: WindowId,
    action: String,
    task: String,
}

struct Toast {
    window: Window,
    webview: WebView,
    ready: bool,
    last: Option<Notice>,
    rounded_size: (u32, u32),
}

impl Toast {
    fn new(
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        live: Option<&WebView>,
        proxy: EventLoopProxy<UiEvent>,
    ) -> Result<Self, String> {
        let window = WindowBuilder::new()
            .with_title("拾事 · 事项更新")
            .with_visible(false)
            .with_focused(false)
            .with_skip_taskbar(true)
            .with_always_on_top(true)
            .with_decorations(false)
            .with_undecorated_shadow(false)
            .with_resizable(false)
            .with_background_color(SOLID)
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .build(target)
            .map_err(|e| e.to_string())?;
        let id = window.id();
        let first = std::sync::atomic::AtomicBool::new(true);
        let webview = super::window::builder(context, live)
            .with_html(html())
            .with_focused(false)
            .with_theme(wry::Theme::Dark)
            .with_background_color(SOLID)
            .with_devtools(false)
            .with_navigation_handler(move |_| first.swap(false, std::sync::atomic::Ordering::SeqCst))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                if request.body().len() > 512 {
                    return;
                }
                let Ok(v) = serde_json::from_str::<Value>(request.body()) else { return };
                let action = v["action"].as_str().unwrap_or("");
                if ["ready", "presented", "dismiss", "open"].contains(&action) {
                    let _ = proxy.send_event(UiEvent::Notice(Command {
                        window: id,
                        action: action.into(),
                        task: v["id"].as_str().unwrap_or("").into(),
                    }));
                }
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        Ok(Self { window, webview, ready: false, last: None, rounded_size: (0, 0) })
    }

    fn round_corners(&mut self) {
        let size = self.window.outer_size();
        if self.rounded_size != (size.width, size.height) && super::window::round_corners(&self.window) {
            self.rounded_size = (size.width, size.height);
        }
    }
}

#[derive(Default)]
pub struct Surface {
    windows: Vec<Toast>,
}

impl Surface {
    /// Create only as many windows as simultaneous notices need.
    pub fn prepare(
        &mut self,
        count: usize,
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        live: Option<&WebView>,
        proxy: &EventLoopProxy<UiEvent>,
    ) -> Result<(), String> {
        while self.windows.len() < count.min(MAX_VISIBLE) {
            let toast = Toast::new(target, context, live.or(self.webview()), proxy.clone())?;
            self.windows.push(toast);
        }
        Ok(())
    }

    pub fn webview(&self) -> Option<&WebView> {
        self.windows.first().map(|t| &t.webview)
    }

    pub fn contains(&self, id: WindowId) -> bool {
        self.windows.iter().any(|t| t.window.id() == id)
    }

    /// The notice to open, for `open`.
    pub fn command(&mut self, c: &Command, queue: &mut Queue) -> Option<Notice> {
        let t = self.windows.iter_mut().find(|t| t.window.id() == c.window)?;
        if c.action == "ready" {
            t.ready = true;
            return None;
        }
        let n = t.last.clone().filter(|n| n.id == c.task)?;
        match c.action.as_str() {
            "dismiss" => queue.close(&n.id),
            "open" => {
                queue.close(&n.id);
                return Some(n);
            }
            _ => {}
        }
        None
    }

    pub fn close(&self, id: WindowId, queue: &mut Queue) {
        if let Some(n) = self.windows.iter().find(|t| t.window.id() == id).and_then(|t| t.last.as_ref()) {
            queue.close(&n.id);
        }
    }

    /// Lay the active notices out on the anchor's monitor; without a main
    /// window yet (a tray-only start), on the primary monitor.
    pub fn refresh(&mut self, queue: &mut Queue, anchor: Option<&Window>) {
        use windows_sys::Win32::{
            Foundation::POINT,
            Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
                MONITOR_DEFAULTTOPRIMARY,
            },
            UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE},
        };
        let monitor = match anchor {
            Some(a) => unsafe { MonitorFromWindow(a.hwnd() as _, MONITOR_DEFAULTTONEAREST) },
            None => unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) },
        };
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return;
        }
        let Some(scale) = anchor.or(self.windows.first().map(|t| &t.window)).map(Window::scale_factor) else { return };
        let gap = (10. * scale).round() as i32;
        let height = (HEIGHT * scale).round() as i32;
        let capacity = ((info.rcWork.bottom - info.rcWork.top - gap) / (height + gap)).max(1) as usize;
        let at = Instant::now();
        let rows = queue.active(at, capacity.min(self.windows.len()));
        let mut bottom = info.rcWork.bottom - gap;
        for (index, t) in self.windows.iter_mut().enumerate() {
            let Some(row) = rows.get(index).filter(|_| t.ready) else {
                unsafe { ShowWindow(t.window.hwnd() as _, SW_HIDE) };
                if rows.get(index).is_none() {
                    t.last = None;
                }
                continue;
            };
            if t.last.as_ref() != Some(row) {
                let _ = t.webview.evaluate_script(&format!("window.setNotice({})", json!(row)));
                t.last = Some(row.clone());
            }
            let size = t.window.outer_size();
            let position = PhysicalPosition::new(
                (info.rcWork.right - size.width as i32 - gap).max(info.rcWork.left),
                (bottom - size.height as i32).max(info.rcWork.top),
            );
            if t.window.outer_position().ok() != Some(position) {
                t.window.set_outer_position(position);
            }
            t.round_corners();
            if !t.window.is_visible() {
                unsafe { ShowWindow(t.window.hwnd() as _, SW_SHOWNOACTIVATE) };
            }
            queue.shown(&row.id, at);
            bottom -= size.height as i32 + gap;
        }
    }
}
