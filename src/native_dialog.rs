//! Small native dialogs also used before WebView initialization. Their owner and
//! screen placement are explicit, including the installer helper process.
use crate::storage::{self, Progress, TransferPlan};
use serde_json::json;
use std::{
    fs,
    path::Path,
    ptr::null_mut,
    sync::{Arc, Mutex},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, RECT, S_FALSE, S_OK},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    UI::{Controls::*, WindowsAndMessaging::*},
};
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub fn rect(hwnd: isize) -> [i32; 4] {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetWindowRect(hwnd as HWND, &mut r);
    }
    [r.left, r.top, r.right, r.bottom]
}
fn center(hwnd: HWND, reference: [i32; 4]) -> serde_json::Value {
    unsafe {
        let [left, top, right, bottom] = reference;
        let reference = RECT {
            left,
            top,
            right,
            bottom,
        };
        let monitor = MonitorFromRect(&reference, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let work = info.rcWork;
        let before = rect(hwnd as isize);
        let width = before[2] - before[0];
        let height = before[3] - before[1];
        let x = (left + (right - left - width) / 2)
            .clamp(work.left, (work.right - width).max(work.left));
        let y = (top + (bottom - top - height) / 2)
            .clamp(work.top, (work.bottom - height).max(work.top));
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        json!({"owner": [left,top,right,bottom], "dialog":rect(hwnd as isize), "expected":[x,y],
            "work_area":[work.left,work.top,work.right,work.bottom]})
    }
}
struct DialogContext {
    reference: [i32; 4],
    progress: Option<Arc<Mutex<Progress>>>,
    ready: Option<std::path::PathBuf>,
    restart_args: Vec<String>,
    last_text: String,
    smoke: bool,
    metrics: serde_json::Value,
}
unsafe extern "system" fn callback(
    hwnd: HWND,
    notification: i32,
    w: usize,
    _: isize,
    data: isize,
) -> i32 {
    let context = &mut *(data as *mut DialogContext);
    if notification == TDN_CREATED {
        context.metrics = center(hwnd, context.reference);
        if let Some(ready) = &context.ready {
            if let Err(error) = fs::write(ready, b"ready") {
                if let Some(progress) = &context.progress {
                    *progress.lock().unwrap() = Progress {
                        done: true,
                        error: format!("无法连接迁移窗口：{error}"),
                        ..Default::default()
                    };
                }
            }
            SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, IDOK as usize, 0);
            SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_RANGE as u32, 0, 100 << 16);
        }
    } else if notification == TDN_TIMER {
        if context.smoke && w >= 250 {
            context.metrics["actual"] = json!(rect(hwnd as isize));
            PostMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDNO as usize, 0);
        }
        if let Some(shared) = &context.progress {
            let progress = shared.lock().unwrap().clone();
            let text = if progress.error.is_empty() {
                format!(
                    "{}\n{}% · {:.1} / {:.1} MB",
                    progress.phase,
                    progress.percent(),
                    progress.copied as f64 / 1048576.,
                    progress.total as f64 / 1048576.
                )
            } else {
                format!(
                    "{}\n\n原目录的数据仍保留。点击“打开拾事”继续使用。",
                    progress.error
                )
            };
            if text != context.last_text {
                SendMessageW(
                    hwnd,
                    TDM_SET_ELEMENT_TEXT as u32,
                    TDE_CONTENT as usize,
                    wide(&text).as_ptr() as isize,
                );
                SendMessageW(
                    hwnd,
                    TDM_SET_PROGRESS_BAR_POS as u32,
                    progress.percent() as usize,
                    0,
                );
                context.last_text = text;
            }
            if progress.done {
                SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, IDOK as usize, 1);
                if progress.error.is_empty() {
                    PostMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDOK as usize, 0);
                }
            }
        }
    } else if notification == TDN_BUTTON_CLICKED {
        if let Some(shared) = &context.progress {
            let progress = shared.lock().unwrap().clone();
            if !progress.done {
                return S_FALSE;
            }
            if !progress.error.is_empty() {
                if let Err(error) = storage::restart(&context.restart_args) {
                    shared.lock().unwrap().error = error;
                    return S_FALSE;
                }
            }
        }
    }
    S_OK
}
pub fn installer_confirmation(
    owner: isize,
    smoke: bool,
) -> Result<(i32, serde_json::Value), String> {
    if unsafe { IsWindow(owner as HWND) } == 0 {
        return Err("安装窗口已关闭".into());
    }
    let title = wide("拾事安装程序");
    let heading = wide("拾事正在运行，是否强制退出并继续安装？");
    let content =
        wide("正在进行的 AI 任务会中断，未保存的编辑可能丢失。已保存的事项、配置和日志会保留。");
    let mut context = DialogContext {
        reference: rect(owner),
        progress: None,
        ready: None,
        restart_args: vec![],
        last_text: String::new(),
        smoke,
        metrics: json!({}),
    };
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner as HWND,
        dwFlags: TDF_POSITION_RELATIVE_TO_WINDOW
            | TDF_CALLBACK_TIMER
            | TDF_SIZE_TO_CONTENT
            | TDF_ALLOW_DIALOG_CANCELLATION,
        dwCommonButtons: TDCBF_YES_BUTTON | TDCBF_NO_BUTTON,
        pszWindowTitle: title.as_ptr(),
        pszMainInstruction: heading.as_ptr(),
        pszContent: content.as_ptr(),
        nDefaultButton: IDNO,
        pfCallback: Some(callback),
        lpCallbackData: &mut context as *mut _ as isize,
        ..unsafe { std::mem::zeroed() }
    };
    let mut button = IDNO;
    let result = unsafe { TaskDialogIndirect(&config, &mut button, null_mut(), null_mut()) };
    if result < 0 {
        return Err(format!("无法打开安装确认窗口（{result}）"));
    }
    Ok((
        if button == IDCANCEL { IDNO } else { button },
        context.metrics,
    ))
}

pub fn transfer_window(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("迁移计划过大".into());
    }
    let plan: TransferPlan = serde_json::from_slice(&bytes).map_err(|_| "迁移计划不可读")?;
    if path.parent().and_then(|p| p.canonicalize().ok()) != plan.root.canonicalize().ok()
        || !path
            .file_name()
            .is_some_and(|p| p.to_string_lossy().starts_with(".data-transfer-"))
    {
        return Err("迁移计划位置不匹配".into());
    }
    let transition = storage::transition_lock(&plan.root)?;
    let parent = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, plan.parent_pid) };
    if parent.is_null() {
        return Err("原拾事进程不可用，目录未切换".into());
    }
    let parent_handle = parent as usize;
    let shared = Arc::new(Mutex::new(Progress {
        phase: "正在等待拾事退出".into(),
        ..Default::default()
    }));
    let ready = path.with_extension("ready");
    let worker_ready = ready.clone();
    let worker_shared = shared.clone();
    let worker_plan = plan.clone();
    let worker = std::thread::spawn(move || {
        // OpenProcess was called while the parent was alive, so PID reuse cannot
        // make the helper copy from a process that is still writing.
        let result: Result<(), String> = (|| {
            for _ in 0..600 {
                if worker_ready.is_file() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !worker_ready.is_file() {
                return Err("迁移窗口未就绪，目录未切换".into());
            }
            let wait = unsafe { WaitForSingleObject(parent_handle as _, 60_000) };
            if wait != 0 {
                return Err("等待原进程退出超时，目录未切换".into());
            }
            // The lock must be available after the process handle signaled.
            let source_lock = storage::lock_source(&worker_plan.source)?;
            storage::transfer(&worker_plan, |mut p| {
                p.done = false;
                *worker_shared.lock().unwrap() = p;
            })?;
            drop(source_lock);
            drop(transition);
            storage::restart(&worker_plan.restart_args)?;
            Ok(())
        })();
        unsafe {
            CloseHandle(parent_handle as _);
        }
        let mut progress = worker_shared.lock().unwrap();
        progress.done = true;
        if let Err(error) = result {
            progress.error = error;
        }
    });
    let title = wide("拾事 · 数据目录");
    let heading = wide(if plan.copy {
        "正在迁移数据"
    } else {
        "正在切换数据目录"
    });
    let content = wide("正在等待拾事退出…");
    let button_text = wide("打开拾事");
    let button = TASKDIALOG_BUTTON {
        nButtonID: IDOK,
        pszButtonText: button_text.as_ptr(),
    };
    let mut context = DialogContext {
        reference: plan.parent_rect,
        progress: Some(shared),
        ready: Some(ready.clone()),
        restart_args: plan.restart_args,
        last_text: String::new(),
        smoke: false,
        metrics: json!({}),
    };
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_CALLBACK_TIMER | TDF_SHOW_PROGRESS_BAR | TDF_SIZE_TO_CONTENT,
        pszWindowTitle: title.as_ptr(),
        pszMainInstruction: heading.as_ptr(),
        pszContent: content.as_ptr(),
        pButtons: &button,
        cButtons: 1,
        pfCallback: Some(callback),
        lpCallbackData: &mut context as *mut _ as isize,
        ..unsafe { std::mem::zeroed() }
    };
    let result = unsafe { TaskDialogIndirect(&config, null_mut(), null_mut(), null_mut()) };
    if result < 0 {
        return Err(format!("无法打开迁移进度窗口（{result}），目录未切换"));
    }
    let _ = worker.join();
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(ready);
    Ok(())
}

/// Only synthetic windows: no app process or installer is stopped.
pub fn smoke(root: &Path) -> Result<(), String> {
    if root.exists() {
        return Err("原生弹窗自检需要全新隔离目录".into());
    }
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut results = vec![];
    for (x, y) in [(100, 80), (720, 450)] {
        let title = wide("Synthetic installer owner");
        let class = wide("STATIC");
        let owner = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                560,
                440,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if owner.is_null() {
            return Err("无法创建合成安装窗口".into());
        }
        let result = installer_confirmation(owner as isize, true);
        unsafe {
            DestroyWindow(owner);
        }
        let (button, metrics) = result?;
        if button != IDNO
            || metrics["actual"][0] != metrics["expected"][0]
            || metrics["actual"][1] != metrics["expected"][1]
        {
            return Err("安装确认窗口未按安装窗口居中".into());
        }
        results.push(metrics);
    }
    fs::write(
        root.join("native-dialog-smoke.json"),
        json!({"ok":true,"positions":results}).to_string(),
    )
    .map_err(|e| e.to_string())
}
