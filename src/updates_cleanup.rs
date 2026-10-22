//! Remove only stale, recognizable update-owned files; never recurse into unknown data.
use std::{
    fs, io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.file_attributes() & 0x400 == 0)
}
fn stale(path: &Path, at: SystemTime) -> bool {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| at.duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_AFTER)
}
fn uuid(text: &str) -> bool {
    uuid::Uuid::parse_str(text).is_ok()
}
fn package_file(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("Shishi-Setup-")
        .and_then(|s| s.strip_suffix(".exe"))
    else {
        return false;
    };
    rest.rsplit_once('-').is_some_and(|(version, digest)| {
        super::version(version).is_some()
            && digest.len() == 12
            && digest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}
fn temporary_file(name: &str) -> bool {
    if let Some(id) = name
        .strip_prefix("download-")
        .and_then(|s| s.strip_suffix(".part"))
    {
        return uuid(id);
    }
    name.rsplit_once('.')
        .is_some_and(|(id, ext)| uuid(id) && ["in", "out", "err"].contains(&ext))
}

pub(super) fn prune(directory: &Path, keep: Option<&Path>, at: SystemTime) -> io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    crate::storage::plain_directory(directory).map_err(io::Error::other)?;
    let parent = directory.canonicalize()?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep == Some(path.as_path()) || !stale(&path, at) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_attributes() & 0x400 != 0 {
            continue;
        }
        if metadata.is_file() && (package_file(&name) || temporary_file(&name)) {
            fs::remove_file(path)?;
        } else if metadata.is_dir() && name.strip_prefix("apply-").is_some_and(uuid) {
            prune_helper(&parent, &path)?;
        }
    }
    Ok(())
}

fn prune_helper(parent: &Path, directory: &Path) -> io::Result<()> {
    let resolved = directory.canonicalize()?;
    if resolved.parent() != Some(parent) {
        return Ok(());
    }
    let files = fs::read_dir(directory)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<PathBuf>>>()?;
    const OWNED: &[&str] = &[
        "release-helpers.ps1",
        "shishi-update.exe",
        "WebView2Loader.dll",
        "plan.json",
        "plan.ready",
        "plan.next.json",
    ];
    if files.iter().any(|path| {
        !plain_file(path)
            || !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| OWNED.contains(&name))
    }) {
        return Ok(());
    }
    let executable = directory.join("shishi-update.exe");
    if executable.exists() {
        // A running/mapped helper cannot be opened for exclusive write on Windows.
        // Leave the entire directory intact when its executable is still in use.
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&executable)
        else {
            return Ok(());
        };
        drop(file);
    }
    for file in files {
        fs::remove_file(file)?;
    }
    fs::remove_dir(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_cache_and_locked_helper_are_retained() {
        let root = std::env::temp_dir().join(format!("shishi-cache-lock-{}", crate::model::id()));
        fs::create_dir_all(&root).unwrap();
        let package = root.join("Shishi-Setup-1.1.1-aaaaaaaaaaaa.exe");
        fs::write(&package, b"synthetic").unwrap();
        prune(&root, None, SystemTime::now()).unwrap();
        assert!(package.exists());
        let helper = root.join(format!("apply-{}", crate::model::id()));
        fs::create_dir(&helper).unwrap();
        let executable = helper.join("shishi-update.exe");
        fs::write(&executable, b"synthetic").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&executable)
            .unwrap();
        prune(
            &root,
            None,
            SystemTime::now() + STALE_AFTER + Duration::from_secs(60),
        )
        .unwrap();
        assert!(executable.exists());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_stale_owned_cache_is_removed_and_ready_or_unknown_data_survives() {
        let root = std::env::temp_dir().join(format!("shishi-cache-{}", crate::model::id()));
        fs::create_dir_all(&root).unwrap();
        let old = root.join("Shishi-Setup-1.1.1-aaaaaaaaaaaa.exe");
        let ready = root.join("Shishi-Setup-1.1.2-bbbbbbbbbbbb.exe");
        let partial = root.join(format!("download-{}.part", crate::model::id()));
        let unknown = root.join("personal-notes.json");
        let helper = root.join(format!("apply-{}", crate::model::id()));
        let protected_helper = root.join(format!("apply-{}", crate::model::id()));
        fs::create_dir(&helper).unwrap();
        fs::create_dir(&protected_helper).unwrap();
        for name in OWNED_FIXTURE {
            fs::write(helper.join(name), b"fixture").unwrap();
        }
        fs::write(protected_helper.join("notes.txt"), b"keep").unwrap();
        for path in [&old, &ready, &partial, &unknown] {
            fs::write(path, b"fixture").unwrap();
        }
        let future = SystemTime::now() + STALE_AFTER + Duration::from_secs(10);
        prune(&root, Some(&ready), future).unwrap();
        assert!(!old.exists() && !partial.exists() && !helper.exists());
        assert!(ready.exists() && unknown.exists() && protected_helper.join("notes.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
    const OWNED_FIXTURE: &[&str] = &[
        "shishi-update.exe",
        "WebView2Loader.dll",
        "plan.json",
        "plan.ready",
    ];
}
