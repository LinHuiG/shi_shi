use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};
use windows_sys::Win32::System::LibraryLoader::*;

#[derive(Clone, Serialize, Deserialize)]
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
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub entrypoint: String,
    pub files: Vec<String>,
    pub capabilities: Vec<String>,
    pub config_fields: Vec<Field>,
}
#[derive(Clone)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
}
fn simple(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn relative(s: &str) -> bool {
    !s.is_empty()
        && !s.contains([':', '\\'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
fn plain_file(base: &Path, name: &str) -> Result<PathBuf, String> {
    if !relative(name) {
        return Err("插件文件名必须为包内相对路径".into());
    }
    let mut path = base.to_path_buf();
    for component in name.split('/') {
        path.push(component);
        let meta = fs::symlink_metadata(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("插件包缺少文件“{name}”。请安装包含 DLL 和依赖文件的完整插件包；源码 ZIP 不能直接安装。")
            } else {
                format!("无法读取插件文件“{name}”：{e}")
            }
        })?;
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("插件包不允许链接或重解析目录".into());
        }
    }
    if !path.is_file() {
        return Err("插件文件不存在".into());
    }
    let canonical = path.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(base.canonicalize().map_err(|e| e.to_string())?) {
        return Err("插件文件超出包目录".into());
    }
    Ok(canonical)
}
impl Plugin {
    pub fn read(path: &Path) -> Result<Self, String> {
        let dir = path
            .parent()
            .ok_or("插件目录无效")?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() > 128 * 1024 {
            return Err("插件描述文件过大".into());
        }
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if manifest.api_version != 1 || !simple(&manifest.id) || manifest.name.is_empty() {
            return Err("不支持的插件描述或接口版本".into());
        }
        if !manifest.files.contains(&manifest.entrypoint)
            || manifest.files.len() > 128
            || !manifest.entrypoint.ends_with(".dll")
        {
            return Err("插件需要声明 DLL 及所需文件".into());
        }
        for method in [
            "discover",
            "validate",
            "fetch_incremental",
            "fetch_range",
            "lookup_messages",
        ] {
            if !manifest.capabilities.iter().any(|m| m == method) {
                return Err(format!("插件缺少 {method} 接口"));
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for field in &manifest.config_fields {
            if !simple(&field.key)
                || !keys.insert(&field.key)
                || !["file", "directory", "text", "password"].contains(&field.kind.as_str())
            {
                return Err("配置字段格式无效".into());
            }
        }
        let mut size = 0;
        for file in &manifest.files {
            let path = plain_file(&dir, file)?;
            size += fs::metadata(path).map_err(|e| e.to_string())?.len();
        }
        if size > 256 * 1024 * 1024 {
            return Err("插件包超过 256 MiB".into());
        }
        Ok(Self { manifest, dir })
    }
    pub fn validate_fields(&self, config: &Value) -> Result<(), String> {
        if !config.is_object() {
            return Err("配置需要是对象".into());
        }
        for field in &self.manifest.config_fields {
            let value = config[&field.key].as_str().unwrap_or("");
            if field.required && value.trim().is_empty() {
                return Err(format!("请填写{}", field.label));
            }
            if !value.is_empty() {
                if field.kind == "file" && !Path::new(value).is_file() {
                    return Err(format!("{}不是有效文件", field.label));
                }
                if field.kind == "directory" && !Path::new(value).is_dir() {
                    return Err(format!("{}不是有效目录", field.label));
                }
            }
        }
        Ok(())
    }
    pub fn call(
        &self,
        method: &str,
        config: &Value,
        params: Value,
        work: &Path,
    ) -> Result<Value, String> {
        if !self.manifest.capabilities.iter().any(|m| m == method) {
            return Err("插件未提供此接口".into());
        }
        fs::create_dir_all(work).map_err(|e| e.to_string())?;
        let library = Library::open(&plain_file(&self.dir, &self.manifest.entrypoint)?)?;
        let request=serde_json::to_vec(&json!({"protocol":1,"method":method,"config":config,"params":params,"context":{"plugin_dir":self.dir,"work_dir":work,"timeout_ms":90000}})).map_err(|e|e.to_string())?;
        library.call(&request)
    }
}
struct Library(windows_sys::Win32::Foundation::HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.0);
        }
    }
}
impl Library {
    fn open(path: &Path) -> Result<Self, String> {
        let path = path
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "无法加载插件 DLL：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(handle))
    }
    fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize, String> {
        unsafe {
            GetProcAddress(self.0, name.as_ptr()).ok_or("插件缺少 ABI 导出函数".into())
        }
    }
    fn call(&self, input: &[u8]) -> Result<Value, String> {
        const MAX: usize = 32 * 1024 * 1024;
        if input.len() > MAX {
            return Err("插件请求超过 32 MiB".into());
        }
        unsafe {
            let abi: unsafe extern "C" fn() -> u32 =
                std::mem::transmute(self.symbol(b"shishi_plugin_abi\0")?);
            if abi() != 1 {
                return Err("插件 ABI 不兼容".into());
            }
            let call: unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8 =
                std::mem::transmute(self.symbol(b"shishi_plugin_call\0")?);
            let free: unsafe extern "C" fn(*mut u8, usize) =
                std::mem::transmute(self.symbol(b"shishi_plugin_free\0")?);
            let mut len = 0;
            let ptr = call(input.as_ptr(), input.len(), &mut len);
            if ptr.is_null() {
                return Err("插件返回空响应".into());
            }
            if len > MAX {
                free(ptr, len);
                return Err("插件响应超过 32 MiB".into());
            }
            let parsed = serde_json::from_slice::<Value>(std::slice::from_raw_parts(ptr, len));
            free(ptr, len);
            let value = parsed.map_err(|e| format!("插件响应无效：{e}"))?;
            if value["ok"] != true {
                return Err(value["error"].as_str().unwrap_or("插件调用失败").into());
            }
            Ok(value["result"].clone())
        }
    }
}
pub fn catalog(root: &Path) -> (Vec<Plugin>, Vec<String>) {
    let mut found = std::collections::BTreeMap::new();
    let mut errors = vec![];
    // No bundled or legacy executable-adjacent plugins are loaded implicitly.
    for parent in [crate::storage::data_dir(&root).join("plugins")] {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = entry.path().join("plugin.json");
                if !path.is_file() {
                    continue;
                }
                match Plugin::read(&path) {
                    Ok(p) => {
                        found.insert(p.manifest.id.clone(), p);
                    }
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
            }
        }
    }
    (found.into_values().collect(), errors)
}
pub fn find(root: &Path, id: &str) -> Result<Plugin, String> {
    catalog(root)
        .0
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or("插件尚未安装或文件损坏".into())
}
#[cfg(test)]
pub fn install(root: &Path, path: &Path) -> Result<Manifest, String> {
    install_with(root, path, |_, _| Ok(())).map(|installed| installed.manifest)
}
pub struct Installed {
    pub manifest: Manifest,
    pub status: &'static str,
}
// The caller holds plugin_access exclusively until files and application state are committed.
pub fn install_with(
    root: &Path,
    path: &Path,
    commit: impl FnOnce(&Manifest, bool) -> Result<(), String>,
) -> Result<Installed, String> {
    if path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("zip"))
        .unwrap_or(false)
    {
        return install_zip(root, path, commit);
    }
    install_manifest(root, path, commit)
}
fn install_zip(
    root: &Path,
    path: &Path,
    commit: impl FnOnce(&Manifest, bool) -> Result<(), String>,
) -> Result<Installed, String> {
    let parent = crate::storage::data_dir(&root).join("tmp/plugin-packages");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let parent = parent.canonicalize().map_err(|e| e.to_string())?;
    let unpack = parent.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&unpack).map_err(|e| e.to_string())?;
    let result = (|| {
        let system = std::env::var_os("SystemRoot").ok_or("无法定位 Windows 系统目录")?;
        let mut command = std::process::Command::new(
            PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        );
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            include_str!("../assets/unpack-plugin.ps1"),
        ]);
        command.env(
            "SHISHI_PLUGIN_PACKAGE",
            json!({"path":path.canonicalize().map_err(|e|e.to_string())?,"destination":unpack})
                .to_string(),
        );
        let (output, _) = crate::process::run(
            command,
            None,
            90,
            &crate::storage::data_dir(&root).join("tmp"),
        )
        .map_err(|e| format!("插件 ZIP 解压或校验失败：{e}"))?;
        let value: Value = serde_json::from_str(output.trim())
            .map_err(|e| format!("插件 ZIP 返回结果无效：{e}"))?;
        let manifest = PathBuf::from(value["manifest"].as_str().ok_or("ZIP 中未找到插件描述")?)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !manifest.starts_with(&unpack) {
            return Err("插件描述不在解压目录中".into());
        }
        install_manifest(root, &manifest, commit)
    })();
    // This uniquely created directory is owned by this operation; ZIP entries cannot create links.
    if unpack.parent() == Some(parent.as_path()) {
        let _ = fs::remove_dir_all(&unpack);
    }
    result
}
fn same_package(a: &Plugin, b: &Plugin) -> bool {
    use std::io::Read;
    if json!(a.manifest) != json!(b.manifest) {
        return false;
    }
    a.manifest.files.iter().all(|name| {
        let compare = || -> std::io::Result<bool> {
            let mut left = fs::File::open(a.dir.join(name))?;
            let mut right = fs::File::open(b.dir.join(name))?;
            let mut remaining = left.metadata()?.len();
            if remaining != right.metadata()?.len() {
                return Ok(false);
            }
            let (mut x, mut y) = ([0u8; 65536], [0u8; 65536]);
            while remaining > 0 {
                let count = remaining.min(x.len() as u64) as usize;
                left.read_exact(&mut x[..count])?;
                right.read_exact(&mut y[..count])?;
                if x[..count] != y[..count] {
                    return Ok(false);
                }
                remaining -= count as u64;
            }
            Ok(true)
        };
        compare().unwrap_or(false)
    })
}
fn clean_replaced(dir: &Path, old: &Plugin) {
    // Only declared plain files belong to the package. Preserve any unrelated user files.
    for name in old
        .manifest
        .files
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("plugin.json"))
    {
        if let Ok(path) = plain_file(dir, name) {
            let _ = fs::remove_file(path);
        }
        let path = dir.join(name);
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == dir || !p.starts_with(dir) {
                break;
            }
            let _ = fs::remove_dir(p);
            parent = p.parent();
        }
    }
    let _ = fs::remove_dir(dir);
}
fn install_manifest(
    root: &Path,
    path: &Path,
    commit: impl FnOnce(&Manifest, bool) -> Result<(), String>,
) -> Result<Installed, String> {
    let plugin = Plugin::read(path)?;
    let parent = crate::storage::data_dir(&root).join("plugins");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let target = parent.join(&plugin.manifest.id);
    let replacing = target.exists();
    if replacing {
        use std::os::windows::fs::MetadataExt;
        let meta = fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
            return Err("现有插件目录异常，未替换任何文件".into());
        }
    }
    let old = Plugin::read(&target.join("plugin.json")).ok();
    if old.as_ref().is_some_and(|old| same_package(old, &plugin)) {
        return Ok(Installed {
            manifest: plugin.manifest,
            status: "unchanged",
        });
    }
    let staging = parent.join(format!(".install-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
    let result = (|| {
        for name in &plugin.manifest.files {
            let dest = staging.join(name);
            fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::copy(plain_file(&plugin.dir, name)?, dest).map_err(|e| e.to_string())?;
        }
        fs::write(
            staging.join("plugin.json"),
            serde_json::to_vec_pretty(&plugin.manifest).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        Plugin::read(&staging.join("plugin.json"))?;
        let backup = parent.join(format!(".previous-{}", uuid::Uuid::new_v4()));
        if replacing {
            fs::rename(&target, &backup).map_err(|e| {
                format!("插件文件正在被使用或目录不可写，本次未更新，请稍后重试：{e}")
            })?;
        }
        if let Err(error) = fs::rename(&staging, &target) {
            if replacing {
                fs::rename(&backup, &target).map_err(|restore| {
                    format!(
                        "更新失败：{error}；旧插件保留在 {}，恢复失败：{restore}",
                        backup.display()
                    )
                })?;
            }
            return Err(format!("插件更新失败，旧插件未改变：{error}"));
        }
        if let Err(error) = commit(&plugin.manifest, replacing) {
            fs::rename(&target, &staging).map_err(|rollback| {
                format!(
                    "保存失败：{error}；撤销更新失败：{rollback}；旧插件保留在 {}",
                    backup.display()
                )
            })?;
            if replacing {
                fs::rename(&backup, &target).map_err(|rollback| {
                    format!(
                        "保存失败：{error}；旧插件保留在 {}，恢复失败：{rollback}",
                        backup.display()
                    )
                })?;
            }
            return Err(format!("{error}；插件安装已撤销"));
        }
        if let Some(old) = old {
            clean_replaced(&backup, &old);
        }
        Ok(Installed {
            manifest: plugin.manifest,
            status: if replacing { "updated" } else { "installed" },
        })
    })();
    if result.is_err() && staging.starts_with(&parent) {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Set SHISHI_TEST_PACKAGE to an external plugin ZIP; validates without loading its code"]
    fn external_plugin_zip_installs_and_keeps_manifest_identity() {
        let package = std::env::var_os("SHISHI_TEST_PACKAGE").expect("plugin ZIP path");
        let root = std::env::temp_dir().join(format!("shishi-package-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        if let Some(previous) = std::env::var_os("SHISHI_TEST_PREVIOUS_PACKAGE") {
            let old = install(&root, Path::new(&previous)).unwrap();
            let result = install_with(&root, Path::new(&package), |new, replacing| {
                assert!(replacing);
                assert_eq!(old.id, new.id);
                assert_ne!(old.version, new.version);
                Ok(())
            })
            .unwrap();
            assert_eq!(result.status, "updated");
            assert_eq!(
                install_with(&root, Path::new(&package), |_, _| panic!(
                    "same ZIP must be a no-op"
                ))
                .unwrap()
                .status,
                "unchanged"
            );
        }
        let installed = install(&root, Path::new(&package)).unwrap();
        let (found, errors) = catalog(&root);
        assert!(errors.is_empty());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest.id, installed.id);
        assert_eq!(found[0].manifest.name, installed.name);
        assert_eq!(
            found[0].manifest.config_fields.len(),
            installed.config_fields.len()
        );
        println!(
            "Installed {} {} with {} fields",
            installed.name,
            installed.version,
            installed.config_fields.len()
        );
        assert_eq!(
            fs::read_dir(root.join("data/tmp/plugin-packages"))
                .unwrap()
                .count(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zip_install_accepts_root_and_nested_packages_without_loading_code() {
        for name in ["root", "nested"] {
            let root = std::env::temp_dir().join(format!("shishi-zip-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            // A non-ASCII path with shell metacharacters must be passed as data.
            let package = root.join("插件 $(literal) ' package.ZIP");
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("tests/fixtures/plugins/{name}.zip")),
                &package,
            )
            .unwrap();
            let installed = install(&root, &package).unwrap();
            assert_eq!(installed.id, "zip-fixture");
            let dir = root.join("data/plugins/zip-fixture");
            assert_eq!(
                fs::read_to_string(dir.join("plugin.dll")).unwrap(),
                "inert DLL fixture"
            );
            assert_eq!(
                fs::read_to_string(dir.join("assets/说明.txt")).unwrap(),
                "resource"
            );
            assert!(!dir.join("not-declared.txt").exists());
            assert_eq!(
                install_with(&root, &package, |_, _| panic!(
                    "unchanged package must not rewrite state"
                ))
                .unwrap()
                .status,
                "unchanged"
            );
            assert_eq!(
                fs::read_to_string(dir.join("plugin.dll")).unwrap(),
                "inert DLL fixture"
            );
            assert_eq!(
                fs::read_dir(root.join("data/tmp/plugin-packages"))
                    .unwrap()
                    .count(),
                0
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn invalid_zip_packages_leave_no_partial_installation() {
        for name in [
            "traversal",
            "multiple",
            "duplicate",
            "missing",
            "reserved",
            "symlink",
            "corrupt",
            "source",
        ] {
            let root = std::env::temp_dir().join(format!("shishi-zip-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let package = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/plugins/{name}.zip"));
            assert!(install(&root, &package).is_err(), "accepted {name}");
            assert!(!root.join("data/plugins/zip-fixture").exists());
            assert!(!root.join("data/tmp/plugin-packages/escape.txt").exists());
            assert_eq!(
                fs::read_dir(root.join("data/tmp/plugin-packages"))
                    .unwrap()
                    .count(),
                0,
                "left files for {name}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn unsafe_manifest_paths_are_rejected() {
        for path in [
            "../plugin.dll",
            "C:/plugin.dll",
            "/absolute.dll",
            "a/../../b.dll",
            "a\\b.dll",
            "a//b.dll",
        ] {
            assert!(!relative(path), "{path}");
        }
        assert!(relative("assets/library.dll"));
        assert!(!simple("../provider"));
    }
    #[test]
    fn source_archives_and_missing_binaries_have_actionable_errors() {
        let root =
            std::env::temp_dir().join(format!("shishi-package-errors-{}", uuid::Uuid::new_v4()));
        for (name, expected) in [
            ("source", "源码 ZIP"),
            ("missing", "插件包缺少文件“plugin.dll”"),
        ] {
            let package = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/plugins/{name}.zip"));
            let error = install(&root, &package)
                .err()
                .expect("invalid package accepted");
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("os error 2"), "{error}");
            assert!(!root.join("data/plugins/zip-fixture").exists());
            assert_eq!(
                fs::read_dir(root.join("data/tmp/plugin-packages"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
}
