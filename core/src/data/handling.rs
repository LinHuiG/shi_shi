//! Handling runs: the session ShiShi starts in Codex or DSH for a task.

use rusqlite::{params, Connection};
use serde_json::{Map, Value};

use super::Db;
use crate::ai::Mode;
use crate::handling::{head, Run};
use crate::state::Scope;
use crate::tasks::model::Task;
use crate::{Config, Error, Result};

fn query(conn: &Connection, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare(&format!("SELECT data FROM handling_runs WHERE {filter} ORDER BY created, rowid"))?;
    let rows = stmt.query_map(args, |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.iter().map(|s| serde_json::from_str(s)).collect::<serde_json::Result<_>>()?)
}

pub(crate) fn get(conn: &Connection, id: &str) -> Result<Option<Run>> {
    Ok(query(conn, "id = ?1", &[&id])?.pop())
}

pub(crate) fn save(conn: &Connection, run: &Run) -> Result<()> {
    conn.execute(
        "INSERT INTO handling_runs(id, task_id, status, created, data) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET status = excluded.status, data = excluded.data",
        params![run.id, run.task_id, run.status, run.created, serde_json::to_string(run)?],
    )?;
    Ok(())
}

/// Snapshot key `handling`: the latest run of each task the page shows.
/// Live first turns are laid over by [`crate::handling::part`].
pub(crate) fn part(conn: &Connection, scope: &Scope, out: &mut Map<String, Value>) -> Result<()> {
    if scope.page == "tasks" || !scope.task.is_empty() || !scope.selected.is_empty() {
        let latest = "id IN (SELECT id FROM handling_runs h WHERE created =
            (SELECT max(created) FROM handling_runs WHERE task_id = h.task_id))";
        let rows = query(conn, latest, &[])?
            .iter()
            .filter(|r| scope.page == "tasks" || scope.refers(&r.task_id))
            .map(head)
            .collect::<Vec<_>>();
        out.insert("handling".into(), rows.into());
    }
    Ok(())
}

impl Db {
    pub async fn handling_run(&self, id: String) -> Result<Option<Run>> {
        self.read(move |c| get(c, &id)).await
    }

    /// First turns a previous process left running were cut off with it.
    pub async fn interrupt_handling_runs(&self) -> Result<()> {
        self.write(|tx| {
            for mut run in query(tx, "status IN ('running', 'waiting')", &[])? {
                run.status = "interrupted".into();
                save(tx, &run)?;
            }
            Ok(())
        })
        .await
    }

    pub async fn save_handling_run(&self, run: Run) -> Result<()> {
        self.write(move |tx| save(tx, &run)).await
    }

    /// A finished first turn: the run, its usage and its activity log.
    pub async fn finish_handling_run(&self, run: Run, usage: Value, log: Value) -> Result<()> {
        self.write(move |tx| {
            save(tx, &run)?;
            super::ai::record_usage(tx, "handling", &usage)?;
            super::activity::log(tx, "handling_round", &log).map(drop)
        })
        .await
    }

    /// Count one manual call against the daily limit.
    pub async fn reserve_handling_call(&self, config: std::sync::Arc<Config>) -> Result<()> {
        self.write(move |tx| super::ai::reserve_call(tx, &config, "handling", Mode::Manual, true)).await
    }

    /// The task, its runs and the effective policy a new run starts from.
    pub async fn handling_start(&self, task: String) -> Result<(Task, Vec<Run>, String)> {
        self.read(move |c| {
            let task = super::tasks::get(c, &task)?.ok_or_else(|| Error::new("事项不存在"))?;
            let runs = query(c, "task_id = ?1", &[&task.id])?;
            Ok((task, runs, super::prompts::effective(c, "handling")?))
        })
        .await
    }

    /// The task and effective policy the handling dialog opens on.
    pub async fn handling_prepare(&self, task: String) -> Result<(Task, String)> {
        self.read(move |c| {
            let task = super::tasks::get(c, &task)?.ok_or_else(|| Error::new("事项不存在"))?;
            Ok((task, super::prompts::effective(c, "handling")?))
        })
        .await
    }
}
