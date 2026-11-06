//! Offline tests with synthetic tools. Commands call the `tool_fixture`
//! example, so `sh` and the Windows host's `cmd.exe` run the same steps.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shishi_api::ToolFailureKind;

use super::session::tool_result_text;
use super::*;
use crate::{dispatch, Config, Ctx};

fn ctx() -> Arc<Ctx> {
    Ctx::memory(Config::default()).unwrap()
}

fn draft(name: &str, template: &str, params: &[&str]) -> Value {
    json!({
        "name": name, "description": "测试工具", "command_template": template, "enabled": true,
        "parameters": params.iter().map(|p| json!({"name": p, "description": "值"})).collect::<Vec<_>>(),
        "definition_version": 2,
    })
}

async fn save(ctx: &Arc<Ctx>, tool: Value) -> Tool {
    serde_json::from_value(dispatch(ctx, "tool_save", json!({"tool": tool})).await.unwrap()).unwrap()
}

/// Run a data future to completion from synchronous test code.
fn block_on<T: Send + 'static>(f: impl std::future::Future<Output = T> + Send + 'static) -> T {
    std::thread::spawn(move || tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f))
        .join()
        .unwrap()
}

fn store(ctx: &Ctx, tool: Value) -> Tool {
    let (db, tool) = (ctx.db.clone(), serde_json::from_value(tool).unwrap());
    block_on(async move { db.save_tool(tool).await }).unwrap()
}

/// A template calling the fixture with `steps`.
fn fx(steps: &str) -> String {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let exe = deps.parent().unwrap().join("examples").join(format!("tool_fixture{}", std::env::consts::EXE_SUFFIX));
    format!("\"{}\" {steps}", exe.display())
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn run(session: &Session, tool: &str, call: &str, args: Value) -> std::result::Result<Value, Failure> {
    let text = session.call(tool, call, &args, &AtomicBool::new(false), later(), None)?;
    Ok(serde_json::from_str(&text).unwrap())
}

fn add_task(ctx: &Ctx, id: &str) {
    let id = id.to_string();
    ctx.db
        .test_write_blocking(move |tx| {
            tx.execute(
                "INSERT INTO tasks(id, number, title, status, status_changed_at, created, updated)
                 VALUES (?1, (SELECT count(*) + 1 FROM tasks), '合成事项', '待处理', 0, 0, 0)",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn registry_saves_atomically_and_checks_versions() {
    let ctx = ctx();
    let tool = save(&ctx, draft("echo_text", "printf %s {{text}}", &["text"])).await;
    assert!(!tool.id.is_empty() && tool.enabled);
    let err = dispatch(&ctx, "tool_save", json!({"tool": draft("echo_text", "true", &[])})).await.unwrap_err();
    assert_eq!(err.0, "工具名已存在");
    let mut stale = json!(tool);
    stale["updated"] = json!(tool.updated - 1);
    let err = dispatch(&ctx, "tool_save", json!({"tool": stale})).await.unwrap_err();
    assert_eq!(err.0, "工具已修改，请重新打开编辑");
    // An incomplete draft may be stored disabled but never enabled.
    let half: Tool =
        serde_json::from_value(dispatch(&ctx, "tool_save", json!({"tool": {"name": "half"}})).await.unwrap()).unwrap();
    let err = dispatch(&ctx, "tool_toggle", json!({"id": half.id, "updated": half.updated, "enabled": true}))
        .await
        .unwrap_err();
    assert_eq!(err.0, "启用/测试前请填写工具名、说明和 CMD 命令模板");
    let off =
        dispatch(&ctx, "tool_toggle", json!({"id": tool.id, "updated": tool.updated, "enabled": false})).await.unwrap();
    let err = dispatch(&ctx, "tool_delete", json!({"id": tool.id, "updated": tool.updated})).await.unwrap_err();
    assert_eq!(err.0, "工具已修改，请刷新");
    dispatch(&ctx, "tool_delete", json!({"id": tool.id, "updated": off["updated"]})).await.unwrap();
    let list = dispatch(&ctx, "tool_list", json!({})).await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0].get("compatibility_issue").is_none());
    let state = dispatch(&ctx, "state", json!({"page": "settings"})).await.unwrap();
    assert_eq!(state["config"]["tools"][0]["name"], "half");
}

#[tokio::test]
async fn making_prompt_neither_registers_nor_runs() {
    let ctx = ctx();
    let text = dispatch(&ctx, "tool_making_prompt", json!({"tool": {"name": "lookup"}})).await.unwrap();
    let text = text["text"].as_str().unwrap();
    assert!(text.contains("lookup") && text.contains("待填写"));
    assert!(!text.contains("{{tool_name}}"));
    assert_eq!(dispatch(&ctx, "tool_list", json!({})).await.unwrap(), json!([]));
}

#[tokio::test]
async fn preview_quotes_losslessly_and_refuses_what_it_cannot_express() {
    let ctx = ctx();
    let tool = draft("t", "lookup --id {{id}} --note {{note}}", &["id", "note"]);
    let out = dispatch(&ctx, "tool_command_preview", json!({"tool": tool, "arguments": {"id": "A 1"}})).await.unwrap();
    assert_eq!(out["command"], "lookup --id \"A 1\" --note \"\"");
    for bad in ["含\"引号", "100%", "末尾\\"] {
        let err =
            dispatch(&ctx, "tool_command_preview", json!({"tool": tool, "arguments": {"id": bad}})).await.unwrap_err();
        assert!(err.0.starts_with("参数包含 CMD 通用引用不能无损表达"), "{bad}");
    }
    let err = dispatch(&ctx, "tool_command_preview", json!({"tool": tool, "arguments": {"id": 1}})).await.unwrap_err();
    assert_eq!(err.0, "参数 id 必须是字符串");
    let err = dispatch(&ctx, "tool_command_preview", json!({"tool": tool, "arguments": {"x": "1"}})).await.unwrap_err();
    assert_eq!(err.0, "包含未定义参数");
    let glued = draft("t", "lookup --id={{id}}", &["id"]);
    let err = dispatch(&ctx, "tool_command_preview", json!({"tool": glued, "arguments": {}})).await.unwrap_err();
    assert_eq!(err.0, "每个 {{参数名}} 必须独占一个参数，前面留空格，外面无需加引号");
}

fn tool(name: &str, template: &str, params: &[&str]) -> Tool {
    let mut t: Tool = serde_json::from_value(draft(name, template, params)).unwrap();
    t.id = format!("id-{name}");
    t
}

/// Registers a tool so the live recheck before each start finds it.
fn saved(ctx: &Ctx, name: &str, template: &str, params: &[&str]) -> Tool {
    store(ctx, draft(name, template, params))
}

fn exec(t: &Tool, args: Value, cancel: &AtomicBool, deadline: Instant) -> CommandResult {
    runner::execute_with_start(
        t,
        "c1",
        &args,
        || cancel.load(std::sync::atomic::Ordering::SeqCst),
        deadline,
        |cmd| runner::spawn(cmd).map_err(|e| (false, e)),
    )
}

#[test]
fn command_results_report_status_limits_and_deadlines() {
    let idle = AtomicBool::new(false);
    let ok = exec(&tool("a", &fx("out {{v}} err err"), &["v"]), json!({"v": "中 文"}), &idle, later());
    assert_eq!(
        (ok.status.as_str(), ok.stdout.as_str(), ok.stderr.as_str(), ok.exit_code),
        ("completed", "中 文", "err", Some(0))
    );
    let failed = exec(&tool("b", &fx("exit 7"), &[]), json!({}), &idle, later());
    assert_eq!((failed.status.as_str(), failed.exit_code), ("nonzero_exit", Some(7)));
    let big = exec(&tool("c", &fx("fill 40000"), &[]), json!({}), &idle, later());
    assert!(big.truncated && big.stdout.len() == runner::OUTPUT_LIMIT, "{}", big.stdout.len());
    let bad = exec(&tool("d", &fx("invalid"), &[]), json!({}), &idle, later());
    assert_eq!(bad.status, "output_error");
    assert_eq!(bad.error, "工具输出不是有效 UTF-8，请调整工具输出编码");
    let slow = exec(&tool("e", &fx("sleep 5"), &[]), json!({}), &idle, Instant::now() + Duration::from_millis(300));
    assert!(slow.timed_out && slow.status == "timed_out" && slow.duration_ms < 4000);
}

#[test]
fn cancellation_kills_the_tree_and_keeps_partial_output() {
    let root = crate::testing::temp_dir("tool-cancel");
    let ready = root.join("ready");
    let started = Instant::now();
    // Cancel after stdout is flushed and the child exists, even on a busy host.
    let r = runner::execute_with_start(
        &tool("a", &fx("out partial child 20 sleep 0 touch {{ready}} sleep 20"), &["ready"]),
        "c1",
        &json!({"ready": ready}),
        || ready.exists(),
        later(),
        |cmd| runner::spawn(cmd).map_err(|e| (false, e)),
    );
    std::fs::remove_dir_all(root).unwrap();
    assert!(r.cancelled && r.status == "cancelled", "{r:?}");
    assert_eq!(r.stdout, "partial");
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn session(ctx: &Arc<Ctx>, run: &str, task: &str, tools: Vec<Tool>) -> Session {
    Session::new(ctx, run, "classify", task, tools, json!({"driver": "api"}))
}

fn stored(ctx: &Ctx, run: &str) -> Vec<CallRecord> {
    ctx.db.tool_calls_blocking(run.into()).unwrap()
}

#[test]
fn live_recheck_blocks_disabled_or_changed_tools_and_intents_persist_first() {
    let ctx = ctx();
    let t = saved(&ctx, "mark", &fx("out ran"), &[]);
    let s = session(&ctx, "run-1", "", vec![t.clone()]);
    let ctx2 = ctx.clone();
    let probe = std::thread::spawn(move || {
        let done = run(&s, "mark", "c1", json!({})).unwrap();
        assert_eq!(done["stdout"], "ran");
        // Metadata may change mid-session; only the command matters.
        let mut renamed = json!(t);
        renamed["description"] = json!("新说明");
        let t2 = store(&ctx2, renamed);
        assert_eq!(run(&s, "mark", "c2", json!({})).unwrap()["status"], "completed");
        let mut changed = json!(t2);
        changed["command_template"] = json!(fx("out other"));
        let t3 = store(&ctx2, changed);
        let r = run(&s, "mark", "c3", json!({})).unwrap();
        assert_eq!(r["status"], "rejected");
        assert_eq!(r["error"], "工具执行配置已变更，本轮旧命令未执行；请依据已有资料完成。");
        let db = ctx2.db.clone();
        block_on(async move { db.toggle_tool(t3.id, Some(t3.updated), false).await }).unwrap();
        let r = run(&s, "mark", "c4", json!({})).unwrap();
        assert_eq!(r["error"], "工具已停用、删除或事项已不可用，未执行命令");
        s.finish(None, &Value::Null).unwrap();
    });
    probe.join().unwrap();
    let calls = stored(&ctx, "run-1");
    assert_eq!(calls.iter().map(|c| c.call_id.as_str()).collect::<Vec<_>>(), ["c1", "c2", "c3", "c4"]);
    assert!(calls.iter().all(|c| c.result.is_some()));
}

#[test]
fn intent_is_stored_before_the_process_and_a_closed_task_blocks_calls() {
    let ctx = ctx();
    let dir = crate::testing::temp_dir("tool-intent");
    let flag = dir.join("started");
    let t = saved(&ctx, "wait", &fx("touch {{p}} sleep 1"), &["p"]);
    add_task(&ctx, "task-1");
    let s = Arc::new(session(&ctx, "run-2", "task-1", vec![t]));
    let (worker, path) = (s.clone(), flag.display().to_string());
    let handle = std::thread::spawn(move || run(&worker, "wait", "c1", json!({"p": path})).unwrap());
    let until = Instant::now() + Duration::from_secs(20);
    while !flag.exists() {
        if handle.is_finished() || Instant::now() > until {
            panic!("fixture never started: {:?}", handle.join());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let open = stored(&ctx, "run-2");
    assert_eq!(open.len(), 1);
    assert!(open[0].result.is_none(), "intent without result while running");
    assert_eq!(handle.join().unwrap()["status"], "completed");
    ctx.db
        .test_write_blocking(|tx| Ok(tx.execute("UPDATE tasks SET merged_into = 'x' WHERE id = 'task-1'", [])?))
        .unwrap();
    std::fs::remove_file(&flag).unwrap();
    let r = run(&s, "wait", "c2", json!({"p": flag.display().to_string()})).unwrap();
    assert_eq!(r["status"], "rejected");
    assert!(!flag.exists(), "no process started");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn uncertain_results_are_never_replayed_but_known_results_allow_new_calls() {
    let ctx = ctx();
    let slow = saved(&ctx, "slow", &fx("sleep {{s}}"), &["s"]);
    let s = session(&ctx, "run-3", "", vec![slow.clone()]);
    let deadline = Instant::now() + Duration::from_millis(300);
    let timed = s.call("slow", "c1", &json!({"s": "3"}), &AtomicBool::new(false), deadline, None).unwrap();
    assert!(timed.contains("timed_out"));
    let err = run(&s, "slow", "c2", json!({"s": "3"})).unwrap_err();
    assert_eq!(err.kind, ToolFailureKind::UnsafeReplay);
    assert!(err.message.unwrap().starts_with("先前调用 c1 的命令可能已执行"));
    // Same call id twice is refused outright.
    assert_eq!(run(&s, "slow", "c1", json!({"s": "0"})).unwrap_err().kind, ToolFailureKind::Failed);
    // Different arguments, then a known result, may run again.
    assert_eq!(run(&s, "slow", "c3", json!({"s": "0"})).unwrap()["status"], "completed");
    assert_eq!(run(&s, "slow", "c4", json!({"s": "0"})).unwrap()["status"], "completed");
    // An intent left by a crash blocks the batch check as well.
    let id = slow.id.clone();
    ctx.db
        .test_write_blocking(move |tx| {
            tx.execute(
                "INSERT INTO tool_calls(run_id, seq, call_id, tool_id, tool, arguments, started)
                 VALUES ('run-3', 9, 'c9', ?1, 'slow', '{\"s\":\"5\"}', 0)",
                [&id],
            )?;
            Ok(())
        })
        .unwrap();
    let args = json!({"s": "5"});
    let err = s.check_batch(&[("c10", "slow", &args)]).unwrap_err();
    assert_eq!(err.kind, ToolFailureKind::UnsafeReplay);
    assert!(err.message.unwrap().contains("执行中"));
}

#[test]
fn invalid_batches_are_recorded_rejected_and_nothing_runs() {
    let ctx = ctx();
    let t = tool("echo", &fx("out {{v}}"), &["v"]);
    let s = session(&ctx, "run-4", "", vec![t]);
    let (good, bad) = (json!({"v": "ok"}), json!({"v": "100%"}));
    let err = s.check_batch(&[("a", "echo", &good), ("b", "echo", &bad)]).unwrap_err();
    assert_eq!(err.kind, ToolFailureKind::InvalidArguments);
    let calls = stored(&ctx, "run-4");
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.result.as_ref().unwrap().status == "rejected"));
    assert_eq!(s.check_batch(&[("c", "missing", &good)]).unwrap_err().kind, ToolFailureKind::Denied);
    // Provider-side refusals keep the raw arguments and no invented metadata.
    s.record_rejection(&json!({"error": "参数不是 JSON", "response": {"tool_calls": [
        {"id": "d", "function": {"name": "echo", "arguments": "{\"v\":\"x\"}"}},
        {"id": "e", "function": {"name": "ghost", "arguments": "not json"}}]}}))
        .unwrap();
    let calls = stored(&ctx, "run-4");
    assert_eq!(calls[2].arguments, json!({"v": "x"}));
    assert_eq!((calls[3].tool_id.as_str(), calls[3].arguments.clone()), ("", json!("not json")));
}

#[test]
fn rate_limit_counts_starts_per_tool() {
    let runtime = Runtime::default();
    let t = tool("a", "true", &[]);
    for _ in 0..registry::CALLS_PER_MINUTE {
        runtime.charge(&t).unwrap();
    }
    assert_eq!(runtime.charge(&t).unwrap_err().0, "已达到此工具每分钟调用上限，未执行命令");
    runtime.charge(&tool("b", "true", &[])).unwrap();
}

#[test]
fn exhausted_continuation_budget_records_rejection_without_running() {
    let ctx = Ctx::memory(Config { retrieval_budget_chars: 200, ..Config::default() }).unwrap();
    let dir = crate::testing::temp_dir("tool-budget");
    let flag = dir.join("ran");
    let t = saved(&ctx, "touch", &fx("touch {{p}}"), &["p"]);
    let s = session(&ctx, "run-5", "", vec![t]);
    let err = run(&s, "touch", "c1", json!({"p": flag.display().to_string()})).unwrap_err();
    assert_eq!(err.kind, ToolFailureKind::BudgetExhausted);
    assert!(!flag.exists());
    let calls = stored(&ctx, "run-5");
    assert_eq!(calls[0].result.as_ref().unwrap().error, "本次工具续轮已达到输入预算，未执行命令");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn api_budget_counts_outer_json_and_low_budgets_start_nothing() {
    let ctx = ctx();
    let t = saved(&ctx, "echo", &fx("out {{v}}"), &["v"]);
    let s = session(&ctx, "run-6", "", vec![t]);
    let cancel = AtomicBool::new(false);
    let text = s.call("echo", "c1", &json!({"v": "\"q\""}), &cancel, later(), Some(10_000)).unwrap_err();
    // Double quotes cannot be expressed losslessly; the provider is told.
    assert_eq!(text.kind, ToolFailureKind::Denied);
    let ok = s.call("echo", "c2", &json!({"v": "abc"}), &cancel, later(), Some(10_000)).unwrap();
    assert!(ok.contains("\"stdout\":\"abc\""));
    let err = s.call("echo", "c3", &json!({"v": "x"}), &cancel, later(), Some(50)).unwrap_err();
    assert_eq!(err.kind, ToolFailureKind::BudgetExhausted);
    assert_eq!(stored(&ctx, "run-6")[2].result.as_ref().unwrap().status, "rejected");
}

#[test]
fn trimmed_results_share_space_count_escapes_and_keep_metadata() {
    let full = CommandResult {
        tool: "t".into(),
        call_id: "c".into(),
        status: "completed".into(),
        exit_code: Some(0),
        stdout: "o\n".repeat(400),
        stderr: "错".repeat(30),
        ..Default::default()
    };
    let whole = tool_result_text(full.clone(), 100_000, true).unwrap();
    assert_eq!(serde_json::from_str::<CommandResult>(&whole).unwrap(), full);
    let cut = tool_result_text(full.clone(), 600, true).unwrap();
    let escaped: usize = serde_json::to_string(&cut).unwrap().chars().count() - 2;
    assert!(escaped <= 600, "{escaped}");
    let r: CommandResult = serde_json::from_str(&cut).unwrap();
    assert!(r.truncated && full.stdout.starts_with(&r.stdout) && full.stderr.starts_with(&r.stderr));
    assert!(!r.stderr.is_empty(), "stderr keeps a share");
    assert_eq!((r.status.as_str(), r.exit_code), ("completed", Some(0)));
    assert!(tool_result_text(full, 40, true).is_none());
}

#[tokio::test]
async fn local_test_shares_limits_cancellation_and_records() {
    let ctx = ctx();
    let t = draft("echo", &fx("out {{v}}"), &["v"]);
    let r = dispatch(&ctx, "tool_test", json!({"tool": t, "test_id": "t1", "arguments": {"v": "hi"}})).await.unwrap();
    assert_eq!((r["status"].as_str(), r["stdout"].as_str()), (Some("completed"), Some("hi")));
    let run = dispatch(&ctx, "tool_run_get", json!({"run_id": "t1"})).await.unwrap();
    assert_eq!((run["purpose"].as_str(), run["status"].as_str()), (Some("tool_test"), Some("completed")));
    assert_eq!(run["source"], Value::Null);
    assert!(run.get("prompt_version").is_none());
    assert!(run["tools"][0].get("command_template").is_none());
    // A cancel that arrives first is remembered for that test id.
    assert_eq!(dispatch(&ctx, "tool_test_cancel", json!({"test_id": "t2"})).await.unwrap(), json!(true));
    let r = dispatch(&ctx, "tool_test", json!({"tool": t, "test_id": "t2", "arguments": {}})).await.unwrap();
    assert_eq!(r["status"], "cancelled");
    let run = dispatch(&ctx, "tool_run_get", json!({"run_id": "t2"})).await.unwrap();
    assert_eq!(run["status"], "cancelled");
    let err = dispatch(&ctx, "tool_test", json!({"tool": t, "arguments": {}})).await.unwrap_err();
    assert_eq!(err.0, "缺少测试编号");
    let failed =
        dispatch(&ctx, "tool_test", json!({"tool": t, "test_id": "t3", "arguments": {"v": "%"}})).await.unwrap();
    assert_eq!(failed["status"], "rejected");
    let log = ctx.db.test_read(|conn| crate::data::activity::recent(conn, 20)).await.unwrap();
    assert!(log.iter().any(|e| e.to_string().contains("tool_call_finished")));
}

#[tokio::test]
async fn interrupted_runs_and_history_filters_scope_and_pages() {
    let ctx = ctx();
    let shared = ctx.clone();
    let (echo, other) = exec::blocking(move || {
        add_task(&shared, "task-a");
        Ok((saved(&shared, "echo", &fx("out {{v}}"), &["v"]), saved(&shared, "other", &fx("exit 0"), &[])))
    })
    .await
    .unwrap();
    let shared = ctx.clone();
    let (e, o) = (echo.clone(), other.clone());
    exec::blocking(move || {
        let s = session(&shared, "run-a", "task-a", vec![e.clone()]);
        for i in 0..25 {
            run(&s, "echo", &format!("a{i}"), json!({"v": format!("v{i}")})).unwrap();
        }
        s.finish(None, &Value::Null)?;
        let s = Session::new(&shared, "run-b", "format", "", vec![e, o], json!({"driver": "api"}));
        run(&s, "echo", "b1", json!({"v": "linked"})).unwrap();
        run(&s, "other", "b2", json!({})).unwrap();
        s.finish(None, &Value::Null)?;
        shared.db.test_write_blocking(|tx| {
            crate::data::tools::link_tool_calls(tx, &json!({"_tool_run_id": "run-b", "tool_call_ids": ["b1"]}), &["task-a".into(), "task-b".into()])
        })?;
        // A run left open by a dead session reads as interrupted.
        let s = Session::new(&shared, "run-c", "edit", "", vec![], Value::Null);
        shared.db.test_write_blocking(|tx| {
            tx.execute(
                "INSERT INTO tool_runs(id, request_id, purpose, tools, started, status) VALUES ('run-c', 'run-c', 'edit', '[]', 0, 'running')",
                [],
            )?;
            Ok(())
        })?;
        drop(s);
        Ok(())
    })
    .await
    .unwrap();
    let c = dispatch(&ctx, "tool_run_get", json!({"run_id": "run-c"})).await.unwrap();
    assert_eq!(c["status"], "interrupted");
    assert_eq!(c["error"], "运行已结束但终态未能保存，请查看日志。已执行命令不会自动重放。");

    let page = dispatch(&ctx, "tool_run_history", json!({"task_id": "task-a", "page": 2})).await.unwrap();
    assert_eq!(
        (page["total"].as_u64(), page["pages"].as_u64(), page["rows"].as_array().unwrap().len()),
        (Some(26), Some(2), 6)
    );
    let found = dispatch(&ctx, "tool_run_history", json!({"query": "LINKED 事项生成"})).await.unwrap();
    assert_eq!(found["total"], 1);
    assert_eq!(found["rows"][0]["call_id"], "b1");
    let by_tool = dispatch(&ctx, "tool_run_history", json!({"tool_id": other.id})).await.unwrap();
    assert_eq!(by_tool["total"], 1);
    let names: Vec<&str> = by_tool["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"echo") && names.contains(&"other"));
    let none = dispatch(&ctx, "tool_run_history", json!({"day": "2000-01-01"})).await.unwrap();
    assert_eq!(none["total"], 0);
    let today = dispatch(&ctx, "tool_run_history", json!({"day": crate::time::today()})).await.unwrap();
    assert_eq!(today["total"], 27);
    for (p, msg) in
        [(json!({"day": "2024-2-1"}), "日期无效"), (json!({"query": "长".repeat(201)}), "搜索内容不能超过 200 字")]
    {
        assert_eq!(dispatch(&ctx, "tool_run_history", p).await.unwrap_err().0, msg);
    }

    // The task's view of run-b shows only the linked call.
    let b = dispatch(&ctx, "tool_run_get", json!({"run_id": "run-b", "task_id": "task-b"})).await.unwrap();
    assert_eq!(b["call_count"], 1);
    assert_eq!(b["calls"][0]["task_ids"], json!(["task-a", "task-b"]));
    let err = dispatch(&ctx, "tool_run_get", json!({"run_id": "run-b", "task_id": "task-b", "call_index": 1}))
        .await
        .unwrap_err();
    assert_eq!(err.0, "该调用不属于当前事项");
    for (index, msg) in [(json!(-1), "调用序号无效"), (json!(9), "调用记录已清理")] {
        let err = dispatch(&ctx, "tool_run_get", json!({"run_id": "run-b", "call_index": index})).await.unwrap_err();
        assert_eq!(err.0, msg);
    }
    let err = dispatch(&ctx, "tool_run_get", json!({"run_id": "run-c", "task_id": "task-a"})).await.unwrap_err();
    assert_eq!(err.0, "该调用不属于当前事项");
    let latest = dispatch(&ctx, "tool_run_get", json!({"task_id": "task-b"})).await.unwrap();
    assert_eq!(latest["id"], "run-b");
    assert_eq!(dispatch(&ctx, "tool_run_get", json!({"run_id": "gone"})).await.unwrap(), Value::Null);

    // Reopening the database marks runs a previous process left open.
    let dir = crate::testing::temp_dir("tool-recover");
    let path = dir.join("shishi.db");
    crate::data::Db::open(&path)
        .unwrap()
        .test_write(|tx| {
            tx.execute(
                "INSERT INTO tool_runs(id, request_id, purpose, tools, started, status) VALUES ('run-c', 'run-c', 'edit', '[]', 0, 'running')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let c = crate::data::Db::open(&path).unwrap().tool_run("run-c".into(), false).await.unwrap().unwrap();
    assert_eq!(
        (c.status.as_str(), c.error.as_str()),
        ("interrupted", "软件上次退出前运行未完成；已执行操作的结果可能不确定，不会自动重放")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn single_task_results_link_every_call_and_explicit_lists_only_those() {
    let ctx = ctx();
    let s = session(&ctx, "run-l", "", vec![saved(&ctx, "echo", &fx("out {{v}}"), &["v"])]);
    run(&s, "echo", "x", json!({"v": "1"})).unwrap();
    run(&s, "echo", "y", json!({"v": "2"})).unwrap();
    let count = |ctx: &Ctx| {
        ctx.db
            .test_read_blocking(
                |c| Ok(c.query_row("SELECT count(*) FROM tool_call_tasks", [], |r| r.get::<_, i64>(0))?),
            )
            .unwrap()
    };
    ctx.db
        .test_write_blocking(|tx| {
            crate::data::tools::link_tool_calls(tx, &json!({"_tool_run_id": "run-l"}), &["t1".into(), "t2".into()])
        })
        .unwrap();
    assert_eq!(count(&ctx), 0);
    ctx.db
        .test_write_blocking(|tx| {
            crate::data::tools::link_tool_calls(tx, &json!({"_tool_run_id": "run-l"}), &["t1".into()])
        })
        .unwrap();
    assert_eq!(count(&ctx), 2);
    ctx.db
        .test_write_blocking(|tx| {
            crate::data::tools::link_tool_calls(
                tx,
                &json!({"_tool_run_id": "run-l", "tool_call_ids": ["x", "ghost"]}),
                &["t3".into()],
            )
        })
        .unwrap();
    assert_eq!(count(&ctx), 3);
}
