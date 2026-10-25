//! Data location is separate from the executable. The locator is committed only
//! after the old process has exited and a complete copy has been written.
use crate::engine::App;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{fs::MetadataExt, process::CommandExt},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{atomic::Ordering, Mutex, OnceLock},
    time::{Duration, Instant},
};

const LOCATOR: &str = "data-location.json";
static LOCATIONS: OnceLock<Mutex<BTreeMap<PathBuf, PathBuf>>> = OnceLock::new();
fn locations() -> &'static Mutex<BTreeMap<PathBuf, PathBuf>> {
    LOCATIONS.get_or_init(Default::default)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Location {
    version: u32,
    directory: PathBuf,
}

/// Registered at startup, never changed in a running application. Tests without
/// a locator retain the original <root>/data layout. MCP processes register the
/// same executable root before resolving tickets and snapshots.
pub fn data_dir(root: &Path) -> PathBuf {
    locations()
        .lock()
        .unwrap()
        .get(root)
        .cloned()
        .unwrap_or_else(|| root.join("data"))
}
pub fn initialize(root: &Path) -> Result<(), String> {
    let directory = match fs::read(root.join(LOCATOR)) {
        Ok(bytes) => {
            let location: Location = serde_json::from_slice(&bytes)
                .map_err(|_| "数据目录设置损坏，保留原文件并停止启动")?;
            if location.version != 1 || !location.directory.is_absolute() {
                return Err("数据目录设置版本或路径无效，停止启动".into());
            }
            plain_directory(&location.directory)?;
            if !location.directory.is_dir() {
                return Err(
                    "已配置的数据目录不可用，请恢复该目录或检查 data-location.json；未创建新数据"
                        .into(),
                );
            }
            location.directory
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => root.join("data"),
        Err(error) => return Err(format!("无法读取数据目录设置：{error}")),
    };
    locations()
        .lock()
        .unwrap()
        .insert(root.to_path_buf(), directory);
    Ok(())
}

/// Explicit locations may be outside the installation. Reparse points anywhere
/// in the path remain forbidden, including the old implicit data directory.
pub fn plain_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("数据目录必须是完整路径，不能包含 ..".into());
    }
    let mut part = PathBuf::new();
    for component in path.components() {
        part.push(component);
        match fs::symlink_metadata(&part) {
            Ok(meta) if meta.file_attributes() & 0x400 != 0 || !meta.is_dir() => {
                return Err("数据目录不能经过符号链接、junction 或普通文件".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法检查数据目录：{error}")),
        }
    }
    Ok(())
}
fn canonical(path: &Path) -> Result<PathBuf, String> {
    let path = path
        .canonicalize()
        .map_err(|error| format!("数据目录不可用：{error}"))?;
    let text = path.to_string_lossy();
    Ok(if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{unc}"))
    } else if let Some(local) = text.strip_prefix("\\\\?\\") {
        PathBuf::from(local)
    } else {
        path
    })
}
fn nested(a: &Path, b: &Path) -> bool {
    // Windows directory identity is insensitive to slash spelling and case.
    let key = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let a = key(a);
    let b = key(b);
    a == b || a.starts_with(&(b.trim_end_matches('\\').to_string() + "\\"))
}
pub fn transition_lock(root: &Path) -> Result<File, String> {
    transition_access(root, false)
}
pub fn transition_read(root: &Path) -> Result<File, String> {
    transition_access(root, true)
}
fn transition_access(root: &Path, shared: bool) -> Result<File, String> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(root.join(".data-location.lock"))
        .map_err(|e| e.to_string())?;
    (if shared {
        FileExt::try_lock_shared(&file)
    } else {
        file.try_lock_exclusive()
    })
    .map_err(|_| "正在切换数据目录，请等待迁移窗口完成后重试")?;
    Ok(file)
}
fn data_lock(directory: &Path) -> Result<File, String> {
    if fs::symlink_metadata(directory.join("app.lock"))
        .is_ok_and(|m| m.file_attributes() & 0x400 != 0)
    {
        return Err("数据目录锁不能是链接或 junction".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(directory.join("app.lock"))
        .map_err(|e| e.to_string())?;
    file.try_lock_exclusive()
        .map_err(|_| "所选数据目录正在被另一个拾事实例使用")?;
    Ok(file)
}
pub fn lock_source(directory: &Path) -> Result<File, String> {
    data_lock(directory)
}
fn target(root: &Path, value: &Value, copy: bool) -> Result<PathBuf, String> {
    let path = PathBuf::from(value["directory"].as_str().ok_or("请选择数据目录")?);
    plain_directory(&path)?;
    let to = canonical(&path)?;
    let from = canonical(&data_dir(root))?;
    if nested(&to, &from) || nested(&from, &to) {
        return Err("新旧数据目录不能相同，也不能互相包含".into());
    }
    if nested(&canonical(root)?, &to) {
        return Err("数据目录不能是程序目录或它的上级目录，请选择独立文件夹".into());
    }
    let _lock = data_lock(&to)?;
    if copy && !empty_target(&to)? {
        return Err("迁移需要空文件夹，所选目录已有文件；未覆盖任何文件".into());
    }
    if !copy {
        validate_state_file(&to)?;
    }
    Ok(to)
}
fn empty_target(directory: &Path) -> Result<bool, String> {
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        if entry.map_err(|e| e.to_string())?.file_name() != "app.lock" {
            return Ok(false);
        }
    }
    Ok(true)
}
fn validate_state_file(directory: &Path) -> Result<(), String> {
    crate::managed_data::load_from_directory(directory)?;
    crate::prompt_config::validate_directory(directory)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TransferPlan {
    pub root: PathBuf,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub copy: bool,
    pub parent_pid: u32,
    pub parent_rect: [i32; 4],
    pub restart_args: Vec<String>,
    expected_locator: Option<Vec<u8>>,
}
#[derive(Default, Clone)]
pub struct Progress {
    pub total: u64,
    pub copied: u64,
    pub phase: String,
    pub done: bool,
    pub error: String,
}
impl Progress {
    pub fn percent(&self) -> u32 {
        if self.done && self.error.is_empty() {
            100
        } else if self.total == 0 {
            0
        } else {
            ((u128::from(self.copied) * 100 / u128::from(self.total)).min(99)) as u32
        }
    }
}
fn read_locator(root: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(root.join(LOCATOR)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub fn begin(app: &App, params: &Value, parent_rect: [i32; 4]) -> Result<(), String> {
    if app.storage_changing.swap(true, Ordering::SeqCst) {
        return Err("数据目录变更已开始，请等待迁移窗口".into());
    }
    let result = (|| {
        if !app.storage_ready() {
            return Err("扫描或 AI 任务正在进行，请完成或停止后再更改数据目录".into());
        }
        app.log_writer.flush()?;
        let copy = params["copy"].as_bool().ok_or("请选择是否迁移当前数据")?;
        let destination = target(&app.root, params, copy)?;
        let plan = TransferPlan {
            root: app.root.clone(),
            source: canonical(&data_dir(&app.root))?,
            destination,
            copy,
            parent_pid: std::process::id(),
            parent_rect,
            restart_args: std::env::args().skip(1).collect(),
            expected_locator: read_locator(&app.root)?,
        };
        // Not inside data: it must survive both the copy and cache exclusion.
        let job = app
            .root
            .join(format!(".data-transfer-{}.json", crate::model::id()));
        write_new(&job, &serde_json::to_vec(&plan).map_err(|e| e.to_string())?)?;
        let mut child = match Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .arg("--data-transfer")
            .arg(&job)
            .creation_flags(0x08000000)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&job);
                return Err(format!("迁移窗口无法启动，目录未变更：{error}"));
            }
        };
        let ready = job.with_extension("ready");
        let start = Instant::now();
        loop {
            if ready.is_file() {
                return Ok(());
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                let _ = fs::remove_file(&job);
                return Err(format!("迁移窗口启动失败（{status}），目录未变更"));
            }
            if start.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&job);
                return Err("迁移窗口启动超时，目录未变更".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })();
    if result.is_err() {
        app.storage_changing.store(false, Ordering::SeqCst);
    }
    result
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())
}

#[derive(Clone, PartialEq, Eq)]
struct CopyEntry {
    relative: PathBuf,
    size: u64,
    modified: std::time::SystemTime,
}
fn inventory(
    base: &Path,
    relative: &Path,
    entries: &mut Vec<CopyEntry>,
    directories: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for entry in fs::read_dir(base.join(relative)).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = relative.join(entry.file_name());
        if relative.as_os_str().is_empty()
            && ["app.lock", "webview", "tmp"]
                .contains(&entry.file_name().to_string_lossy().as_ref())
        {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("数据中含链接或 junction，请移走链接后再迁移；原目录保留".into());
        }
        if meta.is_dir() {
            directories.push(path.clone());
            inventory(base, &path, entries, directories)?;
        } else if meta.is_file() {
            entries.push(CopyEntry {
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
pub fn transfer(plan: &TransferPlan, update: impl Fn(Progress)) -> Result<(), String> {
    plain_directory(&plan.source)?;
    plain_directory(&plan.destination)?;
    if nested(&plan.source, &plan.destination) || nested(&plan.destination, &plan.source) {
        return Err("新旧目录范围冲突".into());
    }
    if read_locator(&plan.root)? != plan.expected_locator {
        return Err("数据目录设置已变化，本次未切换".into());
    }
    initialize(&plan.root)?;
    if canonical(&data_dir(&plan.root))? != canonical(&plan.source)? {
        return Err("原数据目录与迁移计划不匹配".into());
    }
    if nested(&canonical(&plan.root)?, &plan.destination) {
        return Err("数据目录不能包含程序目录".into());
    }
    // Caller owns source app.lock and the transition lock until the locator is
    // committed; target app.lock blocks other installations using this folder.
    let _destination_lock = data_lock(&plan.destination)?;
    let mut final_progress = Progress::default();
    if plan.copy {
        if !empty_target(&plan.destination)? {
            return Err("新目录已有文件，未覆盖任何文件".into());
        }
        crate::state_store::checkpoint(&plan.source)?;
        let mut entries = Vec::new();
        let mut directories = Vec::new();
        inventory(&plan.source, Path::new(""), &mut entries, &mut directories)?;
        entries.sort_by(|a, b| a.relative.cmp(&b.relative));
        directories.sort();
        let before = (entries.clone(), directories.clone());
        let total = entries
            .iter()
            .try_fold(0u64, |n, e| n.checked_add(e.size))
            .ok_or("数据总量过大")?;
        let mut progress = Progress {
            total,
            phase: "正在迁移数据".into(),
            ..Default::default()
        };
        update(progress.clone());
        for directory in directories {
            fs::create_dir(plan.destination.join(directory)).map_err(|e| e.to_string())?;
        }
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut verify = vec![0u8; 1024 * 1024];
        for entry in entries {
            let from = plan.source.join(&entry.relative);
            let mut source = File::open(&from).map_err(|e| e.to_string())?;
            let mut dest = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(plan.destination.join(&entry.relative))
                .map_err(|e| e.to_string())?;
            let mut copied = 0;
            loop {
                let n = source.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                dest.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
                copied += n as u64;
                progress.copied += n as u64;
                update(progress.clone());
            }
            dest.sync_all().map_err(|e| e.to_string())?;
            source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            dest.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            loop {
                let n = source.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                dest.read_exact(&mut verify[..n])
                    .map_err(|e| e.to_string())?;
                if buffer[..n] != verify[..n] {
                    return Err("复制校验失败，未切换目录；原数据保留".into());
                }
            }
            let meta = source.metadata().map_err(|e| e.to_string())?;
            if copied != entry.size
                || meta.len() != entry.size
                || meta.modified().ok() != Some(entry.modified)
            {
                return Err("复制期间源文件发生变化，未切换目录；原数据保留".into());
            }
        }
        let mut after_entries = Vec::new();
        let mut after_directories = Vec::new();
        inventory(
            &plan.source,
            Path::new(""),
            &mut after_entries,
            &mut after_directories,
        )?;
        after_entries.sort_by(|a, b| a.relative.cmp(&b.relative));
        after_directories.sort();
        if before != (after_entries, after_directories) {
            return Err("源目录在迁移期间发生变化，未切换目录；原数据保留".into());
        }
        relocate_workdir(plan)?;
        final_progress.total = total;
        final_progress.copied = total;
    }
    validate_state_file(&plan.destination)?;
    if read_locator(&plan.root)? != plan.expected_locator {
        return Err("数据目录设置已变化，本次未切换".into());
    }
    let next = plan
        .root
        .join(format!(".data-location-{}.next", crate::model::id()));
    let location = Location {
        version: 1,
        directory: plan.destination.clone(),
    };
    write_new(
        &next,
        &serde_json::to_vec_pretty(&location).map_err(|e| e.to_string())?,
    )?;
    fs::rename(&next, plan.root.join(LOCATOR)).map_err(|e| {
        let _ = fs::remove_file(&next);
        format!("无法保存新目录，原设置保留：{e}")
    })?;
    final_progress.phase = "目录已更新，正在重启拾事".into();
    final_progress.done = true;
    update(final_progress);
    Ok(())
}
fn relocate_workdir(plan: &TransferPlan) -> Result<(), String> {
    // Only a managed AI directory moves. Absolute external workspaces and all
    // historical command text, messages and run cwd values are left untouched.
    let names: &[&str] = if plan.destination.join(crate::state_store::FILE).exists() {
        &[crate::state_store::FILE]
    } else {
        &["state.json", "state.backup.json"]
    };
    for &name in names {
        let path = plan.destination.join(name);
        if !path.is_file() {
            continue;
        }
        let mut state: Value = if name == crate::state_store::FILE {
            serde_json::to_value(crate::state_store::load(&plan.destination)?)
                .map_err(|e| e.to_string())?
        } else {
            serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        };
        let Some(workdir) = state["config"]["analysis_workdir"].as_str() else {
            continue;
        };
        let old_path = PathBuf::from(workdir);
        let old = canonical(&old_path).unwrap_or(old_path);
        let mut changed = false;
        if let Ok(relative) = old.strip_prefix(plan.source.join("ai-work")) {
            state["config"]["analysis_workdir"] =
                json!(plan.destination.join("ai-work").join(relative));
            changed = true;
        }
        // Only plugin-declared path fields may be rebased. Free text, passwords,
        // external source files and historical evidence are never rewritten.
        if let Some(sources) = state["chat_sources"].as_array_mut() {
            for source in sources {
                let Some(id) = source["plugin_id"].as_str() else {
                    continue;
                };
                if !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    continue;
                }
                let manifest = plan
                    .destination
                    .join("plugins")
                    .join(id)
                    .join("plugin.json");
                let fields = fs::read(&manifest)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<crate::plugins::Manifest>(&b).ok())
                    .map(|m| m.config_fields)
                    .unwrap_or_default();
                for field in fields
                    .into_iter()
                    .filter(|f| ["file", "directory"].contains(&f.kind.as_str()))
                {
                    let Some(path) = source["config"][&field.key].as_str() else {
                        continue;
                    };
                    if let Ok(path) = canonical(Path::new(path)) {
                        if let Ok(relative) = path.strip_prefix(&plan.source) {
                            source["config"][&field.key] = json!(plan.destination.join(relative));
                            changed = true;
                        }
                    }
                }
            }
        }
        if changed {
            if name == crate::state_store::FILE {
                let state = serde_json::from_value(state).map_err(|e| e.to_string())?;
                crate::state_store::Store::open(&plan.destination)?.save(&state)?;
                crate::state_store::checkpoint(&plan.destination)?;
                continue;
            }
            let mut file = File::create(path).map_err(|e| e.to_string())?;
            file.write_all(&serde_json::to_vec(&state).map_err(|e| e.to_string())?)
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub fn restart(args: &[String]) -> Result<(), String> {
    Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .args(args)
        .creation_flags(0x08000000)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("目录已切换，但自动启动失败；请重新打开拾事：{e}"))
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
