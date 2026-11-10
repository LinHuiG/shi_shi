//! The data thread. It owns the only SQLite connection; every query and
//! business transaction runs here as a short closure, so the UI thread and the
//! executor never block on SQL and no transaction spans a model, network or
//! approval wait. A successful write bumps the in-memory revision that the
//! host pushes to the page instead of polling for changes.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use rusqlite::{Connection, Transaction, TransactionBehavior};
use tokio::sync::{oneshot, watch};

use crate::{Error, Result};

// Each business area keeps its SQL private to `data`; the rest of the crate
// goes through `Db`. Tests open the areas to reach fixtures and helpers.
macro_rules! areas {
    ($($m:ident),* $(,)?) => {$(
        #[cfg(not(test))]
        mod $m;
        #[cfg(test)]
        pub(crate) mod $m;
    )*};
}
areas!(
    activity,
    ai,
    analysis,
    archive,
    audit,
    config,
    credentials,
    handling,
    kv,
    messages,
    pages,
    prompts,
    retention,
    rows,
    rules,
    schedules,
    schema,
    settings,
    sources,
    state,
    tags,
    task_query,
    tasks,
    tools,
    traces,
);

pub use config::ConfigStore;

/// The data directory of an install: `data-location.json` when the user
/// moved it, else `<root>/data`.
pub fn directory(root: &Path) -> std::result::Result<PathBuf, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Location {
        version: u32,
        directory: PathBuf,
    }
    match std::fs::read(root.join("data-location.json")) {
        Ok(bytes) => {
            let value: Location = serde_json::from_slice(&bytes).map_err(|_| "数据目录设置损坏，未启动新后台")?;
            if value.version != 1 || !value.directory.is_absolute() || !value.directory.is_dir() {
                return Err("配置的数据目录不可用，未创建新数据".into());
            }
            Ok(value.directory)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(root.join("data")),
        Err(e) => Err(format!("无法读取数据目录设置：{e}")),
    }
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Db {
    jobs: mpsc::Sender<Job>,
    revision: Arc<watch::Sender<u64>>,
    pulse: Arc<watch::Sender<u64>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::start(Connection::open(path)?)
    }

    /// An isolated in-memory database for tests.
    pub fn memory() -> Result<Self> {
        Self::start(Connection::open_in_memory()?)
    }

    fn start(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        crate::data::schema::migrate(&mut conn)?;
        // Runs a previous process left open can no longer finish.
        tools::recover(&conn)?;
        archive::recover(&conn)?;
        let (jobs, inbox) = mpsc::channel::<Job>();
        std::thread::Builder::new().name("shishi-data".into()).spawn(move || {
            for job in inbox {
                // A panicking closure drops its reply sender; the caller
                // sees an error and the thread keeps serving others.
                let _ = catch_unwind(AssertUnwindSafe(|| job(&mut conn)));
            }
        })?;
        Ok(Self { jobs, revision: Arc::new(watch::channel(0).0), pulse: Arc::new(watch::channel(0).0) })
    }

    fn submit<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    ) -> oneshot::Receiver<Result<R>> {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = tx.send(f(conn));
        });
        let _ = self.jobs.send(job);
        rx
    }

    fn finish<R>(result: std::result::Result<Result<R>, oneshot::error::RecvError>) -> Result<R> {
        result.unwrap_or_else(|_| Err(Error::new("数据线程中断")))
    }

    /// Run a read-only query.
    async fn read<R: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> Result<R> {
        Self::finish(self.submit(move |conn| f(conn)).await)
    }

    /// Run one business transaction and publish a revision only when rows changed.
    async fn write<R: Send + 'static>(&self, f: impl FnOnce(&Transaction) -> Result<R> + Send + 'static) -> Result<R> {
        let (value, changed) = Self::finish(self.submit(move |conn| transact(conn, f)).await)?;
        if changed {
            self.bump();
        }
        Ok(value)
    }

    /// Blocking variants for code already off the executor (AI workers,
    /// tests). Never call these from the UI thread or inside async code.
    fn read_blocking<R: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> Result<R> {
        Self::finish(self.submit(move |conn| f(conn)).blocking_recv())
    }

    fn write_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let (value, changed) = Self::finish(self.submit(move |conn| transact(conn, f)).blocking_recv())?;
        if changed {
            self.bump();
        }
        Ok(value)
    }

    /// Move the WAL into the main file and truncate it, so a closed data
    /// directory is one self-contained `shishi.db`. Called on exit.
    pub fn checkpoint_blocking(&self) -> Result<()> {
        Self::finish(
            self.submit(|conn| {
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                Ok(())
            })
            .blocking_recv(),
        )
    }

    /// File work that belongs with data, such as the atomic config write.
    pub async fn file<R: Send + 'static>(&self, f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
        Self::finish(self.submit(move |_| f()).await)
    }

    pub fn revision(&self) -> u64 {
        *self.revision.borrow()
    }

    /// For the UI: wakes on business changes and runtime state updates.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.pulse.subscribe()
    }

    /// Wakes only when business data changes.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// Publish a business change that did not go through `write`, such as a
    /// config save or the AI queue starting or ending work.
    pub fn bump(&self) {
        self.revision.send_modify(|r| *r += 1);
        self.poke();
    }

    /// Publish a runtime-only change (live AI output, download progress):
    /// the page refreshes but sees an unchanged business revision.
    pub fn poke(&self) {
        self.pulse.send_modify(|r| *r += 1);
    }
}

/// Raw SQL on the data thread, for test fixtures only; product code goes
/// through the typed methods.
#[cfg(test)]
impl Db {
    pub async fn test_read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.read(f).await
    }

    pub async fn test_write<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.write(f).await
    }

    pub fn test_read_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.read_blocking(f)
    }

    pub fn test_write_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.write_blocking(f)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self(format!("数据库错误：{value}"))
    }
}

fn transact<R>(conn: &mut Connection, f: impl FnOnce(&Transaction) -> Result<R>) -> Result<(R, bool)> {
    let before = conn.total_changes();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = f(&tx)?;
    tx.commit()?;
    Ok((value, conn.total_changes() != before))
}

#[cfg(test)]
mod tests;
