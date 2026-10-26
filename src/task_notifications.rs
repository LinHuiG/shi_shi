//! One post-commit notice per task. Pending notices get five minutes from display.
use crate::{model::Audit, UiEvent};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Theme, Window, WindowBuilder, WindowId},
};
use wry::{WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};
const LIFETIME: Duration = Duration::from_secs(300);
const WIDTH: f64 = 320.;
const HEIGHT: f64 = 140.;
const MAX_VISIBLE: usize = 3;

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Notice {
    id: String,
    title: String,
    summary: String,
}
struct Pending {
    notice: Notice,
    shown_at: Option<Instant>,
}
#[derive(Default)]
pub struct Queue {
    items: Vec<Pending>,
    pub revision: u64,
}
impl Queue {
    pub fn task_changes(&mut self, before: &[crate::model::Task], after: &[crate::model::Task]) {
        let previous: BTreeMap<_, _> = before.iter().map(|t| (t.id.as_str(), t)).collect();
        self.push(
            &after
                .iter()
                .map(|t| Audit {
                    kind: "ai_update".into(),
                    task_id: t.id.clone(),
                    before: previous
                        .get(t.id.as_str())
                        .map_or(Value::Null, |t| json!(t)),
                    after: json!(t),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        );
    }
    pub fn push(&mut self, audits: &[Audit]) {
        for notice in changes(audits) {
            if let Some(item) = self.items.iter_mut().find(|i| i.notice.id == notice.id) {
                item.notice = notice;
                item.shown_at = None;
            } else {
                self.items.push(Pending {
                    notice,
                    shown_at: None,
                });
            }
            self.revision += 1;
        }
    }
    fn active(&mut self, at: Instant, capacity: usize) -> Vec<Notice> {
        self.items.retain(|i| {
            i.shown_at
                .is_none_or(|shown| at.saturating_duration_since(shown) < LIFETIME)
        });
        self.items
            .iter()
            .take(capacity)
            .map(|i| i.notice.clone())
            .collect()
    }
    fn shown(&mut self, id: &str, at: Instant) {
        if let Some(i) = self.items.iter_mut().find(|i| i.notice.id == id) {
            i.shown_at.get_or_insert(at);
        }
    }
    pub fn dismiss_task(&mut self, id: &str) {
        let old = self.items.len();
        self.items.retain(|i| i.notice.id != id);
        if old != self.items.len() {
            self.revision += 1;
        }
    }
    pub fn dismiss(&mut self) {
        self.items.clear();
        self.revision += 1;
    }
    pub fn pending(&self) -> bool {
        !self.items.is_empty()
    }
    pub fn count(&self) -> usize {
        self.items.len()
    }
}
fn short_summary(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()
        && text.chars().count() <= 30
        && !text.chars().any(char::is_control)
        && !text.contains(['\u{2028}', '\u{2029}']))
    .then(|| text.into())
}
fn changes(audits: &[Audit]) -> Vec<Notice> {
    let mut tasks = BTreeMap::<&str, (&Value, &Value, &str)>::new();
    for a in audits
        .iter()
        .filter(|a| ["ai_create", "ai_update", "ai_evidence"].contains(&a.kind.as_str()))
    {
        tasks
            .entry(&a.task_id)
            .and_modify(|r| {
                r.1 = &a.after;
                r.2 = &a.change_summary;
            })
            .or_insert((&a.before, &a.after, &a.change_summary));
    }
    tasks
        .into_iter()
        .filter_map(|(id, (before, after, summary))| {
            let changed = |key: &str| before[key] != after[key];
            let fallback = if before.is_null() {
                "新增事项".into()
            } else if changed("archived") {
                if after["archived"] == true {
                    "事项已归档"
                } else {
                    "事项已恢复"
                }
                .into()
            } else if changed("status") {
                short_summary(&format!(
                    "状态改为{}",
                    after["status"].as_str().unwrap_or("未设置")
                ))
                .unwrap_or("事项状态已更新".into())
            } else if changed("blocker") {
                if after["blocker"].as_str().unwrap_or("").is_empty() {
                    "阻塞已解除"
                } else {
                    "阻塞信息已更新"
                }
                .into()
            } else if changed("due") {
                "截止时间已调整".into()
            } else if changed("tags") || changed("priority") {
                "事项标签已调整".into()
            } else if changed("waiting_for") {
                "等待对象已更新".into()
            } else if changed("title") || changed("description") || changed("summary") {
                "事项内容已更新".into()
            } else if changed("archive_note") {
                "归档备注已更新".into()
            } else if changed("merged_into") {
                "事项已合并".into()
            } else if changed("evidence") {
                "补充了消息依据".into()
            } else {
                return None;
            };
            Some(Notice {
                id: id.into(),
                title: after["title"].as_str().unwrap_or("未命名事项").into(),
                summary: short_summary(summary).unwrap_or(fallback),
            })
        })
        .collect()
}
pub fn html() -> String {
    include_str!("../assets/task-notification.html").replace(
        "/* THEME */",
        include_str!("../assets/ui/styles/tokens.css"),
    )
}
#[derive(Debug)]
pub struct Command {
    window: WindowId,
    pub action: String,
    task: String,
}
struct Toast {
    window: Window,
    webview: WebView,
    ready: bool,
    presented: bool,
    last: Option<Notice>,
    rounded_size: (u32, u32),
}
impl Toast {
    fn new(
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
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
            .with_background_color((32, 32, 32, 255))
            .with_theme(Some(Theme::Dark))
            .with_inner_size(LogicalSize::new(WIDTH, HEIGHT))
            .build(target)
            .map_err(|e| e.to_string())?;
        let window_id = window.id();
        let first = std::sync::atomic::AtomicBool::new(true);
        let webview = WebViewBuilder::new_with_web_context(context)
            .with_html(html())
            .with_focused(false)
            .with_theme(wry::Theme::Dark)
            .with_background_color((32, 32, 32, 255))
            .with_devtools(false)
            .with_navigation_handler(move |_| {
                first.swap(false, std::sync::atomic::Ordering::SeqCst)
            })
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                if request.body().len() > 512 {
                    return;
                }
                let Ok(v) = serde_json::from_str::<Value>(request.body()) else {
                    return;
                };
                let action = v["action"].as_str().unwrap_or("");
                if ["ready", "presented", "dismiss", "open"].contains(&action) {
                    let _ = proxy.send_event(UiEvent::NoticeCommand(Command {
                        window: window_id,
                        action: action.into(),
                        task: v["id"].as_str().unwrap_or("").into(),
                    }));
                }
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            window,
            webview,
            ready: false,
            presented: false,
            last: None,
            rounded_size: (0, 0),
        })
    }
    fn round_corners(&mut self) {
        let size = self.window.outer_size();
        if self.rounded_size == (size.width, size.height) {
            return;
        }
        // Native clipping includes WebView children on Windows 10 and 11.
        unsafe {
            use windows_sys::Win32::Graphics::Gdi::{
                CreateRoundRectRgn, DeleteObject, SetWindowRgn,
            };
            let diameter = (16. * self.window.scale_factor()).round() as i32;
            let region = CreateRoundRectRgn(
                0,
                0,
                size.width as i32 + 1,
                size.height as i32 + 1,
                diameter,
                diameter,
            );
            if !region.is_null() {
                if SetWindowRgn(self.window.hwnd() as _, region, 1) == 0 {
                    DeleteObject(region as _);
                } else {
                    self.rounded_size = (size.width, size.height);
                }
            }
        }
    }
}
#[derive(Default)]
pub struct Surface {
    windows: Vec<Toast>,
}
impl Surface {
    /// Create only as many windows as simultaneous notices need, reusing at most three.
    pub fn prepare(
        &mut self,
        count: usize,
        target: &EventLoopWindowTarget<UiEvent>,
        context: &mut WebContext,
        proxy: EventLoopProxy<UiEvent>,
    ) -> Result<(), String> {
        while self.windows.len() < count.min(MAX_VISIBLE) {
            self.windows
                .push(Toast::new(target, context, proxy.clone())?);
        }
        Ok(())
    }
    pub fn contains(&self, id: WindowId) -> bool {
        self.windows.iter().any(|t| t.window.id() == id)
    }
    pub fn visible(&self) -> bool {
        self.windows.iter().any(|t| t.window.is_visible())
    }
    pub fn presented(&self) -> bool {
        self.visible()
            && self
                .windows
                .iter()
                .all(|t| t.ready && (!t.window.is_visible() || t.presented))
    }
    pub fn command(&mut self, c: &Command, queue: &mut Queue) -> Option<String> {
        let t = self
            .windows
            .iter_mut()
            .find(|t| t.window.id() == c.window)?;
        if c.action == "ready" {
            t.ready = true;
            return None;
        }
        let n = t.last.as_ref().filter(|n| n.id == c.task)?;
        match c.action.as_str() {
            "presented" => t.presented = true,
            "dismiss" => queue.dismiss_task(&n.id),
            "open" => return Some(n.id.clone()),
            _ => {}
        }
        None
    }
    pub fn close(&self, id: WindowId, queue: &mut Queue) {
        if let Some(n) = self
            .windows
            .iter()
            .find(|t| t.window.id() == id)
            .and_then(|t| t.last.as_ref())
        {
            queue.dismiss_task(&n.id);
        }
    }
    pub fn refresh(&mut self, queue: &mut Queue, anchor: &Window) {
        use windows_sys::Win32::{
            Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            },
            UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE},
        };
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe {
            GetMonitorInfoW(
                MonitorFromWindow(anchor.hwnd() as _, MONITOR_DEFAULTTONEAREST),
                &mut info,
            )
        } == 0
        {
            return;
        }
        let scale = anchor.scale_factor();
        let gap = (10. * scale).round() as i32;
        let height = (HEIGHT * scale).round() as i32;
        let capacity =
            ((info.rcWork.bottom - info.rcWork.top - gap) / (height + gap)).max(1) as usize;
        let at = Instant::now();
        let rows = queue.active(at, capacity.min(self.windows.len()));
        let mut bottom = info.rcWork.bottom - gap;
        for (index, t) in self.windows.iter_mut().enumerate() {
            let row = rows.get(index);
            if row.is_none() || !t.ready {
                unsafe {
                    ShowWindow(t.window.hwnd() as _, SW_HIDE);
                }
                if row.is_none() {
                    t.last = None;
                    t.presented = false;
                }
                continue;
            }
            let row = row.unwrap();
            if t.last.as_ref() != Some(row) {
                t.presented = false;
                let _ = t
                    .webview
                    .evaluate_script(&format!("window.setNotice({})", json!(row)));
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
                unsafe {
                    ShowWindow(t.window.hwnd() as _, SW_SHOWNOACTIVATE);
                }
            }
            queue.shown(&row.id, at);
            bottom -= size.height as i32 + gap;
        }
    }
    /// Isolated native acceptance also measures the formerly uncovered non-client border.
    pub fn smoke(&mut self, queue: &mut Queue, anchor: &Window) -> Value {
        use windows_sys::Win32::{
            Foundation::{POINT, RECT},
            Graphics::Gdi::{
                ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO,
                MONITOR_DEFAULTTONEAREST,
            },
            UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow, GetWindowRect},
        };
        let mut windows = Vec::new();
        for t in self.windows.iter().filter(|t| t.window.is_visible()) {
            unsafe {
                let hwnd = t.window.hwnd() as _;
                let mut outer: RECT = std::mem::zeroed();
                let mut client: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut outer);
                GetClientRect(hwnd, &mut client);
                let mut origin = POINT { x: 0, y: 0 };
                ClientToScreen(hwnd, &mut origin);
                let inset = [
                    origin.x - outer.left,
                    origin.y - outer.top,
                    outer.right - origin.x - client.right,
                    outer.bottom - origin.y - client.bottom,
                ];
                let mut monitor: MONITORINFO = std::mem::zeroed();
                monitor.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                GetMonitorInfoW(
                    MonitorFromWindow(anchor.hwnd() as _, MONITOR_DEFAULTTONEAREST),
                    &mut monitor,
                );
                windows.push(json!({"title":t.last.as_ref().map(|n|&n.title),"nonclient_insets":inset,"inside_work_area":outer.left>=monitor.rcWork.left&&outer.top>=monitor.rcWork.top&&outer.right<=monitor.rcWork.right&&outer.bottom<=monitor.rcWork.bottom,"unfocused":GetForegroundWindow()!=hwnd,"rounded":t.rounded_size!=(0,0),"shadow":t.window.has_undecorated_shadow(),"rect":[outer.left,outer.top,outer.right,outer.bottom]}));
            }
        }
        let count = windows.len();
        let first = self
            .windows
            .iter()
            .find_map(|t| t.last.as_ref().map(|n| n.id.clone()));
        if let Some(id) = first {
            queue.dismiss_task(&id);
        }
        self.refresh(queue, anchor);
        let independent = count >= 2
            && self
                .windows
                .iter()
                .filter(|t| t.window.is_visible())
                .count()
                == count - 1;
        queue.dismiss();
        self.refresh(queue, anchor);
        let valid = windows.iter().all(|v| {
            v["nonclient_insets"] == json!([0, 0, 0, 0])
                && v["inside_work_area"] == true
                && v["unfocused"] == true
                && v["rounded"] == true
                && v["shadow"] == false
        });
        json!({"ok":valid&&independent&&!self.visible()&&!queue.pending(),"windows":windows,"independent_dismissal":independent,"dismissed":!self.visible(),"lifetime_seconds":LIFETIME.as_secs()})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn audit(id: &str, before: Value, after: Value) -> Audit {
        Audit {
            kind: "ai_update".into(),
            task_id: id.into(),
            before,
            after,
            ..Default::default()
        }
    }
    #[test]
    fn summaries_are_short_and_only_meaningful_ai_changes_notify() {
        let mut accepted = audit(
            "a",
            json!({"title":"Test","description":"old"}),
            json!({"title":"Test","description":"new"}),
        );
        accepted.change_summary = "方案已确认，等待执行".into();
        assert_eq!(
            changes(&[accepted.clone()])[0].summary,
            "方案已确认，等待执行"
        );
        accepted.change_summary = "长".repeat(31);
        assert_eq!(changes(&[accepted.clone()])[0].summary, "事项内容已更新");
        accepted.kind = "manual_edit".into();
        assert!(changes(&[accepted]).is_empty());
        assert!(changes(&[audit("a", json!({"updated":1}), json!({"updated":2}))]).is_empty());
    }
    #[test]
    fn each_task_has_one_notice_and_five_minutes_from_display() {
        let mut q = Queue::default();
        for id in ["a", "b", "c", "d"] {
            q.push(&[audit(id, Value::Null, json!({"title":id}))]);
        }
        let at = Instant::now();
        assert_eq!(q.active(at, 3).len(), 3);
        for id in ["a", "b", "c"] {
            q.shown(id, at);
        }
        assert_eq!(
            q.active(at + LIFETIME - Duration::from_millis(1), 3).len(),
            3
        );
        let waiting = q.active(at + LIFETIME, 3);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, "d");
        q.shown("d", at + LIFETIME);
        q.push(&[audit("d", json!({"title":"d"}), json!({"title":"Updated"}))]);
        assert_eq!(q.items.len(), 1);
        assert!(q.items[0].shown_at.is_none());
        q.dismiss_task("missing");
        assert!(q.pending());
        q.dismiss_task("d");
        assert!(!q.pending());
    }
}
