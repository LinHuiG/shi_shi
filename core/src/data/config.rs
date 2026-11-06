//! The configuration file: loaded once, replaced atomically on save.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::Db;
use crate::config::parse;
use crate::{Config, Result};

/// Shared, cheap-to-read handle on the current configuration.
#[derive(Clone)]
pub struct ConfigStore {
    path: PathBuf,
    current: Arc<RwLock<Arc<Config>>>,
}

impl ConfigStore {
    /// Load the file, creating it with defaults when absent.
    pub fn load(path: PathBuf) -> Result<Self> {
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                write_atomic(&path, &config)?;
                config
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, current: Arc::new(RwLock::new(Arc::new(config))) })
    }

    /// A store that never touches disk, for tests.
    pub fn memory(config: Config) -> Self {
        Self { path: PathBuf::new(), current: Arc::new(RwLock::new(Arc::new(config))) }
    }

    pub fn get(&self) -> Arc<Config> {
        self.current.read().unwrap().clone()
    }

    /// Validate and persist a new configuration, then make it current.
    /// Runs on the data thread because it touches the file system.
    pub async fn save(&self, db: &Db, config: Config) -> Result<Arc<Config>> {
        config.validate()?;
        let config = Arc::new(config);
        if !self.path.as_os_str().is_empty() {
            let path = self.path.clone();
            let copy = config.clone();
            db.file(move || write_atomic(&path, &copy)).await?;
        }
        *self.current.write().unwrap() = config.clone();
        db.bump();
        Ok(config)
    }

    /// Apply a change to a copy of the current config and save it.
    pub async fn update(&self, db: &Db, change: impl FnOnce(&mut Config) -> Result<()>) -> Result<Arc<Config>> {
        let mut next = (*self.get()).clone();
        change(&mut next)?;
        self.save(db, next).await
    }
}

/// Write through a flushed sibling temporary file and rename over the target.
pub(super) fn write_atomic(path: &Path, config: &Config) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("config.json")));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(serde_json::to_string_pretty(config)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)?;
    Ok(())
}
