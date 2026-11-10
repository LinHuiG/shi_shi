//! The console client (`shishi.exe <command>`) and the discovery it reads
//! from the owner. The client never opens the business database: it sends
//! each request to the running owner, which uses the page's dispatcher.
use crate::transport as wire;
use serde_json::{json, Value};
use std::{
    fs,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Methods the owner answers itself, before core and host routing.
pub const METHODS: [&str; 2] = ["cli_methods", "cli_skill"];

pub fn methods() -> Value {
    let names = shishi_core::methods()
        .into_iter()
        .chain(crate::HOST_METHODS)
        .chain(METHODS)
        .collect::<std::collections::BTreeSet<_>>();
    json!({"protocol_version":1,"methods":names,"gui_only":["pick"],"native_progress_window":["storage_change"],
        "examples":{
            "tasks_query":{"focus":"待处理","limit":20,"offset":0},
            "ai_accuracy":{"days":30},
            "task_get":{"id":"<task-id>"},
            "edit_create":{"method":"edit","params":{"task":{"title":"示例事项","status":"待处理"}}},
            "edit_update":{"method":"edit","params":{"task":{"id":"<task-id>","updated":"<从 task_get 读取的整数>","title":"更新标题"}}},
            "action":{"id":"<task-id>","action":"done"},
            "memory_save":{"kind":"temporary","expected":"<从 memory_read 读取的整数>","text":"内容"},
            "ai_job_start":{"method":"analyze","params":{}},
            "ai_job_result":{"job_id":"<job-id>"}},
        "notes":["所有变更复用图形界面的业务校验与保存；事项修改必须携带读取到的 updated，记忆保存必须携带 revision 作为 expected。","查询长操作的 job_id 不会重复提交。后台重启后查不到的操作，请核对当前数据和日志，不自动重发。","CLI 的文件输入使用 --input 或 --params；业务方法的参数与客户端 IPC 一致。"]})
}

/// The CLI skill followed by this program's path and `--root`.
pub fn skill(root: &Path) -> Value {
    let cli = std::env::current_exe().unwrap_or_else(|_| root.join("shishi.exe"));
    let path = cli.to_string_lossy().replace('\'', "''");
    let root = root.to_string_lossy().replace('\'', "''");
    json!({"text":format!("{}\n\n## 当前实例的 CLI\n\n以下路径和根目录对应当前打开的拾事。后续命令沿用此入口和 `--root`，将 `--help` 替换为业务命令。\n\n```powershell\n& '{}' --root '{}' --help\n```\n", include_str!("../../docs/skills/shishi-cli/SKILL.md"), path, root)})
}

const HELP: &str = "拾事 CLI\n\nshishi.exe backend start|status|stop [--force]\nshishi.exe ui open|close [--force]\nshishi.exe methods\nshishi.exe tasks list [--params JSON]\nshishi.exe tasks get <事项ID>\nshishi.exe handling brief <处置ID>\nshishi.exe memory read\nshishi.exe memory save --input <JSON文件>\nshishi.exe call <页面业务方法> [--params JSON | --input <JSON文件或->]\nshishi.exe job <操作编号>\n\n全局：--root <安装/隔离目录>，--version，--help\n普通命令自动启动并保持后台；backend stop 释放 CLI 常驻标识。\nhandling brief 输出 UTF-8 原文，其余命令输出 JSON；--input - 从标准输入读取 JSON，避免命令行转义。\n长操作可用 call ai_job_start 提交，再用 job 查询，不重复提交。\n退出默认保留另一端，只有 --force 或明确回答 y 才同时关闭。\n不带命令启动图形界面。";

/// Options of the desktop and helper modes that take a value.
const DESKTOP_VALUES: [&str; 5] = ["--root", "--ui-dir", "--analysis-submit-mcp", "--apply-update", "--data-transfer"];

/// A command word, `--help` or `--version` selects the console client;
/// the desktop and its helper modes take only options.
pub fn requested(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" | "--version" | "-V" => return true,
            "--input" | "--params" => return true,
            a if DESKTOP_VALUES.contains(&a) => {
                args.next();
            }
            a if !a.starts_with('-') => return true,
            _ => {}
        }
    }
    false
}

/// The program is a GUI-subsystem binary, so Windows gives it no console.
/// Write to the console of the shell that started it; redirected streams
/// (pipes, files) are inherited as they are and stay untouched.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{
            AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
    };
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for (id, device) in
            [(STD_INPUT_HANDLE, "CONIN$"), (STD_OUTPUT_HANDLE, "CONOUT$"), (STD_ERROR_HANDLE, "CONOUT$")]
        {
            let current = GetStdHandle(id);
            if !current.is_null() && current != INVALID_HANDLE_VALUE {
                continue;
            }
            let name = device.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let handle = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if handle != INVALID_HANDLE_VALUE {
                SetStdHandle(id, handle);
            }
        }
    }
}

#[derive(Default, Debug)]
struct Options {
    root: Option<PathBuf>,
    input: Option<String>,
    params: Option<String>,
    force: bool,
    no_jobs: bool,
    words: Vec<String>,
}
fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut o = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => o.root = Some(PathBuf::from(args.next().ok_or("--root 缺少路径")?)),
            "--input" => o.input = Some(args.next().ok_or("--input 缺少文件路径或 -")?),
            "--params" => o.params = Some(args.next().ok_or("--params 缺少 JSON")?),
            "--force" => o.force = true,
            "--no-background-jobs" => o.no_jobs = true,
            "--help" | "-h" => o.words = vec!["help".into()],
            "--version" | "-V" => o.words = vec!["version".into()],
            flag if flag.starts_with('-') => return Err(format!("未知选项：{flag}")),
            _ => o.words.push(arg),
        }
    }
    if o.input.is_some() && o.params.is_some() {
        return Err("--input 和 --params 只能使用一个".into());
    }
    if o.no_jobs && o.root.is_none() {
        return Err("隔离验证选项必须同时指定 --root".into());
    }
    Ok(o)
}
fn payload(o: &Options) -> Result<Value, String> {
    let raw = match o.input.as_deref() {
        Some(path) => {
            let input: Box<dyn Read> = match path {
                "-" => Box::new(std::io::stdin()),
                path => Box::new(fs::File::open(path).map_err(|e| format!("无法读取输入文件：{e}"))?),
            };
            let mut raw = String::new();
            input.take(wire::REQUEST_LIMIT + 1).read_to_string(&mut raw).map_err(|e| format!("无法读取输入：{e}"))?;
            raw
        }
        None => o.params.clone().unwrap_or_else(|| "{}".into()),
    };
    if raw.len() as u64 > wire::REQUEST_LIMIT {
        return Err("输入超过 8 MiB".into());
    }
    let p: Value =
        serde_json::from_str(raw.trim_start_matches('\u{feff}')).map_err(|e| format!("输入不是有效 JSON：{e}"))?;
    if !p.is_object() {
        return Err("参数必须是 JSON 对象".into());
    }
    Ok(p)
}
#[cfg(windows)]
mod spawn {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT},
        System::Threading::*,
    };
    pub struct Background(HANDLE);
    impl Drop for Background {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    impl Background {
        pub fn try_wait(&mut self) -> Result<Option<u32>, String> {
            unsafe {
                if WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT {
                    return Ok(None);
                }
                let mut code = 0;
                if GetExitCodeProcess(self.0, &mut code) == 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok(Some(code))
            }
        }
    }
    fn quoted(value: &OsStr) -> Vec<u16> {
        let mut out = vec![b'"' as u16];
        let mut slashes = 0;
        for unit in value.encode_wide() {
            if unit == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            out.extend(std::iter::repeat_n(b'\\' as u16, if unit == b'"' as u16 { slashes * 2 + 1 } else { slashes }));
            slashes = 0;
            out.push(unit);
        }
        out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        out.push(b'"' as u16);
        out
    }
    pub fn background(binary: &Path, root: &Path, no_jobs: bool) -> Result<Background, String> {
        let mut args = vec![binary.as_os_str(), OsStr::new("--background"), OsStr::new("--root"), root.as_os_str()];
        if no_jobs {
            args.extend([OsStr::new("--no-background-jobs"), OsStr::new("--no-auto-update")]);
        }
        let mut command = Vec::new();
        for arg in args {
            if !command.is_empty() {
                command.push(b' ' as u16);
            }
            command.extend(quoted(arg));
        }
        command.push(0);
        let executable = binary.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
        // Do not inherit *any* handles. Redirecting only stdin/out/err still lets
        // other inheritable pipe handles keep an agent's captured CLI call open.
        for flags in [CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW] {
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let mut line = command.clone();
            let started = unsafe {
                CreateProcessW(
                    executable.as_ptr(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    flags,
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup,
                    &mut info,
                )
            };
            if started != 0 {
                unsafe {
                    CloseHandle(info.hThread);
                }
                return Ok(Background(info.hProcess));
            }
            let error = std::io::Error::last_os_error();
            if flags == CREATE_NO_WINDOW || error.raw_os_error() != Some(5) {
                return Err(format!("后台启动失败：{error}"));
            }
        }
        unreachable!()
    }
}

#[cfg(not(windows))]
mod spawn {
    use std::{
        path::Path,
        process::{Child, Command, Stdio},
    };
    /// Non-Windows: the headless owner, detached from our standard streams.
    pub struct Background(Child);
    impl Background {
        pub fn try_wait(&mut self) -> Result<Option<u32>, String> {
            Ok(self.0.try_wait().map_err(|e| e.to_string())?.map(|s| s.code().unwrap_or(1) as u32))
        }
    }
    pub fn background(binary: &Path, root: &Path, no_jobs: bool) -> Result<Background, String> {
        let mut command = Command::new(binary);
        command.arg("--background").arg("--root").arg(root);
        if no_jobs {
            command.args(["--no-background-jobs", "--no-auto-update"]);
        }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        command.spawn().map(Background).map_err(|e| format!("后台启动失败：{e}"))
    }
}

fn ensure(root: &Path, no_jobs: bool) -> Result<wire::Route, String> {
    if let Some(route) = wire::probe(root)? {
        return Ok(route);
    }
    let binary = std::env::current_exe().map_err(|e| e.to_string())?;
    let data = wire::data_directory(root)?;
    let mut child = spawn::background(&binary, root, no_jobs)?;
    let deadline = Instant::now() + Duration::from_secs(65);
    let mut failed_at = None;
    while Instant::now() < deadline {
        if let Some(route) = wire::probe(root)? {
            return Ok(route);
        }
        if let Some(status) = child.try_wait()? {
            if status != 0 {
                // A simultaneous GUI/CLI launch can lose the startup lock
                // while the winning owner is still publishing its endpoint.
                let at = failed_at.get_or_insert_with(Instant::now);
                if at.elapsed() > Duration::from_secs(10) {
                    let record = std::fs::read_to_string(data.join(crate::owner::STARTUP_ERROR));
                    return Err(format!("后台启动失败（{status}）：{}", record.unwrap_or_default().trim()));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("后台尚未就绪，未提交业务操作".into())
}
fn run(o: Options) -> Result<Value, String> {
    let w = o.words.iter().map(String::as_str).collect::<Vec<_>>();
    let root = std::path::absolute(match &o.root {
        Some(p) => p.clone(),
        None => std::env::current_exe().map_err(|e| e.to_string())?.parent().unwrap().to_path_buf(),
    })
    .map_err(|e| e.to_string())?;
    let mut p = payload(&o)?;
    let method = match w.as_slice() {
        ["backend", "status"] => {
            return match wire::probe(&root)? {
                Some(r) => wire::success(wire::call(&r, "backend.status", json!({}))?),
                None => Ok(json!({"running":false,"gui":false,"cli":false})),
            };
        }
        ["backend", "start"] => "backend.start",
        ["backend", "stop"] => "backend.stop",
        ["ui", "open"] => "ui.open",
        ["ui", "close"] => "ui.close",
        ["methods"] => "cli_methods",
        ["tasks", "list"] => "tasks_query",
        ["tasks", "get", id] => {
            p["id"] = json!(id);
            "task_get"
        }
        ["handling", "brief", id] => {
            p["run_id"] = json!(id);
            p["thread_id"] = json!(std::env::var("CODEX_THREAD_ID").ok());
            "handling_brief"
        }
        ["memory", "read"] => "memory_read",
        ["memory", "save"] => "memory_save",
        ["job", id] => {
            p["job_id"] = json!(id);
            "ai_job_result"
        }
        ["call", method] => method,
        _ => return Err("命令不完整或未知，请运行 shishi.exe --help".into()),
    };
    let stopping = method == "backend.stop" || method == "ui.close";
    let route = if stopping {
        match wire::probe(&root)? {
            Some(r) => r,
            None => return Ok(json!({"running":false,"gui":false,"cli":false})),
        }
    } else {
        ensure(&root, o.no_jobs)?
    };
    if stopping {
        p["force"] = json!(o.force);
    }
    let mut result = wire::success(wire::call(&route, method, p)?)?;
    if stopping && result["closing"] != true {
        let other = if method == "backend.stop" { "图形端" } else { "CLI" };
        eprintln!("已退出当前端，{other}仍在使用后台。");
        if !o.force && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            eprint!("是否强制关闭另一端及整个后台？[y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                result = wire::success(wire::call(&route, method, json!({"force":true}))?)?;
            }
        }
    }
    Ok(result)
}
/// Run one console command; returns the process exit code.
pub fn main(args: Vec<String>) -> i32 {
    #[cfg(windows)]
    attach_console();
    let mut brief = false;
    let result = parse(args).and_then(|o| {
        if o.words.is_empty() || o.words == ["help"] {
            println!("{HELP}");
            return Ok(None);
        }
        if o.words == ["version"] {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        brief = o.words[0] == "handling";
        run(o).map(Some)
    });
    match result {
        Ok(Some(value)) if brief => println!("{}", value.as_str().unwrap()),
        Ok(Some(value)) => println!("{}", serde_json::to_string_pretty(&json!({"ok":true,"data":value})).unwrap()),
        Ok(None) => {}
        Err(error) => {
            println!("{}", json!({"ok":false,"error":error}));
            return 1;
        }
    }
    0
}
#[cfg(test)]
mod tests;
