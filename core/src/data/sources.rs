//! Configured message sources (`sources` table), in the order they were added,
//! and the transactions that store what a scan or range read brought.

use std::collections::BTreeSet;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::messages::Change;
use super::{activity, audit, kv, messages, retention, rows, Db};
use crate::messages::{cutoff, Message, REVISION};
use crate::sources::{next_version, Fetched, Source, MAX_SOURCES, STALE};
use crate::state::Scope;
use crate::time::{now, DAY};
use crate::{bail, Error, Result};

/// Sources matching `filter`, in the order they were added.
fn select(conn: &Connection, filter: &str, p: impl rusqlite::Params) -> Result<Vec<Source>> {
    rows::all(conn, &format!("SELECT * FROM sources WHERE {filter} ORDER BY position"), p)
}

fn list(conn: &Connection) -> Result<Vec<Source>> {
    select(conn, "1", [])
}

fn get(conn: &Connection, id: &str) -> Result<Option<Source>> {
    rows::one(conn, "SELECT * FROM sources WHERE id = ?1", [id])
}

fn any_enabled(conn: &Connection) -> Result<bool> {
    Ok(conn.prepare_cached("SELECT 1 FROM sources WHERE enabled = 1")?.exists([])?)
}

/// The row of `s`: its configuration and cursor as JSON text, no cursor as NULL.
fn row(s: &Source) -> Value {
    let mut row = json!(s);
    row["config"] = s.config.to_string().into();
    row["cursor"] = (!s.cursor.is_null()).then(|| s.cursor.to_string()).into();
    row
}

pub(super) fn insert(conn: &Connection, s: &Source) -> Result<()> {
    let mut row = row(s);
    row["position"] =
        conn.query_row("SELECT coalesce(max(position), -1) + 1 FROM sources", [], |r| r.get::<_, i64>(0))?.into();
    rows::put(conn, "sources", "id", &row)
}

fn update(conn: &Connection, s: &Source) -> Result<()> {
    rows::update(conn, "sources", &["id"], &row(s)).map(drop)
}

/// What one scan stored.
pub struct ScanOutcome {
    pub counts: Value,
    pub sources: usize,
    pub successful: usize,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl Db {
    pub async fn sources(&self) -> Result<Vec<Source>> {
        self.read(list).await
    }

    pub async fn source(&self, id: String) -> Result<Option<Source>> {
        self.read(move |conn| get(conn, &id)).await
    }

    pub async fn any_source_enabled(&self) -> Result<bool> {
        self.read(any_enabled).await
    }

    /// The enabled sources, each with the messages it must look up again. A
    /// checkpoint later than `start` means the clock went back: it becomes
    /// `start`, is logged, and that source sits out this scan.
    pub async fn scan_plan(&self, start: i64, since: i64) -> Result<Vec<(Source, Vec<(String, String)>)>> {
        self.write(move |tx| {
            let mut plan = Vec::new();
            for s in select(tx, "enabled = 1", [])? {
                if s.last_scan > start {
                    tx.execute("UPDATE sources SET last_scan = ?1 WHERE id = ?2", params![start, s.id])?;
                    let message = format!("「{}」的读取断点晚于当前时间，已改为当前时间，本次跳过", s.name);
                    activity::log(
                        tx,
                        "scan_clock",
                        &json!({"message": message, "source": s.id, "checkpoint": s.last_scan}),
                    )?;
                    continue;
                }
                let tracked = tracked(tx, &s.id, since)?;
                plan.push((s, tracked));
            }
            Ok(plan)
        })
        .await
    }

    /// Store every source's result in one transaction. A source whose
    /// configuration changed while it was read is ignored; a failed source
    /// keeps its cursor and checkpoint.
    pub async fn commit_scan(
        &self,
        results: Vec<(Source, Result<Fetched>)>,
        start: i64,
        days: u32,
    ) -> Result<ScanOutcome> {
        self.write(move |tx| commit_scan(tx, results, start, days)).await
    }

    pub async fn finish_scan(&self, at: i64) -> Result<()> {
        self.write(move |tx| kv::set(tx, "last_scan_finished_at", &at)).await
    }

    /// Store a range read of `source` unless its configuration changed.
    pub async fn commit_range(&self, source: Source, rows: Vec<Message>, days: u32) -> Result<usize> {
        self.write(move |tx| {
            let current = get(tx, &source.id)?;
            if current.map(|s| s.updated) != Some(source.updated) {
                bail!("来源配置已变化，请重试");
            }
            let cutoff = cutoff(days);
            let mut count = 0;
            for m in &rows {
                if messages::allowed(tx, &m.id, m.time, cutoff)? {
                    super::messages::upsert(tx, m)?;
                    count += 1;
                }
            }
            reconcile_withdrawals(tx)?;
            kv::bump(tx, REVISION)?;
            kv::set(tx, "last_scan_finished_at", &now())?;
            Ok(count)
        })
        .await
    }

    /// When the automatic scan last ran; `None` without an enabled source.
    pub async fn last_scan(&self) -> Result<Option<i64>> {
        self.read(|conn| {
            let last = kv::get_i64(conn, "last_scan")?.max(kv::get_i64(conn, "last_scan_finished_at")?);
            Ok(any_enabled(conn)?.then_some(last))
        })
        .await
    }

    pub async fn record_scan_control(&self, before: Value, after: Value, enabled: bool) -> Result<()> {
        self.write(move |tx| {
            audit::write(tx, "scan_control", "", "修改自动扫描", &before, &after)?;
            if !enabled {
                super::analysis::reset_filter(tx)?;
            }
            Ok(())
        })
        .await
    }

    /// Disable the plugin's sources before its files go away.
    pub async fn disable_plugin_sources(&self, plugin: String) -> Result<()> {
        self.write(move |tx| {
            for mut s in select(tx, "plugin_id = ?1", [&plugin])? {
                s.enabled = false;
                s.updated = next_version(s.updated);
                update(tx, &s)?;
            }
            let detail = "卸载插件，保留来源配置、历史消息和事项";
            audit::write(tx, "plugin_uninstall", "", detail, &Value::Null, &json!({"plugin_id": plugin}))?;
            Ok(())
        })
        .await
    }

    /// `source_save` with the account the plugin validated (`key`).
    pub async fn save_source(&self, mut source: Source, key: Option<String>) -> Result<Value> {
        self.write(move |tx| {
            if source.id.is_empty() {
                let count = tx.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get::<_, i64>(0))? as usize;
                if count >= MAX_SOURCES {
                    bail!("最多配置 16 个消息来源");
                }
                source.account_key = key.unwrap_or_default();
                source.id = uuid::Uuid::new_v4().to_string();
                source.cursor = Value::Null;
                source.last_scan = 0;
                source.last_error.clear();
                source.updated = now();
                insert(tx, &source)?;
            } else {
                let old = get(tx, &source.id)?.ok_or_else(|| Error::new("消息来源不存在"))?;
                if old.updated != source.updated || old.plugin_id != source.plugin_id {
                    bail!("{STALE}");
                }
                // A different account or file needs its own identity; old
                // evidence is never relabelled.
                if old.config != source.config
                    && old.last_scan > 0
                    && (key.as_deref() != Some(old.account_key.as_str()) || old.account_key.is_empty())
                {
                    bail!("此来源已有消息。更换账号或文件请新建来源；可停用旧来源并保留证据。");
                }
                source.account_key = key.unwrap_or(old.account_key);
                source.cursor = old.cursor;
                source.last_scan = old.last_scan;
                source.last_error = old.last_error;
                source.updated = next_version(old.updated);
                update(tx, &source)?;
            }
            let after = json!({"id": source.id, "name": source.name, "enabled": source.enabled});
            audit::write(tx, "source_save", "", "保存消息来源", &Value::Null, &after)?;
            Ok(serde_json::to_value(&source)?)
        })
        .await
    }

    /// `source_toggle` at version `expected`, with the validated account.
    pub async fn toggle_source(&self, id: String, expected: i64, enabled: bool, key: Option<String>) -> Result<Value> {
        self.write(move |tx| {
            let mut source = get(tx, &id)?.ok_or_else(|| Error::new("消息来源不存在"))?;
            if source.updated != expected {
                bail!("{STALE}");
            }
            source.enabled = enabled;
            source.updated = next_version(source.updated);
            if let Some(key) = key {
                source.account_key = key;
            }
            update(tx, &source)?;
            let detail =
                if enabled { "启用消息来源" } else { "停用消息来源，保留配置与读取进度" };
            let before = json!({"id": id, "enabled": !enabled});
            audit::write(tx, "source_toggle", "", detail, &before, &json!({"id": id, "enabled": enabled}))?;
            Ok(serde_json::to_value(source)?)
        })
        .await
    }

    /// `source_remove`; returns whether no enabled source is left.
    pub async fn remove_source(&self, id: String, updated: Option<i64>) -> Result<bool> {
        self.write(move |tx| {
            let old = get(tx, &id)?.ok_or_else(|| Error::new("消息来源不存在"))?;
            if Some(old.updated) != updated {
                bail!("消息来源已变化");
            }
            tx.execute("DELETE FROM sources WHERE id = ?1", [&id])?;
            let before = json!({"id": old.id, "name": old.name});
            audit::write(tx, "source_remove", "", "移除消息来源，保留历史证据", &before, &Value::Null)?;
            Ok(!any_enabled(tx)?)
        })
        .await
    }
}

/// Messages the source must look up again: evidence of live tasks, and
/// recent image messages whose files have not arrived yet.
fn tracked(conn: &Connection, source: &str, since: i64) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.external_id FROM messages m WHERE m.source_id = ?1 AND (
           m.id IN (SELECT e.message_id FROM task_evidence e JOIN tasks t ON t.id = e.task_id WHERE t.archived = 0)
           OR (m.time >= ?2 AND json_extract(m.payload, '$.kind') IN ('image', 'richText')
               AND (NOT EXISTS (SELECT 1 FROM message_images i WHERE i.message_id = m.id)
                    OR EXISTS (SELECT 1 FROM message_images i WHERE i.message_id = m.id AND i.path = ''))))
         ORDER BY m.id",
    )?;
    let rows = stmt.query_map(params![source, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn commit_scan(
    conn: &Connection,
    results: Vec<(Source, Result<Fetched>)>,
    start: i64,
    days: u32,
) -> Result<ScanOutcome> {
    let cutoff = cutoff(days);
    let current = list(conn)?;
    let (mut received, mut added, mut updated, mut unchanged, mut duplicates, mut ignored) = (0, 0, 0, 0, 0, 0);
    let (mut successful, mut warnings, mut errors) = (0, Vec::new(), Vec::new());
    let sources = results.len();
    for (read, result) in results {
        if let Ok((rows, ..)) = &result {
            received += rows.len();
        }
        let Some(mut source) =
            current.iter().find(|s| s.id == read.id && s.updated == read.updated && s.enabled).cloned()
        else {
            if let Ok((rows, ..)) = &result {
                ignored += rows.len();
            }
            warnings.push(format!("{}配置已变化，本次结果已忽略", read.name));
            continue;
        };
        match result {
            Ok((rows, cursor, checkpoint)) => {
                let mut seen = BTreeSet::new();
                for m in rows {
                    if !seen.insert(m.id.clone()) {
                        duplicates += 1;
                        continue;
                    }
                    if !messages::allowed(conn, &m.id, m.time, cutoff)? {
                        ignored += 1;
                        continue;
                    }
                    match super::messages::upsert(conn, &m)? {
                        Change::Added => added += 1,
                        Change::Updated => updated += 1,
                        Change::Unchanged => unchanged += 1,
                    }
                }
                if source.last_scan > 0 && start - source.last_scan > DAY {
                    audit::write(
                        conn,
                        "scan_gap",
                        "",
                        "来源离线超过24小时，可按时间段补读",
                        &json!({"source": source.id, "from": source.last_scan}),
                        &json!(start),
                    )?;
                }
                source.cursor = cursor;
                source.last_scan = checkpoint;
                source.last_error.clear();
                update(conn, &source)?;
                successful += 1;
            }
            Err(e) => {
                source.last_error = e.0.clone();
                update(conn, &source)?;
                errors.push(format!("{}：{}", source.name, e.0));
            }
        }
    }
    reconcile_withdrawals(conn)?;
    messages::prune(conn, days)?;
    if successful > 0 {
        kv::set(conn, "last_scan", &start)?;
    }
    kv::set(conn, "last_error", &errors.join("；"))?;
    if added + updated > 0 {
        kv::bump(conn, REVISION)?;
    }
    Ok(ScanOutcome {
        counts: json!({
            "received": received, "added": added, "updated": updated,
            "unchanged": unchanged, "duplicates": duplicates, "ignored": ignored,
        }),
        sources,
        successful,
        warnings,
        errors,
    })
}

/// Drop withdrawn messages from task evidence and reopen or cancel the tasks
/// that relied on them.
pub(crate) fn reconcile_withdrawals(conn: &Connection) -> Result<()> {
    let affected: Vec<String> = rows::column(
        conn,
        "SELECT DISTINCT e.task_id FROM task_evidence e JOIN messages m ON m.id = e.message_id WHERE m.withdrawn = 1",
        [],
    )?;
    if affected.is_empty() {
        return Ok(());
    }
    conn.execute("DELETE FROM task_evidence WHERE message_id IN (SELECT id FROM messages WHERE withdrawn = 1)", [])?;
    // Without evidence left a model task is cancelled and a manual one kept;
    // with evidence left only a completion is reopened.
    conn.execute(
        "UPDATE tasks SET updated = ?2,
           status = CASE WHEN NOT EXISTS (SELECT 1 FROM task_evidence WHERE task_id = id) THEN iif(manual, status, '已取消')
                         WHEN status = '已完成' THEN '待处理' ELSE status END,
           reason = CASE WHEN NOT EXISTS (SELECT 1 FROM task_evidence WHERE task_id = id)
                           THEN iif(manual, '消息来源已撤回；保留人工事项，请确认', '唯一消息来源已撤回')
                         WHEN status = '已完成' THEN '来源发生撤回，重新核对完成依据' ELSE reason END,
           status_changed_at = CASE WHEN NOT EXISTS (SELECT 1 FROM task_evidence WHERE task_id = id) AND NOT manual AND status <> '已取消'
                                      OR EXISTS (SELECT 1 FROM task_evidence WHERE task_id = id) AND status = '已完成'
                                    THEN ?2 ELSE status_changed_at END
         WHERE id IN (SELECT value FROM json_each(?1))",
        params![json!(affected).to_string(), now()],
    )?;
    Ok(())
}

/// Snapshot keys owned by sources and messages.
pub(super) fn part(conn: &Connection, _: &Scope, out: &mut Map<String, Value>) -> Result<()> {
    let sources: Vec<Value> = list(conn)?
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v.as_object_mut().map(|o| o.remove("cursor"));
            v
        })
        .collect();
    out.insert("chat_sources".into(), sources.into());
    out.insert("last_scan".into(), kv::get_i64(conn, "last_scan")?.into());
    out.insert("last_error".into(), kv::get::<String>(conn, "last_error")?.unwrap_or_default().into());
    out.insert(REVISION.into(), kv::get_i64(conn, REVISION)?.into());
    out.insert("retention_digest".into(), retention::load(conn)?);
    Ok(())
}
