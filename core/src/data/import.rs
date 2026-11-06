//! One-time import of 1.1.68 data, used only by the installer's migration
//! tool (`shishi-migrate`, the only build with the `import` feature). The
//! tool reads the old files; this side writes the new tables in one
//! transaction. A record the new schema or checks reject is skipped with a
//! reason, and the rest still lands.

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{activity, config, prompts, sources, tasks, tools, Db};
use crate::prompts::TARGETS;
use crate::sources::Source;
use crate::tasks::model::{Task, STATUSES};
use crate::tools::Tool;
use crate::{Config, Result};

/// Old records already shaped as the new types. A task's `number` is its old
/// number, 0 when it had none.
#[derive(Default)]
pub struct Legacy {
    pub tasks: Vec<Task>,
    pub tools: Vec<Tool>,
    /// The user's own text per prompt target.
    pub prompts: Vec<(String, String)>,
    pub temporary_memory: String,
    pub archived_memory: String,
    pub sources: Vec<Source>,
}

/// What landed and what did not, for the log.
#[derive(Debug, Default)]
pub struct Report {
    pub counts: Map<String, Value>,
    pub skipped: Vec<String>,
}

impl Db {
    pub fn import_blocking(&self, legacy: Legacy, mut report: Report) -> Result<Report> {
        self.write_blocking(move |tx| {
            let mut count = |name: &str, n: usize| {
                report.counts.insert(name.into(), n.into());
            };
            let mut skipped = Vec::new();
            let mut tasks = legacy.tasks;
            // Old numbers first, in order, so they keep their numbers; the
            // rest are numbered after them.
            tasks.sort_by_key(|t| if t.number > 0 { t.number } else { i64::MAX });
            let mut n = 0;
            for t in &tasks {
                if !STATUSES.contains(&t.status.as_str()) {
                    skipped.push(format!("事项「{}」：状态「{}」新版没有", t.title, t.status));
                    continue;
                }
                let r = one(tx, || {
                    tasks::save(tx, t)?;
                    if t.number > 0 {
                        tx.execute("UPDATE tasks SET number = ?1 WHERE id = ?2", rusqlite::params![t.number, t.id])?;
                    }
                    Ok(())
                });
                match r {
                    Ok(()) => n += 1,
                    Err(e) => skipped.push(format!("事项「{}」：{e}", t.title)),
                }
            }
            count("tasks", n);

            let mut n = 0;
            for t in &legacy.tools {
                let mut t = t.clone();
                if t.validate(false).is_err() {
                    skipped.push(format!("工具「{}」：定义不符合新版格式", t.name));
                    continue;
                }
                if t.enabled && t.validate(true).is_err() {
                    t.enabled = false;
                    skipped.push(format!("工具「{}」：不能直接启用，已导入为停用", t.name));
                }
                match one(tx, || tools::store(tx, &t)) {
                    Ok(()) => n += 1,
                    Err(e) => skipped.push(format!("工具「{}」：{e}", t.name)),
                }
            }
            count("tools", n);

            let mut n = 0;
            for (target, text) in &legacy.prompts {
                if !TARGETS.contains(&target.as_str()) || text.trim().is_empty() {
                    continue;
                }
                if prompts::effective(tx, target)? != *text {
                    prompts::record(tx, target, text, "migrate", "从1.1.68迁移")?;
                    n += 1;
                }
            }
            count("prompts", n);

            let mut n = 0;
            for (kind, text) in [("temporary", &legacy.temporary_memory), ("archived", &legacy.archived_memory)] {
                if !text.is_empty() {
                    let m = prompts::memory(tx, kind)?;
                    prompts::replace_memory(tx, kind, m.revision, text)?;
                    n += 1;
                }
            }
            count("memory", n);

            let mut n = 0;
            for s in &legacy.sources {
                match one(tx, || sources::insert(tx, s)) {
                    Ok(()) => n += 1,
                    Err(e) => skipped.push(format!("消息来源「{}」：{e}", s.name)),
                }
            }
            count("sources", n);

            report.skipped.extend(skipped);
            let detail = json!({
                "message": summary(&report),
                "counts": report.counts,
                "skipped": report.skipped,
            });
            activity::log(tx, "migration", &detail)?;
            Ok(report)
        })
    }
}

/// One record inside a savepoint, so a rejected row leaves nothing behind.
fn one(tx: &rusqlite::Connection, f: impl FnOnce() -> Result<()>) -> Result<()> {
    tx.execute_batch("SAVEPOINT record")?;
    let r = f();
    tx.execute_batch(if r.is_ok() { "RELEASE record" } else { "ROLLBACK TO record; RELEASE record" })?;
    r
}

/// The one-line log text.
pub fn summary(r: &Report) -> String {
    let names = [
        ("tasks", "事项"),
        ("tools", "工具"),
        ("prompts", "提示词"),
        ("memory", "记忆"),
        ("sources", "消息来源"),
        ("connections", "AI接入"),
        ("credentials", "凭据"),
        ("settings", "设置"),
    ];
    let parts: Vec<String> =
        names.iter().filter_map(|(k, label)| r.counts.get(*k).map(|n| format!("{label}{n}"))).collect();
    format!("已迁移1.1.68数据：{}；跳过{}项", parts.join("、"), r.skipped.len())
}

/// The new config from an old settings object: every field the new format
/// has is taken when it parses and passes the checks on its own, and each AI
/// connection on its own; the rest keep their defaults and are reported.
pub fn config_from(old: &Value, report: &mut Report) -> Config {
    let mut current = serde_json::to_value(Config::default()).unwrap_or_default();
    let keys: Vec<String> = current.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let (mut settings, mut connections) = (0, 0);
    let take = |current: &mut Value, key: &str, value: Value, report: &mut Report| {
        let mut next = current.clone();
        match key {
            "ai_connections" => next[key].as_array_mut().map(|a| a.push(value.clone())).unwrap_or_default(),
            _ => next[key] = value.clone(),
        }
        let checked = serde_json::from_value::<Config>(next.clone())
            .map_err(|e| e.to_string())
            .and_then(|c| c.validate().map_err(|e| e.0));
        match checked {
            Ok(()) => {
                *current = next;
                true
            }
            Err(e) => {
                let what = match key {
                    "ai_connections" => format!("AI接入「{}」", value["name"].as_str().unwrap_or_default()),
                    _ => format!("设置 {key}"),
                };
                report.skipped.push(format!("{what}：{e}"));
                false
            }
        }
    };
    // Connections first: the model selection refers to them.
    if let Some(list) = old.get("ai_connections").and_then(Value::as_array) {
        for c in list {
            connections += take(&mut current, "ai_connections", c.clone(), report) as usize;
        }
    }
    for key in keys.iter().filter(|k| *k != "ai_connections") {
        if let Some(value) = old.get(key) {
            settings += take(&mut current, key, value.clone(), report) as usize;
        }
    }
    report.counts.insert("connections".into(), connections.into());
    report.counts.insert("settings".into(), settings.into());
    serde_json::from_value(current).unwrap_or_default()
}

/// Write the config file the app will load.
pub fn write_config(path: &Path, c: &Config) -> Result<()> {
    config::write_atomic(path, c)
}

impl Db {
    /// Copy the whole database into one self-contained file at `path`.
    pub fn save_as_blocking(&self, path: &Path) -> Result<()> {
        let path = path.to_string_lossy().into_owned();
        self.read_blocking(move |conn| {
            conn.execute("VACUUM INTO ?1", [path])?;
            Ok(())
        })
    }
}
