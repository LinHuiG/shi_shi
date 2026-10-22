//! Verified downloads, daily local-package checks and a detached upgrade helper.
//! Network access starts only from the desktop scheduler or an explicit check.
#[path = "updates_cleanup.rs"]
mod cleanup;
use crate::{engine::App, model::now};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
const BASE: &str = "https://github.com/LinHuiG/shi_shi/releases/download/";
const MAX_SIZE: u64 = 64 * 1024 * 1024;
const DAY_MS: i64 = 86_400_000;
const CLOCK_GAP_MS: i64 = 10_000;
const ACTION_WINDOW_MS: i64 = 60_000;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version(text: &str) -> Option<[u64; 3]> {
    let parts = text
        .strip_prefix('v')
        .unwrap_or(text)
        .split('.')
        .collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || p.len() > 9
                || !p.bytes().all(|b| b.is_ascii_digit())
                || p.len() > 1 && p.starts_with('0')
        })
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}
pub(crate) fn build_time() -> i64 {
    env!("SHISHI_BUILD_TIME").parse::<i64>().unwrap_or(0) * 1000
}
#[derive(Clone, Serialize, Deserialize, Debug)]
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
    fn path(&self, root: &Path) -> PathBuf {
        root.join(format!(
            "Shishi-Setup-{}-{}.exe",
            self.version,
            &self.digest[..12]
        ))
    }
}
fn release(value: &Value, current: &str, built: i64) -> Result<Option<Package>, String> {
    if value["draft"] != false || value["prerelease"] != false {
        return Ok(None);
    }
    let tag = value["tag_name"].as_str().ok_or("版本信息缺少版本号")?;
    let Some(v) = version(tag) else {
        return Err("发布版本号无效".into());
    };
    if Some(v) <= version(current) {
        return Ok(None);
    }
    let published =
        chrono::DateTime::parse_from_rfc3339(value["published_at"].as_str().unwrap_or(""))
            .map_err(|_| "发布时间无效")?
            .timestamp_millis();
    // Private and public version series may differ. A release published before
    // this binary was built must never replace newer, unpublished test code.
    if published <= built {
        return Ok(None);
    }
    let normalized = format!("{}.{}.{}", v[0], v[1], v[2]);
    let name = format!("Shishi-Setup-{normalized}.exe");
    let asset = value["assets"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["name"] == name && r["state"] == "uploaded")
        })
        .ok_or("新版本尚未提供 Windows 安装包")?;
    let package = Package {
        version: normalized,
        tag: tag.into(),
        url: asset["browser_download_url"].as_str().unwrap_or("").into(),
        size: asset["size"].as_u64().unwrap_or(0),
        digest: asset["digest"]
            .as_str()
            .unwrap_or("")
            .strip_prefix("sha256:")
            .unwrap_or("")
            .to_ascii_lowercase(),
        published,
    };
    if !package.valid() {
        return Err("安装包来源、大小或 SHA-256 信息无效，未下载".into());
    }
    Ok(Some(package))
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
    automatic_install: Option<Scheduled>,
    #[serde(skip)]
    package: Option<Package>,
}
pub struct Runtime {
    inner: Mutex<View>,
    schedule: Mutex<Schedule>,
    pub revision: AtomicU64,
}
#[derive(Clone, Serialize)]
struct Scheduled {
    id: String,
    expires_at: i64,
    #[serde(skip)]
    digest: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Schedule {
    reminder_day: i64,
    install_day: i64,
}
// Observe a live crossing only. Startup, suspend and clock jumps never replay
// a missed boundary; the persistent day claim also prevents a clock rewind or
// a same-minute restart from repeating an operation.
fn crossed(previous: Option<i64>, at: i64, hour: i64) -> Option<(i64, i64)> {
    let previous = previous?;
    if at <= previous || at - previous > CLOCK_GAP_MS {
        return None;
    }
    let day = (at + 8 * 3_600_000).div_euclid(DAY_MS);
    let boundary = day * DAY_MS - 8 * 3_600_000 + hour * 3_600_000;
    (previous < boundary && boundary <= at).then_some((day, boundary))
}
fn cache(root: &Path) -> PathBuf {
    crate::storage::data_dir(root).join("updates")
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let next = path.with_extension("next.json");
    let mut file = fs::File::create(&next).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, value).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(next, path).map_err(|e| e.to_string())
}
impl Runtime {
    pub fn load(root: &Path) -> Self {
        let dir = cache(root);
        let mut view = View {
            status: "idle".into(),
            ..Default::default()
        };
        if let Ok(bytes) = fs::read(dir.join("ready.json")) {
            if let Ok(p) = serde_json::from_slice::<Package>(&bytes) {
                if p.newer(VERSION, build_time()) && verify(&p.path(&dir), &p).is_ok() {
                    view.status = "ready".into();
                    view.version = p.version.clone();
                    view.progress = 100;
                    view.checked_at = now();
                    view.package = Some(p);
                }
            }
        }
        let protected = fs::read(dir.join("ready.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Package>(&bytes).ok())
            .filter(Package::valid)
            .map(|p| p.path(&dir));
        let _ = cleanup::prune(&dir, protected.as_deref(), std::time::SystemTime::now());
        Self {
            inner: Mutex::new(view),
            schedule: Mutex::new(
                fs::read(dir.join("schedule.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default(),
            ),
            revision: AtomicU64::new(0),
        }
    }
    pub fn snapshot(&self) -> Value {
        json!(*self.inner.lock().unwrap())
    }
    fn change(&self, f: impl FnOnce(&mut View)) {
        f(&mut self.inner.lock().unwrap());
        self.revision.fetch_add(1, Ordering::SeqCst);
    }
    fn finish_current(&self, at: i64) {
        self.change(|v| {
            if let Some(package) = &v.package {
                v.status = "ready".into();
                v.version = package.version.clone();
                v.progress = 100;
                v.current_until = 0;
            } else {
                v.status = "current".into();
                v.current_until = at.saturating_add(30_000);
            }
            v.retry_at = 0;
        });
    }
    fn fail(&self, error: String) {
        self.change(|v| {
            // A failed refresh must retain a previously verified local installer.
            if let Some(package) = &v.package {
                v.status = "ready".into();
                v.version = package.version.clone();
                v.progress = 100;
            } else {
                v.status = "failed".into();
            }
            v.error = error;
        });
    }
    fn claim(&self, root: &Path, day: i64, hour: i64) -> Result<bool, String> {
        let mut saved = self.schedule.lock().unwrap();
        let mut next = saved.clone();
        let field = if hour == 9 {
            &mut next.reminder_day
        } else {
            &mut next.install_day
        };
        if *field >= day {
            return Ok(false);
        }
        *field = day;
        let dir = cache(root);
        crate::storage::plain_directory(&dir)?;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        atomic_json(&dir.join("schedule.json"), &next)?;
        *saved = next;
        Ok(true)
    }
    fn clear_expired(&self, at: i64, enabled: bool) {
        let mut view = self.inner.lock().unwrap();
        let expired = |v: &Option<Scheduled>| {
            v.as_ref().is_some_and(|v| {
                !enabled || at >= v.expires_at || at < v.expires_at - ACTION_WINDOW_MS
            })
        };
        if expired(&view.reminder) || expired(&view.automatic_install) {
            if expired(&view.reminder) {
                view.reminder = None;
            }
            if expired(&view.automatic_install) {
                view.automatic_install = None;
            }
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn take_install(&self, id: &str, at: i64, enabled: bool) -> Result<(), String> {
        let mut view = self.inner.lock().unwrap();
        let request = view
            .automatic_install
            .take()
            .ok_or("本次定时更新已过期或处理")?;
        self.revision.fetch_add(1, Ordering::SeqCst);
        if !enabled
            || request.id != id
            || at >= request.expires_at
            || at < request.expires_at - ACTION_WINDOW_MS
            || view
                .package
                .as_ref()
                .is_none_or(|p| p.digest != request.digest)
        {
            return Err("本次定时更新已过期或安装包已变化".into());
        }
        Ok(())
    }
}
fn schedule_tick(app: &Arc<App>, previous: Option<i64>, at: i64) -> Result<(), String> {
    let enabled = app.state.lock().unwrap().config.auto_update;
    app.updates.clear_expired(at, enabled);
    if !enabled || app.storage_changing.load(Ordering::SeqCst) {
        return Ok(());
    }
    for hour in [4, 9] {
        let Some((day, boundary)) = crossed(previous, at, hour) else {
            continue;
        };
        if !app.updates.claim(&app.root, day, hour)? {
            continue;
        }
        // Inspect only the verified local cache, never a network result. Claim
        // the check even when empty, so a later download waits until tomorrow.
        let dir = cache(&app.root);
        let Some(package) = fs::read(dir.join("ready.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Package>(&b).ok())
            .filter(|p| p.newer(VERSION, build_time()) && verify(&p.path(&dir), p).is_ok())
        else {
            continue;
        };
        let mut view = app.updates.inner.lock().unwrap();
        if ["checking", "downloading", "installing"].contains(&view.status.as_str()) {
            drop(view);
            app.log(
                "update_schedule_skipped",
                json!({"hour":hour,"reason":"update_busy"}),
            );
            continue;
        }
        view.status = "ready".into();
        view.error.clear();
        view.version = package.version.clone();
        view.progress = 100;
        let request = Scheduled {
            id: format!("{day}:{hour}:{}", package.digest),
            expires_at: boundary + ACTION_WINDOW_MS,
            digest: package.digest.clone(),
        };
        view.package = Some(package);
        if hour == 9 {
            view.reminder = Some(request);
        } else {
            view.automatic_install = Some(request);
        }
        app.updates.revision.fetch_add(1, Ordering::SeqCst);
    }
    Ok(())
}
fn check_interval(sample: u64) -> i64 {
    (60 + (sample % 61) as i64) * 60_000
}
fn check_due(view: &View, automatic: bool, at: i64) -> bool {
    !["checking", "downloading", "installing"].contains(&view.status.as_str())
        && !(view.status == "current" && at < view.current_until)
        && at >= view.retry_at
        && (!automatic
            || (view.automatic_install.is_none()
                && view.reminder.is_none()
                && at >= view.next_check_at))
}
pub fn check(app: Arc<App>, automatic: bool) -> Result<Value, String> {
    if app.storage_changing.load(Ordering::SeqCst) {
        return Err("正在切换数据目录".into());
    }
    if automatic && !app.state.lock().unwrap().config.auto_update {
        return Ok(app.updates.snapshot());
    }
    {
        let mut view = app.updates.inner.lock().unwrap();
        let at = now();
        if !check_due(&view, automatic, at) {
            return Ok(json!(*view));
        }
        view.status = "checking".into();
        view.error.clear();
        view.progress = 0;
        view.checked_at = at;
        view.next_check_at =
            at.saturating_add(check_interval(uuid::Uuid::new_v4().as_u128() as u64));
        view.current_until = 0;
        view.retry_at = 0;
        app.updates.revision.fetch_add(1, Ordering::SeqCst);
    }
    let result = app.updates.snapshot();
    std::thread::spawn(move || {
        if let Err(error) = fetch(&app) {
            app.updates.fail(error);
        }
    });
    Ok(result)
}
fn metadata_failure(value: &Value) -> Option<String> {
    match value["error"].as_str()? {
        "rate_limited" => {
            let retry = value["retry_at"]
                .as_i64()
                .filter(|at| *at > 0 && *at <= 4_102_444_800_000)
                .map(crate::model::stamp);
            Some(match retry {
                Some(at) => format!("GitHub 请求次数已达上限，可在 {at} 后重试"),
                None => "GitHub 请求次数已达上限，请稍后重试".into(),
            })
        }
        "http_error" => Some(
            match value["status"].as_u64().filter(|s| (400..=599).contains(s)) {
                Some(code) => format!("GitHub 更新服务暂时不可用（HTTP {code}），请稍后重试"),
                None => "GitHub 更新服务返回异常响应，请稍后重试".into(),
            },
        ),
        _ => None,
    }
}
fn metadata_retry_at(value: &Value, at: i64) -> i64 {
    if value["error"] != "rate_limited" {
        return 0;
    }
    value["retry_at"]
        .as_i64()
        .filter(|retry| *retry > at && *retry <= at.saturating_add(DAY_MS))
        .unwrap_or_else(|| at.saturating_add(60_000))
}
fn fetch(app: &Arc<App>) -> Result<(), String> {
    let dir = cache(&app.root);
    crate::storage::plain_directory(&dir)?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let metadata=transfer(app,&dir,&json!({"mode":"metadata","url":"https://api.github.com/repos/LinHuiG/shi_shi/releases/latest"}),40)
        .map_err(|_|"更新检查失败，请检查网络后重试")?;
    let value: Value = serde_json::from_str(metadata.trim()).map_err(|_| "版本信息格式无效")?;
    if let Some(error) = metadata_failure(&value) {
        app.updates
            .change(|v| v.retry_at = metadata_retry_at(&value, now()));
        return Err(error);
    }
    if value["not_found"] == true {
        app.updates.finish_current(now());
        return Ok(());
    }
    let Some(package) = release(&value, VERSION, build_time())? else {
        app.updates.finish_current(now());
        return Ok(());
    };
    let file = package.path(&dir);
    app.updates.change(|v| {
        v.status = "downloading".into();
        v.version = package.version.clone();
    });
    if verify(&file, &package).is_err() {
        let partial = dir.join(format!("download-{}.part", crate::model::id()));
        let result = transfer(
            app,
            &dir,
            &json!({"mode":"download","url":package.url,"path":partial,"size":package.size}),
            620,
        );
        let result = result.and_then(|_| verify(&partial, &package));
        if let Err(e) = result {
            let _ = fs::remove_file(&partial);
            return Err(format!("更新下载或校验失败，请重新检查：{e}"));
        }
        fs::rename(partial, &file).map_err(|e| e.to_string())?;
    }
    atomic_json(&dir.join("ready.json"), &package)?;
    if let Err(error) = cleanup::prune(&dir, Some(&file), std::time::SystemTime::now()) {
        app.log(
            "error",
            json!({"message":format!("更新缓存清理失败：{error}"),"failure_class":"update_cache"}),
        );
    }
    app.updates.change(|v| {
        if v.reminder
            .as_ref()
            .is_some_and(|r| r.digest != package.digest)
        {
            v.reminder = None;
        }
        if v.automatic_install
            .as_ref()
            .is_some_and(|r| r.digest != package.digest)
        {
            v.automatic_install = None;
        }
        v.status = "ready".into();
        v.progress = 100;
        v.package = Some(package);
    });
    Ok(())
}
fn transfer(app: &App, dir: &Path, input: &Value, timeout: u64) -> Result<String, String> {
    let script = dir.join("download.ps1");
    fs::write(&script, include_str!("../assets/update-download.ps1")).map_err(|e| e.to_string())?;
    let system = std::env::var_os("SystemRoot").ok_or("系统目录不可用")?;
    let mut command =
        Command::new(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script);
    crate::process::run_with_progress(
        command,
        Some(&input.to_string()),
        timeout,
        dir,
        &mut |text| {
            for line in text.lines() {
                if let Some(n) = line
                    .trim()
                    .strip_prefix("progress:")
                    .and_then(|v| v.parse::<u8>().ok())
                {
                    app.updates.change(|v| v.progress = n.min(99));
                }
            }
        },
    )
    .map(|(out, _)| out)
}
pub fn start_scheduler(app: Arc<App>) {
    std::thread::spawn(move || {
        let mut previous = Some(now());
        while !crate::process::stopping() {
            std::thread::sleep(Duration::from_secs(1));
            let at = now();
            if let Err(error) = schedule_tick(&app, previous, at) {
                app.error(&format!("定时更新检查失败：{error}"));
            }
            previous = Some(at);
            if app.storage_changing.load(Ordering::SeqCst) {
                continue;
            }
            let enabled = app.state.lock().unwrap().config.auto_update;
            if enabled {
                let _ = check(app.clone(), true);
            }
        }
    });
}
pub fn open_link(target: &str) -> Result<Value, String> {
    let url = match target {
        "github" => "https://github.com/LinHuiG/shi_shi",
        "blog" => "https://blog.linhui.fun/",
        "intro" => "https://blog.linhui.fun/posts/shishi-ai-context-engineering/",
        _ => return Err("链接无效".into()),
    };
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let opened = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if opened as usize <= 32 {
        Err("无法打开浏览器，请检查默认浏览器设置".into())
    } else {
        Ok(json!(true))
    }
}

fn verify(path: &Path, p: &Package) -> Result<(), String> {
    let metadata = plain_file(path)?;
    if !metadata.is_file() || metadata.len() != p.size || sha256(path)? != p.digest {
        return Err("安装包校验不一致".into());
    }
    Ok(())
}
fn plain_file(path: &Path) -> Result<fs::Metadata, String> {
    use std::os::windows::fs::MetadataExt;
    crate::storage::plain_directory(path.parent().ok_or("文件路径无效")?)?;
    let meta = fs::symlink_metadata(path).map_err(|_| "文件不可用")?;
    if !meta.is_file() || meta.file_attributes() & 0x400 != 0 {
        return Err("更新文件不能是链接或目录".into());
    }
    Ok(meta)
}
fn sha256(path: &Path) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::*;
    unsafe {
        let mut algorithm = std::ptr::null_mut();
        let mut hash = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_SHA256_ALGORITHM, std::ptr::null(), 0)
            < 0
        {
            return Err("SHA-256 不可用".into());
        }
        let result = (|| {
            if BCryptCreateHash(
                algorithm,
                &mut hash,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                0,
                0,
            ) < 0
            {
                return Err("SHA-256 初始化失败".into());
            }
            let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
            let mut buffer = vec![0u8; 65536];
            loop {
                let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                if BCryptHashData(hash, buffer.as_ptr(), n as u32, 0) < 0 {
                    return Err("SHA-256 读取失败".into());
                }
            }
            let mut digest = [0u8; 32];
            if BCryptFinishHash(hash, digest.as_mut_ptr(), 32, 0) < 0 {
                return Err("SHA-256 校验失败".into());
            }
            Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
        })();
        if !hash.is_null() {
            BCryptDestroyHash(hash);
        }
        BCryptCloseAlgorithmProvider(algorithm, 0);
        result
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    parent: u32,
    root: PathBuf,
    data: PathBuf,
    executable: PathBuf,
    package: Package,
    installer: PathBuf,
    args: Vec<String>,
}
pub fn install(app: &Arc<App>, scheduled: Option<&str>) -> Result<Value, String> {
    if let Some(id) = scheduled {
        let enabled = app.state.lock().unwrap().config.auto_update;
        app.updates.take_install(id, now(), enabled)?;
    }
    if app.storage_changing.swap(true, Ordering::SeqCst) {
        return Err("已有重启操作正在执行".into());
    }
    let result = (|| {
        if !app.storage_ready() {
            return Err("请等待当前 AI 工作结束或先终止处置，再更新重启".into());
        }
        let package = {
            let view = app.updates.inner.lock().unwrap();
            if view.status != "ready" {
                return Err("没有已准备好的更新，请先检查更新".into());
            }
            view.package.clone().ok_or("没有已下载的更新")?
        };
        let dir = cache(&app.root);
        let installer = package.path(&dir);
        verify(&installer, &package)?;
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        if !fs::read_to_string(executable.parent().unwrap().join("install-info.txt"))
            .unwrap_or_default()
            .starts_with("Shishi ")
        {
            return Err("当前是开发运行目录，请使用安装包安装后再执行自动升级".into());
        }
        let helper_dir = dir.join(format!("apply-{}", crate::model::id()));
        fs::create_dir(&helper_dir).map_err(|e| e.to_string())?;
        let helper = helper_dir.join("shishi-update.exe");
        fs::copy(&executable, &helper).map_err(|e| e.to_string())?;
        fs::copy(
            executable.parent().unwrap().join("WebView2Loader.dll"),
            helper_dir.join("WebView2Loader.dll"),
        )
        .map_err(|e| e.to_string())?;
        let plan = Plan {
            parent: std::process::id(),
            root: app.root.clone(),
            data: crate::storage::data_dir(&app.root),
            executable,
            package,
            installer,
            args: std::env::args().skip(1).collect(),
        };
        let plan_path = helper_dir.join("plan.json");
        atomic_json(&plan_path, &plan)?;
        Command::new(helper)
            .arg("--apply-update")
            .arg(&plan_path)
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        while !plan_path.with_extension("ready").is_file() {
            if started.elapsed() > Duration::from_secs(10) {
                return Err("更新助手未能启动，应用保持运行".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        app.updates.change(|v| v.status = "installing".into());
        Ok(json!(true))
    })();
    if result.is_err() {
        app.storage_changing.store(false, Ordering::SeqCst);
    }
    result
}
pub fn helper(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let p: Plan = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|_| "更新计划无效")?;
    if !p.package.valid()
        || p.executable.file_name().and_then(|s| s.to_str()) != Some("shishi.exe")
        || p.installer != p.package.path(&p.data.join("updates"))
    {
        return Err("更新路径无效".into());
    }
    plain_file(&p.executable)?;
    verify(&p.installer, &p.package)?;
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, p.parent) };
    if process.is_null() {
        return Err("原应用不可用，未开始安装".into());
    }
    fs::write(path.with_extension("ready"), b"ready").map_err(|e| e.to_string())?;
    let waited = unsafe { WaitForSingleObject(process, 60000) };
    unsafe {
        CloseHandle(process);
    }
    if waited != WAIT_OBJECT_0 {
        return Err("原应用尚未退出，未开始安装".into());
    }
    let install_dir = p.executable.parent().ok_or("安装目录无效")?;
    let result = (|| {
        verify(&p.installer, &p.package)?;
        release_helpers(install_dir, path.parent().ok_or("更新助手目录无效")?)?;
        let mut command = Command::new(&p.installer);
        command.args(["/S", "/UPDATE"]);
        // Existing native smoke mode is always isolated and must never touch
        // the real installed application's registry or shortcuts.
        if p.args.iter().any(|a| a == "--ui-smoke") && p.root == install_dir {
            command.arg("/TEST");
        }
        let mut child = command
            .raw_arg(format!("/D={}", install_dir.display()))
            .creation_flags(0x08000000)
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
        let installed =
            fs::read_to_string(install_dir.join("install-info.txt")).unwrap_or_default();
        let expected = format!("Shishi {}", p.package.version);
        if !status.success() || installed.lines().next() != Some(expected.as_str()) {
            return Err(format!("安装未成功完成：安装器退出码 {}，目标版本 {}，当前安装标记 {}。保留原数据，请手动重试安装包。",
                status.code().map_or_else(||"未知".into(),|n|n.to_string()), p.package.version,
                installed.lines().next().unwrap_or("未找到")));
        }
        atomic_json(
            &p.data.join("updates/history.json"),
            &json!({"version":p.package.version,"time":now()}),
        )?;
        let _ = fs::remove_file(p.data.join("updates/ready.json"));
        let _ = fs::remove_file(p.data.join("updates/last-error.json"));
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = atomic_json(
            &p.data.join("updates/last-error.json"),
            &json!({"time":now(),"version":p.package.version,"error":error}),
        );
    }
    Command::new(&p.executable)
        .args(&p.args)
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| format!("自动启动失败，请重新打开拾事：{e}"))?;
    result
}

/// The desktop parent has already exited. Release only its installation's MCP
/// servers, including those held by an external client; never stop that client.
fn release_helpers(install_dir: &Path, work: &Path) -> Result<(), String> {
    let system = std::env::var_os("SystemRoot").ok_or("系统目录不可用")?;
    let script = work.join("release-helpers.ps1");
    fs::write(&script, include_str!("../installer/process.ps1")).map_err(|e| e.to_string())?;
    let mut command =
        Command::new(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-InstallDir")
        .arg(install_dir)
        .args(["-Action", "ReleaseHelpers"]);
    let result = crate::process::run(command, None, 30, work);
    let _ = fs::remove_file(script);
    result.map(|_| ()).map_err(|error| {
        format!("无法释放升级所需的程序文件。请从托盘退出拾事后重试；不会关闭外部 Codex 或其他安装。详情：{error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_checks_start_on_restart_and_use_fresh_sixty_to_one_twenty_minute_delays() {
        let at = now();
        assert_eq!(check_interval(0), 60 * 60_000);
        assert_eq!(check_interval(60), 120 * 60_000);
        assert_eq!(check_interval(61), 60 * 60_000);
        let mut view = View {
            status: "ready".into(),
            checked_at: at,
            ..Default::default()
        };
        assert!(
            check_due(&view, true, at),
            "cached package must not suppress startup check"
        );
        view.next_check_at = at + check_interval(0);
        assert!(!check_due(&view, true, at + 59 * 60_000));
        assert!(check_due(&view, true, at + 60 * 60_000));
        assert!(check_due(&view, false, at));
        view.next_check_at = at + check_interval(60);
        assert!(!check_due(&view, true, at + 119 * 60_000));
        assert!(check_due(&view, true, at + 120 * 60_000));
        view.retry_at = at + DAY_MS;
        assert!(!check_due(&view, true, at + 120 * 60_000));
        let app = Arc::new(
            App::load(
                std::env::temp_dir().join(format!("shishi-update-disabled-{}", crate::model::id())),
            )
            .unwrap(),
        );
        app.state.lock().unwrap().config.auto_update = false;
        assert_eq!(check(app, true).unwrap()["checked_at"], 0);
    }
    #[test]
    fn rate_limit_retry_deadlines_gate_manual_and_automatic_network_calls() {
        let at = now();
        let app = Arc::new(
            App::load(
                std::env::temp_dir().join(format!("shishi-rate-limit-{}", crate::model::id())),
            )
            .unwrap(),
        );
        app.updates.change(|v| {
            v.status = "failed".into();
            v.error = "quota".into();
            v.retry_at = at + 60_000;
        });
        assert_eq!(check(app.clone(), false).unwrap()["checked_at"], 0);
        assert_eq!(check(app, true).unwrap()["status"], "failed");
        assert_eq!(
            metadata_retry_at(&json!({"error":"rate_limited","retry_at":at+120_000}), at),
            at + 120_000
        );
        assert_eq!(
            metadata_retry_at(&json!({"error":"rate_limited","retry_at":i64::MAX}), at),
            at + 60_000
        );
        assert_eq!(metadata_retry_at(&json!({"error":"http_error"}), at), 0);
    }
    #[test]
    fn metadata_failures_distinguish_rate_limits_without_exposing_upstream_content() {
        let limited =
            metadata_failure(&json!({"error":"rate_limited","retry_at":1_700_000_000_000i64}))
                .unwrap();
        assert!(limited.contains("请求次数已达上限"));
        assert!(limited.contains(&crate::model::stamp(1_700_000_000_000)));
        assert!(!limited.contains("网络"));
        assert_eq!(
            metadata_failure(&json!({"error":"rate_limited","retry_at":i64::MAX})).unwrap(),
            "GitHub 请求次数已达上限，请稍后重试"
        );
        assert!(
            metadata_failure(&json!({"error":"http_error","status":503,"body":"untrusted"}))
                .unwrap()
                .contains("HTTP 503")
        );
        assert!(
            !metadata_failure(&json!({"error":"http_error","status":"untrusted"}))
                .unwrap()
                .contains("untrusted")
        );
        assert!(metadata_failure(&json!({"not_found":true})).is_none());
        let (app, _, package) = scheduled_app();
        app.updates.change(|v| {
            v.version = "unavailable-newer-download".into();
            v.progress = 5;
        });
        app.updates.fail("quota".into());
        let retained = app.updates.snapshot();
        assert_eq!(retained["status"], "ready");
        assert_eq!(retained["version"], package.version);
        assert_eq!(retained["progress"], 100);
        app.updates.finish_current(now());
        assert_eq!(app.updates.snapshot()["status"], "ready");
        assert_eq!(app.updates.snapshot()["current_until"], 0);
    }
    #[test]
    fn no_update_cooldown_starts_at_completion_and_blocks_duplicate_requests() {
        let app = Arc::new(
            App::load(std::env::temp_dir().join(format!("shishi-cooldown-{}", crate::model::id())))
                .unwrap(),
        );
        app.updates.finish_current(1_000);
        assert_eq!(app.updates.snapshot()["current_until"], 31_000);
        let finished = now();
        app.updates.finish_current(finished);
        let view = check(app.clone(), false).unwrap();
        assert_eq!(view["status"], "current");
        assert_eq!(view["current_until"], finished + 30_000);
        assert_eq!(view["checked_at"], 0);
        assert_eq!(check(app, true).unwrap()["status"], "current");
    }
    fn fixture() -> Value {
        json!({"tag_name":"v9.10.0","published_at":"2099-01-01T00:00:00Z","draft":false,"prerelease":false,"assets":[{"name":"Shishi-Setup-9.10.0.exe","state":"uploaded","size":3,"digest":format!("sha256:{}","a".repeat(64)),"browser_download_url":format!("{BASE}v9.10.0/Shishi-Setup-9.10.0.exe")}]})
    }
    fn scheduled_app() -> (Arc<App>, PathBuf, Package) {
        let root = std::env::temp_dir().join(format!("shishi-update-clock-{}", crate::model::id()));
        let dir = cache(&root);
        fs::create_dir_all(&dir).unwrap();
        let mut package = release(&fixture(), "1.0.0", 0).unwrap().unwrap();
        let bytes = dir.join("hash-source");
        fs::write(&bytes, b"abc").unwrap();
        package.digest = sha256(&bytes).unwrap();
        fs::write(package.path(&dir), b"abc").unwrap();
        atomic_json(&dir.join("ready.json"), &package).unwrap();
        (Arc::new(App::load(root.clone()).unwrap()), root, package)
    }
    fn boundary(hour: i64) -> i64 {
        22_000 * DAY_MS - 8 * 3_600_000 + hour * 3_600_000
    }
    #[test]
    fn daily_boundaries_skip_startup_suspend_clock_jumps_and_late_downloads() {
        for hour in [4, 9] {
            let at = boundary(hour);
            assert!(crossed(None, at, hour).is_none());
            assert!(crossed(None, at + 1, hour).is_none());
            assert!(crossed(Some(at + 1), at + 1000, hour).is_none());
            assert!(crossed(Some(at - CLOCK_GAP_MS), at + 1, hour).is_none());
            assert!(crossed(Some(at + 1), at - 1, hour).is_none());
            assert_eq!(crossed(Some(at - 500), at + 500, hour), Some((22_000, at)));
            assert_eq!(
                crossed(Some(at + DAY_MS - 500), at + DAY_MS, hour),
                Some((22_001, at + DAY_MS))
            );
        }
    }
    #[test]
    fn local_checks_repeat_next_day_not_on_restart_and_do_not_require_later_choice() {
        let (app, root, _) = scheduled_app();
        let at = boundary(9);
        schedule_tick(&app, None, at + 1000).unwrap();
        assert!(app.updates.snapshot()["reminder"].is_null());
        schedule_tick(&app, Some(at - 500), at).unwrap();
        assert!(app.updates.snapshot()["reminder"]["id"].is_string());
        schedule_tick(&app, Some(at), at + ACTION_WINDOW_MS).unwrap();
        assert!(app.updates.snapshot()["reminder"].is_null());
        drop(app);
        let app = Arc::new(App::load(root.clone()).unwrap());
        schedule_tick(&app, Some(at - 500), at).unwrap();
        assert!(
            app.updates.snapshot()["reminder"].is_null(),
            "same day is persisted"
        );
        let next_install = boundary(4) + DAY_MS;
        schedule_tick(&app, Some(next_install - 500), next_install).unwrap();
        let id = app.updates.snapshot()["automatic_install"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(app.updates.take_install(&id, next_install, true).is_ok());
        assert!(
            app.updates.take_install(&id, next_install, true).is_err(),
            "not submitted twice"
        );
        let next_reminder = at + DAY_MS;
        schedule_tick(&app, Some(next_reminder - 500), next_reminder).unwrap();
        assert!(app.updates.snapshot()["reminder"]["id"].is_string());
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn local_checks_reject_missing_corrupt_packages_and_disabled_updates() {
        let (app, root, package) = scheduled_app();
        let at = boundary(9);
        fs::write(package.path(&cache(&root)), b"bad").unwrap();
        schedule_tick(&app, Some(at - 500), at).unwrap();
        assert!(app.updates.snapshot()["reminder"].is_null());
        fs::write(package.path(&cache(&root)), b"abc").unwrap();
        schedule_tick(&app, Some(at), at + 500).unwrap();
        assert!(
            app.updates.snapshot()["reminder"].is_null(),
            "a package appearing late waits"
        );
        fs::remove_file(cache(&root).join("ready.json")).unwrap();
        let next = at + DAY_MS;
        schedule_tick(&app, Some(next - 500), next).unwrap();
        assert!(app.updates.snapshot()["reminder"].is_null());
        atomic_json(&cache(&root).join("ready.json"), &package).unwrap();
        app.state.lock().unwrap().config.auto_update = false;
        let install_at = boundary(4) + 2 * DAY_MS;
        schedule_tick(&app, Some(install_at - 500), install_at).unwrap();
        assert!(app.updates.snapshot()["automatic_install"].is_null());
        app.state.lock().unwrap().config.auto_update = true;
        schedule_tick(&app, Some(install_at), install_at + 500).unwrap();
        assert!(
            app.updates.snapshot()["automatic_install"].is_null(),
            "enabling is not catch-up"
        );
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn automatic_install_tickets_expire_and_recheck_switch_and_package_identity() {
        let (app, root, _) = scheduled_app();
        let at = boundary(4);
        for (offset, enabled, changed) in [
            (ACTION_WINDOW_MS, true, false),
            (-1, true, false),
            (0, false, false),
            (0, true, true),
        ] {
            app.updates.schedule.lock().unwrap().install_day = 0;
            schedule_tick(&app, Some(at - 500), at).unwrap();
            let id = app.updates.snapshot()["automatic_install"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            if changed {
                app.updates
                    .change(|v| v.package.as_mut().unwrap().digest = "0".repeat(64));
            }
            assert!(app.updates.take_install(&id, at + offset, enabled).is_err());
        }
        app.updates.schedule.lock().unwrap().install_day = 0;
        schedule_tick(&app, Some(at - 500), at).unwrap();
        app.state.lock().unwrap().config.auto_update = false;
        schedule_tick(&app, Some(at), at + 1000).unwrap();
        assert!(app.updates.snapshot()["automatic_install"].is_null());
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn build_time_is_embedded_and_not_a_runtime_timestamp() {
        assert!(build_time() > 0 && build_time() <= now());
        assert_eq!(
            build_time(),
            env!("SHISHI_BUILD_TIME").parse::<i64>().unwrap() * 1000
        );
    }
    #[test]
    fn release_selection_requires_newer_version_publication_and_verified_asset() {
        assert!(version("1.10.0") > version("1.9.9"));
        for invalid in ["1.2", "1.2.3-beta", "../1.2.3", "01.2.3", "1.2.3.4"] {
            assert!(version(invalid).is_none());
        }
        assert!(release(&fixture(), "1.2.0", 0).unwrap().is_some());
        assert!(release(&fixture(), "10.0.0", 0).unwrap().is_none());
        assert!(release(&fixture(), "1.2.0", i64::MAX).unwrap().is_none());
        for key in ["draft", "prerelease"] {
            let mut f = fixture();
            f[key] = json!(true);
            assert!(release(&f, "1.0.0", 0).unwrap().is_none());
        }
        for (key, value) in [
            (
                "browser_download_url",
                json!("https://example.com/untrusted.exe"),
            ),
            ("digest", json!(null)),
            ("size", json!(MAX_SIZE + 1)),
        ] {
            let mut f = fixture();
            f["assets"][0][key] = value;
            assert!(release(&f, "1.0.0", 0).is_err());
        }
    }
    #[test]
    fn package_hash_cache_and_legacy_history_without_display_fields() {
        let root = std::env::temp_dir().join(format!("shishi-updates-{}", crate::model::id()));
        let dir = cache(&root);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bytes");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut p = release(&fixture(), "1.0.0", 0).unwrap().unwrap();
        p.digest = sha256(&file).unwrap();
        assert!(verify(&file, &p).is_ok());
        fs::write(&file, b"abd").unwrap();
        assert!(verify(&file, &p).is_err());
        fs::write(p.path(&dir), b"abc").unwrap();
        atomic_json(&dir.join("ready.json"), &p).unwrap();
        atomic_json(
            &dir.join("history.json"),
            &json!({"version":"0.0.1","time":123}),
        )
        .unwrap();
        assert!(Runtime::load(&root).snapshot().get("updated_at").is_none());
        atomic_json(
            &dir.join("history.json"),
            &json!({"version":VERSION,"time":123}),
        )
        .unwrap();
        assert!(Runtime::load(&root)
            .snapshot()
            .get("installed_at")
            .is_none());
        let app = Arc::new(App::load(root.clone()).unwrap());
        assert_eq!(app.updates.snapshot()["status"], "ready");
        assert!(check_due(&app.updates.inner.lock().unwrap(), true, now()));
        app.updates
            .change(|v| v.next_check_at = now() + 60 * 60_000);
        assert_eq!(check(app.clone(), true).unwrap()["status"], "ready");
        app.updates.change(|v| v.status = "downloading".into());
        assert_eq!(check(app.clone(), false).unwrap()["status"], "downloading");
        assert!(app.state.lock().unwrap().config.auto_update);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}
