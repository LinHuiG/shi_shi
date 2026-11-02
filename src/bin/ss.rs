//! Console entry point. Never opens or modifies the business database.
#[path = "../cli_transport.rs"]
mod cli_transport;
use cli_transport as wire;
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    fs,
    io::{IsTerminal, Read, Write},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT},
    System::Threading::*,
};

const HELP: &str = "拾事 CLI\n\nss backend start|status|stop [--force]\nss ui open|close [--force]\nss methods\nss tasks list [--params JSON]\nss tasks get <事项ID>\nss memory read\nss memory save --input <JSON文件>\nss call <页面业务方法> [--params JSON | --input <JSON文件或->]\nss job <操作编号>\n\n全局：--root <安装/隔离目录>，--version，--help\n普通命令自动启动并保持后台；backend stop 释放 CLI 常驻标识。\n输出为 UTF-8 JSON；--input - 从标准输入读取 JSON，避免命令行转义。\n长操作可用 call ai_job_start 提交，再用 job 查询，不重复提交。\n退出默认保留另一端，只有 --force 或明确回答 y 才同时关闭。";

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
    let raw = if let Some(path) = &o.input {
        if path == "-" {
            let mut raw = String::new();
            std::io::stdin()
                .take(wire::REQUEST_LIMIT + 1)
                .read_to_string(&mut raw)
                .map_err(|e| e.to_string())?;
            raw
        } else {
            let mut raw = String::new();
            fs::File::open(path)
                .and_then(|f| f.take(wire::REQUEST_LIMIT + 1).read_to_string(&mut raw))
                .map_err(|e| format!("无法读取输入文件：{e}"))?;
            raw
        }
    } else {
        o.params.clone().unwrap_or_else(|| "{}".into())
    };
    if raw.len() as u64 > wire::REQUEST_LIMIT {
        return Err("输入超过 8 MiB".into());
    }
    let p: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("输入不是有效 JSON：{e}"))?;
    if !p.is_object() {
        return Err("参数必须是 JSON 对象".into());
    }
    Ok(p)
}
struct Background(HANDLE);
impl Drop for Background {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl Background {
    fn try_wait(&self) -> Result<Option<u32>, String> {
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
        out.extend(std::iter::repeat_n(
            b'\\' as u16,
            if unit == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        out.push(unit);
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    out
}
fn background(
    binary: &std::path::Path,
    root: &std::path::Path,
    no_jobs: bool,
) -> Result<Background, String> {
    let args = if no_jobs {
        vec![
            binary.as_os_str(),
            OsStr::new("--background"),
            OsStr::new("--root"),
            root.as_os_str(),
            OsStr::new("--no-background-jobs"),
            OsStr::new("--no-auto-update"),
        ]
    } else {
        vec![
            binary.as_os_str(),
            OsStr::new("--background"),
            OsStr::new("--root"),
            root.as_os_str(),
        ]
    };
    let mut command = Vec::new();
    for arg in args {
        if !command.is_empty() {
            command.push(b' ' as u16);
        }
        command.extend(quoted(arg));
    }
    command.push(0);
    let executable = binary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // Do not inherit *any* handles. Redirecting only stdin/out/err still lets
    // other inheritable pipe handles keep an agent's captured CLI call open.
    for flags in [
        CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB,
        CREATE_NO_WINDOW,
    ] {
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
fn ensure(root: &std::path::Path, no_jobs: bool) -> Result<wire::Route, String> {
    if let Some(route) = wire::probe(root)? {
        return Ok(route);
    }
    let parent = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("无法定位主程序")?
        .to_path_buf();
    let binary = parent.join("shishi.exe");
    if !binary.is_file() {
        return Err("找不到同目录的 shishi.exe，请使用完整安装包".into());
    }
    wire::data_directory(root)?;
    let child = background(&binary, root, no_jobs)?;
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
                    return Err(format!(
                        "后台启动失败（{status}），请检查数据目录中的 startup-error.txt"
                    ));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("后台尚未就绪，未提交业务操作；请查看 startup-error.txt".into())
}
fn run(o: Options) -> Result<Value, String> {
    let w = o.words.iter().map(String::as_str).collect::<Vec<_>>();
    let root = std::path::absolute(match &o.root {
        Some(p) => p.clone(),
        None => std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .unwrap()
            .to_path_buf(),
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
        ["memory", "read"] => "memory_read",
        ["memory", "save"] => "memory_save",
        ["job", id] => {
            p["job_id"] = json!(id);
            "ai_job_result"
        }
        ["call", method] => method,
        _ => return Err("命令不完整或未知，请运行 ss --help".into()),
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
        let other = if method == "backend.stop" {
            "图形端"
        } else {
            "CLI"
        };
        eprintln!("已退出当前端，{other}仍在使用后台。");
        if !o.force && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            eprint!("是否强制关闭另一端及整个后台？[y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|e| e.to_string())?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                result = wire::success(wire::call(&route, method, json!({"force":true}))?)?;
            }
        }
    }
    Ok(result)
}
fn main() {
    let result = parse(std::env::args().skip(1)).and_then(|o| {
        if o.words.is_empty() || o.words == ["help"] {
            println!("{HELP}");
            return Ok(None);
        }
        if o.words == ["version"] {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        run(o).map(Some)
    });
    match result {
        Ok(Some(value)) => println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok":true,"data":value})).unwrap()
        ),
        Ok(None) => {}
        Err(error) => {
            println!("{}", json!({"ok":false,"error":error}));
            std::process::exit(1);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_file_and_inline_are_mutually_exclusive() {
        assert!(parse(["--input", "a.json", "--params", "{}"].map(str::to_owned)).is_err());
    }
    #[test]
    fn forced_stop_is_explicit_and_default_is_no() {
        let o = parse(["backend", "stop"].map(str::to_owned)).unwrap();
        assert!(!o.force);
        assert_eq!(o.words, ["backend", "stop"]);
    }
    #[test]
    fn accepts_global_options_after_subcommand() {
        let o =
            parse(["tasks", "get", "事项-a", "--root", r"C:\Synthetic Data"].map(str::to_owned))
                .unwrap();
        assert_eq!(o.words, ["tasks", "get", "事项-a"]);
        assert!(o.root.is_some());
    }
}
