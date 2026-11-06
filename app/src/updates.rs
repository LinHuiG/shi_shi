//! Verified update downloads, the 4:00 automatic install, the 9:00 reminder and
//! the detached install helper. The view lives in `ctx.flags` (`state.updates`);
//! the window loop wakes this module only at its next deadline.
//! Network access starts only from a due check or an explicit page request.
#![cfg_attr(not(windows), allow(dead_code))]
use crate::storage::write_json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shishi_api::http_client::{self, HttpRequest};
use shishi_core::time::{now, DAY};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Mutex},
    time::{Duration, SystemTime},
};

const BASE: &str = "https://github.com/LinHuiG/shi_shi/releases/download/";
const LATEST: &str = "https://api.github.com/repos/LinHuiG/shi_shi/releases/latest";
const MAX_SIZE: u64 = 64 * 1024 * 1024;
/// How late a wake may observe a boundary and still act on it.
const CLOCK_GAP_MS: i64 = 10_000;
const ACTION_WINDOW_MS: i64 = 60_000;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const BUSY: [&str; 3] = ["checking", "downloading", "installing"];
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version(text: &str) -> Option<[u64; 3]> {
    let parts = text.strip_prefix('v').unwrap_or(text).split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) || p.len() > 1 && p.starts_with('0')
        })
    {
        return None;
    }
    Some([parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?])
}

/// Releases published before this binary was built never replace it.
fn build_time() -> i64 {
    env!("SHISHI_BUILD_TIME").parse::<i64>().unwrap_or(0) * 1000
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Package {
    version: String,
    tag: String,
    url: String,
    size: u64,
    digest: String,
    published: i64,
}

impl Package {
    fn valid(&self) -> bool {
        version(&self.version).is_some()
            && version(&self.tag) == version(&self.version)
            && self.digest.len() == 64
            && self.digest.bytes().all(|b| b.is_ascii_hexdigit())
            && self.size > 0
            && self.size <= MAX_SIZE
            && self.url == format!("{BASE}{}/Shishi-Setup-{}.exe", self.tag, self.version)
    }

    fn newer(&self, current: &str, built: i64) -> bool {
        self.valid() && version(&self.version) > version(current) && self.published > built
    }

    fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("Shishi-Setup-{}-{}.exe", self.version, &self.digest[..12]))
    }
}

fn release(value: &Value, current: &str, built: i64) -> Result<Option<Package>, String> {
    if value["draft"] != false || value["prerelease"] != false {
        return Ok(None);
    }
    let tag = value["tag_name"].as_str().ok_or("版本信息缺少版本号")?;
    let v = version(tag).ok_or("发布版本号无效")?;
    if Some(v) <= version(current) {
        return Ok(None);
    }
    let published = chrono::DateTime::parse_from_rfc3339(value["published_at"].as_str().unwrap_or(""))
        .map_err(|_| "发布时间无效")?
        .timestamp_millis();
    if published <= built {
        return Ok(None);
    }
    let normalized = format!("{}.{}.{}", v[0], v[1], v[2]);
    let name = format!("Shishi-Setup-{normalized}.exe");
    let asset = value["assets"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == name && r["state"] == "uploaded"))
        .ok_or("新版本尚未提供 Windows 安装包")?;
    let package = Package {
        version: normalized,
        tag: tag.into(),
        url: asset["browser_download_url"].as_str().unwrap_or("").into(),
        size: asset["size"].as_u64().unwrap_or(0),
        digest: asset["digest"].as_str().unwrap_or("").strip_prefix("sha256:").unwrap_or("").to_ascii_lowercase(),
        published,
    };
    if !package.valid() {
        return Err("安装包来源、大小或 SHA-256 信息无效，未下载".into());
    }
    Ok(Some(package))
}

#[derive(Clone, Serialize)]
struct Scheduled {
    id: String,
    expires_at: i64,
    #[serde(skip)]
    digest: String,
}

/// What a scheduler step found due.
#[derive(Debug, Default, PartialEq)]
pub struct Due {
    /// 9:00: show the window with the reminder.
    pub remind: bool,
    /// 4:00: install the verified cache now.
    pub install: bool,
}

#[derive(Default, Serialize)]
struct View {
    status: String,
    version: String,
    progress: u8,
    error: String,
    checked_at: i64,
    current_until: i64,
    retry_at: i64,
    #[serde(skip)]
    next_check_at: i64,
    reminder: Option<Scheduled>,
    #[serde(skip)]
    package: Option<Package>,
}

impl View {
    /// Present a verified package as ready; without one the status stays.
    fn show(&mut self, package: Option<Package>) {
        if let Some(p) = &package {
            self.status = "ready".into();
            self.version = p.version.clone();
            self.progress = 100;
        }
        self.package = package;
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Schedule {
    reminder_day: i64,
    install_day: i64,
}

/// The UTC+8 `hour` boundary crossed since `previous`, observed live: a
/// startup, suspend or clock jump that lands late never replays it, and the
/// persisted day claim stops a rewind or restart from repeating it.
fn crossed(previous: Option<i64>, at: i64, hour: i64) -> Option<(i64, i64)> {
    let previous = previous?;
    let day = (at + 8 * 3_600_000).div_euclid(DAY);
    let boundary = day * DAY - 8 * 3_600_000 + hour * 3_600_000;
    (at > previous && previous < boundary && boundary <= at && at - boundary <= CLOCK_GAP_MS).then_some((day, boundary))
}

/// The next `hour` boundary strictly after `at`.
fn next_boundary(at: i64, hour: i64) -> i64 {
    let day = (at + 8 * 3_600_000).div_euclid(DAY);
    let today = day * DAY - 8 * 3_600_000 + hour * 3_600_000;
    if today > at {
        today
    } else {
        today + DAY
    }
}

fn check_interval(sample: u64) -> i64 {
    (60 + (sample % 61) as i64) * 60_000
}

fn check_due(view: &View, automatic: bool, at: i64) -> bool {
    !BUSY.contains(&view.status.as_str())
        && !(view.status == "current" && at < view.current_until)
        && at >= view.retry_at
        && (!automatic || (view.reminder.is_none() && at >= view.next_check_at))
}

fn sha256(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    io::copy(&mut fs::File::open(path).map_err(|e| e.to_string())?, &mut hash).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hash.finalize()))
}

fn verify(path: &Path, p: &Package) -> Result<(), String> {
    let size = fs::metadata(path).map_err(|_| "文件不可用")?.len();
    if size != p.size || sha256(path)? != p.digest {
        return Err("安装包校验不一致".into());
    }
    Ok(())
}

/// The cached package recorded by the last download, when still installable.
fn ready_package(dir: &Path) -> Option<Package> {
    let package: Package = serde_json::from_slice(&fs::read(dir.join("ready.json")).ok()?).ok()?;
    (package.newer(VERSION, build_time()) && verify(&package.path(dir), &package).is_ok()).then_some(package)
}

/// Remove update-owned files (installers, partial downloads, helper copies)
/// untouched for a day, except `keep`. A running helper's files stay locked.
fn prune(dir: &Path, keep: Option<&Path>, at: SystemTime) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let (path, name) = (entry.path(), entry.file_name().to_string_lossy().into_owned());
        let owned = name.starts_with("Shishi-Setup-") && name.ends_with(".exe")
            || name.starts_with("download-") && name.ends_with(".part");
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| at.duration_since(m).ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if keep == Some(path.as_path()) || !stale {
            continue;
        }
        if owned {
            let _ = fs::remove_file(path);
        } else if name.starts_with("apply-") && path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

type Publish = Box<dyn Fn(Value) + Send + Sync>;

pub struct Updates {
    dir: PathBuf,
    view: Mutex<View>,
    schedule: Mutex<Schedule>,
    publish: Publish,
    /// Last scheduler observation (epoch ms).
    previous: Mutex<Option<i64>>,
    /// An install or data transfer is replacing this process.
    pub restarting: AtomicBool,
}

impl Updates {
    /// `dir` is `data/updates`; `publish` receives every view change.
    pub fn load(dir: PathBuf, publish: impl Fn(Value) + Send + Sync + 'static) -> Self {
        let package = ready_package(&dir);
        let mut view =
            View { status: "idle".into(), checked_at: if package.is_some() { now() } else { 0 }, ..Default::default() };
        view.show(package);
        prune(&dir, view.package.as_ref().map(|p| p.path(&dir)).as_deref(), SystemTime::now());
        let schedule =
            fs::read(dir.join("schedule.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let updates = Self {
            dir,
            view: Mutex::new(view),
            schedule: Mutex::new(schedule),
            publish: Box::new(publish),
            previous: Mutex::new(Some(now())),
            restarting: AtomicBool::new(false),
        };
        updates.change(|_| {});
        updates
    }

    pub fn snapshot(&self) -> Value {
        json!(*self.view.lock().unwrap())
    }

    fn change(&self, f: impl FnOnce(&mut View)) {
        let mut view = self.view.lock().unwrap();
        f(&mut view);
        (self.publish)(json!(*view));
    }

    fn finish_current(&self, at: i64) {
        self.change(|v| {
            v.current_until = if v.package.is_some() { 0 } else { at.saturating_add(30_000) };
            v.retry_at = 0;
            let package = v.package.take();
            v.show(package);
            if v.package.is_none() {
                v.status = "current".into();
            }
        });
    }

    /// A failed refresh keeps a previously verified local installer.
    pub fn fail(&self, error: String) {
        self.change(|v| {
            v.status = "failed".into();
            let package = v.package.take();
            v.show(package);
            v.error = error;
        });
    }

    fn claim(&self, day: i64, hour: i64) -> Result<bool, String> {
        let mut saved = self.schedule.lock().unwrap();
        let mut next = saved.clone();
        let field = if hour == 9 { &mut next.reminder_day } else { &mut next.install_day };
        if *field >= day {
            return Ok(false);
        }
        *field = day;
        fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        write_json(&self.dir.join("schedule.json"), &next)?;
        *saved = next;
        Ok(true)
    }

    fn clear_expired(&self, at: i64, enabled: bool) {
        let expired = |v: &Option<Scheduled>| {
            v.as_ref().is_some_and(|v| !enabled || at >= v.expires_at || at < v.expires_at - ACTION_WINDOW_MS)
        };
        if expired(&self.view.lock().unwrap().reminder) {
            self.change(|view| view.reminder = None);
        }
    }

    /// Act on 4:00/9:00 boundaries crossed since the last call. Only the
    /// verified local cache is offered, never a network result.
    fn schedule_tick(&self, previous: Option<i64>, at: i64, enabled: bool) -> Result<Due, String> {
        self.clear_expired(at, enabled);
        let mut due = Due::default();
        if !enabled || self.restarting.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(due);
        }
        for hour in [4, 9] {
            let Some((day, boundary)) = crossed(previous, at, hour) else { continue };
            // Claim even when nothing is cached, so a later download waits.
            if !self.claim(day, hour)? {
                continue;
            }
            let Some(package) = ready_package(&self.dir) else { continue };
            if BUSY.contains(&self.view.lock().unwrap().status.as_str()) {
                continue;
            }
            self.change(|view| {
                view.error.clear();
                if hour == 9 {
                    view.reminder = Some(Scheduled {
                        id: format!("{day}:{hour}:{}", package.digest),
                        expires_at: boundary + ACTION_WINDOW_MS,
                        digest: package.digest.clone(),
                    });
                }
                view.show(Some(package));
            });
            if hour == 9 {
                due.remind = true;
            } else {
                due.install = true;
            }
        }
        Ok(due)
    }

    /// Epoch ms of the next moment `tick` has work: a boundary, a due
    /// automatic check or a reminder expiry.
    pub fn next_wake(&self, at: i64, enabled: bool) -> i64 {
        // Keep a crossed boundary due until tick observes it.
        let previous = self.previous.lock().unwrap().unwrap_or(at).min(at);
        let view = self.view.lock().unwrap();
        let mut next = next_boundary(previous, 4).min(next_boundary(previous, 9));
        if let Some(reminder) = &view.reminder {
            next = next.min(reminder.expires_at);
        }
        if enabled && !BUSY.contains(&view.status.as_str()) {
            let mut due = view.next_check_at.max(view.retry_at);
            if view.status == "current" {
                due = due.max(view.current_until);
            }
            if view.reminder.is_none() {
                next = next.min(due);
            }
        }
        next
    }

    /// The scheduler step, run by the window loop at `next_wake`. A due
    /// install is the caller's to start (`install`), since it ends the app.
    pub fn tick(self: &std::sync::Arc<Self>, enabled: bool) -> Due {
        let at = now();
        let previous = self.previous.lock().unwrap().replace(at);
        let due = self.schedule_tick(previous, at, enabled).unwrap_or_else(|error| {
            self.fail(format!("定时更新检查失败：{error}"));
            Due::default()
        });
        if enabled && !due.install {
            let _ = self.check(true, enabled);
        }
        due
    }

    /// Start a check unless one is running or not yet due; returns the view.
    pub fn check(self: &std::sync::Arc<Self>, automatic: bool, enabled: bool) -> Result<Value, String> {
        if self.restarting.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("正在重启拾事，请稍后再检查更新".into());
        }
        if automatic && !enabled {
            return Ok(self.snapshot());
        }
        let mut due = false;
        self.change(|view| {
            let at = now();
            if !check_due(view, automatic, at) {
                return;
            }
            due = true;
            view.status = "checking".into();
            view.error.clear();
            view.progress = 0;
            view.checked_at = at;
            view.next_check_at = at.saturating_add(check_interval(uuid::Uuid::new_v4().as_u128() as u64));
            view.current_until = 0;
            view.retry_at = 0;
        });
        if due {
            #[cfg(windows)]
            {
                let updates = self.clone();
                std::thread::Builder::new()
                    .name("shishi-update-check".into())
                    .spawn(move || {
                        if let Err(error) = updates.fetch() {
                            updates.fail(error);
                        }
                    })
                    .map_err(|e| e.to_string())?;
            }
            #[cfg(not(windows))]
            self.fail("此平台不支持自动更新".into());
        }
        Ok(self.snapshot())
    }
}

/// Release check and download; `check` runs them on Windows only.
impl Updates {
    /// The latest release, or `None` when the repository has none yet.
    fn latest(&self, url: &str) -> Result<Option<Value>, String> {
        let headers = [("Accept", "application/vnd.github+json")];
        let request = HttpRequest { headers: &headers, ..HttpRequest::get(url, Duration::from_secs(30), 1 << 20) };
        let response = http_client::send(&request, None).map_err(|_| "更新检查失败，请检查网络后重试")?;
        match response.status {
            200..=299 => serde_json::from_slice(&response.body).map(Some).map_err(|_| "版本信息格式无效".into()),
            404 => Ok(None),
            status @ (403 | 429) => {
                self.change(|v| v.retry_at = now().saturating_add(60_000));
                Err(format!("GitHub 请求次数已达上限（HTTP {status}），请稍后重试"))
            }
            status => Err(format!("GitHub 更新服务暂时不可用（HTTP {status}），请稍后重试")),
        }
    }

    /// Stream the installer into `dest`, following HTTPS redirects.
    fn download(&self, package: &Package, dest: &Path) -> Result<(), String> {
        let request = HttpRequest {
            cancel: Some(&self.restarting),
            ..HttpRequest::get(&package.url, Duration::from_secs(600), package.size as usize)
        };
        let mut shown = 0;
        let status = http_client::download(&request, dest, &mut |written| {
            let percent = (written * 100 / package.size).min(99) as u8;
            if percent != shown {
                shown = percent;
                self.change(|v| v.progress = percent);
            }
        })
        .map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        verify(dest, package)
    }

    fn fetch(&self) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let package = match self.latest(LATEST)? {
            Some(value) => release(&value, VERSION, build_time())?,
            None => None,
        };
        let Some(package) = package else {
            self.finish_current(now());
            return Ok(());
        };
        let file = package.path(&self.dir);
        self.change(|v| {
            v.status = "downloading".into();
            v.version = package.version.clone();
        });
        if verify(&file, &package).is_err() {
            let partial = self.dir.join(format!("download-{}.part", uuid::Uuid::new_v4()));
            if let Err(e) = self.download(&package, &partial) {
                let _ = fs::remove_file(&partial);
                return Err(format!("更新下载或校验失败，请重新检查：{e}"));
            }
            fs::rename(partial, &file).map_err(|e| e.to_string())?;
        }
        write_json(&self.dir.join("ready.json"), &package)?;
        prune(&self.dir, Some(&file), SystemTime::now());
        self.change(|v| {
            if v.reminder.as_ref().is_some_and(|r| r.digest != package.digest) {
                v.reminder = None;
            }
            v.show(Some(package));
        });
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{os::windows::process::CommandExt, process::Command, time::Instant};

    const NO_WINDOW: u32 = 0x0800_0000;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        parent: u32,
        data: PathBuf,
        executable: PathBuf,
        package: Package,
        installer: PathBuf,
        args: Vec<String>,
    }

    impl Updates {
        /// Hand the verified installer to a copied helper; the caller exits
        /// once this returns Ok (the service signals `Handoff`).
        pub fn install(&self, args: Vec<String>) -> Result<Value, String> {
            use std::sync::atomic::Ordering;
            if self.restarting.swap(true, Ordering::SeqCst) {
                return Err("已有重启操作正在执行".into());
            }
            let result = (|| {
                let package = {
                    let view = self.view.lock().unwrap();
                    if view.status != "ready" {
                        return Err("没有已准备好的更新，请先检查更新".into());
                    }
                    view.package.clone().ok_or("没有已下载的更新")?
                };
                let installer = package.path(&self.dir);
                verify(&installer, &package)?;
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                let install_dir = executable.parent().ok_or("安装目录无效")?;
                if !fs::read_to_string(install_dir.join("install-info.txt")).unwrap_or_default().starts_with("Shishi ")
                {
                    return Err("当前是开发运行目录，请使用安装包安装后再执行自动升级".into());
                }
                let helper_dir = self.dir.join(format!("apply-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&helper_dir).map_err(|e| e.to_string())?;
                let helper = helper_dir.join("shishi-update.exe");
                fs::copy(&executable, &helper).map_err(|e| e.to_string())?;
                fs::copy(install_dir.join("WebView2Loader.dll"), helper_dir.join("WebView2Loader.dll"))
                    .map_err(|e| e.to_string())?;
                let plan = Plan {
                    parent: std::process::id(),
                    data: self.dir.parent().ok_or("数据目录无效")?.to_path_buf(),
                    executable,
                    package,
                    installer,
                    args,
                };
                let plan_path = helper_dir.join("plan.json");
                write_json(&plan_path, &plan)?;
                crate::owner::hand_off(&helper, "--apply-update", &plan_path, Duration::from_secs(10))
                    .map_err(|e| format!("更新助手{e}，应用保持运行"))?;
                self.change(|v| v.status = "installing".into());
                Ok(json!(true))
            })();
            if result.is_err() {
                self.restarting.store(false, Ordering::SeqCst);
            }
            result
        }
    }

    /// `--apply-update <plan>`: wait for the parent, run the installer
    /// silently, verify the installed version and start the app again.
    pub fn helper(path: &Path) -> Result<(), String> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0},
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        let p: Plan =
            serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|_| "更新计划无效")?;
        if !p.package.valid()
            || p.executable.file_name().and_then(|s| s.to_str()) != Some("shishi.exe")
            || p.installer != p.package.path(&p.data.join("updates"))
        {
            return Err("更新路径无效".into());
        }
        verify(&p.installer, &p.package)?;
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, p.parent) };
        if process.is_null() {
            return Err("原应用不可用，未开始安装".into());
        }
        fs::write(path.with_extension("ready"), b"ready").map_err(|e| e.to_string())?;
        let waited = unsafe { WaitForSingleObject(process, 60_000) };
        unsafe { CloseHandle(process) };
        if waited != WAIT_OBJECT_0 {
            return Err("原应用尚未退出，未开始安装".into());
        }
        let install_dir = p.executable.parent().ok_or("安装目录无效")?;
        let result = (|| {
            verify(&p.installer, &p.package)?;
            let mut child = Command::new(&p.installer)
                .args(["/S", "/UPDATE"])
                .raw_arg(format!("/D={}", install_dir.display()))
                .creation_flags(NO_WINDOW)
                .spawn()
                .map_err(|e| e.to_string())?;
            let began = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    break status;
                }
                if began.elapsed() > Duration::from_secs(180) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("更新安装超时，请手动检查安装结果".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let installed = fs::read_to_string(install_dir.join("install-info.txt")).unwrap_or_default();
            let expected = format!("Shishi {}", p.package.version);
            if !status.success() || installed.lines().next() != Some(expected.as_str()) {
                return Err(format!(
                    "安装未成功完成：安装器退出码 {}，目标版本 {}，当前安装标记 {}。保留原数据，请手动重试安装包。",
                    status.code().map_or_else(|| "未知".into(), |n| n.to_string()),
                    p.package.version,
                    installed.lines().next().unwrap_or("未找到")
                ));
            }
            let _ = fs::remove_file(p.data.join("updates/ready.json"));
            Ok(())
        })();
        crate::owner::launch(&p.executable, &p.args)?;
        result
    }
}

#[cfg(windows)]
pub use native::helper;

#[cfg(test)]
mod tests;
