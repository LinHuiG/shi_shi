//! `storage_change`: move or switch the data directory. The running owner
//! validates the target and starts `--data-transfer <plan>`, then exits; the
//! helper takes the old `app.lock` once the owner is gone, copies and
//! verifies, commits `data-location.json` and starts the app again. The old
//! directory is never deleted.
#![cfg_attr(not(windows), allow(dead_code))]
use crate::transport as wire;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const LOCATOR: &str = "data-location.json";
/// Never copied: locks, the route, WebView profile and scratch space.
const SKIPPED: [&str; 6] = ["app.lock", "webview", "tmp", "cache", wire::ROUTE_FILE, "local-service.tmp"];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    root: PathBuf,
    source: PathBuf,
    destination: PathBuf,
    copy: bool,
    restart_args: Vec<String>,
    expected_locator: Option<Vec<u8>>,
}

fn linked(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    meta.file_type().is_symlink()
}

/// Absolute, no `..`, and no link or plain file anywhere along the path.
fn plain_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("数据目录必须是完整路径，不能包含 ..".into());
    }
    let mut part = PathBuf::new();
    for component in path.components() {
        part.push(component);
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match fs::symlink_metadata(&part) {
            Ok(meta) if linked(&meta) || !meta.is_dir() => {
                return Err("数据目录不能经过符号链接、junction 或普通文件".into());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("无法检查数据目录：{e}")),
        }
    }
    Ok(())
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    let path = path.canonicalize().map_err(|e| format!("数据目录不可用：{e}"))?;
    let text = path.to_string_lossy();
    Ok(if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        path
    })
}

/// `a` equals or lies inside `b`, case-insensitive like Windows paths.
fn nested(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    let (a, b) = (key(a), key(b));
    a == b || a.starts_with(&(b.trim_end_matches('/').to_string() + "/"))
}

/// A UNC path or a mapped network drive. SQLite's WAL needs local shared
/// memory and byte-range locks that network file systems do not honour.
#[cfg(windows)]
fn network(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let text = path.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if plain.starts_with(r"\\") || plain.starts_with("UNC\\") {
        return true;
    }
    let Some(drive) = plain.get(..3).filter(|d| d.ends_with(":\\")) else { return false };
    let wide: Vec<u16> = std::ffi::OsStr::new(drive).encode_wide().chain([0]).collect();
    // DRIVE_REMOTE
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) == 4 }
}

#[cfg(not(windows))]
fn network(_: &Path) -> bool {
    false
}

/// `app.lock` of a data directory, opened but not yet locked.
pub fn lock_file(directory: &Path) -> Result<File, String> {
    if fs::symlink_metadata(directory.join("app.lock")).is_ok_and(|m| linked(&m)) {
        return Err("数据目录锁不能是链接或 junction".into());
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("app.lock"))
        .map_err(|e| e.to_string())
}

fn data_lock(directory: &Path) -> Result<File, String> {
    let file = lock_file(directory)?;
    // A child process being started elsewhere in this process briefly
    // shares the lock until its exec closes the descriptor; wait that out.
    let deadline = Instant::now() + Duration::from_millis(500);
    while file.try_lock().is_err() {
        if Instant::now() >= deadline {
            return Err("所选数据目录正在被另一个拾事实例使用".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(file)
}

fn empty(directory: &Path) -> Result<bool, String> {
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        if entry.map_err(|e| e.to_string())?.file_name() != "app.lock" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_locator(root: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(root.join(LOCATOR)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Validate `{directory, copy}` against the running installation.
fn prepare(root: &Path, data: &Path, params: &Value, restart_args: Vec<String>) -> Result<Plan, String> {
    let copy = params["copy"].as_bool().ok_or("请选择是否迁移当前数据")?;
    let path = PathBuf::from(params["directory"].as_str().filter(|s| !s.is_empty()).ok_or("请选择数据目录")?);
    plain_directory(&path)?;
    let destination = canonical(&path)?;
    let source = canonical(data)?;
    if nested(&destination, &source) || nested(&source, &destination) {
        return Err("新旧数据目录不能相同，也不能互相包含".into());
    }
    if nested(&canonical(root)?, &destination) {
        return Err("数据目录不能是程序目录或它的上级目录，请选择独立文件夹".into());
    }
    if network(&destination) {
        return Err("数据目录不能放在网络盘或共享文件夹，数据库在网络上容易损坏；请选择本机磁盘".into());
    }
    let _lock = data_lock(&destination)?;
    let blank = empty(&destination)?;
    if copy && !blank {
        return Err("迁移需要空文件夹，所选目录已有文件；未覆盖任何文件".into());
    }
    if !copy && !blank && !destination.join("shishi.db").is_file() {
        return Err("所选目录不是拾事数据目录（缺少 shishi.db），也不是空文件夹".into());
    }
    Ok(Plan { root: root.into(), source, destination, copy, restart_args, expected_locator: read_locator(root)? })
}

/// Called on the owner: start the helper and wait until it holds the plan.
/// On Ok the caller must exit without relaunching (`Signal::Handoff`).
pub fn begin(owner: &crate::owner::Owner, params: &Value) -> Result<Value, String> {
    if owner.ctx.flags.busy() || !owner.ctx.flags.analyzing().is_empty() {
        return Err("扫描或 AI 任务正在进行，请完成或停止后再更改数据目录".into());
    }
    let restart = owner.args.restart(owner.owners.gui(), owner.owners.cli());
    let plan = prepare(&owner.root, &owner.data, params, restart)?;
    // Beside the program, not inside data: it must survive the copy.
    let job = owner.root.join(format!(".data-transfer-{}.json", uuid::Uuid::new_v4().simple()));
    write_json(&job, &plan)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    crate::owner::hand_off(&exe, "--data-transfer", &job, Duration::from_secs(30)).map_err(|e| {
        let _ = fs::remove_file(&job);
        format!("迁移助手{e}，目录未变更")
    })?;
    Ok(Value::Bool(true))
}

/// Replace `path` in one rename, so a reader sees the old or the new file.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let next = path.with_extension("next.json");
    let written = (|| {
        let mut file = File::create(&next)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&next, path)
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&next);
        e.to_string()
    })
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Entry {
    relative: PathBuf,
    size: u64,
    modified: std::time::SystemTime,
}

fn inventory(base: &Path, relative: &Path, files: &mut Vec<Entry>, dirs: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(base.join(relative)).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if relative.as_os_str().is_empty() && SKIPPED.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let path = relative.join(&name);
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if linked(&meta) {
            return Err("数据中含链接或 junction，请移走链接后再迁移；原目录保留".into());
        }
        if meta.is_dir() {
            dirs.push(path.clone());
            inventory(base, &path, files, dirs)?;
        } else if meta.is_file() {
            files.push(Entry {
                relative: path,
                size: meta.len(),
                modified: meta.modified().map_err(|e| e.to_string())?,
            });
        } else {
            return Err("数据目录包含不支持的文件类型，未切换目录".into());
        }
    }
    Ok(())
}

fn listing(base: &Path) -> Result<(Vec<Entry>, Vec<PathBuf>), String> {
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    inventory(base, Path::new(""), &mut files, &mut dirs)?;
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    dirs.sort();
    Ok((files, dirs))
}

/// Copy every file, read it back and compare, and require the source to be
/// unchanged afterwards.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let before = listing(source)?;
    for dir in &before.1 {
        fs::create_dir(destination.join(dir)).map_err(|e| e.to_string())?;
    }
    let (mut buffer, mut check) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    for entry in &before.0 {
        let to = destination.join(&entry.relative);
        let mut input = File::open(source.join(&entry.relative)).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(&to).map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        let (mut a, mut b) = (
            File::open(source.join(&entry.relative)).map_err(|e| e.to_string())?,
            File::open(&to).map_err(|e| e.to_string())?,
        );
        loop {
            let n = a.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                if b.read(&mut check[..1]).map_err(|e| e.to_string())? != 0 {
                    return Err("复制校验失败，未切换目录；原数据保留".into());
                }
                break;
            }
            b.read_exact(&mut check[..n]).map_err(|_| "复制校验失败，未切换目录；原数据保留")?;
            if buffer[..n] != check[..n] {
                return Err("复制校验失败，未切换目录；原数据保留".into());
            }
        }
        if copied != entry.size {
            return Err("复制期间源文件发生变化，未切换目录；原数据保留".into());
        }
    }
    if listing(source)? != before {
        return Err("源目录在迁移期间发生变化，未切换目录；原数据保留".into());
    }
    Ok(())
}

/// The helper's work once it holds both data locks.
fn apply(plan: &Plan) -> Result<(), String> {
    plain_directory(&plan.source)?;
    plain_directory(&plan.destination)?;
    if nested(&plan.source, &plan.destination) || nested(&plan.destination, &plan.source) {
        return Err("新旧目录范围冲突".into());
    }
    if read_locator(&plan.root)? != plan.expected_locator
        || canonical(&wire::data_directory(&plan.root)?)? != plan.source
    {
        return Err("数据目录设置已变化，本次未切换".into());
    }
    let _destination = data_lock(&plan.destination)?;
    if plan.copy {
        if !empty(&plan.destination)? {
            return Err("新目录已有文件，未覆盖任何文件".into());
        }
        copy_tree(&plan.source, &plan.destination)?;
    }
    let location = serde_json::json!({"version": 1, "directory": plan.destination});
    write_json(&plan.root.join(LOCATOR), &location).map_err(|e| format!("无法保存新目录，原设置保留：{e}"))
}

/// `--data-transfer <plan>`: wait for the owner to release the old data,
/// switch, then always start the app again (on the old data on failure).
pub fn transfer(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(path);
    let plan: Plan = serde_json::from_slice(&bytes).map_err(|_| "迁移计划无效")?;
    fs::write(path.with_extension("ready"), b"ready").map_err(|e| e.to_string())?;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(60);
        let _source = loop {
            match data_lock(&plan.source) {
                Ok(lock) => break lock,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return Err("原后台尚未退出，目录未变更".to_string()),
            }
        };
        apply(&plan)
    })();
    let _ = fs::remove_file(path.with_extension("ready"));
    crate::owner::launch(&std::env::current_exe().map_err(|e| e.to_string())?, &plan.restart_args)?;
    result
}

#[cfg(test)]
mod tests;
