use std::{mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

const TRAY_MESSAGE: u32 = WM_APP + 17;
const SHOW_MESSAGE: u32 = WM_APP + 18;
const SHOW: usize = 1;
const EXIT: usize = 2;
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Show,
    Exit,
}
struct State {
    callback: Box<dyn Fn(Action)>,
    taskbar_created: u32,
}
pub struct Tray {
    hwnd: HWND,
    // The boxed address stays stable while the native message window owns its pointer.
    _state: Box<State>,
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn window_name(root: &std::path::Path) -> Vec<u16> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    root.to_string_lossy().to_lowercase().hash(&mut hash);
    wide(&format!("ShishiTray-{:x}", hash.finish()))
}
pub fn show_existing(root: &std::path::Path) -> bool {
    unsafe {
        let hwnd = FindWindowW(
            wide("ShishiTrayWindow").as_ptr(),
            window_name(root).as_ptr(),
        );
        !hwnd.is_null() && PostMessageW(hwnd, SHOW_MESSAGE, 0, 0) != 0
    }
}
unsafe fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1usize as _);
    if data.hIcon.is_null() {
        data.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let tip = wide("拾事 · 后台运行\n点击打开，右键退出");
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}
impl Tray {
    pub fn new(
        root: &std::path::Path,
        callback: impl Fn(Action) + 'static,
    ) -> Result<Self, String> {
        unsafe {
            let name = wide("ShishiTrayWindow");
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&class);
            let mut state = Box::new(State {
                callback: Box::new(callback),
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            });
            let hwnd = CreateWindowExW(
                0,
                name.as_ptr(),
                window_name(root).as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                (&mut *state as *mut State).cast(),
            );
            if hwnd.is_null() {
                return Err(format!(
                    "无法创建托盘窗口：{}",
                    std::io::Error::last_os_error()
                ));
            }
            if Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)) == 0 {
                DestroyWindow(hwnd);
                return Err(format!(
                    "无法添加托盘图标：{}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self {
                hwnd,
                _state: state,
            })
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &icon_data(self.hwnd));
            DestroyWindow(self.hwnd);
        }
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
    if !state.is_null() {
        let state = &*state;
        if message == SHOW_MESSAGE {
            (state.callback)(Action::Show);
            return 0;
        }
        if message == state.taskbar_created && message != 0 {
            Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
            return 0;
        }
        if message == TRAY_MESSAGE {
            match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => (state.callback)(Action::Show),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let menu = CreatePopupMenu();
                    if !menu.is_null() {
                        AppendMenuW(menu, MF_STRING, SHOW, wide("打开拾事").as_ptr());
                        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                        AppendMenuW(menu, MF_STRING, EXIT, wide("退出").as_ptr());
                        let mut cursor = POINT { x: 0, y: 0 };
                        GetCursorPos(&mut cursor);
                        SetForegroundWindow(hwnd);
                        let choice = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                            cursor.x,
                            cursor.y,
                            0,
                            hwnd,
                            std::ptr::null(),
                        );
                        PostMessageW(hwnd, WM_NULL, 0, 0);
                        DestroyMenu(menu);
                        match choice as usize {
                            SHOW => (state.callback)(Action::Show),
                            EXIT => (state.callback)(Action::Exit),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return 0;
        }
    }
    if message == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}
