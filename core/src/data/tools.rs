//! Tool persistence: the tool library, runs and their calls, task links,
//! the history search and restart recovery.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::{rows, Db};
use crate::tools::{CallRecord, CommandResult, Failure, HistoryQuery, Run, RunMeta, Tool};
use crate::{time, Error, Result};

const MAX_TOOLS: i64 = 200;

impl Db {
    /// The whole library in registration order.
    pub async fn tools(&self) -> Result<Vec<Tool>> {
        self.read(|conn| list(conn)).await
    }

    /// Insert or update a validated tool; an edit must carry the stored
    /// `updated` so a stale editor never overwrites.
    pub async fn save_tool(&self, mut tool: Tool) -> Result<Tool> {
        self.write(move |tx| {
            let taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tools WHERE id <> ?1 AND name = ?2 AND name <> '')",
                params![tool.id, tool.name],
                |r| r.get(0),
            )?;
            if taken {
                crate::bail!("工具名已存在");
            }
            if tool.id.is_empty() {
                let count: i64 = tx.query_row("SELECT count(*) FROM tools", [], |r| r.get(0))?;
                if count >= MAX_TOOLS {
                    crate::bail!("工具库最多200项");
                }
                tool.id = uuid::Uuid::new_v4().to_string();
                tool.updated = time::now();
            } else {
                let old = find(tx, &tool.id)?.ok_or_else(|| Error::new("工具已删除"))?;
                if old.updated != tool.updated {
                    crate::bail!("工具已修改，请重新打开编辑");
                }
                tool.updated = time::now().max(old.updated + 1);
            }
            store(tx, &tool)?;
            Ok(tool)
        })
        .await
    }

    pub async fn toggle_tool(&self, id: String, updated: Option<i64>, enabled: bool) -> Result<Tool> {
        self.write(move |tx| {
            let mut tool = current(tx, &id, updated)?;
            tool.validate(enabled)?;
            tool.enabled = enabled;
            tool.updated = time::now().max(tool.updated + 1);
            store(tx, &tool)?;
            Ok(tool)
        })
        .await
    }

    /// Delete a tool; past calls keep their records and task links.
    pub async fn delete_tool(&self, id: String, updated: Option<i64>) -> Result<()> {
        self.write(move |tx| {
            let tool = current(tx, &id, updated)?;
            tx.execute("DELETE FROM tools WHERE id = ?1", [tool.id])?;
            Ok(())
        })
        .await
    }

    /// The run `id`, or with `by_task` the newest run relating to task `id`.
    pub async fn tool_run(&self, id: String, by_task: bool) -> Result<Option<Run>> {
        self.read(move |conn| run(conn, if by_task { RELATES } else { "id = ?1" }, &id)).await
    }

    /// One `tool_run_history` page of calls, 20 per page, newest
    /// first, filtered and paged in SQL; `tools` lists the tools the related
    /// calls used, for the filter.
    pub async fn tool_run_history(&self, q: HistoryQuery) -> Result<Value> {
        self.read(move |conn| history(conn, &q)).await
    }

    /// Calls of a run in call order.
    pub async fn tool_calls(&self, run_id: String) -> Result<Vec<CallRecord>> {
        self.read(move |conn| calls(conn, &run_id)).await
    }

    pub fn tool_calls_blocking(&self, run_id: String) -> Result<Vec<CallRecord>> {
        self.read_blocking(move |conn| calls(conn, &run_id))
    }

    /// Record a rejected batch: every call with its result, logging the
    /// ones naming a known tool.
    pub fn record_rejected_batch_blocking(&self, run: RunMeta, rows: Vec<(CallRecord, Option<Value>)>) -> Result<()> {
        self.write_blocking(move |tx| {
            ensure_run(tx, &run)?;
            for (call, log) in rows {
                insert_call(tx, &run.id, &call)?;
                if let Some(log) = log {
                    super::activity::log(tx, "tool_call_finished", &log)?;
                }
            }
            Ok(())
        })
    }

    /// Record calls the provider refused, skipping ids already stored.
    pub fn record_provider_rejections_blocking(&self, run: RunMeta, rows: Vec<(CallRecord, Value)>) -> Result<()> {
        self.write_blocking(move |tx| {
            ensure_run(tx, &run)?;
            for (call, log) in rows {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE run_id = ?1 AND call_id = ?2)",
                    params![run.id, call.call_id],
                    |r| r.get(0),
                )?;
                if !exists {
                    insert_call(tx, &run.id, &call)?;
                    super::activity::log(tx, "tool_call_finished", &log)?;
                }
            }
            Ok(())
        })
    }

    /// Persist a call's intent before its process starts. The replay check
    /// and the insert share one transaction so a concurrent callback cannot
    /// replay it; returns the failure when the call must not run.
    pub fn start_tool_call_blocking(
        &self,
        run: RunMeta,
        tool: Tool,
        intent: CallRecord,
        log: Value,
    ) -> Result<Option<Failure>> {
        self.write_blocking(move |tx| {
            ensure_run(tx, &run)?;
            let records = calls(tx, &run.id)?;
            if let Some(failure) = crate::tools::session::replay_failure(&records, &tool, &intent) {
                return Ok(Some(failure));
            }
            insert_call(tx, &run.id, &intent)?;
            super::activity::log(tx, "tool_call_started", &log)?;
            Ok(None)
        })
    }

    pub fn finish_tool_call_blocking(
        &self,
        run_id: String,
        call_id: String,
        result: CommandResult,
        log: Value,
    ) -> Result<()> {
        self.write_blocking(move |tx| {
            store_result(tx, &run_id, &call_id, time::now(), &result)?;
            super::activity::log(tx, "tool_call_finished", &log)?;
            Ok(())
        })
    }

    /// End a run that has a row. A test run takes the status of its last call.
    pub fn finish_tool_run_blocking(
        &self,
        run_id: String,
        test: bool,
        mut status: String,
        mut error: String,
        usage: Value,
    ) -> Result<()> {
        self.write_blocking(move |tx| {
            let exists: bool =
                tx.query_row("SELECT EXISTS(SELECT 1 FROM tool_runs WHERE id = ?1)", [&run_id], |r| r.get(0))?;
            if !exists {
                return Ok(());
            }
            if test {
                if let Some(last) = calls(tx, &run_id)?.pop().and_then(|c| c.result) {
                    status = last.status;
                    error = last.error;
                }
            }
            tx.execute(
                "UPDATE tool_runs SET finished = ?2, status = ?3, error = ?4, usage = ?5 WHERE id = ?1",
                params![run_id, time::now(), status, error, usage.to_string()],
            )?;
            Ok(())
        })
    }

    /// The tool as stored now when it is enabled and `task` (if any) is
    /// still open; the live recheck before each process start.
    pub fn runnable_tool_blocking(&self, id: String, task: String) -> Result<Option<Tool>> {
        self.read_blocking(move |conn| {
            let open =
                task.is_empty() || conn.prepare_cached("SELECT 1 FROM tasks WHERE id = ?1 AND merged_into = ''")?.exists([&task])?;
            Ok(find(conn, &id)?.filter(|t| t.enabled && open))
        })
    }
}

pub(super) fn list(conn: &Connection) -> Result<Vec<Tool>> {
    rows::all(conn, "SELECT * FROM tools ORDER BY rowid", [])
}

fn find(conn: &Connection, id: &str) -> Result<Option<Tool>> {
    rows::one(conn, "SELECT * FROM tools WHERE id = ?1", [id])
}

/// Enabled tools, as offered to classify, format, edit, archive and handling.
pub(crate) fn enabled_tools(conn: &Connection) -> Result<Vec<Tool>> {
    rows::all(conn, "SELECT * FROM tools WHERE enabled = 1 ORDER BY rowid", [])
}

pub(super) fn store(conn: &Connection, tool: &Tool) -> Result<()> {
    rows::put(conn, "tools", "id", tool)
}

fn current(conn: &Connection, id: &str, updated: Option<i64>) -> Result<Tool> {
    let tool = find(conn, id)?.ok_or("工具已删除")?;
    if Some(tool.updated) != updated {
        crate::bail!("工具已修改，请刷新");
    }
    Ok(tool)
}

/// Runs are created on the first call, so requests without calls leave none.
fn ensure_run(conn: &Connection, run: &RunMeta) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tool_runs(id, request_id, purpose, source, task_id, tools, started, status)
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, 'running')",
        params![
            run.id,
            run.purpose,
            run.source.to_string(),
            run.task_id,
            serde_json::to_string(&run.tools)?,
            time::now()
        ],
    )?;
    Ok(())
}

/// A `tool_calls` row; its result once it has a status.
fn call_row(r: &rusqlite::Row) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        call_id: r.get("call_id")?,
        tool_id: r.get("tool_id")?,
        tool: r.get("tool")?,
        arguments: serde_json::from_str(&r.get::<_, String>("arguments")?).unwrap_or(Value::Null),
        started: r.get("started")?,
        finished: r.get::<_, Option<i64>>("finished")?.unwrap_or(0),
        result: r.get::<_, Option<String>>("status")?.map(|_| rows::record(r)).transpose()?,
    })
}

fn calls(conn: &Connection, run_id: &str) -> Result<Vec<CallRecord>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM tool_calls WHERE run_id = ?1 ORDER BY seq")?;
    let rows = stmt.query_map([run_id], call_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn insert_call(conn: &Connection, run_id: &str, call: &CallRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO tool_calls(run_id, seq, call_id, tool_id, tool, arguments, started)
         VALUES (?1, (SELECT count(*) FROM tool_calls WHERE run_id = ?1), ?2, ?3, ?4, ?5, ?6)",
        params![run_id, call.call_id, call.tool_id, call.tool, call.arguments.to_string(), call.started],
    )?;
    if let Some(result) = &call.result {
        store_result(conn, run_id, &call.call_id, call.finished, result)?;
    }
    Ok(())
}

fn store_result(conn: &Connection, run_id: &str, call_id: &str, finished: i64, r: &CommandResult) -> Result<()> {
    let mut row = json!(r);
    row.as_object_mut().unwrap().remove("tool");
    (row["run_id"], row["call_id"], row["finished"]) = (json!(run_id), json!(call_id), json!(finished));
    if rows::update(conn, "tool_calls", &["run_id", "call_id"], &row)? == 0 {
        crate::bail!("调用记录丢失");
    }
    Ok(())
}

/// Whether a call's searchable text holds every term: its tool, arguments,
/// result and run identity plus the purpose and status labels the page shows.
fn call_matches(purpose: &str, request_id: &str, tool_id: &str, call: &CallRecord, terms: &[String]) -> bool {
    let label = match purpose {
        "classify" => "事项分析",
        "format" => "事项生成",
        "edit" => "一句话修改",
        "archive" => "归档",
        "tool_test" => "测试运行",
        "tool_call" => "手动工具调用",
        "handling" => "AI 一键处置",
        _ => "历史记录",
    };
    let status = match call.result.as_ref().map(|r| r.status.as_str()) {
        Some("completed") => "执行完成",
        Some("nonzero_exit") => "非零退出",
        Some("start_failed") => "启动失败",
        Some("output_error") => "输出解码/读取失败",
        Some("timed_out") => "超时",
        Some("cancelled") => "已取消",
        Some("rejected") => "执行前拒绝",
        Some("running") => "运行中",
        Some("failed") => "运行失败",
        Some("interrupted") => "已中断",
        _ => "结果未确定",
    };
    let content = json!({
        "tool": call.tool, "tool_id": tool_id, "arguments": call.arguments, "result": call.result,
        "purpose": purpose, "request_id": request_id, "call_id": call.call_id,
    });
    let text = format!("{content} {label} {status}").to_lowercase();
    terms.iter().all(|term| text.contains(term))
}

/// SQL `tool_call_matches(terms, purpose, request_id, tool_key, <CALL_COLUMNS>)`;
/// `terms` is the lowercased query.
pub(super) fn tool_call_matches(ctx: &rusqlite::functions::Context) -> rusqlite::Result<bool> {
    let terms: Vec<String> = ctx.get::<String>(0)?.split_whitespace().map(str::to_owned).collect();
    if terms.is_empty() {
        return Ok(true);
    }
    let text = |i| ctx.get::<Option<String>>(i).map(Option::unwrap_or_default);
    let flag = |i| ctx.get::<Option<bool>>(i).map(Option::unwrap_or_default);
    let result = ctx.get::<Option<String>>(10)?.map(|status| -> rusqlite::Result<_> {
        Ok(CommandResult {
            tool: text(6)?,
            call_id: text(4)?,
            status,
            exit_code: ctx.get(11)?,
            stdout: text(12)?,
            stderr: text(13)?,
            duration_ms: ctx.get::<Option<i64>>(14)?.unwrap_or(0) as u64,
            truncated: flag(15)?,
            timed_out: flag(16)?,
            cancelled: flag(17)?,
            error: text(18)?,
        })
    });
    let call = CallRecord {
        call_id: text(4)?,
        tool_id: text(5)?,
        tool: text(6)?,
        arguments: serde_json::from_str(&text(7)?).unwrap_or(Value::Null),
        started: 0,
        finished: 0,
        result: result.transpose()?,
    };
    Ok(call_matches(&text(1)?, &text(2)?, &text(3)?, &call, &terms))
}

/// The newest run matching `filter` (an SQL condition on `tool_runs` over
/// `?1`), with its calls and the tasks each call is linked to.
fn run(conn: &Connection, filter: &str, arg: &str) -> Result<Option<Run>> {
    let sql = format!(
        "SELECT id, request_id, purpose, source, task_id, tools, started, finished, status, answer, error, usage
         FROM tool_runs WHERE {filter} ORDER BY rowid DESC LIMIT 1"
    );
    let found = conn
        .query_row(&sql, [arg], |r| {
            let text = |i: usize| r.get::<_, Option<String>>(i).map(Option::unwrap_or_default);
            let json = |i: usize| text(i).map(|t| serde_json::from_str(&t).unwrap_or(Value::Null));
            Ok(Run {
                id: r.get(0)?,
                request_id: r.get(1)?,
                purpose: r.get(2)?,
                source: json(3)?,
                task_id: text(4)?,
                tools: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                started: r.get(6)?,
                finished: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                status: r.get(8)?,
                answer: text(9)?,
                error: text(10)?,
                usage: json(11)?,
                calls: Vec::new(),
            })
        })
        .optional()?;
    let Some(mut run) = found else { return Ok(None) };
    let mut linked = HashMap::<String, Vec<String>>::new();
    let mut links = conn.prepare("SELECT call_id, task_id FROM tool_call_tasks WHERE run_id = ?1 ORDER BY rowid")?;
    for row in links.query_map([&run.id], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
        let (call, task) = row?;
        linked.entry(call).or_default().push(task);
    }
    for call in calls(conn, &run.id)? {
        let ids = linked.remove(&call.call_id).unwrap_or_default();
        run.calls.push((call, ids));
    }
    Ok(Some(run))
}

const RELATES: &str = "(task_id = ?1 OR id IN (SELECT run_id FROM tool_call_tasks WHERE task_id = ?1))";

/// Mark runs left running by a previous process as interrupted.
pub(super) fn recover(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE tool_runs SET status = 'interrupted', finished = ?1,
           error = '软件上次退出前运行未完成；已执行操作的结果可能不确定，不会自动重放'
         WHERE status = 'running'",
        [time::now()],
    )?;
    Ok(())
}

/// Attribute calls of the run named by `result._tool_run_id` to the tasks a
/// committed AI result produced: the calls it lists in `tool_call_ids`, or
/// all of them when it lists none and produced exactly one task.
pub(crate) fn link_tool_calls(conn: &Connection, result: &Value, task_ids: &[String]) -> Result<()> {
    let Some(run) = result.get("_tool_run_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let selected: Vec<String> = match result.get("tool_call_ids") {
        Some(ids) => {
            ids.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
        }
        None if task_ids.len() == 1 => calls(conn, run)?.into_iter().map(|c| c.call_id).collect(),
        None => Vec::new(),
    };
    for call in selected {
        for task in task_ids.iter().filter(|t| !t.is_empty()) {
            conn.execute(
                "INSERT OR IGNORE INTO tool_call_tasks(run_id, call_id, task_id)
                 SELECT run_id, call_id, ?3 FROM tool_calls WHERE run_id = ?1 AND call_id = ?2",
                params![run, call, task],
            )?;
        }
    }
    Ok(())
}

/// Calls with the run fields history filters on; `?1` is the task, `?2` the
/// purpose. A call relates to the task through its run's task, or through
/// explicit links when the run has none. `tool_key` falls back to the
/// frozen tool of the same name for calls recorded without an id.
const RELATED: &str = "WITH related AS (
    SELECT r.rowid AS run_row, r.id AS run_id, r.purpose, r.request_id, c.seq,
        CASE WHEN c.tool_id <> '' THEN c.tool_id ELSE coalesce((SELECT json_extract(t.value, '$.id')
            FROM json_each(r.tools) AS t WHERE json_extract(t.value, '$.name') = c.tool LIMIT 1), '') END AS tool_key,
        c.call_id, c.tool_id, c.tool, c.arguments, c.started, c.finished, c.status, c.exit_code, c.stdout, c.stderr,
        c.duration_ms, c.truncated, c.timed_out, c.cancelled, c.error
    FROM tool_runs AS r JOIN tool_calls AS c ON c.run_id = r.id
    WHERE (?2 = '' OR r.purpose = ?2)
      AND (?1 = '' OR CASE WHEN coalesce(r.task_id, '') = '' THEN EXISTS(SELECT 1 FROM tool_call_tasks AS l
          WHERE l.run_id = c.run_id AND l.call_id = c.call_id AND l.task_id = ?1) ELSE r.task_id = ?1 END))";

/// `?3` is the lowercased query.
const MATCHES: &str = "tool_call_matches(?3, purpose, request_id, tool_key, call_id, tool_id, tool, arguments, started,
    finished, status, exit_code, stdout, stderr, duration_ms, truncated, timed_out, cancelled, error)";

fn history(conn: &Connection, q: &HistoryQuery) -> Result<Value> {
    let args = params![q.task, q.purpose, q.terms, q.tool, q.day.map(|d| d.0), q.day.map(|d| d.1)];
    let from = format!(
        "{RELATED} SELECT * FROM related WHERE (?4 = '' OR tool_key = ?4)
            AND (?5 IS NULL OR (started >= ?5 AND started < ?6)) AND {MATCHES}"
    );
    let total: i64 = conn.query_row(&format!("SELECT count(*) FROM ({from})"), args, |r| r.get(0))?;
    let (page, pages) = crate::tools::page_of(q.page, total as usize);
    let sql = format!(
        "SELECT run_id, seq AS call_index, call_id, tool_key AS tool_id, tool, arguments, started, coalesce(finished, 0) AS finished,
            purpose, coalesce(status, 'unknown') AS status, exit_code, CASE WHEN status IS NOT NULL THEN coalesce(duration_ms, 0) END AS duration_ms
         FROM ({from}) ORDER BY started DESC, run_row DESC, seq LIMIT 20 OFFSET {}",
        (page - 1) * 20
    );
    let rows = rows::objects(conn, &sql, args, &json!({"arguments": null}))?;
    let tools_sql = format!(
        "{RELATED} SELECT tool_key AS id, tool AS name FROM (SELECT tool_key, tool,
            row_number() OVER (PARTITION BY tool_key ORDER BY run_row DESC, seq) AS n FROM related
            WHERE tool_key <> '')
        WHERE n = 1 ORDER BY tool_key"
    );
    let tools = rows::objects(conn, &tools_sql, params![q.task, q.purpose], &Value::Null)?;
    Ok(json!({"rows": rows, "total": total, "page": page, "pages": pages, "tools": tools}))
}

/// Snapshot keys: the tool list rides inside `config` as in 1.x.
pub(crate) fn part(conn: &Connection, _: &crate::state::Scope, out: &mut serde_json::Map<String, Value>) -> Result<()> {
    out.entry("config").or_insert_with(|| json!({}))["tools"] = json!(list(conn)?);
    Ok(())
}
