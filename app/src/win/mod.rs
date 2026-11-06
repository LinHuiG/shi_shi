//! Windows desktop: window, tray, tray panel, toasts and the host-owned page methods.
pub mod backdrop;
mod notify;
mod panel;
pub mod preview;
pub mod tray;
mod window;

use crate::{owner::Owner, service::Signal, updates::Updates};
use serde_json::{json, Value};
use shishi_core::HostEvent;
use std::{
    path::Path,
    sync::{atomic::Ordering, Arc, OnceLock},
};

pub use window::run;

/// Everything that reaches the main thread through the event loop proxy.
pub enum UiEvent {
    Signal(Signal),
    Host(HostEvent),
    /// Answer to one page request.
    Reply(Value),
    Ready,
    /// Answer to one tray panel request.
    PanelReply(Value),
    Panel(panel::Command),
    NoticeOpened(String),
    Notice(notify::Command),
    Tray(tray::Action),
    /// The store changed to this revision.
    Refresh(u64),
    /// An update scheduler step finished; true raises the 9:00 reminder.
    /// A due 4:00 install was already started on the step's thread.
    Ticked(bool),
    /// Shutdown finished: leave, optionally starting again.
    Stopped {
        restart: bool,
    },
}

pub fn fatal(text: &str) {
    rfd::MessageDialog::new().set_title("拾事").set_description(text).set_level(rfd::MessageLevel::Error).show();
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Open a file, folder or URL with the system handler.
pub fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    if target.is_empty() || target.contains('\0') {
        return Err("链接无效".into());
    }
    let (verb, target) = (wide("open"), wide(target));
    let result = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
    };
    if result as isize <= 32 {
        return Err("无法打开，请检查系统默认程序设置".into());
    }
    Ok(())
}

/// Ask whether tray exit should also stop the CLI's backend.
fn force_exit(hwnd: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO};
    let text = wide("CLI 仍在使用后台。是否同时强制关闭 CLI 后台？\n\n默认只退出图形端，CLI 和后台继续运行。强制关闭会中断正在执行的工作。");
    let title = wide("退出拾事");
    unsafe {
        MessageBoxW(hwnd as _, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION) == IDYES
    }
}

static UPDATES: OnceLock<Arc<Updates>> = OnceLock::new();

/// The process's update state; the view is published through `ctx.flags`.
fn updates(owner: &Owner) -> &'static Arc<Updates> {
    UPDATES.get_or_init(|| {
        let ctx = owner.ctx.clone();
        Arc::new(Updates::load(owner.data.join("updates"), move |view| ctx.flags.set_updates(&ctx, view)))
    })
}

fn updates_enabled(owner: &Owner) -> bool {
    owner.ctx.config.get().auto_update && !owner.args.no_auto_update
}

/// Page and CLI methods the host answers itself; `None` for core methods.
pub fn handle(owner: &Arc<Owner>, method: &str, params: &Value) -> Option<Result<Value, String>> {
    Some(match method {
        "pick" => crate::picker::pick(params),
        "open_data" => shell_open(&owner.data.display().to_string()).map(|_| json!(true)),
        "about_open" => {
            let url = match params["target"].as_str() {
                Some("github") => "https://github.com/LinHuiG/shi_shi",
                Some("blog") => "https://github.com/LinHuiG/shi_shi/tree/main/docs",
                Some("intro") => "https://github.com/LinHuiG/shi_shi#readme",
                _ => return Some(Err("链接无效".into())),
            };
            shell_open(url).map_err(|_| "无法打开浏览器，请检查默认浏览器设置".into()).map(|_| json!(true))
        }
        "update_check" => updates(owner).check(false, updates_enabled(owner)),
        "update_install" => updates(owner).install(owner.args.restart(owner.owners.gui(), owner.owners.cli())),
        "storage_change" => {
            // Shares the restart guard with installs: one replacement at a time.
            let guard = &updates(owner).restarting;
            if guard.swap(true, Ordering::SeqCst) {
                return Some(Err("已有重启操作正在执行".into()));
            }
            let result = crate::storage::begin(owner, params);
            if result.is_err() {
                guard.store(false, Ordering::SeqCst);
            }
            result
        }
        _ => return None,
    })
}

/// Load the page: the embedded bundle, or `--ui-dir` for development.
fn page(ui_dir: Option<&Path>) -> Result<String, String> {
    let Some(dir) = ui_dir else { return Ok(include_str!(concat!(env!("OUT_DIR"), "/ui.html")).to_string()) };
    crate::ui_bundle::assemble(&|path| std::fs::read_to_string(dir.join(path)))
        .map_err(|e| format!("界面目录无法组装（{}）：{e}", dir.display()))
}
