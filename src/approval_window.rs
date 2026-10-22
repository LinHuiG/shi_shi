//! Native approval surface. The request and answer protocol are shared with the
//! main window; closing this window only dismisses its current presentation.
use crate::{engine::App, UiEvent};
use serde_json::Value;
use std::collections::BTreeSet;
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Theme, Window, WindowBuilder, WindowId},
};
use wry::{WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

#[derive(Default)]
struct Queue {
    dismissed: BTreeSet<String>,
    current: Option<String>,
}
impl Queue {
    fn select<'a>(&mut self, rows: &'a [Value]) -> Option<&'a Value> {
        let present: BTreeSet<_> = rows.iter().filter_map(|r| r["key"].as_str()).collect();
        self.dismissed.retain(|key| present.contains(key.as_str()));
        let selected = self
            .current
            .as_deref()
            .and_then(|key| {
                rows.iter()
                    .find(|row| row["key"] == key && !self.dismissed.contains(key))
            })
            .or_else(|| {
                rows.iter().find(|r| {
                    r["key"]
                        .as_str()
                        .is_some_and(|key| !self.dismissed.contains(key))
                })
            });
        self.current = selected.and_then(|r| r["key"].as_str()).map(str::to_owned);
        selected
    }
    fn dismiss(&mut self) {
        if let Some(key) = self.current.take() {
            self.dismissed.insert(key);
        }
    }
}

pub fn html() -> String {
    let styles = format!("{}\n{}\n{}\n{}",
        include_str!("../assets/ui/styles/tokens.css"),
        include_str!("../assets/ui/styles/base.css"),
        include_str!("../assets/ui/styles/motion.css"),
        "body{margin:0;overflow:hidden;background:var(--bg);color:var(--text);font-family:'Segoe UI','Microsoft YaHei UI',sans-serif}#dialog.approval-shell{width:100vw;height:100dvh;max-height:none;margin:0;padding:16px;border:0;border-radius:0;box-sizing:border-box}.approval-count{white-space:nowrap}.approval-scroll{overscroll-behavior:contain}.approval-footer>div{gap:6px}button{min-height:30px}");
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'"><title>拾事 · AI 审批</title><style>{styles}</style></head><body><main id="dialog" class="approval-shell"></main><script>{client}
{content}</script></body></html>"#,
        client = include_str!("../assets/ui/scripts/approval-desktop.js"),
        content = include_str!("../assets/ui/scripts/approval-content.js")
    )
}

pub struct DesktopApprovals {
    window: Window,
    webview: WebView,
    queue: Queue,
    ready: bool,
    last: Option<Value>,
}
impl DesktopApprovals {
    pub fn new(
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        proxy: EventLoopProxy<UiEvent>,
        smoke: bool,
    ) -> Result<Self, String> {
        let window = WindowBuilder::new()
            .with_title("拾事 · AI 审批")
            .with_visible(false)
            .with_focused(false)
            .with_skip_taskbar(true)
            .with_always_on_top(true)
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(540., 500.))
            .with_min_inner_size(LogicalSize::new(380., 300.))
            .build(target)
            .map_err(|e| e.to_string())?;
        let html = if smoke {
            html().replacen("<script>", "<script>window.approvalSmoke=true;", 1)
        } else {
            html()
        };
        let first = std::sync::atomic::AtomicBool::new(true);
        let webview = WebViewBuilder::new_with_web_context(context)
            .with_html(html)
            .with_focused(false)
            .with_theme(wry::Theme::Dark)
            .with_devtools(false)
            .with_navigation_handler(move |_| {
                first.swap(false, std::sync::atomic::Ordering::SeqCst)
            })
            .with_ipc_handler(move |request| {
                if let Ok(value) = serde_json::from_str(request.body()) {
                    let _ = proxy.send_event(UiEvent::ApprovalCommand(value));
                }
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            window,
            webview,
            queue: Queue::default(),
            ready: false,
            last: None,
        })
    }
    pub fn hwnd(&self) -> isize {
        self.window.hwnd() as isize
    }
    pub fn visible(&self) -> bool {
        self.window.is_visible()
    }
    pub fn metrics(&self) -> Value {
        use windows_sys::Win32::{
            Foundation::RECT,
            Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            },
            UI::WindowsAndMessaging::GetWindowRect,
        };
        let mut inside = false;
        unsafe {
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let mut rect: RECT = std::mem::zeroed();
            if GetMonitorInfoW(
                MonitorFromWindow(self.window.hwnd() as _, MONITOR_DEFAULTTONEAREST),
                &mut info,
            ) != 0
                && GetWindowRect(self.window.hwnd() as _, &mut rect) != 0
            {
                inside = rect.left >= info.rcWork.left
                    && rect.top >= info.rcWork.top
                    && rect.right <= info.rcWork.right
                    && rect.bottom <= info.rcWork.bottom;
            }
        }
        serde_json::json!({"visible":self.visible(),"inside_work_area":inside})
    }
    pub fn id(&self) -> WindowId {
        self.window.id()
    }
    pub fn ready(&mut self) {
        self.ready = true;
    }
    fn hide(&self) {
        // Tao queues visibility changes. Do not mix a queued hide with the
        // synchronous non-activating show below: it could hide the new card.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                self.window.hwnd() as _,
                windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }
    }
    pub fn dismiss(&mut self) {
        self.queue.dismiss();
        self.hide();
        self.last = None;
    }
    pub fn hide_key(&mut self, key: &str) {
        if self.queue.current.as_deref() == Some(key) {
            self.dismiss();
        }
    }
    pub fn reply(&self, reply: &Value) {
        let _ = self
            .webview
            .evaluate_script(&format!("window.receive({reply})"));
    }
    pub fn refresh(&mut self, app: &App, anchor: &Window) {
        let rows = app.desktop_approvals();
        let value = self.queue.select(&rows).cloned();
        if !self.ready {
            return;
        }
        if value == self.last {
            return;
        }
        let changed = self.last.as_ref().map(|v| &v["key"]) != value.as_ref().map(|v| &v["key"]);
        if changed {
            self.hide();
        }
        let _ = self.webview.evaluate_script(&format!(
            "window.setApproval({})",
            value.as_ref().unwrap_or(&Value::Null)
        ));
        if value.is_some() {
            if changed {
                self.position(anchor);
            }
            // Show the independent desktop window without taking keyboard focus.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                    self.window.hwnd() as _,
                    windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                );
            }
        } else {
            self.hide();
        }
        self.last = value;
    }
    pub fn position(&self, anchor: &Window) {
        use windows_sys::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        unsafe {
            let monitor = MonitorFromWindow(anchor.hwnd() as _, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) != 0 {
                let margin = (16. * self.window.scale_factor()).round() as i32;
                let width = (info.rcWork.right - info.rcWork.left - 2 * margin).max(240) as u32;
                let height = (info.rcWork.bottom - info.rcWork.top - 2 * margin).max(200) as u32;
                let size = self.window.outer_size();
                if size.width > width || size.height > height {
                    let inner = self.window.inner_size();
                    self.window.set_inner_size(tao::dpi::PhysicalSize::new(
                        width.saturating_sub(size.width.saturating_sub(inner.width)),
                        height.saturating_sub(size.height.saturating_sub(inner.height)),
                    ));
                }
                let size = self.window.outer_size();
                self.window.set_outer_position(PhysicalPosition::new(
                    (info.rcWork.right - size.width as i32 - margin).max(info.rcWork.left),
                    (info.rcWork.bottom - size.height as i32 - margin).max(info.rcWork.top),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn dismissal_does_not_resolve_requests_and_new_requests_can_appear() {
        let mut q = Queue::default();
        let rows = vec![
            json!({"key":"run-a:request-1"}),
            json!({"key":"run-b:request-1"}),
        ];
        assert_eq!(q.select(&rows).unwrap(), &rows[0]);
        q.dismiss();
        assert_eq!(q.select(&rows).unwrap(), &rows[1]);
        q.dismiss();
        assert!(q.select(&rows).is_none());
        q.select(&[]);
        assert_eq!(q.select(&rows).unwrap(), &rows[0]);
    }
    #[test]
    fn resolved_or_terminated_current_request_moves_to_the_next_without_reopening() {
        let mut q = Queue::default();
        q.select(&[json!({"key":"old"})]);
        let rows = vec![json!({"key":"new"})];
        assert_eq!(q.select(&rows).unwrap(), &rows[0]);
        assert!(q.select(&[]).is_none());
    }
}
