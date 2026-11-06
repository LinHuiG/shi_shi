//! Installed plugins: manifest validation, the catalog under `plugins/`, and
//! installation from a ZIP or a `plugin.json`. Nothing here loads plugin
//! code; calls go through the host's plugin worker.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{bail, Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub entrypoint: String,
    pub files: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub config_fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub api_version: u32,
    pub entrypoint: String,
    pub methods: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

pub const REQUIRED: [&str; 5] = ["discover", "validate", "fetch_incremental", "fetch_range", "lookup_messages"];
const MAX_TOTAL: u64 = 256 * 1024 * 1024;

pub fn simple(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// A declared package file: a plain file inside `base`, reached without links.
pub fn plain_file(base: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains([':', '\\']) || name.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        bail!("插件文件名必须为包内相对路径");
    }
    let mut path = base.to_path_buf();
    for component in name.split('/') {
        path.push(component);
        let meta = fs::symlink_metadata(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error(format!("插件包缺少文件“{name}”。请安装包含 DLL 和依赖文件的完整插件包；源码 ZIP 不能直接安装。"))
            }
            _ => Error(format!("无法读取插件文件“{name}”：{e}")),
        })?;
        if is_link(&meta) {
            bail!("插件包不允许链接或重解析目录");
        }
    }
    if !path.is_file() {
        bail!("插件文件不存在");
    }
    Ok(path)
}

impl Plugin {
    pub fn read(path: &Path) -> Result<Self> {
        let dir = path.parent().ok_or("插件目录无效")?.canonicalize()?;
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(path)?).map_err(|_| Error::new("不支持的插件描述或接口版本"))?;
        if manifest.api_version != 1 || !simple(&manifest.id) || manifest.name.is_empty() {
            bail!("不支持的插件描述或接口版本");
        }
        if !manifest.files.contains(&manifest.entrypoint) || !manifest.entrypoint.ends_with(".dll") {
            bail!("插件需要声明 DLL 及所需文件");
        }
        if let Some(method) = REQUIRED.iter().find(|m| !manifest.capabilities.iter().any(|c| c == *m)) {
            bail!("插件缺少 {method} 接口");
        }
        let mut keys = BTreeSet::new();
        if manifest.config_fields.iter().any(|f| {
            !simple(&f.key)
                || !keys.insert(&f.key)
                || !["file", "directory", "text", "password"].contains(&f.kind.as_str())
        }) {
            bail!("配置字段格式无效");
        }
        let mut size = 0;
        for file in &manifest.files {
            size += fs::metadata(plain_file(&dir, file)?)?.len();
        }
        if size > MAX_TOTAL {
            bail!("插件包超过 256 MiB");
        }
        if let Some(s) = &manifest.settings {
            let unique: BTreeSet<_> = s.methods.iter().collect();
            if s.api_version != 1
                || !s.entrypoint.ends_with(".html")
                || !manifest.files.contains(&s.entrypoint)
                || s.methods.is_empty()
                || unique.len() != s.methods.len()
                || !s.methods.iter().all(|m| simple(m))
            {
                bail!("插件配置页面声明无效");
            }
        }
        Ok(Self { manifest, dir })
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("plugin.json")
    }

    pub fn has(&self, method: &str) -> bool {
        self.manifest.capabilities.iter().any(|m| m == method)
    }

    pub fn validate_fields(&self, config: &Value) -> Result<()> {
        if !config.is_object() {
            bail!("配置需要是对象");
        }
        for field in &self.manifest.config_fields {
            let value = config[&field.key].as_str().unwrap_or("");
            let label = &field.label;
            if field.required && value.trim().is_empty() {
                bail!("请填写{label}");
            }
            let path = Path::new(value);
            if !value.is_empty() && field.kind == "file" && !path.is_file() {
                bail!("{label}不是有效文件");
            }
            if !value.is_empty() && field.kind == "directory" && !path.is_dir() {
                bail!("{label}不是有效目录");
            }
        }
        Ok(())
    }

    /// The request body for one plugin method.
    pub fn request(&self, method: &str, config: &Value, params: Value, work: &Path) -> Value {
        json!({
            "protocol": 1, "method": method, "config": config, "params": params,
            "context": {"plugin_dir": self.dir, "work_dir": work, "timeout_ms": 90000},
        })
    }
}

pub fn plugins_dir(root: &Path) -> PathBuf {
    root.join("plugins")
}

/// Installed plugins by id and one error line per broken `plugin.json`.
pub fn catalog(root: &Path) -> (Vec<Plugin>, Vec<String>) {
    let mut found = BTreeMap::new();
    let mut errors = vec![];
    for entry in fs::read_dir(plugins_dir(root)).into_iter().flatten().flatten() {
        let path = entry.path().join("plugin.json");
        if entry.file_name().to_string_lossy().starts_with('.') || !path.is_file() {
            continue;
        }
        match Plugin::read(&path) {
            Ok(p) => drop(found.insert(p.manifest.id.clone(), p)),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    (found.into_values().collect(), errors)
}

pub fn find(root: &Path, id: &str) -> Result<Plugin> {
    catalog(root).0.into_iter().find(|p| p.manifest.id == id).ok_or_else(|| Error::new("插件尚未安装或文件损坏"))
}

#[derive(Debug)]
pub struct Installed {
    pub manifest: Manifest,
    pub status: &'static str,
}

/// Install from a ZIP or a `plugin.json`. The caller holds the exclusive
/// plugin lock and has released the worker so no DLL is held open.
pub fn install(root: &Path, path: &Path) -> Result<Installed> {
    if !path.extension().is_some_and(|s| s.eq_ignore_ascii_case("zip")) {
        return install_manifest(root, path);
    }
    let unpack = root.join("tmp/plugin-packages").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&unpack)?;
    let result = super::zip::extract(path, &unpack).and_then(|manifest| install_manifest(root, &manifest));
    let _ = fs::remove_dir_all(&unpack);
    result
}

/// Remove only what the old manifest declared; user files stay.
pub fn clean_replaced(dir: &Path, old: &Plugin) {
    for name in old.manifest.files.iter().map(String::as_str).chain(["plugin.json"]) {
        if let Ok(path) = plain_file(dir, name) {
            let _ = fs::remove_file(path);
        }
        let path = dir.join(name);
        for parent in path.ancestors().skip(1).take_while(|p| *p != dir && p.starts_with(dir)) {
            let _ = fs::remove_dir(parent);
        }
    }
    let _ = fs::remove_dir(dir);
}

fn install_manifest(root: &Path, path: &Path) -> Result<Installed> {
    let plugin = Plugin::read(path)?;
    let parent = plugins_dir(root);
    fs::create_dir_all(&parent)?;
    let target = parent.join(&plugin.manifest.id);
    let replacing = target.exists();
    if replacing && !fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
        bail!("现有插件目录异常，未替换任何文件");
    }
    let old = Plugin::read(&target.join("plugin.json")).ok();
    let read = |p: &Plugin, name: &String| fs::read(p.dir.join(name)).ok();
    if old.as_ref().is_some_and(|old| {
        old.manifest == plugin.manifest
            && plugin.manifest.files.iter().all(|f| read(old, f).is_some() && read(old, f) == read(&plugin, f))
    }) {
        return Ok(Installed { manifest: plugin.manifest, status: "unchanged" });
    }
    let staging = parent.join(format!(".install-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        for name in &plugin.manifest.files {
            let dest = staging.join(name);
            fs::create_dir_all(dest.parent().unwrap())?;
            fs::copy(plain_file(&plugin.dir, name)?, dest)?;
        }
        fs::write(staging.join("plugin.json"), serde_json::to_vec_pretty(&plugin.manifest)?)?;
        // Swap whole directories so a failure leaves the working version.
        let backup = parent.join(format!(".previous-{}", uuid::Uuid::new_v4()));
        if replacing {
            fs::rename(&target, &backup)
                .map_err(|e| Error(format!("插件文件正在被使用或目录不可写，本次未更新，请稍后重试：{e}")))?;
        }
        if let Err(error) = fs::rename(&staging, &target) {
            if replacing && fs::rename(&backup, &target).is_err() {
                bail!("插件更新失败：{error}；旧插件保留在 {}", backup.display());
            }
            bail!("插件更新失败，旧插件未改变：{error}");
        }
        if let Some(old) = &old {
            clean_replaced(&backup, old);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.map(|_| Installed { manifest: plugin.manifest, status: if replacing { "updated" } else { "installed" } })
}

#[cfg(test)]
pub use super::testing::fixture;

#[cfg(test)]
mod tests;
