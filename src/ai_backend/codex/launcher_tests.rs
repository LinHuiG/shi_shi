use super::*;
use crate::model::Config;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

struct Installation(PathBuf);

impl Installation {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("shishi-cli-upgrade-{}", crate::model::id()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn version(&self, name: &str) -> PathBuf {
        self.0.join("OpenAI/Codex/bin").join(name).join("codex.exe")
    }

    fn install(&self, name: &str, modified: u64) -> PathBuf {
        let path = self.version(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000 + modified))
            .unwrap();
        path
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve(path: &Path) -> Result<PathBuf, String> {
    resolve_cli(&path.to_string_lossy()).map(PathBuf::from)
}

#[test]
fn existing_selected_version_and_custom_executable_keep_their_identity() {
    let installation = Installation::new();
    let original = installation.install("0000000000000001", 1);
    installation.install("0000000000000002", 2);
    assert_eq!(resolve(&original).unwrap(), original);
    let custom = installation.0.join("chosen.exe");
    fs::write(&custom, b"synthetic").unwrap();
    assert_eq!(resolve(&custom).unwrap(), custom);
}

#[test]
fn removed_version_recovers_without_caching_across_successive_upgrades() {
    let installation = Installation::new();
    let original = installation.install("0000000000000001", 1);
    let second = installation.install("0000000000000002", 2);
    fs::remove_file(&original).unwrap();
    assert_eq!(resolve(&original).unwrap(), second);
    let third = installation.install("0000000000000003", 3);
    fs::remove_file(&second).unwrap();
    assert_eq!(resolve(&original).unwrap(), third);
    fs::remove_file(&third).unwrap();
    assert!(resolve(&original).is_err());
}

#[test]
fn replacement_uses_file_time_not_hash_order_and_skips_incomplete_versions() {
    let installation = Installation::new();
    installation.install("ffffffffffffffff", 1);
    let current = installation.install("0000000000000002", 2);
    installation.install("unpacked-temporary-files", 10);
    fs::create_dir_all(installation.version("eeeeeeeeeeeeeeee").parent().unwrap()).unwrap();
    assert_eq!(
        resolve(&installation.version("1111111111111111")).unwrap(),
        current
    );
    assert_eq!(
        latest_bundled_cli(current.parent().unwrap().parent().unwrap()).unwrap(),
        current
    );
}

#[test]
fn recovery_is_confined_to_the_saved_installation() {
    let first = Installation::new();
    let second = Installation::new();
    second.install("0000000000000002", 10);
    let missing = first.version("0000000000000001");
    assert!(resolve(&missing).is_err());
    let current = first.install("0000000000000002", 1);
    assert_eq!(resolve(&missing).unwrap(), current);
    for path in [
        first.0.join("missing.exe"),
        first.0.join("OpenAI/Codex/bin/custom/codex.exe"),
        first.0.join("Other/Codex/bin/0000000000000001/codex.exe"),
        first
            .0
            .join("OpenAI/Codex/bin/0000000000000001/unrelated.exe"),
    ] {
        assert!(resolve(&path).is_err());
    }
    assert!(bundled_cli_root(Path::new("OpenAI/Codex/bin/0000000000000001/codex.exe")).is_none());
}

#[test]
fn an_existing_directory_is_not_treated_as_a_removed_program() {
    let installation = Installation::new();
    let path = installation.version("0000000000000001");
    fs::create_dir_all(&path).unwrap();
    installation.install("0000000000000002", 2);
    assert!(resolve(&path).is_err());
}

#[test]
fn concurrent_resolution_does_not_mutate_or_create_saved_paths() {
    let installation = Installation::new();
    let current = installation.install("0000000000000002", 2);
    let missing = installation.version("0000000000000001");
    std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| scope.spawn(|| resolve(&missing)))
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap(), current);
        }
    });
    assert!(!missing.exists());
}

#[test]
fn an_unavailable_path_remains_editable_without_catalog_validation() {
    let installation = Installation::new();
    let config = Config {
        codex: installation
            .version("0000000000000001")
            .to_string_lossy()
            .into(),
        model: "synthetic".into(),
        ..Config::default()
    };
    assert!(validate_selection(&config.codex, &config.model, &config.reasoning_effort).is_ok());
}
