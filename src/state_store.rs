//! Row-oriented durable state. Business records and progress share one SQLite
//! transaction. Configuration is an immutable JSON generation referenced by that
//! transaction, so a crash cannot pair new progress with old configuration.
use crate::model::{State, STATE_SCHEMA_VERSION};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const FILE: &str = "state.sqlite3";
const VERSION: i64 = 1;
type Rows = BTreeMap<String, (i64, String)>;

pub struct Store {
    connection: Connection,
    directory: PathBuf,
    previous: Option<State>,
    rows: BTreeMap<&'static str, Rows>,
    config: Option<(String, String)>,
}

fn error(e: impl std::fmt::Display) -> String {
    format!("业务数据库：{e}")
}
fn encode(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(error)
}
fn config_path(directory: &Path, name: &str) -> Result<PathBuf, String> {
    if !name.ends_with(".json") || uuid::Uuid::parse_str(name.trim_end_matches(".json")).is_err() {
        return Err(error("配置引用无效"));
    }
    Ok(directory.join("settings").join(name))
}
fn connection(directory: &Path, write: bool) -> Result<Connection, String> {
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let db = Connection::open_with_flags(directory.join(FILE), flags).map_err(error)?;
    db.busy_timeout(Duration::from_secs(5)).map_err(error)?;
    db.pragma_update(None, "foreign_keys", true)
        .map_err(error)?;
    Ok(db)
}

impl Store {
    pub fn query_task_history(
        &mut self,
        id: &str,
        terms: Vec<String>,
        ascending: bool,
        requested: usize,
    ) -> Result<Value, String> {
        let searching = !terms.is_empty();
        self.connection
            .create_scalar_function(
                "audit_matches",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                move |context| {
                    let body: String = context.get(0)?;
                    let row: Value = serde_json::from_str(&body)
                        .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                    Ok(crate::task_history::matches(&row, &terms))
                },
            )
            .map_err(error)?;
        let tx = self.connection.transaction().map_err(error)?;
        let scope = "task_id=?1 AND substr(json_extract(payload,'$.kind'),1,14)<>'archive_memory'";
        let all: i64 = tx
            .query_row(
                &format!("SELECT count(*) FROM audit WHERE {scope}"),
                [id],
                |r| r.get(0),
            )
            .map_err(error)?;
        let predicate = format!(
            "{scope}{}",
            if searching {
                " AND audit_matches(payload)"
            } else {
                ""
            }
        );
        let total: i64 = if searching {
            tx.query_row(
                &format!("SELECT count(*) FROM audit WHERE {predicate}"),
                [id],
                |r| r.get(0),
            )
            .map_err(error)?
        } else {
            all
        };
        let pages = (total as usize).div_ceil(50).max(1);
        let page = requested.max(1).min(pages);
        let direction = if ascending { "ASC" } else { "DESC" };
        let mut statement = tx.prepare(&format!("SELECT ordinal,payload FROM audit WHERE {predicate} ORDER BY time {direction},ordinal {direction} LIMIT 50 OFFSET ?2")).map_err(error)?;
        let rows = statement
            .query_map(params![id, ((page - 1) * 50) as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(error)?;
        let rows = rows
            .map(|row| {
                let (index, body) = row.map_err(error)?;
                let mut value: Value = serde_json::from_str(&body).map_err(error)?;
                value["index"] = json!(index);
                value["kind_label"] = json!(crate::task_history::label(
                    value["kind"].as_str().unwrap_or("")
                ));
                Ok(value)
            })
            .collect::<Result<Vec<_>, String>>()?;
        drop(statement);
        tx.commit().map_err(error)?;
        Ok(json!({"rows":rows,"total":total,"all":all,"page":page,"pages":pages}))
    }
    pub fn query_messages(
        &mut self,
        state: &State,
        query: &str,
        range: std::ops::RangeInclusive<i64>,
        ascending: bool,
        requested: usize,
        size: usize,
    ) -> Result<Value, String> {
        let (since, until) = (*range.start(), *range.end());
        let keep = encode(&crate::messages::protected(state))?;
        let deleted = encode(&state.deleted_messages.keys().collect::<Vec<_>>())?;
        let cutoff = crate::model::now()
            - i64::from(state.config.message_retention_days.max(1)) * 86_400_000;
        let predicate = "withdrawn=0 AND time>=?1 AND time<=?2 AND (time>=?3 OR id IN (SELECT value FROM json_each(?4))) AND id NOT IN (SELECT value FROM json_each(?5)) AND (?6='' OR EXISTS (SELECT 1 FROM json_each(messages.search) WHERE instr(value,?6)>0))";
        let tx = self.connection.transaction().map_err(error)?;
        let total: i64 = tx
            .query_row(
                &format!("SELECT count(*) FROM messages WHERE {predicate}"),
                params![since, until, cutoff, keep, deleted, query],
                |r| r.get(0),
            )
            .map_err(error)?;
        let total = usize::try_from(total).map_err(error)?;
        let pages = total.div_ceil(size).max(1);
        let page = requested.min(pages);
        let direction = if ascending { "ASC" } else { "DESC" };
        let mut statement = tx.prepare(&format!("SELECT payload FROM messages WHERE {predicate} ORDER BY time {direction},id {direction} LIMIT ?7 OFFSET ?8")).map_err(error)?;
        let rows = statement
            .query_map(
                params![
                    since,
                    until,
                    cutoff,
                    keep,
                    deleted,
                    query,
                    size as i64,
                    ((page - 1) * size) as i64
                ],
                |r| r.get::<_, String>(0),
            )
            .map_err(error)?;
        let rows = rows
            .map(|row| serde_json::from_str::<Value>(&row.map_err(error)?).map_err(error))
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        tx.commit().map_err(error)?;
        Ok(json!({"rows":rows,"page":page,"page_size":size,"total":total,"pages":pages}))
    }
    pub fn open(directory: &Path) -> Result<Self, String> {
        let db = connection(directory, true)?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(error)?;
        if version != 0 && version != VERSION {
            return Err(error("不支持的数据库版本，已停止保存"));
        }
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(error)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(error)?;
        db.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS metadata (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL, config TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS pending_import (id INTEGER PRIMARY KEY CHECK(id=1), source_hash TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, time INTEGER NOT NULL, withdrawn INTEGER NOT NULL, search TEXT NOT NULL, payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_time ON messages(time,id);
            CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS audit (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, task_id TEXT NOT NULL, time INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS audit_task_time ON audit(task_id,time,ordinal);
            CREATE TABLE IF NOT EXISTS archive_versions (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS reports (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS advice_revisions (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS handling_runs (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tool_runs (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);
            PRAGMA user_version=1;
            COMMIT;").map_err(error)?;
        let config = db
            .query_row("SELECT config FROM metadata WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(error)?
            .map(|name| {
                fs::read_to_string(config_path(directory, &name)?)
                    .map(|body| (body, name))
                    .map_err(error)
            })
            .transpose()?;
        Ok(Self {
            connection: db,
            directory: directory.into(),
            previous: None,
            rows: BTreeMap::new(),
            config,
        })
    }

    pub fn save(&mut self, state: &State) -> Result<(), String> {
        self.save_import(state, None)
    }

    fn save_import(&mut self, state: &State, source_hash: Option<&str>) -> Result<(), String> {
        state.work_scheduler.validate().map_err(error)?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(error("状态版本不匹配"));
        }
        // Sharing makes this metadata projection independent of retained message,
        // task, audit and archive payload size.
        let mut metadata = state.clone();
        metadata.messages = Default::default();
        metadata.tasks = Default::default();
        metadata.audit = Default::default();
        metadata.archive_versions = Default::default();
        metadata.reports = Default::default();
        metadata.advice_revisions = Default::default();
        metadata.handling.runs = Default::default();
        metadata.tool_runs = Default::default();
        let config = encode(&metadata.config)?;
        metadata.config = Default::default();
        let payload = encode(&metadata)?;
        let config_name = match &self.config {
            Some((previous, name)) if previous == &config => name.clone(),
            _ => {
                let directory = self.directory.join("settings");
                fs::create_dir_all(&directory).map_err(error)?;
                let name = format!("{}.json", uuid::Uuid::new_v4());
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(&name))
                    .map_err(error)?;
                file.write_all(config.as_bytes())
                    .and_then(|_| file.sync_all())
                    .map_err(error)?;
                name
            }
        };
        let mut changed: BTreeMap<&'static str, Rows> = BTreeMap::new();
        macro_rules! rows {
            ($field:ident, $key:expr) => {
                if self
                    .previous
                    .as_ref()
                    .is_none_or(|old| !state.$field.same_snapshot(&old.$field))
                {
                    let mut rows = Rows::new();
                    for (index, row) in state.$field.iter().enumerate() {
                        let key: String = ($key)(index, row);
                        if rows.insert(key, (index as i64, encode(row)?)).is_some() {
                            return Err(error(concat!("重复记录编号：", stringify!($field))));
                        }
                    }
                    changed.insert(stringify!($field), rows);
                }
            };
        }
        let message_changed = self
            .previous
            .as_ref()
            .is_none_or(|old| !state.messages.same_snapshot(&old.messages));
        if message_changed {
            changed.insert(
                "messages",
                state
                    .messages
                    .iter()
                    .map(|(id, m)| Ok((id.clone(), (0, encode(m)?))))
                    .collect::<Result<Rows, String>>()?,
            );
        }
        rows!(tasks, |_: usize, row: &crate::model::Task| row.id.clone());
        rows!(audit, |i: usize, _: &crate::model::Audit| i.to_string());
        rows!(
            archive_versions,
            |_: usize, row: &crate::model::ArchiveGroup| row.id.clone()
        );
        rows!(reports, |i: usize, _: &Value| i.to_string());
        rows!(tool_runs, |_: usize, row: &crate::tool_calling::ToolRun| {
            row.id.clone()
        });
        rows!(advice_revisions, |i: usize, _: &Value| i.to_string());
        if self
            .previous
            .as_ref()
            .is_none_or(|old| !state.handling.runs.same_snapshot(&old.handling.runs))
        {
            changed.insert(
                "handling_runs",
                state
                    .handling
                    .runs
                    .iter()
                    .enumerate()
                    .map(|(i, row)| Ok((i.to_string(), (i as i64, encode(row)?))))
                    .collect::<Result<Rows, String>>()?,
            );
        }
        let tx = self.connection.transaction().map_err(error)?;
        for (&table, next) in &changed {
            // A newly opened writer learns existing keys once. Subsequent saves
            // compare only collections that changed, and write only changed rows.
            if !self.rows.contains_key(table) {
                let mut query = tx
                    .prepare(&format!("SELECT id,ordinal,payload FROM {table}"))
                    .map_err(error)?;
                let rows = query
                    .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
                    .map_err(error)?;
                self.rows
                    .insert(table, rows.collect::<Result<Rows, _>>().map_err(error)?);
            }
            let previous = &self.rows[table];
            let mut remove = tx
                .prepare_cached(&format!("DELETE FROM {table} WHERE id=?1"))
                .map_err(error)?;
            for id in previous.keys().filter(|id| !next.contains_key(*id)) {
                remove.execute([id]).map_err(error)?;
            }
            for (id, (ordinal, body)) in next {
                if previous.get(id).is_some_and(|(old_ordinal, old_body)| {
                    old_ordinal == ordinal && old_body == body
                }) {
                    continue;
                }
                if table == "messages" {
                    let message = &state.messages[id];
                    let search = encode(
                        &[
                            &message.text,
                            &message.sender,
                            &message.group,
                            &message.chat,
                            &message.source_name,
                        ]
                        .map(|v| v.to_lowercase()),
                    )?;
                    tx.prepare_cached("INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET ordinal=excluded.ordinal,time=excluded.time,withdrawn=excluded.withdrawn,search=excluded.search,payload=excluded.payload").map_err(error)?
                        .execute(params![id, ordinal, message.time, message.withdrawn, search, body]).map_err(error)?;
                } else if table == "audit" {
                    let index: usize = id.parse().map_err(error)?;
                    let row = &state.audit[index];
                    tx.prepare_cached("INSERT INTO audit VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET ordinal=excluded.ordinal,task_id=excluded.task_id,time=excluded.time,payload=excluded.payload").map_err(error)?
                        .execute(params![id, index as i64, row.task_id, row.time, body]).map_err(error)?;
                } else {
                    tx.prepare_cached(&format!("INSERT INTO {table} VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET ordinal=excluded.ordinal,payload=excluded.payload")).map_err(error)?
                        .execute(params![id, ordinal, body]).map_err(error)?;
                }
            }
        }
        tx.execute("INSERT INTO metadata VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,config=excluded.config WHERE payload<>excluded.payload OR config<>excluded.config", params![payload, config_name]).map_err(error)?;
        if let Some(hash) = source_hash {
            tx.execute("INSERT INTO pending_import VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET source_hash=excluded.source_hash", [hash]).map_err(error)?;
        } else {
            // The first normal save makes SQLite authoritative. Until then an
            // interrupted installer may leave the old executable in use.
            tx.execute("DELETE FROM pending_import", [])
                .map_err(error)?;
        }
        tx.commit().map_err(error)?;
        self.rows.extend(changed);
        self.config = Some((config, config_name));
        self.previous = Some(state.clone());
        Ok(())
    }
}

pub fn load(directory: &Path) -> Result<State, String> {
    let mut db = connection(directory, false)?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(error)?;
    if version != VERSION {
        return Err(error("不支持的数据库版本，保留原数据并停止加载"));
    }
    let tx = db.transaction().map_err(error)?;
    let (body, config): (String, String) = tx
        .query_row("SELECT payload,config FROM metadata WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(error)?;
    let mut state: Value = serde_json::from_str(&body).map_err(error)?;
    if state["schema_version"] != STATE_SCHEMA_VERSION {
        return Err(error("状态版本不匹配"));
    }
    state["config"] =
        serde_json::from_slice(&fs::read(config_path(directory, &config)?).map_err(error)?)
            .map_err(error)?;
    for table in [
        "messages",
        "tasks",
        "audit",
        "archive_versions",
        "reports",
        "advice_revisions",
        "handling_runs",
        "tool_runs",
    ] {
        if table == "tool_runs" && !tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tool_runs')",[],|r|r.get::<_,bool>(0)).map_err(error)? {
            // Additive table; older supported databases have no tool records.
            continue;
        }
        let mut query = tx
            .prepare(&format!("SELECT id,payload FROM {table} ORDER BY ordinal"))
            .map_err(error)?;
        let rows = query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(error)?;
        let mut list = Vec::new();
        let mut map = serde_json::Map::new();
        for row in rows {
            let (id, body) = row.map_err(error)?;
            let value = serde_json::from_str(&body).map_err(error)?;
            if table == "messages" {
                map.insert(id, value);
            } else {
                list.push(value);
            }
        }
        if table == "messages" {
            state[table] = Value::Object(map);
        } else if table == "handling_runs" {
            state["handling"]["runs"] = Value::Array(list);
        } else {
            state[table] = Value::Array(list);
        }
    }
    let state = crate::managed_data::decode_state(state).map_err(error)?;
    state.work_scheduler.validate().map_err(error)?;
    tx.commit().map_err(error)?;
    Ok(state)
}

/// Called with the application's data lock held. The primary database is
/// published only after a complete import and semantic round-trip validation.
/// The original JSON is deliberately retained as a pre-upgrade backup.
pub fn migrate(directory: &Path) -> Result<(), String> {
    if directory.join(FILE).exists() {
        load(directory)?;
        let mut store = Store::open(directory)?;
        let previous: Option<String> = store
            .connection
            .query_row(
                "SELECT source_hash FROM pending_import WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        if let Some(previous) = previous {
            let current = crate::updates::sha256(&directory.join("state.json"))?;
            if current != previous {
                let state = crate::managed_data::load_legacy(directory)?;
                store.save_import(&state, Some(&current))?;
            }
        }
        // Preserve pending imports and business rows; only metadata changes.
        let payload: String = store
            .connection
            .query_row("SELECT payload FROM metadata WHERE id=1", [], |row| {
                row.get(0)
            })
            .map_err(error)?;
        let mut metadata: Value = serde_json::from_str(&payload).map_err(error)?;
        if crate::managed_data::upgrade_legacy_plugin_flags(&mut metadata).map_err(error)? {
            store
                .connection
                .execute(
                    "UPDATE metadata SET payload=?1 WHERE id=1",
                    [serde_json::to_string(&metadata).map_err(error)?],
                )
                .map_err(error)?;
        }
        return Ok(());
    }
    if !directory.join("state.json").exists() {
        return Ok(());
    }
    let state = crate::managed_data::load_legacy(directory)?;
    let source_hash = crate::updates::sha256(&directory.join("state.json"))?;
    let staging = directory.join(format!(".migration-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).map_err(error)?;
    {
        let mut store = Store::open(&staging)?;
        store.save_import(&state, Some(&source_hash))?;
        store
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
            .map_err(error)?;
        let integrity: String = store
            .connection
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(error)?;
        if integrity != "ok" {
            return Err(error("迁移完整性检查失败；原始数据已保留"));
        }
    }
    if serde_json::to_value(load(&staging)?).map_err(error)?
        != serde_json::to_value(&state).map_err(error)?
    {
        return Err(error("迁移前后数据不一致；原始数据已保留"));
    }
    let settings = directory.join("settings");
    fs::create_dir_all(&settings).map_err(error)?;
    for entry in fs::read_dir(staging.join("settings")).map_err(error)? {
        let entry = entry.map_err(error)?;
        fs::rename(entry.path(), settings.join(entry.file_name())).map_err(error)?;
    }
    fs::rename(staging.join(FILE), directory.join(FILE)).map_err(error)?;
    // Only empty staging directories are removed; failed imports are retained.
    let _ = fs::remove_dir(staging.join("settings"));
    let _ = fs::remove_dir(staging);
    Ok(())
}

pub fn migrate_installation(root: &Path) -> Result<(), String> {
    let _transition = crate::storage::transition_lock(root)?;
    crate::storage::initialize(root)?;
    let directory = crate::storage::data_dir(root);
    if !directory.exists() {
        return Ok(());
    }
    let _lock = crate::storage::lock_source(&directory)?;
    migrate(&directory)
}

/// The transfer helper holds the exclusive transition and data locks, so no
/// writer or MCP reader can race the copy of this checkpointed database.
pub fn checkpoint(directory: &Path) -> Result<(), String> {
    if !directory.join(FILE).exists() {
        return Ok(());
    }
    let db = connection(directory, true)?;
    let busy: i64 = db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .map_err(error)?;
    if busy != 0 {
        return Err(error("数据库仍在使用，未开始复制"));
    }
    let mode: String = db
        .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
        .map_err(error)?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(error("数据库尚未关闭写入日志，未开始复制"));
    }
    Ok(())
}

/// Run only while holding the exclusive installation transition and data locks.
/// Live readers may still reference an older generation, so normal commits must
/// never delete configuration files. Startup has no competing MCP readers.
pub fn clean_config_generations(directory: &Path) -> Result<(), String> {
    let db = connection(directory, false)?;
    let current: String = db
        .query_row("SELECT config FROM metadata WHERE id=1", [], |r| r.get(0))
        .map_err(error)?;
    config_path(directory, &current)?;
    let settings = directory.join("settings");
    crate::storage::plain_directory(&settings)?;
    for entry in fs::read_dir(settings).map_err(error)? {
        let entry = entry.map_err(error)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != current
            && config_path(directory, &name).is_ok()
            && entry.file_type().map_err(error)?.is_file()
        {
            fs::remove_file(entry.path()).map_err(error)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Audit, Message, Task};

    fn directory() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("shishi-sqlite-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        directory
    }
    fn legacy(directory: &Path, state: &State) {
        fs::write(
            directory.join("state.json"),
            serde_json::to_vec(state).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn migration_round_trip_retries_old_writes_until_first_new_commit() {
        let directory = directory();
        let mut state = State::default();
        state.tasks.push(Task {
            id: "task".into(),
            title: "Before upgrade".into(),
            ..Default::default()
        });
        state.messages.insert(
            "message".into(),
            Message {
                id: "message".into(),
                text: "Synthetic evidence".into(),
                ..Default::default()
            },
        );
        state.tasks[0].evidence.direct.push("message".into());
        state.audit.push(Audit {
            task_id: "task".into(),
            kind: "ai_create".into(),
            after: json!(state.tasks[0]),
            ..Default::default()
        });
        legacy(&directory, &state);
        let original = fs::read(directory.join("state.json")).unwrap();
        migrate(&directory).unwrap();
        assert_eq!(json!(load(&directory).unwrap()), json!(state));
        assert_eq!(fs::read(directory.join("state.json")).unwrap(), original);
        state.tasks[0].title = "Old app resumed after failed install".into();
        legacy(&directory, &state);
        migrate(&directory).unwrap();
        assert_eq!(json!(load(&directory).unwrap()), json!(state));
        {
            let mut store = Store::open(&directory).unwrap();
            state.tasks[0].title = "New app committed".into();
            store.save(&state).unwrap();
        }
        legacy(&directory, &State::default());
        migrate(&directory).unwrap();
        assert_eq!(
            load(&directory).unwrap().tasks[0].title,
            "New app committed"
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn retired_plugin_flags_upgrade_sqlite_metadata_once_without_touching_rows() {
        let directory = directory();
        let mut state = State::default();
        state.chat_sources.push(crate::model::ChatSource {
            id: "source".into(),
            plugin_id: "old-plugin".into(),
            enabled: true,
            updated: 5,
            cursor: json!({"offset":12}),
            ..Default::default()
        });
        state.messages.insert(
            "message".into(),
            Message {
                id: "message".into(),
                text: "evidence".into(),
                ..Default::default()
            },
        );
        legacy(&directory, &state);
        migrate(&directory).unwrap();
        {
            let store = Store::open(&directory).unwrap();
            let body: String = store
                .connection
                .query_row("SELECT payload FROM metadata WHERE id=1", [], |r| r.get(0))
                .unwrap();
            let mut value: Value = serde_json::from_str(&body).unwrap();
            value["disabled_plugins"] = json!(["old-plugin"]);
            store
                .connection
                .execute(
                    "UPDATE metadata SET payload=?1 WHERE id=1",
                    [value.to_string()],
                )
                .unwrap();
            store.connection.execute_batch("CREATE TRIGGER forbid_rewrite BEFORE UPDATE ON messages BEGIN SELECT RAISE(ABORT,'unexpected message rewrite'); END;").unwrap();
        }
        assert!(!load(&directory).unwrap().chat_sources[0].enabled);
        migrate(&directory).unwrap();
        migrate(&directory).unwrap();
        let loaded = load(&directory).unwrap();
        assert!(!loaded.chat_sources[0].enabled);
        assert_eq!(loaded.chat_sources[0].updated, 6);
        assert_eq!(loaded.chat_sources[0].cursor, json!({"offset":12}));
        assert_eq!(loaded.messages["message"].text, "evidence");
        let store = Store::open(&directory).unwrap();
        let body: String = store
            .connection
            .query_row("SELECT payload FROM metadata WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(serde_json::from_str::<Value>(&body)
            .unwrap()
            .get("disabled_plugins")
            .is_none());
        let pending: i64 = store
            .connection
            .query_row("SELECT count(*) FROM pending_import", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 1);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn large_collection_writes_only_changed_rows_and_rolls_back_cursor_with_records() {
        let directory = directory();
        let mut state = State::default();
        let at = crate::model::now();
        for index in 0..20_000 {
            let id = format!("message-{index:05}");
            state.messages.insert(
                id.clone(),
                Message {
                    id,
                    time: at + index,
                    text: format!("Synthetic {index}"),
                    ..Default::default()
                },
            );
        }
        let mut store = Store::open(&directory).unwrap();
        store.save(&state).unwrap();
        store.connection.execute_batch("CREATE TABLE writes(id TEXT); CREATE TRIGGER changed_message AFTER UPDATE ON messages BEGIN INSERT INTO writes VALUES(new.id); END;").unwrap();
        state.messages.get_mut("message-00123").unwrap().text = "Changed".into();
        state.config.daily_calls += 1;
        store.save(&state).unwrap();
        fs::write(directory.join("settings/user-note.txt"), "preserve").unwrap();
        clean_config_generations(&directory).unwrap();
        assert_eq!(fs::read_dir(directory.join("settings")).unwrap().count(), 2);
        let writes: i64 = store
            .connection
            .query_row("SELECT count(*) FROM writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(writes, 1);
        let page = store
            .query_messages(&state, "", 0..=i64::MAX, false, 2, 100)
            .unwrap();
        assert_eq!(page["total"], 20_000);
        assert_eq!(page["rows"].as_array().unwrap().len(), 100);
        assert_eq!(page["rows"][0]["id"], "message-19899");
        let before = json!(load(&directory).unwrap());
        store.connection.execute_batch("CREATE TRIGGER fail_commit BEFORE INSERT ON metadata BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
        state.messages.remove("message-00001");
        state.last_analysis_success_at = at;
        assert!(store.save(&state).is_err());
        assert_eq!(json!(load(&directory).unwrap()), before);
        store
            .connection
            .execute_batch("DROP TRIGGER fail_commit")
            .unwrap();
        store.save(&state).unwrap();
        assert_eq!(json!(load(&directory).unwrap()), json!(state));
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}
