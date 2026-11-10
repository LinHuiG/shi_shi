//! Table layout versions. `PRAGMA user_version` is the number of applied
//! steps; each step is ordered SQL applied in one transaction at startup.
//! Version 1 is the 2.0 layout, assembled from one fragment per domain so a
//! domain's tables sit next to its code. Later versions append steps.
//! Version 2 hands handling over to the client: projects come only from
//! Codex or DSH, and the context audit and learning proposals are dropped.

use rusqlite::Connection;

use crate::Result;

const STEPS: &[&str] = &[
    concat!(
    include_str!("sql/core.sql"),
    include_str!("sql/sources.sql"),
    include_str!("sql/messages.sql"),
    include_str!("sql/tasks.sql"),
    include_str!("sql/analysis.sql"),
    include_str!("sql/ai.sql"),
    include_str!("sql/archive.sql"),
    include_str!("sql/prompts.sql"),
    include_str!("sql/tools.sql"),
    include_str!("sql/handling.sql"),
    ),
    "DROP TABLE IF EXISTS handling_projects;
     DROP TABLE IF EXISTS handling_proposals;
     DROP TABLE IF EXISTS handling_context_audit;",
];

/// Scalar functions the queries rely on. SQLite's `lower()` folds ASCII
/// only; search must fold the same way Rust does for the query string.
fn functions(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "has_text",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let hay: String = ctx.get::<Option<String>>(0)?.unwrap_or_default();
            let needle: String = ctx.get(1)?;
            Ok(hay.to_lowercase().contains(&needle))
        },
    )?;
    conn.create_scalar_function(
        "audit_matches",
        8,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        super::task_query::audit_matches,
    )?;
    conn.create_scalar_function(
        "task_focus",
        7,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        super::task_query::focus_of,
    )?;
    conn.create_scalar_function(
        "tool_call_matches",
        19,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        super::tools::tool_call_matches,
    )?;
    conn.create_scalar_function(
        "activity_runtime",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        super::activity::runtime_kind,
    )?;
    conn.create_scalar_function(
        "activity_shown",
        11,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        super::activity::shown,
    )?;
    Ok(())
}

pub fn migrate(conn: &mut Connection) -> Result<()> {
    functions(conn)?;
    let applied: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if applied > STEPS.len() {
        crate::bail!("数据库版本 {applied} 高于本程序支持的 {}，请升级拾事", STEPS.len());
    }
    for (index, step) in STEPS.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(step)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    let tx = conn.transaction()?;
    super::prompts::seed(&tx)?;
    tx.commit()?;
    conn.execute_batch(include_str!("sql/views.sql"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
