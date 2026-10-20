//! Startup validation for application-managed state files.
//!
//! Product data from before the analysis redesign is intentionally not
//! migrated. When a managed state file cannot be loaded, startup lists the
//! exact files and offers a one-click, strictly scoped cleanup that moves them
//! into `data/legacy/<time>/` instead of deleting them. Cancelling preserves
//! every file and stops loading; the app never silently resets to defaults.
use crate::model::{State, STATE_SCHEMA_VERSION};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Files this application may own inside `<root>/data`. Cleanup is restricted
/// to this allowlist; plugin directories, external source configuration,
/// credentials, logs, reports and snapshots are never touched.
pub const CANCELLED: &str = "MANAGED_DATA_CLEANUP_CANCELLED";
/// Temp file of a save that never reached its rename. `state.json` is still the
/// authoritative previous state, so the temp is discarded instead of listed.
const INTERRUPTED_SAVE: &str = "state.next.json";
pub const MANAGED_STATE_FILES: [&str; 4] = [
    "state.json",
    "state.backup.json",
    "state.next.json",
    "legacy-state.json",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    MissingVersion,
    UnsupportedVersion(u64),
    Structural(String),
    Unreadable(String),
    ObsoleteMigration,
    InterruptedSave,
}

impl Problem {
    pub fn label(&self) -> String {
        match self {
            Problem::MissingVersion => "缺少 schema_version".into(),
            Problem::UnsupportedVersion(version) => {
                format!("不支持的 schema_version {version}（当前 {STATE_SCHEMA_VERSION}）")
            }
            Problem::Structural(error) => format!(
                "结构或字段与当前 schema 不兼容：{}",
                error.chars().take(240).collect::<String>()
            ),
            Problem::Unreadable(error) => format!("无法读取：{error}"),
            Problem::ObsoleteMigration => "旧版迁移副本，当前版本不再使用".into(),
            Problem::InterruptedSave => {
                "未完成保存的临时文件；正式文件缺失或无效，它可能是唯一副本".into()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct IncompatibleFile {
    /// Path relative to the app root, using `/` for stable display.
    pub relative: String,
    pub problem: Problem,
    /// Content fingerprint at inspection time. Cleanup refuses to delete a file
    /// that was repaired or replaced while the dialog was open.
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Inspection {
    pub incompatible: Vec<IncompatibleFile>,
}

impl Inspection {
    pub fn is_empty(&self) -> bool {
        self.incompatible.is_empty()
    }
    pub fn message(&self) -> String {
        let rows = self
            .incompatible
            .iter()
            .map(|file| format!("• {}：{}", file.relative, file.problem.label()))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "以下应用管理的数据文件与当前版本不兼容：\n{rows}\n\n\
             点击“清理并继续”把上面列出的文件移到 data/legacy/ 下的时间目录，然后以当前版本新建数据（不迁移旧数据）。\n\
             state.json 内含已保存的事项、设置和提示词，移走后仍可在该目录找回。\n\
             点击“取消”保留全部文件并停止加载。\n\
             其他文件、插件目录、外部消息来源、配置备份和凭据不会被移动。"
        )
    }
}

fn state_dir(root: &Path) -> PathBuf {
    root.join("data")
}

/// Struct deserialization would accept a JSON array for an object field (all
/// defaults), so validate the top-level shape explicitly before deserializing.
fn shape_error(object: &serde_json::Map<String, Value>) -> Option<String> {
    let required = [
        "config",
        "tasks",
        "messages",
        "analysis_memory",
        "work_scheduler",
        "tag_state",
        "audit",
        "suggestions",
    ];
    for key in required {
        if !object.contains_key(key) {
            return Some(format!("缺少必填段 {key}"));
        }
    }
    let object_field = [
        "config",
        "messages",
        "analysis_memory",
        "tag_state",
        "handling",
    ];
    let array_field = [
        "tasks",
        "schedules",
        "audit",
        "suggestions",
        "archive_attempts",
        "archive_versions",
        "prompt_versions",
        "prompt_reviews",
        "chat_sources",
        "reports",
    ];
    for key in object_field {
        if object.get(key).is_some_and(|value| !value.is_object()) {
            return Some(format!("{key} 必须是对象"));
        }
    }
    for key in array_field {
        if object.get(key).is_some_and(|value| !value.is_array()) {
            return Some(format!("{key} 必须是数组"));
        }
    }
    if let Some(tasks) = object.get("tasks").and_then(Value::as_array) {
        for task in tasks {
            if task
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Some("任务记录缺少非空 id".into());
            }
            if task.get("title").and_then(Value::as_str).is_none() {
                return Some("任务记录缺少 title".into());
            }
            if !task.get("evidence").is_some_and(Value::is_object) {
                return Some("任务记录缺少当前 evidence 结构".into());
            }
            if !task.get("tags").is_some_and(Value::is_array) {
                return Some("任务记录缺少 tags 数组".into());
            }
        }
    }
    None
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Strictly validate one managed file and return a content fingerprint. Managed
/// files are checked even when the primary state is valid: a stale backup or
/// obsolete migration copy is exactly what the one-click cleanup should list.
fn inspect_file(root: &Path, name: &str) -> Option<(Problem, Option<String>)> {
    let path = state_dir(root).join(name);
    if !path.exists() {
        return None;
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return Some((Problem::Unreadable(error.to_string()), None)),
    };
    let fingerprint = Some(fingerprint(&bytes));
    if name == "legacy-state.json" {
        return Some((Problem::ObsoleteMigration, fingerprint));
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return Some((Problem::Structural(error.to_string()), fingerprint)),
    };
    let Some(object) = value.as_object() else {
        return Some((Problem::Structural("根节点必须是对象".into()), fingerprint));
    };
    match object.get("schema_version") {
        None => return Some((Problem::MissingVersion, fingerprint)),
        Some(value) => match value.as_u64() {
            Some(version) if version == u64::from(STATE_SCHEMA_VERSION) => {}
            Some(version) => return Some((Problem::UnsupportedVersion(version), fingerprint)),
            None => {
                return Some((
                    Problem::Structural("schema_version 必须是正整数".into()),
                    fingerprint,
                ))
            }
        },
    }
    if let Some(error) = shape_error(object) {
        return Some((Problem::Structural(error), fingerprint));
    }
    match serde_json::from_value::<State>(value) {
        Ok(state) => state
            .work_scheduler
            .validate()
            .err()
            .map(|error| (Problem::Structural(error.to_string()), fingerprint)),
        Err(error) => Some((Problem::Structural(error.to_string()), fingerprint)),
    }
}

/// Validate every explicitly app-owned persisted state artifact, including a
/// stale backup, interrupted save temp or obsolete migration copy.
fn legacy_memory_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    let base = state_dir(root).join("ai-work");
    let Ok(entries) = fs::read_dir(&base) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("memory.md");
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            if let Ok(relative) = path.strip_prefix(root) {
                result.push((relative.to_string_lossy().replace('\\', "/"), path));
            }
        }
    }
    result
}

pub fn inspect(root: &Path) -> Inspection {
    let mut incompatible = Vec::new();
    let mut listed = std::collections::BTreeSet::new();
    for name in MANAGED_STATE_FILES {
        // An unfinished save's temp is irrelevant while the committed state
        // is valid; otherwise it may be the only recoverable copy, so list it.
        if name == INTERRUPTED_SAVE && committed_state_valid(root) {
            continue;
        }
        let found = inspect_file(root, name).or_else(|| {
            // A loadable temp is still listed: the next save would overwrite it.
            let bytes = fs::read(state_dir(root).join(name)).ok()?;
            (name == INTERRUPTED_SAVE)
                .then(|| (Problem::InterruptedSave, Some(fingerprint(&bytes))))
        });
        if let Some((problem, fingerprint)) = found {
            let relative = format!("data/{name}");
            listed.insert(relative.clone());
            incompatible.push(IncompatibleFile {
                relative,
                problem,
                fingerprint,
            });
        }
    }
    if !committed_state_valid(root) && inspect_file(root, "state.backup.json").is_none() {
        if let Ok(bytes) = fs::read(state_dir(root).join("state.backup.json")) {
            let relative = "data/state.backup.json".to_string();
            if listed.insert(relative.clone()) {
                incompatible.push(IncompatibleFile {
                    relative,
                    problem: Problem::InterruptedSave,
                    fingerprint: Some(fingerprint(&bytes)),
                });
            }
        }
    }
    // The pre-redesign shared memory file is no longer imported. If it exists
    // under the app-managed analysis work area, list it for explicit cleanup;
    // an absolute/custom external workdir is left untouched.
    let mut memory_files = legacy_memory_files(root);
    if let Ok(state) = load_state(root) {
        if let Ok(directory) = crate::ai::analysis_workdir(root, &state.config) {
            if directory.starts_with(state_dir(root)) {
                memory_files.push((
                    directory.join("memory.md").to_string_lossy().to_string(),
                    directory.join("memory.md"),
                ));
            }
        }
    }
    for (_, path) in memory_files {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.ends_with("/memory.md")
            && relative.starts_with("data/")
            && listed.insert(relative.clone())
        {
            let bytes = fs::read(&path).unwrap_or_default();
            incompatible.push(IncompatibleFile {
                relative,
                problem: Problem::ObsoleteMigration,
                fingerprint: Some(fingerprint(&bytes)),
            });
        }
    }
    Inspection { incompatible }
}

/// Load the authoritative state with explicit schema gating. No `serde(default)`
/// layer can turn an old or malformed file into a fresh default state here.
pub fn load_state(root: &Path) -> Result<State, String> {
    let path = state_dir(root).join("state.json");
    if !path.exists() {
        return Ok(State::default());
    }
    let bytes = fs::read(&path).map_err(|e| format!("数据文件无法读取：{e}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("数据文件无法解析，保留原文件并停止加载：{e}"))?;
    let object = value
        .as_object()
        .ok_or("数据文件根节点必须是对象，保留原文件并停止加载")?;
    if let Some(error) = shape_error(object) {
        return Err(format!(
            "数据文件结构不兼容（{error}），保留原文件并停止加载；请通过启动清理入口处理 data/state.json"
        ));
    }
    let Some(version) = object.get("schema_version") else {
        return Err(format!(
            "数据文件缺少 schema_version，当前版本不迁移旧数据；请通过启动清理入口处理 data/state.json"
        ));
    };
    let version = version
        .as_u64()
        .filter(|version| *version > 0)
        .ok_or("数据文件 schema_version 无效，停止加载")?;
    if version != u64::from(STATE_SCHEMA_VERSION) {
        return Err(format!(
            "数据文件 schema_version {version} 不受支持（当前 {}），停止加载",
            STATE_SCHEMA_VERSION
        ));
    }
    let state = serde_json::from_value::<State>(value).map_err(|e| {
        format!(
            "数据文件结构不兼容，保留原文件并停止加载：{e}；请通过启动清理入口处理 data/state.json"
        )
    })?;
    state
        .work_scheduler
        .validate()
        .map_err(|e| format!("数据文件嵌套状态版本不兼容，停止加载：{e}"))?;
    Ok(state)
}

fn resolve_managed(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let name = relative
        .strip_prefix("data/")
        .ok_or_else(|| format!("清理范围之外：{relative}"))?;
    let parts = name.split('/').collect::<Vec<_>>();
    let legacy_memory = parts.len() >= 3
        && parts[0] == "ai-work"
        && parts.last() == Some(&"memory.md")
        && parts[1..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && *part != "." && *part != "..");
    if !MANAGED_STATE_FILES.contains(&name) && !legacy_memory {
        return Err(format!("拒绝清理未登记文件：{relative}"));
    }
    let data = fs::canonicalize(state_dir(root))
        .map_err(|e| format!("应用数据目录不可用，未删除任何文件：{e}"))?;
    let canonical_root =
        fs::canonicalize(root).map_err(|e| format!("应用目录不可用，未删除任何文件：{e}"))?;
    if !data.starts_with(&canonical_root) {
        return Err("拒绝清理：应用数据目录是指向应用目录之外的链接/junction".into());
    }
    let path = state_dir(root).join(name);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|e| format!("无法检查 {relative}，未删除任何文件：{e}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "拒绝清理符号链接、junction 或非普通文件：{relative}"
        ));
    }
    let canonical =
        fs::canonicalize(&path).map_err(|e| format!("无法解析 {relative}，未删除任何文件：{e}"))?;
    let expected_parent = if legacy_memory {
        let room = name
            .strip_prefix("ai-work/")
            .and_then(|rest| rest.strip_suffix("/memory.md"))
            .unwrap_or("");
        fs::canonicalize(data.join("ai-work").join(room))
            .map_err(|e| format!("无法解析旧共享笔记目录 {relative}，未删除任何文件：{e}"))?
    } else {
        data.clone()
    };
    if !expected_parent.starts_with(&data) {
        return Err(format!("拒绝清理应用数据目录之外的文件：{relative}"));
    }
    if canonical.parent() != Some(expected_parent.as_path()) {
        return Err(format!("拒绝清理应用数据目录之外的文件：{relative}"));
    }
    Ok(canonical)
}

/// Move exactly the files listed in the inspection into one timestamped
/// `data/legacy/<time>/` directory, keeping their relative layout. Each path is
/// revalidated against the managed allowlist and the canonical `data` directory
/// first. Nothing is deleted, so a mistaken confirmation stays recoverable.
pub fn cleanup(root: &Path, inspection: &Inspection) -> Result<Vec<String>, String> {
    // Resolve every target and verify its content identity before moving any.
    // A file repaired or replaced while the dialog was open is preserved.
    let mut targets = Vec::new();
    for file in &inspection.incompatible {
        let path = resolve_managed(root, &file.relative)?;
        let bytes = fs::read(&path)
            .map_err(|e| format!("无法再次读取 {}，未移动任何文件：{e}", file.relative))?;
        let current = fingerprint(&bytes);
        if file.fingerprint.as_deref() != Some(current.as_str()) {
            return Err(format!(
                "{} 在确认期间已被修改或替换，未移动任何文件；请重启后重新确认",
                file.relative
            ));
        }
        targets.push((file, path));
    }
    // Phase 1: copy every target and verify the copy's fingerprint. Any
    // failure stops here with every original still in place.
    let data = fs::canonicalize(state_dir(root)).map_err(|e| e.to_string())?;
    let legacy = data.join("legacy");
    if legacy.exists()
        && (fs::symlink_metadata(&legacy)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
            || fs::canonicalize(&legacy)
                .map_err(|e| e.to_string())?
                .parent()
                != Some(data.as_path()))
    {
        return Err("历史备份目录越界，所有原文件保留".into());
    }
    let archive = legacy_dir(root);
    for (file, path) in &targets {
        let inner = file
            .relative
            .strip_prefix("data/")
            .unwrap_or(&file.relative);
        let destination = archive.join(inner);
        let copied = destination
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| fs::copy(path, &destination))
            .and_then(|_| fs::read(&destination));
        match copied {
            Ok(bytes) if file.fingerprint.as_deref() == Some(fingerprint(&bytes).as_str()) => {}
            Ok(_) => {
                return Err(format!(
                    "备份 {} 校验不一致，未移出任何文件；备份目录：{}",
                    file.relative,
                    archive.display()
                ))
            }
            Err(error) => {
                return Err(format!(
                    "备份 {} 失败：{error}；未移出任何文件",
                    file.relative
                ))
            }
        }
    }
    // Phase 2: every file is backed up and verified; only now remove them
    // from the active location.
    let mut moved = Vec::new();
    for (file, path) in targets {
        if let Err(error) = fs::remove_file(&path) {
            let done = if moved.is_empty() {
                "尚未移出任何文件".to_string()
            } else {
                format!("已移出：{}", moved.join("、"))
            };
            return Err(format!(
                "移出 {} 失败：{error}；{done}；全部文件已备份到 {}",
                file.relative,
                archive.display()
            ));
        }
        moved.push(file.relative.clone());
    }
    Ok(moved)
}

fn legacy_dir(root: &Path) -> PathBuf {
    let base = state_dir(root).join("legacy");
    // App-wide UTC+8 stamp; never depends on the host time-zone database.
    let stamp = crate::model::stamp(crate::model::now())
        .replace(['-', ':'], "")
        .replace(' ', "-");
    let mut path = base.join(&stamp);
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = base.join(format!("{stamp}-{n}"));
    }
    path
}

/// Discard the temp file of an interrupted save, but only while the committed
/// `state.json` exists and is valid. With a missing or invalid primary the
/// temp is kept and listed for the explicit backup-and-clean flow.
fn committed_state_valid(root: &Path) -> bool {
    state_dir(root).join("state.json").is_file() && inspect_file(root, "state.json").is_none()
}

fn discard_interrupted_save(root: &Path) {
    if !committed_state_valid(root) {
        return;
    }
    let path = state_dir(root).join(INTERRUPTED_SAVE);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
        let _ = fs::remove_file(path);
    }
}

/// Startup entrypoint. `choose_cleanup` receives the exact message and returns
/// true for the one-click cleanup, false for "cancel and stop". Production uses
/// a native dialog; tests inject a deterministic choice.
pub fn startup_with(
    root: &Path,
    smoke: bool,
    choose_cleanup: impl FnOnce(&str) -> bool,
) -> Result<(), String> {
    if !smoke {
        discard_interrupted_save(root);
    }
    let inspection = inspect(root);
    if inspection.is_empty() {
        return Ok(());
    }
    let message = inspection.message();
    if smoke {
        return Err(format!(
            "{message}\n\n自检模式不会弹出对话框，也不会移动或删除文件。"
        ));
    }
    if choose_cleanup(&message) {
        cleanup(root, &inspection)?;
        Ok(())
    } else {
        Err(format!(
            "{CANCELLED}\n{message}\n\n已取消清理：所有文件均已保留，程序停止加载。"
        ))
    }
}

pub fn native_choice(message: &str) -> bool {
    rfd::MessageDialog::new()
        .set_title("拾事 · 数据文件不兼容")
        .set_description(message)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "清理并继续".into(),
            "取消（保留文件）".into(),
        ))
        .show()
        == rfd::MessageDialogResult::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{id, now};
    use serde_json::json;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("shishi-managed-{}", id()))
    }

    fn write_state(root: &Path, value: Value) {
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn missing_version_unsupported_version_and_structure_are_distinct() {
        let root = root();
        write_state(&root, json!({"tasks":[]}));
        assert_eq!(
            inspect(&root).incompatible[0].problem,
            Problem::MissingVersion
        );
        write_state(&root, json!({"schema_version":99}));
        assert_eq!(
            inspect(&root).incompatible[0].problem,
            Problem::UnsupportedVersion(99)
        );
        write_state(
            &root,
            json!({"schema_version":1,"tasks":[{"sources":["old"]}]}),
        );
        assert!(matches!(
            inspect(&root).incompatible[0].problem,
            Problem::Structural(_)
        ));
        write_state(&root, json!({"schema_version":1,"config":[],"tasks":[]}));
        assert!(matches!(
            inspect(&root).incompatible[0].problem,
            Problem::Structural(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_schema_round_trips_without_migration_and_custom_limit_survives_restart() {
        let root = root();
        let mut state = State::default();
        state.config.max_input_chars = 75_000;
        state.config.classify_prompt = "保留的自定义提示词".into();
        state.messages.insert(
            "m".into(),
            crate::model::Message {
                id: "m".into(),
                text: "合成消息".into(),
                time: now(),
                ..Default::default()
            },
        );
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let loaded = load_state(&root).unwrap();
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.config.max_input_chars, 75_000);
        assert_eq!(loaded.config.classify_prompt, "保留的自定义提示词");
        assert!(inspect(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_only_removes_listed_managed_files_and_preserves_unrelated_data() {
        let root = root();
        write_state(
            &root,
            json!({"schema_version":1,"tasks":[{"id":"t","sources":["old"]}]}),
        );
        fs::write(root.join("data/settings.json"), b"external config").unwrap();
        fs::write(root.join("data/state.backup.json"), b"also incompatible").unwrap();
        let inspection = inspect(&root);
        let names = inspection
            .incompatible
            .iter()
            .map(|f| f.relative.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["data/state.json", "data/state.backup.json"]);
        let moved = cleanup(&root, &inspection).unwrap();
        assert_eq!(moved.len(), 2);
        assert!(!root.join("data/state.json").exists());
        assert!(!root.join("data/state.backup.json").exists());
        let archived = fs::read_dir(root.join("data/legacy"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect::<Vec<_>>();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].join("state.json").is_file());
        assert_eq!(
            fs::read(archived[0].join("state.backup.json")).unwrap(),
            b"also incompatible"
        );
        assert!(inspect(&root).is_empty());
        assert_eq!(
            fs::read(root.join("data/settings.json")).unwrap(),
            b"external config"
        );
        assert!(matches!(
            resolve_managed(&root, "data/../prompts.json"),
            Err(_)
        ));
        assert!(matches!(
            resolve_managed(&root, "data/unrelated.json"),
            Err(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_managed_artifacts_are_inspected_and_legacy_memory_is_explicitly_cleaned() {
        let root = root();
        let mut state = State::default();
        state.config.max_input_chars = 65_000;
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fs::write(root.join("data/state.backup.json"), b"{\"tasks\":[]}").unwrap();
        fs::create_dir_all(root.join("data/ai-work/ShiShi")).unwrap();
        fs::write(root.join("data/ai-work/ShiShi/memory.md"), b"old note").unwrap();
        fs::write(root.join("data/ai-work/ShiShi/keep.txt"), b"keep").unwrap();
        let inspection = inspect(&root);
        let names = inspection
            .incompatible
            .iter()
            .map(|file| file.relative.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"data/state.backup.json"));
        assert!(names.contains(&"data/ai-work/ShiShi/memory.md"));
        cleanup(&root, &inspection).unwrap();
        assert!(root.join("data/state.json").exists());
        assert!(!root.join("data/state.backup.json").exists());
        assert!(!root.join("data/ai-work/ShiShi/memory.md").exists());
        assert!(root.join("data/ai-work/ShiShi/keep.txt").exists());
        let archived = fs::read_dir(root.join("data/legacy"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        assert_eq!(
            fs::read(archived.join("ai-work/ShiShi/memory.md")).unwrap(),
            b"old note"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_sections_and_task_ids_are_not_silently_defaulted() {
        let root = root();
        write_state(&root, json!({"schema_version":1}));
        assert!(matches!(
            inspect(&root).incompatible[0].problem,
            Problem::Structural(_)
        ));
        write_state(
            &root,
            json!({"schema_version":1,"config":{},"messages":{},"tasks":[{"title":"无 id"}]}),
        );
        assert!(matches!(
            inspect(&root).incompatible[0].problem,
            Problem::Structural(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_rechecks_content_identity_before_deleting() {
        let root = root();
        write_state(&root, json!({"tasks":[]}));
        let inspection = inspect(&root);
        write_state(
            &root,
            json!({"schema_version":1,"config":{},"messages":{},"tasks":[]}),
        );
        let error = cleanup(&root, &inspection).unwrap_err();
        assert!(error.contains("确认期间已被修改") || error.contains("无法再次读取"));
        assert!(root.join("data/state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn data_directory_junction_outside_root_is_rejected_before_cleanup() {
        // Windows-only regression with a real reparse point (junction), never a
        // simulated path. Both directories are isolated synthetic fixtures.
        let root = root();
        let outside = root.with_extension("outside-data");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("state.json"), b"{\"tasks\":[]}").unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("data"))
            .arg(&outside)
            .output()
            .expect("run mklink");
        assert!(
            output.status.success(),
            "could not create synthetic junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let inspection = inspect(&root);
        let error = cleanup(&root, &inspection).unwrap_err();
        assert!(
            error.contains("链接/junction") || error.contains("目录之外"),
            "{error}"
        );
        assert!(outside.join("state.json").exists());
        // Remove the junction itself before deleting the roots so cleanup of
        // the synthetic fixture can never traverse into the target directory.
        let _ = fs::remove_dir(root.join("data"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn cancel_preserves_every_file_and_stops_loading_and_smoke_never_deletes() {
        let root = root();
        write_state(&root, json!({"tasks":[]}));
        let error = startup_with(&root, false, |_| false).unwrap_err();
        assert!(error.contains("已取消清理"));
        assert!(root.join("data/state.json").exists());
        let error = startup_with(&root, true, |_| true).unwrap_err();
        assert!(error.contains("自检模式"));
        assert!(root.join("data/state.json").exists());
        assert!(startup_with(&root, false, |_| true).is_ok());
        assert!(!root.join("data/state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_save_temp_never_blocks_startup() {
        let root = root();
        write_state(&root, serde_json::to_value(State::default()).unwrap());
        fs::write(root.join("data/state.next.json"), b"{\"sche").unwrap();
        assert!(inspect(&root).is_empty());
        assert!(startup_with(&root, true, |_| false).is_ok());
        assert!(
            root.join("data/state.next.json").exists(),
            "smoke never deletes"
        );
        assert!(startup_with(&root, false, |_| false).is_ok());
        assert!(!root.join("data/state.next.json").exists());
        assert!(root.join("data/state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_save_temp_is_kept_when_the_committed_state_is_missing_or_invalid() {
        let root = root();
        fs::create_dir_all(root.join("data")).unwrap();
        let candidate = serde_json::to_vec(&State::default()).unwrap();
        fs::write(root.join("data/state.next.json"), &candidate).unwrap();
        let names = |root: &Path| {
            inspect(root)
                .incompatible
                .into_iter()
                .map(|f| f.relative)
                .collect::<Vec<_>>()
        };
        // Missing primary: the temp may be the only copy, so it is listed.
        assert_eq!(names(&root), vec!["data/state.next.json"]);
        assert!(startup_with(&root, false, |_| false).is_err());
        assert!(root.join("data/state.next.json").exists());
        // Invalid primary: both listed; confirming backs both up first.
        fs::write(root.join("data/state.json"), b"not json").unwrap();
        assert_eq!(
            names(&root),
            vec!["data/state.json", "data/state.next.json"]
        );
        assert!(startup_with(&root, false, |_| true).is_ok());
        let archived = fs::read_dir(root.join("data/legacy"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        assert_eq!(
            fs::read(archived.join("state.next.json")).unwrap(),
            candidate
        );
        assert_eq!(fs::read(archived.join("state.json")).unwrap(), b"not json");
        fs::remove_dir_all(root).unwrap();
    }
}
