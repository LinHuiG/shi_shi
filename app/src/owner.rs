//! The process that owns one data directory: lock, core runtime and the
//! loopback service. A second launch forwards to it and exits.
use crate::{
    args::Args,
    host::AppHost,
    service::{self, Owners, Service, Signal},
    transport as wire,
};
use serde_json::{json, Value};
use shishi_core::{Ctx, Runtime};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Owner {
    pub args: Args,
    /// Installation or isolated test root.
    pub root: PathBuf,
    /// Data directory (`ctx.root`).
    pub data: PathBuf,
    pub host: Arc<AppHost>,
    pub ctx: Arc<Ctx>,
    pub rt: Runtime,
    pub owners: Arc<Owners>,
    _lock: fs::File,
}

/// `--root`, or the program directory, as an absolute path.
pub fn root(args: &Args) -> Result<PathBuf, String> {
    let root = match &args.root {
        Some(root) => root.clone(),
        None => std::env::current_exe().map_err(|e| e.to_string())?.parent().ok_or("无法定位程序目录")?.to_path_buf(),
    };
    std::path::absolute(root).map_err(|e| e.to_string())
}

/// The last failed start, in the data directory.
pub const STARTUP_ERROR: &str = "startup-error.txt";

/// Become the owner of `data` (resolved from `root`), or hand the launch to
/// the running one (`Ok(None)`).
pub fn start(args: Args, root: PathBuf, data: PathBuf) -> Result<Option<Arc<Owner>>, String> {
    fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let lock = crate::storage::lock_file(&data)?;
    // A restarting owner may still hold the lock for a moment; the running
    // one may still be publishing its route. Only idempotent ownership
    // requests are forwarded, never business calls.
    let method = if args.background { "backend.start" } else { "ui.open" };
    let deadline = Instant::now() + Duration::from_secs(20);
    while lock.try_lock().is_err() {
        if let Ok(Some(route)) = wire::probe(&root) {
            if wire::call(&route, method, json!({})).and_then(wire::success).is_ok() {
                return Ok(None);
            }
        }
        #[cfg(windows)]
        if !args.background && crate::win::tray::show_existing(&root) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(if args.background {
                "已有后台尚未就绪或正在退出，请稍后重试".into()
            } else {
                "拾事已经在运行，请从托盘打开已有窗口。".into()
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // A data transfer may have committed while we waited for the old lock.
    if wire::data_directory(&root)? != data {
        return Err("数据目录刚刚变更，请重新打开拾事".into());
    }
    let _ = fs::remove_file(data.join(STARTUP_ERROR));
    let host = AppHost::new(root.clone(), data.clone());
    let ctx = Ctx::open(data.clone(), host.clone()).map_err(|e| e.0)?;
    let rt = Runtime::start(ctx.clone(), !args.no_jobs).map_err(|e| e.0)?;
    let owners = Arc::new(Owners::default());
    owners.acquire(!args.background)?;
    if args.cli_owner {
        owners.acquire(false)?;
    }
    Ok(Some(Arc::new(Owner { args, root, data, host, ctx, rt, owners, _lock: lock })))
}

impl Owner {
    /// Host methods the core does not own; everything else is core's.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, String> {
        self.route(method, params, false)
    }

    /// `cli`: a console request, which notifies the tasks it changes.
    fn route(self: &Arc<Self>, method: &str, params: Value, cli: bool) -> Result<Value, String> {
        match method {
            "cli_methods" => return Ok(crate::cli::methods()),
            "cli_skill" => return Ok(crate::cli::skill(&self.root)),
            _ => {}
        }
        #[cfg(windows)]
        if let Some(result) = crate::win::handle(self, method, &params) {
            return result;
        }
        #[cfg(not(windows))]
        if crate::HOST_METHODS.contains(&method) {
            return Err("此平台不支持此操作".into());
        }
        match cli {
            true => self.rt.call_cli(method, params),
            false => self.rt.call(method, params),
        }
        .map_err(|e| e.0)
    }

    /// Publish the loopback route. `signal` runs after each reply that
    /// changes the lifecycle.
    pub fn serve(self: &Arc<Self>, signal: impl Fn(Signal) + Send + Sync + 'static) -> Result<Service, String> {
        let owner = self.clone();
        service::start(
            self.data.clone(),
            self.owners.clone(),
            Arc::new(move |method: &str, params| owner.route(method, params, true)),
            Arc::new(signal),
        )
    }

    /// Stop new work, interrupt AI, end every child process tree and give
    /// running requests up to 30 s to leave.
    pub fn shutdown(&self) {
        self.owners.close();
        self.ctx.exec.interrupt(None);
        shishi_core::process::shutdown();
        let _ = shishi_core::Host::plugin_reset(&*self.host);
        self.owners.wait_idle(Duration::from_secs(30));
        // Leave one self-contained database file for copies and backups.
        let _ = self.ctx.db.checkpoint_blocking();
    }

    /// Start this program again with the same isolation and owners. The
    /// new process waits for our lock, so call it right before exiting.
    pub fn relaunch(&self) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        launch(&exe, &self.args.restart(self.owners.gui(), self.owners.cli()))
    }
}

/// Start `program` without a console window; it outlives this process.
pub fn launch(program: &Path, args: &[String]) -> Result<(), String> {
    spawn(program, args).map(drop).map_err(|e| format!("自动启动失败，请重新打开拾事：{e}"))
}

fn spawn(program: &Path, args: &[impl AsRef<std::ffi::OsStr>]) -> std::io::Result<std::process::Child> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
    command.spawn()
}

/// Start a replacing helper (`program flag plan`) and wait until it has
/// taken the plan, marked by `plan.ready`. On Ok the caller exits without
/// relaunching (`Signal::Handoff`).
pub fn hand_off(program: &Path, flag: &str, plan: &Path, timeout: Duration) -> Result<(), String> {
    let mut child = spawn(program, &[flag.as_ref(), plan.as_os_str()]).map_err(|e| format!("无法启动：{e}"))?;
    let (ready, started) = (plan.with_extension("ready"), Instant::now());
    while !ready.is_file() {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("启动失败（{status}）"));
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("启动超时".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
