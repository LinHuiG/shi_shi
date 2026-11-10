//! Offline tests of the archive domain with synthetic tasks and a stub model.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::{json, Value};

use super::run;
use super::*;
use crate::data::archive as store;
use crate::data::kv;
use crate::exec::Cancel;
use crate::tasks::model as tasks;
use crate::{dispatch, exec, Config, Ctx, Db};

type Edit = Box<dyn FnOnce(&Connection) -> Result<()> + Send>;

/// A model that answers from a queue, records every request, can run an
/// edit while "thinking" and accepts at most `max_evidence` evidence rows
/// and `max_delta` report changes.
#[derive(Default)]
struct Stub {
    replies: Mutex<VecDeque<Result<Value>>>,
    seen: Mutex<Vec<Prepared>>,
    edits: Mutex<VecDeque<Option<Edit>>>,
    max_evidence: Option<usize>,
    max_delta: Option<usize>,
    db: Option<Db>,
}

impl Stub {
    fn new(ctx: &Ctx) -> Self {
        Self { db: Some(ctx.db.clone()), ..Default::default() }
    }

    fn reply(self, v: Result<Value>) -> Self {
        self.replies.lock().unwrap().push_back(v);
        self
    }

    fn during(self, edit: impl FnOnce(&Connection) -> Result<()> + Send + 'static) -> Self {
        self.edits.lock().unwrap().push_back(Some(Box::new(edit)));
        self
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Model for Stub {
    fn measure(&self, request: Prepared) -> Result<Measured<Prepared>> {
        let n = request.payload["evidence"].as_array().map_or(0, Vec::len)
            + request.payload["current_tasks"].as_array().map_or(0, Vec::len);
        let delta = request.payload["delta"].as_array().map_or(0, Vec::len);
        let fits = self.max_evidence.is_none_or(|m| n <= m) && self.max_delta.is_none_or(|m| delta <= m);
        let total = if fits { 0 } else { usize::MAX };
        Ok(Measured::new(request, total))
    }

    fn invoke<'a>(&'a self, request: Measured<Prepared>, _cancel: Cancel) -> ModelFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(request.request);
            let edit = self.edits.lock().unwrap().pop_front().flatten();
            if let (Some(edit), Some(db)) = (edit, &self.db) {
                db.test_write(move |tx| edit(tx)).await?;
            }
            self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(json!({"report": {"summary": "ok", "completed": [], "attention": [], "next_steps": []}}))
            })
        })
    }
}

fn ctx() -> Arc<Ctx> {
    Ctx::memory(Config::default()).unwrap()
}

fn summary(text: &str) -> Value {
    json!({"report": {"summary": text, "completed": [], "attention": [], "next_steps": []}})
}

async fn read<R: Send + 'static>(ctx: &Ctx, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> R {
    ctx.db.test_read(f).await.unwrap()
}

async fn write<R: Send + 'static>(ctx: &Ctx, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> R {
    ctx.db.test_write(move |tx| f(tx)).await.unwrap()
}

/// Create a task with one `manual_edit` audit row.
async fn task(ctx: &Ctx, title: &str, status: &str) -> tasks::Task {
    let (title, status) = (title.to_string(), status.to_string());
    write(ctx, move |c| {
        let v = crate::data::tasks::edit(c, &json!({"task": {"title": title, "status": status}}))?;
        Ok(crate::data::tasks::get(c, v["id"].as_str().unwrap())?.unwrap())
    })
    .await
}

async fn get_task(ctx: &Ctx, id: &str) -> tasks::Task {
    let id = id.to_string();
    read(ctx, move |c| Ok(crate::data::tasks::get(c, &id)?.unwrap())).await
}

async fn manual(ctx: &Ctx, model: &Stub) -> Result<Value> {
    run::archive(ctx, model, "manual_archive", Mode::Manual, None).await
}

async fn last_attempt(ctx: &Ctx) -> Attempt {
    read(ctx, |c| Ok(store::last_attempt(c)?.unwrap())).await
}

async fn logs(ctx: &Ctx, kind: &'static str) -> Vec<Value> {
    read(ctx, move |c| {
        let mut stmt = c.prepare("SELECT detail FROM activity WHERE kind = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map([kind], |r| r.get::<_, String>(0))?
            .map(|r| r.map(|t| serde_json::from_str(&t).unwrap()))
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })
    .await
}

async fn cursor(ctx: &Ctx, key: &'static str) -> i64 {
    read(ctx, move |c| kv::get_i64(c, key)).await
}

// ---- archive runs ---------------------------------------------------------

#[tokio::test]
async fn archive_commits_summary_cursor_tasks_and_group_together() {
    let ctx = ctx();
    let done = task(&ctx, "整理周报", "已完成").await;
    let open = task(&ctx, "跟进合同", "待处理").await;
    let stub = Stub::new(&ctx).reply(Ok(summary("本日完成周报")));
    let r = manual(&ctx, &stub).await.unwrap();
    assert_eq!(stub.calls(), 1);
    let sent = &stub.seen.lock().unwrap()[0];
    assert_eq!(sent.purpose, "archive");
    assert_eq!(sent.payload["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(sent.payload["memory"]["writable"], false);
    assert_eq!(sent.payload["_trigger"], "manual_archive");
    assert_eq!((r["summary_status"].as_str(), r["archived_tasks"].as_u64()), (Some("done"), Some(1)));
    assert_eq!(r["archive_complete"], true);
    assert_eq!(r["label"], "手动归档");
    assert!(get_task(&ctx, &done.id).await.archived);
    assert!(!get_task(&ctx, &open.id).await.archived);
    let a = last_attempt(&ctx).await;
    assert_eq!((a.status.as_str(), a.batches, a.processed, a.remaining, a.archived_tasks), ("succeeded", 1, 2, 0, 1));
    assert_eq!(a.report_id, r["id"].as_str().unwrap());
    let max = read(&ctx, |c| {
        Ok(c.query_row(
            "SELECT max(seq) FROM task_audit WHERE kind NOT IN ('archive_item', 'archive_commit')",
            [],
            |r| r.get::<_, i64>(0),
        )?)
    })
    .await;
    assert_eq!(cursor(&ctx, ARCHIVE_CURSOR).await, max);
    assert!(cursor(&ctx, LEARNING_CURSOR).await >= max);
    let h = dispatch(&ctx, "archive_history", json!({})).await.unwrap();
    assert_eq!(h["versions"].as_array().unwrap().len(), 1);
    assert_eq!(h["current"], r["archive_group_id"]);
    assert_eq!(logs(&ctx, "archive_result").await.len(), 1);
}

#[tokio::test]
async fn no_new_input_reuses_the_last_summary_or_falls_back_without_ai() {
    let ctx = ctx();
    let stub = Stub::new(&ctx);
    let first = manual(&ctx, &stub).await.unwrap();
    assert_eq!(stub.calls(), 0);
    assert_eq!(first["summary_status"], "fallback");
    let a = last_attempt(&ctx).await;
    assert_eq!((a.status.as_str(), a.reused), ("succeeded", false));

    // A fallback is not reused; a real summary is.
    task(&ctx, "核对发票", "已完成").await;
    let done = manual(&ctx, &stub).await.unwrap();
    assert_eq!((stub.calls(), done["summary_status"].as_str()), (1, Some("done")));
    let reused = manual(&ctx, &stub).await.unwrap();
    assert_eq!(stub.calls(), 1, "nothing new: no model call");
    assert_eq!(reused["summary_status"], "reused");
    assert_eq!(reused["reused_from"], done["id"]);
    assert_eq!(reused["ai_summary"], done["ai_summary"]);
    let a = last_attempt(&ctx).await;
    assert_eq!((a.status.as_str(), a.reused, a.batches), ("succeeded", true, 0));
    assert_eq!(
        read(&ctx, |c| Ok(
            c.query_row("SELECT count(*) FROM task_audit WHERE kind = 'archive_reuse'", [], |r| r.get::<_, i64>(0))?
        ))
        .await,
        2
    );
}

#[tokio::test]
async fn manual_archive_may_call_the_model_only_to_consolidate_memory() {
    let ctx = ctx();
    write(&ctx, |c| crate::data::prompts::replace_memory(c, "archived", 0, "- 客户：偏好周五沟通").map(drop)).await;
    let stub = Stub::new(&ctx).reply(Ok(json!({
        "report": {"summary": "整理记忆"},
        "_archived_memory_update": {"expected": 1, "before": "- 客户：偏好周五沟通", "text": "客户偏好周五沟通"},
    })));
    let r = manual(&ctx, &stub).await.unwrap();
    assert_eq!((stub.calls(), r["memory_changes"].as_u64()), (1, Some(1)));
    let m = read(&ctx, |c| crate::data::prompts::memory(c, "archived")).await;
    assert_eq!((m.text.as_str(), m.revision), ("客户偏好周五沟通", 2));
    let audit: String =
        read(
            &ctx,
            |c| Ok(c.query_row("SELECT after FROM task_audit WHERE kind = 'archive_memory'", [], |r| r.get(0))?),
        )
        .await;
    assert!(!audit.contains("周五"), "the audit row carries no memory text");
}

#[tokio::test]
async fn archive_merges_the_temporary_memory_then_clears_it() {
    let ctx = ctx();
    let temporary = |text: &'static str, expected: i64| {
        move |c: &Connection| crate::data::prompts::replace_memory(c, "temporary", expected, text).map(drop)
    };
    write(&ctx, temporary("- 合成客户周五前答复", 0)).await;
    let stub = Stub::new(&ctx).reply(Ok(json!({
        "report": {"summary": "合并临时记忆"},
        "_archived_memory_update": {"expected": 0, "before": "", "text": "合成客户通常周五答复"},
    })));
    let r = manual(&ctx, &stub).await.unwrap();
    assert_eq!(stub.seen.lock().unwrap()[0].payload["_temporary_memory"], "- 合成客户周五前答复");
    assert_eq!((r["memory_changes"].as_u64(), r["temporary_memory_cleared"].as_bool()), (Some(1), Some(true)));
    let m = read(&ctx, |c| crate::data::prompts::memory(c, "temporary")).await;
    assert_eq!((m.text.as_str(), m.revision), ("", 2));

    // A note written while the model ran was not merged and is kept.
    write(&ctx, temporary("- 合成旧笔记", 2)).await;
    let stub = Stub::new(&ctx).during(temporary("- 合成新笔记", 3)).reply(Ok(summary("s")));
    let r = manual(&ctx, &stub).await.unwrap();
    assert_eq!(r["temporary_memory_cleared"], false);
    assert_eq!(read(&ctx, |c| crate::data::prompts::memory(c, "temporary")).await.text, "- 合成新笔记");
    assert_eq!(logs(&ctx, "archive_conflict").await[0]["kind"], "temporary_memory");
}

#[tokio::test]
async fn all_batches_of_one_attempt_finish_in_that_attempt() {
    let ctx = ctx();
    for i in 0..5 {
        task(&ctx, &format!("事项{i}"), "已完成").await;
    }
    let stub = Stub { max_evidence: Some(2), ..Stub::new(&ctx) };
    let r = manual(&ctx, &stub).await.unwrap();
    assert_eq!(stub.calls(), 3);
    for sent in stub.seen.lock().unwrap().iter() {
        assert!(sent.payload["current_tasks"].as_array().unwrap().is_empty());
        assert!(sent.payload["evidence"].as_array().unwrap().len() <= 2);
    }
    assert_eq!(r["remaining_events"], 0);
    assert_eq!(r["batch_summaries"].as_array().unwrap().len(), 3);
    let a = last_attempt(&ctx).await;
    assert_eq!((a.status.as_str(), a.batches, a.processed, a.archived_tasks), ("succeeded", 3, 5, 5));
    let reports = read(&ctx, store::reports).await;
    assert_eq!(reports.iter().filter(|r| r["attempt_id"] == a.id.as_str()).count(), 3);
    assert_eq!(read(&ctx, |c| Ok(store::groups(c)?.len())).await, 3);
}

#[tokio::test]
async fn batches_read_evidence_page_by_page_up_to_the_budget() {
    let ctx = ctx();
    for i in 0..9 {
        task(&ctx, &format!("事项{i}"), "已完成").await;
    }
    write(&ctx, |c| crate::data::audit::write(c, "scan_note", "", "不是事项证据", &Value::Null, &Value::Null)).await;
    let m = read(&ctx, store::load).await;
    assert_eq!(
        (m.evidence.len(), m.exhausted, m.total, m.linked.len()),
        (2, false, 9, 2),
        "only the first page is read"
    );
    assert!(m.current.is_empty(), "current tasks wait for a batch that takes all evidence");
    let stub = Stub { max_evidence: Some(4), ..Stub::new(&ctx) };
    let r = manual(&ctx, &stub).await.unwrap();
    let seen = stub.seen.lock().unwrap();
    let sizes: Vec<usize> = seen.iter().map(|p| p.payload["evidence"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, [4, 4, 1]);
    let seqs: Vec<i64> = seen
        .iter()
        .flat_map(|p| p.payload["evidence"].as_array().unwrap().iter().map(|e| e["audit_index"].as_i64().unwrap()))
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "batches continue where the last commit ended");
    assert!(
        seen.iter()
            .all(|p| p.payload["report_tasks"].as_array().unwrap().len()
                == p.payload["evidence"].as_array().unwrap().len()),
        "each batch carries only its linked tasks"
    );
    assert_eq!(seen[0].payload["coverage"]["remaining_events"], 5);
    assert_eq!(r["archive_complete"], true);
    let note: i64 =
        read(&ctx, |c| Ok(c.query_row("SELECT seq FROM task_audit WHERE kind = 'scan_note'", [], |r| r.get(0))?)).await;
    assert!(cursor(&ctx, ARCHIVE_CURSOR).await >= note, "the last batch covers the trailing row");
    assert_eq!(last_attempt(&ctx).await.archived_tasks, 9);
}

#[tokio::test]
async fn a_task_waits_until_its_latest_event_is_processed() {
    let ctx = ctx();
    let t = task(&ctx, "先改后结", "待处理").await;
    let second = task(&ctx, "其他", "已完成").await;
    let id = t.id.clone();
    write(&ctx, move |c| {
        let t = crate::data::tasks::get(c, &id)?.unwrap();
        crate::data::tasks::edit(c, &json!({"task": {"id": id, "updated": t.updated, "status": "已完成"}})).map(drop)
    })
    .await;
    // Batch 1 sees only the first event of `t`; its closing edit is later.
    let stub = Stub { max_evidence: Some(1), ..Stub::new(&ctx) };
    let built = read(&ctx, |c| {
        let m = store::load(c)?;
        let a = Attempt { id: "a".into(), trigger: "manual_archive".into(), ..Default::default() };
        Ok(run::build(&m, &a, Mode::Manual, 0, 1, false))
    })
    .await;
    assert!(built.tasks_to_archive.iter().all(|(id, _)| id != &t.id));
    manual(&ctx, &stub).await.unwrap();
    assert!(get_task(&ctx, &t.id).await.archived);
    assert!(get_task(&ctx, &second.id).await.archived);
}

#[tokio::test]
async fn edits_during_the_model_call_are_skipped_and_logged_not_overwritten() {
    let ctx = ctx();
    let edited = task(&ctx, "会被改", "已完成").await;
    let kept = task(&ctx, "照常归档", "已完成").await;
    write(&ctx, |c| crate::data::prompts::replace_memory(c, "temporary", 0, "- 合成线索").map(drop)).await;
    let id = edited.id.clone();
    let stub = Stub::new(&ctx)
        .during(move |c| {
            let t = crate::data::tasks::get(c, &id)?.unwrap();
            crate::data::tasks::edit(c, &json!({"task": {"id": id, "updated": t.updated, "title": "人工改名"}}))?;
            crate::data::prompts::replace_memory(c, "archived", 0, "人工记忆")?;
            Ok(())
        })
        .reply(Ok(json!({
            "report": {"summary": "s"},
            "_archived_memory_update": {"expected": 0, "before": "", "text": "AI 记忆"},
        })));
    let r = manual(&ctx, &stub).await.unwrap();
    let after = get_task(&ctx, &edited.id).await;
    assert!(!after.archived && after.title == "人工改名", "the manual edit wins");
    assert!(get_task(&ctx, &kept.id).await.archived);
    assert_eq!(read(&ctx, |c| crate::data::prompts::memory(c, "archived")).await.text, "人工记忆");
    // The merge did not land, so the temporary memory waits for the next archive.
    assert_eq!(read(&ctx, |c| crate::data::prompts::memory(c, "temporary")).await.text, "- 合成线索");
    assert_eq!((r["skipped_tasks"].as_u64(), r["memory_changes"].as_u64()), (Some(1), Some(0)));
    assert_eq!(r["temporary_memory_cleared"], false);
    let conflicts = logs(&ctx, "archive_conflict").await;
    let kinds: Vec<&str> = conflicts.iter().map(|c| c["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["memory", "tasks"]);
    assert_eq!(conflicts[1]["detail"], json!([edited.id]));
    assert_eq!(last_attempt(&ctx).await.status, "succeeded");
    // The skipped task stays in scope for the next archive.
    let scope = read(&ctx, store::task_scope).await;
    assert!(scope.iter().any(|t| t.id == edited.id));
}

#[tokio::test]
async fn invalid_memory_results_reject_the_batch_and_keep_earlier_batches() {
    let cases = [
        (json!({"_archived_memory_error": "检索中断"}), "归档记忆未保存：检索中断"),
        (json!({"archived_memory": [{"id": "x"}]}), "归档记忆缺少完整读取校验，本批未提交"),
        (
            json!({"_archived_memory_update": {"expected": 9, "before": "", "text": "x"}}),
            "归档记忆已被人工编辑，本批未提交，请核对后重新归档",
        ),
        (json!({"_archived_memory_update": {"expected": 0, "before": ""}}), "归档记忆正文无效"),
        (
            json!({"_archived_memory_update": {"expected": 0, "before": "", "text": "长".repeat(4001)}}),
            "归档记忆超过4000字符，保留原记忆",
        ),
    ];
    for (bad, message) in cases {
        let ctx = ctx();
        let first = task(&ctx, "第一批", "已完成").await;
        let second = task(&ctx, "第二批", "已完成").await;
        let mut bad = bad;
        bad["report"] = json!({"summary": "x"});
        let stub = Stub { max_evidence: Some(1), ..Stub::new(&ctx) }.reply(Ok(summary("一"))).reply(Ok(bad));
        assert_eq!(manual(&ctx, &stub).await.unwrap_err().0, message);
        assert!(get_task(&ctx, &first.id).await.archived, "{message}: batch 1 stays committed");
        assert!(!get_task(&ctx, &second.id).await.archived, "{message}");
        assert_eq!(read(&ctx, |c| crate::data::prompts::memory(c, "archived")).await.revision, 0);
        let a = last_attempt(&ctx).await;
        assert_eq!((a.status.as_str(), a.error.as_str(), a.batches), ("failed", message, 1));
        assert!(a.finished > 0);
        assert_eq!(logs(&ctx, "archive_failed").await[0]["error"], message);
    }
}

#[tokio::test]
async fn cancellation_and_model_errors_fail_the_attempt_without_a_commit() {
    let ctx = ctx();
    let t = task(&ctx, "未归档", "已完成").await;
    let stub = Stub::new(&ctx).reply(Err(Error::new(exec::CANCELLED)));
    assert_eq!(manual(&ctx, &stub).await.unwrap_err().0, exec::CANCELLED);
    assert!(!get_task(&ctx, &t.id).await.archived);
    assert_eq!(cursor(&ctx, ARCHIVE_CURSOR).await, 0);
    let a = last_attempt(&ctx).await;
    assert_eq!((a.status.as_str(), a.error.as_str()), ("failed", exec::CANCELLED));
    // A new manual archive starts a fresh attempt from the same cursor.
    let stub = Stub::new(&ctx);
    manual(&ctx, &stub).await.unwrap();
    assert!(get_task(&ctx, &t.id).await.archived);
    let attempts = read(&ctx, store::attempts).await;
    assert_eq!(attempts.iter().map(|a| a.status.as_str()).collect::<Vec<_>>(), ["failed", "succeeded"]);
}

#[tokio::test]
async fn oversized_single_items_never_reach_the_model() {
    let ctx = ctx();
    task(&ctx, "太大", "已完成").await;
    let stub = Stub { max_evidence: Some(0), ..Stub::new(&ctx) };
    let e = manual(&ctx, &stub).await.unwrap_err().0;
    assert_eq!(e, "归档的单条材料及必需上下文已超过单次输入上限，未调用 AI；请精简配置或提高输入上限后手动归档");
    assert_eq!(stub.calls(), 0);
}

#[tokio::test]
async fn busy_unconfigured_and_pending_ai_entries() {
    let ctx = ctx();
    // The IPC entries go through the AI call layer.
    let fake = crate::ai::provider::fake::Fake::reply(summary("今日"));
    ctx.ai.set_driver(Some(fake.clone()));
    let report = dispatch(&ctx, "report", json!({})).await.unwrap();
    assert_eq!(fake.calls(), 1, "{report}");
    dispatch(&ctx, "archive", json!({})).await.unwrap();

    ctx.archive.archive_busy.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(dispatch(&ctx, "archive", json!({})).await.unwrap_err().0, "AI 正在处理，请稍后手动归档");
    let stub = Stub::new(&ctx);
    assert_eq!(manual(&ctx, &stub).await.unwrap(), json!({"skipped": "AI 正在处理，请稍后手动归档"}));
    assert_eq!(archive_blocks(&ctx).await.unwrap_err().0, ARCHIVE_BUSY);
    ctx.archive.archive_busy.store(false, std::sync::atomic::Ordering::SeqCst);

    let mut config = Config::default();
    config.model_profiles.as_mut().unwrap().review = None;
    let ctx = Ctx::memory(config).unwrap();
    let e = dispatch(&ctx, "archive", json!({})).await.unwrap_err().0;
    assert_eq!(e, "AI_UNCONFIGURED:归档分析未配置 AI 接入或模型，本次任务未执行");
    assert_eq!(manual(&ctx, &Stub::new(&ctx)).await.unwrap_err().0, e);
    assert_eq!(dispatch(&ctx, "report", json!({})).await.unwrap_err().0, e);
    assert!(read(&ctx, store::attempts).await.is_empty());
    assert!(logs(&ctx, "archive_failed").await.is_empty());
}

/// Shanghai local `YYYY-MM-DD HH:MM:SS` as Unix ms.
fn shanghai(s: &str) -> i64 {
    let t = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
    t.and_utc().timestamp_millis() - crate::ai::work::SHANGHAI_OFFSET_MS
}

#[tokio::test]
async fn queued_work_end_archives_need_their_claim_and_never_retry() {
    let ctx = ctx();
    write(&ctx, |c| store::queue_end_archive(c, "claim-1", "2026-11-03", 5).map(drop)).await;
    assert!(!write(&ctx, |c| store::queue_end_archive(c, "claim-1", "2026-11-03", 6)).await, "a claim queues once");
    assert_eq!(archive_blocks(&ctx).await.unwrap_err().0, ARCHIVE_BUSY, "a queued archive blocks other AI work");
    let stub = Stub::new(&ctx);
    let no_claim = "归档排队缺少匹配的持久化下班 claim；已停止自动执行，等待手动归档";
    let e = run::archive_tick(&ctx, &stub).await.unwrap_err().0;
    assert_eq!(e, no_claim);
    let a = last_attempt(&ctx).await;
    assert_eq!((a.id.as_str(), a.status.as_str(), a.error.as_str()), ("claim-1", "failed", no_claim));
    let again = run::archive_tick(&ctx, &stub).await.unwrap();
    assert_eq!(again["skipped"], "归档失败或等待下次到点；失败仅手动重试");
    assert_eq!(stub.calls(), 0);

    // A real work-end claim queues the next attempt.
    let config = ctx.config.get();
    write(&ctx, move |c| {
        let year = crate::ai::calendar::Year { year: 2026, fetched_at: 1, days: Default::default() };
        crate::data::ai::save_holiday_year(c, &year)?;
        for (before, now) in
            [("2026-11-04 07:59:30", "2026-11-04 08:00:30"), ("2026-11-04 19:59:30", "2026-11-04 20:00:30")]
        {
            crate::data::ai::boundary_tick(c, &config, Some(shanghai(before)), shanghai(now))?;
        }
        Ok(())
    })
    .await;
    let r = run::archive_tick(&ctx, &stub).await.unwrap();
    assert_eq!(r["label"], "下班归档");
    assert_eq!(r["date"], "2026-11-04");
    let a = last_attempt(&ctx).await;
    assert_eq!((a.id.as_str(), a.status.as_str(), a.trigger.as_str()), ("end-2026-11-04", "succeeded", "work_end"));
}

#[tokio::test]
async fn restart_fails_unfinished_work_and_keeps_committed_success() {
    let ctx = ctx();
    let stub = Stub::new(&ctx);
    manual(&ctx, &stub).await.unwrap();
    let done = last_attempt(&ctx).await;
    write(&ctx, |c| {
        store::put_attempt(
            c,
            &Attempt {
                id: "run".into(),
                trigger: "manual_archive".into(),
                status: "running".into(),
                created: 1,
                ..Default::default()
            },
        )?;
        store::queue_end_archive(c, "queued", "2026-11-04", 2)?;
        store::queue_morning(c, "m1", "2026-11-04", 3)
    })
    .await;
    write(&ctx, store::recover).await;
    let attempts = read(&ctx, store::attempts).await;
    assert_eq!(attempts[0], done);
    for a in &attempts[1..] {
        assert_eq!((a.status.as_str(), a.error.as_str()), ("failed", RECOVERY_ERROR));
        assert!(a.finished > done.finished);
    }
    let jobs = read(&ctx, store::jobs).await;
    assert_eq!((jobs[0].status.as_str(), jobs[0].error.as_str()), ("failed", MORNING_RECOVERY_ERROR));
}

// ---- reports --------------------------------------------------------------

async fn report_now(ctx: &Ctx, stub: &Stub) -> Result<Value> {
    reports::report(ctx, stub, "手动报表", Mode::Manual).await
}

#[tokio::test]
async fn reports_summarize_reuse_unchanged_material_and_keep_failures() {
    let ctx = ctx();
    let t = task(&ctx, "整理周报", "待处理").await;
    let stub = Stub::new(&ctx).reply(Ok(json!({"summary": "周报待整理"})));
    let first = report_now(&ctx, &stub).await.unwrap();
    assert_eq!((first["summary_status"].as_str(), first["label"].as_str()), (Some("done"), Some("手动报表")));
    assert_eq!(first["ai_summary"]["summary"], "周报待整理");
    assert!(first["finished"].as_i64().unwrap() > 0);
    let sent = stub.seen.lock().unwrap()[0].clone();
    assert_eq!(sent.purpose, "report");
    assert_eq!(sent.payload["_trigger"], "manual_report");
    assert_eq!(sent.payload["report_tasks"][0]["title"], "整理周报");
    assert_eq!(sent.payload["delta"].as_array().unwrap().len(), 1);
    assert_eq!(cursor(&ctx, REPORT_CURSOR).await, read(&ctx, store::max_seq).await);

    // Nothing new: the previous summary is reused without a model call.
    let again = report_now(&ctx, &stub).await.unwrap();
    assert_eq!(again["summary_status"], "reused");
    assert_eq!(again["ai_summary"], first["ai_summary"]);
    assert_eq!(again["previous_report"]["id"], first["id"]);
    assert!(again["finished"].as_i64() > first["finished"].as_i64());
    assert_eq!(stub.calls(), 1);

    // A model error stays in the report, which is still returned.
    write(&ctx, move |c| {
        crate::data::tasks::edit(c, &json!({"task": {"id": t.id, "updated": t.updated, "status": "进行中"}})).map(drop)
    })
    .await;
    let stub = Stub::new(&ctx).reply(Err(Error::new("模型超时")));
    let failed = report_now(&ctx, &stub).await.unwrap();
    assert_eq!(
        (failed["summary_status"].as_str(), failed["summary_error"].as_str()),
        (Some("failed"), Some("模型超时"))
    );
    let o = read(&ctx, store::outcome).await.unwrap();
    assert_eq!((o.kind, o.status.as_str(), o.error.as_str()), ("report", "failed", "模型超时"));

    // An interruption is an error of `report` and an interrupted report.
    write(&ctx, move |c| task_edit(c, "跟进合同")).await;
    let stub = Stub::new(&ctx).reply(Err(Error::new(exec::CANCELLED)));
    assert_eq!(report_now(&ctx, &stub).await.unwrap_err().0, exec::CANCELLED);
    let stored = read(&ctx, |c| store::first_report(c)).await.unwrap();
    assert_eq!(stored["summary_status"], "interrupted");
    assert_eq!(read(&ctx, store::reports).await.len(), 4);
}

fn task_edit(c: &Connection, title: &str) -> Result<()> {
    crate::data::tasks::edit(c, &json!({"task": {"title": title, "status": "待处理"}})).map(drop)
}

#[tokio::test]
async fn report_details_are_paged_frozen_and_revision_checked() {
    let ctx = ctx();
    for title in ["甲", "乙", "丙"] {
        task(&ctx, title, "待处理").await;
    }
    let stub = Stub::new(&ctx).reply(Ok(json!({"summary": "三件事"})));
    let r = report_now(&ctx, &stub).await.unwrap();
    let id = r["id"].as_str().unwrap().to_string();
    let revision = read(&ctx, store::reports_revision).await;
    let detail = |p: Value| {
        let ctx = ctx.clone();
        async move { dispatch(&ctx, "report_detail", p).await }
    };
    let page = detail(json!({"id": id, "revision": revision, "section": "tasks", "limit": 2})).await.unwrap();
    assert_eq!(
        (page["total"].as_u64(), page["offset"].as_u64(), page["next_offset"].as_u64()),
        (Some(3), Some(0), Some(2))
    );
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    assert!(page["rows"][0].get("description").is_none(), "rows carry display fields only");
    let rest =
        detail(json!({"id": id, "revision": revision, "section": "tasks", "offset": 2, "limit": 0})).await.unwrap();
    assert_eq!((rest["rows"].as_array().unwrap().len(), rest["next_offset"].as_u64()), (1, Some(3)));
    let summary = detail(json!({"id": id, "revision": revision})).await.unwrap();
    assert_eq!(
        (summary["report"]["task_count"].as_u64(), summary["report"]["delta_count"].as_u64()),
        (Some(3), Some(3))
    );
    assert_eq!(summary["report"]["ai_summary"]["summary"], "三件事");

    for (p, e) in [
        (json!({"id": id, "revision": "old"}), "报表已更新，请刷新后展开"),
        (json!({"revision": revision}), "缺少报表编号"),
        (json!({"id": "gone", "revision": revision}), "报表已不在当前历史中"),
        (json!({"id": id, "revision": revision, "section": "memory"}), "未知报表明细"),
    ] {
        assert_eq!(detail(p).await.unwrap_err().0, e);
    }

    // A frozen row whose stored item disappeared reads as unavailable.
    write(&ctx, |c| {
        c.execute(
            "DELETE FROM report_items WHERE key IN (SELECT key FROM report_items WHERE key LIKE 'delta:%' LIMIT 1)",
            [],
        )?;
        Ok(())
    })
    .await;
    let delta = detail(json!({"id": id, "revision": revision, "section": "delta"})).await.unwrap();
    let unavailable = delta["rows"].as_array().unwrap().iter().filter(|r| r["unavailable"] == true).count();
    assert_eq!(unavailable, 1);

    // The catalogue: the newest in summary form, older ones as rows.
    report_now(&ctx, &stub).await.unwrap();
    let catalog = read(&ctx, store::catalog).await;
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0]["summary_status"], "reused");
    assert_eq!(catalog[1].as_object().unwrap().keys().collect::<Vec<_>>(), ["date", "id", "label", "time"]);
    assert_ne!(read(&ctx, store::reports_revision).await, revision);
}

/// Store `t` as `change` leaves it, audited like an analysis round with
/// its change summary.
async fn ai_update(
    ctx: &Ctx,
    t: &tasks::Task,
    summary: &'static str,
    change: impl FnOnce(&mut tasks::Task) + Send + 'static,
) -> tasks::Task {
    let mut t = t.clone();
    write(ctx, move |c| {
        let before = t.payload();
        change(&mut t);
        t.updated += 1;
        let stored = crate::data::tasks::store(c, &t)?;
        let seq = crate::data::audit::write(c, "ai_update", &stored.id, "AI更新事项", &before, &stored.payload())?;
        c.execute("UPDATE task_audit SET change_summary = ?2 WHERE seq = ?1", rusqlite::params![seq, summary])?;
        Ok(stored)
    })
    .await
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[tokio::test]
async fn reports_read_each_task_once_and_of_each_change_what_changed_without_message_ids() {
    let ctx = ctx();
    let t = task(&ctx, "整理周报", "待处理").await;
    let t = ai_update(&ctx, &t, "", |t| t.evidence.direct = ids(&["m1", "m2"])).await;
    ai_update(&ctx, &t, "开始整理周报", |t| {
        t.status = "处理中".into();
        t.evidence.direct.push("m3".into());
        t.evidence.background = ids(&["b1"]);
    })
    .await;
    let stub = Stub::new(&ctx).reply(Ok(json!({"summary": "周报整理中"})));
    report_now(&ctx, &stub).await.unwrap();
    let sent = stub.seen.lock().unwrap()[0].payload.clone();
    let delta = sent["delta"].as_array().unwrap();
    assert_eq!(delta.len(), 3);
    assert!(delta.iter().all(|e| e.get("before").is_none() && e.get("after").is_none()), "no snapshots");
    assert!(delta[0]["changes"].is_null(), "the task itself shows what a creation made");
    assert_eq!(delta[1]["changes"], json!({}));
    assert_eq!(delta[2]["changes"], json!({"status": {"before": "待处理", "after": "处理中"}}));
    assert_eq!(delta[2]["change_summary"], "开始整理周报");
    assert!(delta[2]["time"].is_string());
    let brief = &sent["report_tasks"][0];
    assert!(brief.get("evidence").is_none());
    assert_eq!((brief["title"].as_str(), brief["status"].as_str()), (Some("整理周报"), Some("处理中")));
}

#[test]
fn task_briefs_keep_a_real_summary_and_drop_a_cut_description() {
    let t =
        json!({"id": "t", "description": "很长的内容", "summary": "很长的…", "summary_fallback": true, "manual": true});
    assert_eq!(task_brief(&t), json!({"id": "t", "description": "很长的内容"}));
    let t = json!({"id": "t", "summary": "AI摘要", "summary_fallback": false});
    assert_eq!(task_brief(&t), json!({"id": "t", "summary": "AI摘要"}));
}

#[tokio::test]
async fn reports_start_after_the_last_archive_and_keep_an_unsummarized_delta() {
    let ctx = ctx();
    task(&ctx, "核对合同", "待处理").await;
    manual(&ctx, &Stub::new(&ctx)).await.unwrap();
    write(&ctx, |c| task_edit(c, "跟进发票")).await;
    let only_new = |stub: &Stub| {
        let sent = stub.seen.lock().unwrap()[0].payload.clone();
        let delta = sent["delta"].as_array().unwrap().clone();
        assert_eq!(delta.len(), 1, "changes before the archive stay out");
        assert_eq!(sent["report_tasks"].as_array().unwrap().len(), 2, "open tasks are all read");
        delta[0]["task_id"].clone()
    };

    let stub = Stub::new(&ctx).reply(Err(Error::new("模型超时")));
    assert_eq!(report_now(&ctx, &stub).await.unwrap()["summary_status"], "failed");
    let failed = only_new(&stub);
    assert_eq!(cursor(&ctx, REPORT_CURSOR).await, 0, "a failed summary keeps its delta");

    let stub = Stub::new(&ctx).reply(Ok(json!({"summary": "跟进发票"})));
    assert_eq!(report_now(&ctx, &stub).await.unwrap()["summary_status"], "done");
    assert_eq!(only_new(&stub), failed);
    assert_eq!(cursor(&ctx, REPORT_CURSOR).await, read(&ctx, store::max_seq).await);
    assert_eq!(report_now(&ctx, &stub).await.unwrap()["summary_status"], "reused");
}

#[tokio::test]
async fn reports_over_the_limit_drop_whole_old_days_but_keep_the_last_seven() {
    let ctx = ctx();
    let now = time::now();
    for (i, days) in [10, 10, 8, 3, 0].into_iter().enumerate() {
        task(&ctx, &format!("事项{i}"), "待处理").await;
        let at = now - days * time::DAY;
        write(&ctx, move |c| {
            c.execute("UPDATE task_audit SET time = ?1 WHERE seq = (SELECT max(seq) FROM task_audit)", [at])?;
            Ok(())
        })
        .await;
    }
    let sent = |stub: &Stub| stub.seen.lock().unwrap()[0].payload["delta"].as_array().unwrap().len();

    let stub = Stub { max_delta: Some(3), ..Stub::new(&ctx) }.reply(Err(Error::new("模型超时")));
    let r = report_now(&ctx, &stub).await.unwrap();
    assert_eq!(sent(&stub), 3, "the oldest day goes whole");
    assert_eq!(r["delta_omitted"], 2);

    let stub = Stub { max_delta: Some(1), ..Stub::new(&ctx) };
    let r = report_now(&ctx, &stub).await.unwrap();
    assert_eq!(sent(&stub), 2, "the last seven days are sent even when too large");
    assert_eq!(r["delta_omitted"], 3);
}

#[tokio::test]
async fn archive_events_carry_changed_fields_and_their_summaries() {
    let ctx = ctx();
    let t = task(&ctx, "整理周报", "待处理").await;
    ai_update(&ctx, &t, "开始整理周报", |t| {
        t.status = "处理中".into();
        t.evidence.direct = ids(&["m1"]);
    })
    .await;
    let stub = Stub::new(&ctx);
    manual(&ctx, &stub).await.unwrap();
    let sent = stub.seen.lock().unwrap()[0].payload.clone();
    let event = &sent["evidence"][1]["event"];
    assert_eq!(event["change_summary"], "开始整理周报");
    assert_eq!(
        event["changes"],
        json!({"status": {"before": "待处理", "after": "处理中"}, "evidence": {"direct": {"added": ["m1"]}}})
    );
    assert_eq!(sent["report_tasks"][0]["evidence"]["direct"], json!(["m1"]));
}

#[tokio::test]
async fn batch_summaries_skip_the_first_row_like_the_reference() {
    let ctx = ctx();
    let r = json!({"id": "r1", "time": 1, "batch_summaries": [{"summary": "a"}, {"summary": "b"}, {"summary": "c"}]});
    write(&ctx, move |c| store::put_report(c, &r).map(drop)).await;
    let revision = read(&ctx, store::reports_revision).await;
    let page = dispatch(&ctx, "report_detail", json!({"id": "r1", "revision": revision, "section": "batch_summaries"}))
        .await
        .unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["rows"][0]["summary"], "a");
}

#[tokio::test]
async fn outcome_prefers_later_then_successful_and_truncation_keeps_it() {
    let ctx = ctx();
    write(&ctx, |c| {
        store::put_attempt(
            c,
            &Attempt {
                id: "a".into(),
                trigger: "manual_archive".into(),
                status: "failed".into(),
                created: 5,
                finished: 10,
                ..Default::default()
            },
        )?;
        store::put_report(
            c,
            &json!({"id": "ok", "time": 3, "finished": 10, "label": "手动报表", "summary_status": "done"}),
        )?;
        // Archive batch reports, partial reports and pending ones are no outcome.
        store::put_report(
            c,
            &json!({"id": "batch", "time": 50, "finished": 50, "attempt_id": "a", "summary_status": "done"}),
        )?;
        store::put_report(
            c,
            &json!({"id": "partial", "time": 50, "finished": 50, "remaining_events": 2, "summary_status": "done"}),
        )?;
        store::put_report(c, &json!({"id": "pending", "time": 50, "summary_status": "pending"}))?;
        Ok(())
    })
    .await;
    let o = read(&ctx, store::outcome).await.unwrap();
    assert_eq!((o.kind, o.id.as_str(), o.status.as_str()), ("report", "ok", "succeeded"), "a tie goes to the success");
    assert_eq!(read(&ctx, |c| store::next_finished(c, 4)).await, 11);
    assert_eq!(read(&ctx, |c| store::next_finished(c, 40)).await, 40);

    // A finished job without a time sorts at the start of its day.
    write(&ctx, |c| {
        store::put_job(c, &MorningJobRow::failed("m", "2026-11-04"))?;
        Ok(())
    })
    .await;
    let o = read(&ctx, store::outcome).await.unwrap();
    assert_eq!((o.kind, o.label.as_str(), o.finished), ("morning", "早报", time::day_start("2026-11-04").unwrap()));

    // Sixty pending reports do not push the latest report outcome out.
    write(&ctx, |c| {
        c.execute("DELETE FROM morning_jobs", [])?;
        c.execute("DELETE FROM reports WHERE id IN ('batch', 'partial')", [])?;
        for i in 0..60 {
            store::put_report(c, &json!({"id": format!("p{i}"), "time": 100 + i, "summary_status": "pending"}))?;
        }
        store::truncate(c)
    })
    .await;
    let ids: Vec<String> =
        read(&ctx, store::reports).await.iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), store::REPORT_LIMIT);
    assert_eq!(ids.last().unwrap(), "ok");
    assert!(!ids.contains(&"pending".to_string()) && !ids.contains(&"p0".to_string()));
}

struct MorningJobRow;

impl MorningJobRow {
    fn failed(id: &str, day: &str) -> MorningJob {
        MorningJob {
            id: id.into(),
            business_day: day.into(),
            status: "failed".into(),
            error: "x".into(),
            created: 1,
            finished: 0,
        }
    }
}

#[tokio::test]
async fn morning_jobs_run_once_and_record_their_result() {
    let ctx = ctx();
    task(&ctx, "晨会准备", "待处理").await;
    write(&ctx, |c| store::queue_morning(c, "m1", "2026-11-05", 1)).await;
    write(&ctx, |c| store::queue_morning(c, "m1", "2026-11-05", 2)).await;
    assert_eq!(read(&ctx, store::jobs).await.len(), 1, "a claim queues once");
    let stub = Stub::new(&ctx).reply(Ok(json!({"summary": "早"})));
    let job = reports::morning_tick(&ctx, &stub).await.unwrap().unwrap();
    assert_eq!((job.status.as_str(), job.error.as_str()), ("succeeded", ""));
    let r = read(&ctx, |c| store::first_report(c)).await.unwrap();
    assert_eq!((r["label"].as_str(), r["finished"].as_i64()), (Some("早报"), Some(job.finished)));
    assert_eq!(stub.seen.lock().unwrap()[0].payload["_trigger"], "scheduled_report");
    assert!(reports::morning_tick(&ctx, &stub).await.unwrap().is_none(), "nothing queued");

    write(&ctx, |c| task_edit(c, "新事项")).await;
    write(&ctx, |c| store::queue_morning(c, "m2", "2026-11-06", 3)).await;
    let stub = Stub::new(&ctx).reply(Err(Error::new("模型不可用")));
    let job = reports::morning_tick(&ctx, &stub).await.unwrap().unwrap();
    assert_eq!((job.status.as_str(), job.error.as_str()), ("failed", "模型不可用"));
    assert!(reports::morning_tick(&ctx, &stub).await.unwrap().is_none(), "failures are not retried");
    let o = read(&ctx, store::outcome).await.unwrap();
    assert_eq!((o.kind, o.id.as_str()), ("morning", "m2"));

    // A queued archive blocks the summary; the job fails with its reason.
    write(&ctx, |c| task_edit(c, "再一件")).await;
    write(&ctx, |c| {
        store::queue_end_archive(c, "end", "2026-11-06", 4)?;
        store::queue_morning(c, "m3", "2026-11-07", 4)
    })
    .await;
    let job = reports::morning_tick(&ctx, &stub).await.unwrap().unwrap();
    assert_eq!((job.status.as_str(), job.error.as_str()), ("failed", ARCHIVE_BUSY));
}

#[tokio::test]
async fn a_report_waits_for_a_running_analysis_instead_of_failing() {
    let ctx = ctx();
    task(&ctx, "晨会准备", "待处理").await;
    let analysis = ctx.exec.try_call("classify").unwrap();
    let stub = Arc::new(Stub::new(&ctx).reply(Ok(json!({"summary": "早"}))));
    let report = {
        let (ctx, stub) = (ctx.clone(), stub.clone());
        tokio::spawn(async move { reports::report_result(&ctx, stub.as_ref(), "早报", Mode::Automatic).await })
    };
    while !running(&ctx) {
        tokio::task::yield_now().await;
    }
    assert_eq!(stub.calls(), 0, "the summary waits for the slot");
    drop(analysis);
    let r = report.await.unwrap().unwrap();
    assert_eq!(r["summary_status"], "done");
}

// ---- archive versions -----------------------------------------------------

fn add_rule(c: &Connection, id: &str, title: &str) -> Result<()> {
    c.execute(
        "INSERT INTO suggestions(id, title, rule, evidence, active, decision, created, updated) VALUES (?1, ?2, '先看阻塞', '[]', 1, 'enabled', 1, 1)",
        [id, title],
    )?;
    kv::bump(c, crate::data::rules::RULE_VERSION).map(drop)
}

fn add_tags(c: &Connection, groups: &str) -> Result<()> {
    c.execute(
        "INSERT INTO tag_versions(id, created, action, reason, priority_kind, groups, assignments)
         VALUES (?1, 1, 'manual', '手动', '', ?2, '[]')",
        [new_id(), groups.to_string()],
    )?;
    Ok(())
}

async fn archived_group(ctx: &Ctx, title: &str) -> String {
    task(ctx, title, "已完成").await;
    manual(ctx, &Stub::new(ctx)).await.unwrap();
    read(ctx, |c| Ok(store::latest_group(c)?.unwrap().id)).await
}

#[tokio::test]
async fn rollback_restores_prompt_rules_and_tags_as_a_new_group_only() {
    let ctx = ctx();
    write(&ctx, |c| add_rule(c, "r1", "旧规则")).await;
    let g1 = archived_group(&ctx, "甲").await;
    let p1 = read(&ctx, |c| Ok(crate::data::prompts::current(c, "classify")?.unwrap())).await;
    write(&ctx, |c| {
        c.execute("DELETE FROM suggestions WHERE id = 'r1'", [])?;
        add_rule(c, "r2", "新规则")?;
        crate::data::prompts::record(c, "classify", "新的分类提示词", "manual", "手动保存系统提示词")?;
        add_tags(c, r#"[{"name":"新分组","values":[]}]"#)?;
        crate::data::prompts::replace_memory(
            c,
            "archived",
            crate::data::prompts::memory(c, "archived")?.revision,
            "长期记忆",
        )?;
        Ok(())
    })
    .await;
    let g2 = archived_group(&ctx, "乙").await;
    let before_cursor = cursor(&ctx, ARCHIVE_CURSOR).await;
    let memory_before = read(&ctx, |c| crate::data::prompts::memory(c, "archived")).await;
    let memory_before = (memory_before.text, memory_before.revision);
    let rule_version = cursor(&ctx, crate::data::rules::RULE_VERSION).await;

    let history = dispatch(&ctx, "archive_history", json!({})).await.unwrap();
    assert_eq!(history["current"], g2.as_str());
    assert_eq!(history["memory"], "长期记忆");
    assert_eq!(history["versions"].as_array().unwrap().len(), 2);
    assert_eq!(history["versions"][1]["active_rules"], 1);
    assert_eq!(history["attempts"].as_array().unwrap().len(), 2);
    let v1 = dispatch(&ctx, "archive_version", json!({"id": g1})).await.unwrap();
    assert_eq!((v1["prompt"].as_str(), v1["current"].as_str()), (Some(p1.content.as_str()), Some(g2.as_str())));
    assert_eq!(v1["rules"][0]["id"], "r1");
    assert_eq!(dispatch(&ctx, "archive_version", json!({"id": "x"})).await.unwrap_err().0, "归档版本不存在");

    let rollback = |id: &str, expected: &str| {
        let (ctx, p) = (ctx.clone(), json!({"id": id, "expected": expected}));
        async move { dispatch(&ctx, "archive_rollback", p).await }
    };
    assert_eq!(rollback(&g1, &g1).await.unwrap_err().0, "归档版本已变化，请刷新后重试");
    assert_eq!(rollback("x", &g2).await.unwrap_err().0, "归档版本不存在");
    let r = rollback(&g1, &g2).await.unwrap();

    let prompt = read(&ctx, |c| Ok(crate::data::prompts::current(c, "classify")?.unwrap())).await;
    assert_eq!((prompt.content.as_str(), prompt.action.as_str()), (p1.content.as_str(), "archive_rollback"));
    assert_eq!(prompt.reason, format!("归档整组回滚至 {g1}"));
    assert_eq!(r["prompt_version"], prompt.id.as_str());
    let rules = read(&ctx, crate::data::rules::list).await;
    let rule = |id: &str| rules.iter().find(|r| r["id"] == id).cloned().unwrap();
    assert_eq!(
        (rule("r1")["active"].clone(), rule("r1")["decision"].clone()),
        (json!(true), json!("enabled")),
        "deleted target rule returns"
    );
    assert_eq!(
        (rule("r2")["active"].clone(), rule("r2")["decision"].clone()),
        (json!(false), json!("pending")),
        "later rule is disabled, not deleted"
    );
    let tags: (String, String, String) = read(&ctx, |c| {
        Ok(c.query_row("SELECT action, reason, groups FROM tag_versions ORDER BY seq DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?)
    })
    .await;
    assert_eq!((tags.0.as_str(), tags.1.clone()), ("archive_rollback", format!("随归档版本回滚至 {g1}")));
    let g1_tags = read(&ctx, {
        let g1 = g1.clone();
        move |c| Ok(store::group(c, &g1)?.unwrap().tag_groups)
    })
    .await;
    assert_eq!(serde_json::from_str::<Value>(&tags.2).unwrap(), g1_tags);

    let latest = read(&ctx, |c| Ok(store::latest_group(c)?.unwrap())).await;
    assert_eq!(
        (latest.id.as_str(), latest.action.as_str(), latest.restored_from.as_str()),
        (r["group"].as_str().unwrap(), "rollback", g1.as_str())
    );
    assert_eq!(latest.reason, format!("整组回滚至 {g1}，以新版本记录，不倒退消息/事项进度"));
    assert_eq!(latest.prompt_version, prompt.id);
    assert_eq!(cursor(&ctx, crate::data::rules::RULE_VERSION).await, rule_version + 1);
    // Memory, cursors and archived tasks stay where they are.
    let m = read(&ctx, |c| crate::data::prompts::memory(c, "archived")).await;
    assert_eq!((m.text, m.revision), memory_before);
    assert_eq!(cursor(&ctx, ARCHIVE_CURSOR).await, before_cursor);
    let archived: i64 =
        read(&ctx, |c| Ok(c.query_row("SELECT count(*) FROM tasks WHERE archived = 1", [], |r| r.get(0))?)).await;
    assert_eq!(archived, 2);
    let audit: String = read(&ctx, |c| {
        Ok(c.query_row("SELECT detail FROM task_audit WHERE kind = 'archive_rollback'", [], |r| r.get(0))?)
    })
    .await;
    assert_eq!(audit, "归档整组回滚：仅提示词、规则、标签和私有归档记忆；保留后续建议记录");
}

#[tokio::test]
async fn rollback_refuses_manual_changes_after_the_latest_group() {
    const CONFLICT: &str =
        "最新归档之后提示词、规则或标签已有手动修改，整组回滚会覆盖这些修改，已取消；请先确认当前版本或生成新版本";
    let changes: [fn(&Connection) -> Result<()>; 4] = [
        |c| crate::data::prompts::record(c, "classify", "人工改的提示词", "manual", "手动保存系统提示词").map(drop),
        |c| kv::bump(c, crate::data::rules::RULE_VERSION).map(drop),
        |c| {
            // Same rule version but another enabled rule.
            c.execute("INSERT INTO suggestions(id, title, rule, active, decision, created, updated) VALUES ('x', 't', 'r', 1, 'enabled', 1, 1)", [])?;
            Ok(())
        },
        |c| add_tags(c, "[]"),
    ];
    for change in changes {
        let ctx = ctx();
        let g1 = archived_group(&ctx, "甲").await;
        let g2 = archived_group(&ctx, "乙").await;
        write(&ctx, move |c| change(c)).await;
        let groups = read(&ctx, store::groups).await.len();
        let e = dispatch(&ctx, "archive_rollback", json!({"id": g1, "expected": g2})).await.unwrap_err().0;
        assert_eq!(e, CONFLICT);
        assert_eq!(read(&ctx, store::groups).await.len(), groups, "nothing is written");
    }
}

// ---- retention ------------------------------------------------------------

#[tokio::test]
async fn retention_removes_processed_audit_and_old_archived_tasks_only() {
    let ctx = ctx();
    task(&ctx, "甲", "已完成").await;
    task(&ctx, "乙", "已完成").await;
    manual(&ctx, &Stub::new(&ctx)).await.unwrap();
    let cursor_at = cursor(&ctx, ARCHIVE_CURSOR).await;
    task(&ctx, "丙", "进行中").await; // audit after the cursor
    let at = crate::time::now();
    let (seqs, cited) = write(&ctx, move |c| {
        c.execute("UPDATE task_audit SET time = 1000", [])?;
        c.execute("UPDATE tasks SET archived_at = 1000 WHERE archived = 1", [])?;
        let seqs: Vec<i64> = c
            .prepare("SELECT seq FROM task_audit ORDER BY seq")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let cited = seqs[0];
        c.execute(
            "INSERT INTO suggestions(id, title, rule, evidence, active, decision, created, updated) VALUES ('r', 't', 'r', ?1, 1, 'enabled', 1, 1)",
            [json!([cited]).to_string()],
        )?;
        Ok((seqs, cited))
    })
    .await;
    assert!(write(&ctx, move |c| store::prune(c, 30, at)).await);

    let left: Vec<i64> = read(&ctx, |c| {
        Ok(c.prepare("SELECT seq FROM task_audit ORDER BY seq")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    })
    .await;
    let newest = *seqs.last().unwrap();
    let expected: Vec<i64> = seqs.iter().copied().filter(|s| *s == cited || *s > cursor_at || *s == newest).collect();
    assert_eq!(left, expected, "cited, unprocessed and newest rows stay");
    let removed = seqs.len() - expected.len();
    assert!(removed > 0);
    let titles: Vec<String> = read(&ctx, |c| {
        Ok(c.prepare("SELECT title FROM tasks ORDER BY title")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    })
    .await;
    assert!(!titles.iter().any(|t| t == "甲" || t == "乙"));
    assert!(titles.iter().any(|t| t == "丙"));
    let digest = read(&ctx, crate::data::retention::load).await;
    assert_eq!(digest["records"]["count"], removed as u64);
    assert_eq!(digest["tasks"]["count"], 2);
    assert_eq!(digest["tasks"]["statuses"][0], json!({"name": "已完成", "count": 2}));
    assert!(!digest.to_string().contains("甲"), "only counts are kept");
    // A second pass finds nothing more to remove.
    assert!(!write(&ctx, move |c| store::prune(c, 30, at)).await);
}

#[tokio::test]
async fn history_keeps_latest_reported_and_restored_groups() {
    let ctx = ctx();
    let at = crate::time::now();
    let old = at - (store::HISTORY_DAYS + 1) * crate::time::DAY;
    write(&ctx, move |c| {
        for (id, action, restored) in
            [("g1", "archive", ""), ("g2", "archive", ""), ("g3", "archive", ""), ("g4", "rollback", "g1")]
        {
            c.execute(
                "INSERT INTO archive_groups(id, created, action, restored_from) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, old, action, restored],
            )?;
        }
        c.execute(
            "INSERT INTO reports(id, time, body) VALUES ('rp', ?1, ?2)",
            rusqlite::params![old, json!({"id": "rp", "archive_group_id": "g3"}).to_string()],
        )?;
        c.execute(
            "INSERT INTO archive_attempts(id, trigger, status, created, finished) VALUES ('a1', 'manual_archive', 'failed', ?1, ?1), ('a2', 'manual_archive', 'succeeded', ?1, ?1)",
            [old],
        )?;
        Ok(())
    })
    .await;
    assert!(write(&ctx, move |c| store::prune_history(c, at)).await);
    let ids: Vec<String> = read(&ctx, |c| Ok(store::groups(c)?.into_iter().map(|g| g.id).collect())).await;
    assert_eq!(ids, ["g1", "g3", "g4"], "g2 is the only unreferenced old group");
    let attempts: Vec<String> = read(&ctx, |c| Ok(store::attempts(c)?.into_iter().map(|a| a.id).collect())).await;
    assert_eq!(attempts, ["a2"], "the newest attempt (also the latest outcome) stays");
    assert!(!write(&ctx, move |c| store::prune_history(c, at)).await);
}

// ---- state ----------------------------------------------------------------

#[tokio::test]
async fn state_carries_archive_parts_and_scopes_reports_and_evolution() {
    let ctx = ctx();
    task(&ctx, "甲", "已完成").await;
    manual(&ctx, &Stub::new(&ctx)).await.unwrap();
    let reports_page = dispatch(&ctx, "state", json!({"page": "reports"})).await.unwrap();
    assert_eq!(reports_page["archive_attempt"]["status"], "succeeded");
    assert_eq!(reports_page["morning_jobs"], json!([]));
    assert_eq!(reports_page["report_outcome"]["kind"], "archive");
    assert_eq!(reports_page["reports"].as_array().unwrap().len(), 1);
    assert!(reports_page["reports_revision"].is_string() && reports_page.get("evolution").is_none());
    ctx.db
        .test_write(|tx| {
            tx.execute(
                r#"INSERT INTO reports(id, time, body) VALUES ('newer', 9, '{"id":"newer","label":"新","time":9,"tasks":[1,2]}')"#,
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let two = dispatch(&ctx, "state", json!({"page": "reports"})).await.unwrap();
    assert_eq!((two["reports"][0]["id"].as_str(), two["reports"][0]["task_count"].as_i64()), (Some("newer"), Some(2)));
    let older = two["reports"][1].as_object().unwrap();
    assert!(older.keys().all(|k| ["id", "label", "date", "time"].contains(&k.as_str())), "older reports stay folded");
    assert_eq!(older["id"], reports_page["reports"][0]["id"]);

    let tasks_page = dispatch(&ctx, "state", json!({"page": "tasks"})).await.unwrap();
    assert!(tasks_page["archive_attempt"].is_object(), "always present");
    assert!(tasks_page.get("reports").is_none() && tasks_page.get("evolution").is_none());
    let learning = dispatch(&ctx, "state", json!({"page": "learning"})).await.unwrap();
    assert!(learning["evolution"]["prompts"].is_array() && learning.get("reports").is_none());
    assert!(learning["evolution"]["revision_count"].is_i64());
}

/// Archive what exists so the open tasks become current tasks next time.
async fn settle(ctx: &Ctx) {
    manual(ctx, &Stub::new(ctx)).await.unwrap();
}

#[tokio::test]
async fn current_tasks_over_several_pages_all_join_a_batch_that_fits() {
    let ctx = ctx();
    for i in 0..5 {
        task(&ctx, &format!("进行{i}"), "待处理").await;
    }
    settle(&ctx).await;
    task(&ctx, "新结束", "已完成").await;
    let stub = Stub { max_evidence: Some(6), ..Stub::new(&ctx) };
    manual(&ctx, &stub).await.unwrap();
    let seen = stub.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].payload["current_tasks"].as_array().unwrap().len(), 5, "pages past the first are read");
    assert_eq!(seen[0].payload["evidence"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn current_tasks_over_the_budget_are_left_out_and_counted() {
    let ctx = ctx();
    for i in 0..5 {
        task(&ctx, &format!("进行{i}"), "待处理").await;
    }
    settle(&ctx).await;
    task(&ctx, "新结束", "已完成").await;
    let stub = Stub { max_evidence: Some(3), ..Stub::new(&ctx) };
    manual(&ctx, &stub).await.unwrap();
    let seen = stub.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].payload["current_tasks"].as_array().unwrap().is_empty());
    assert_eq!(seen[0].payload["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(seen[0].payload["coverage"]["omitted_current_tasks"], 5);
}

async fn pending_learning_after(details: &'static [&'static str]) -> bool {
    let ctx = ctx();
    task(&ctx, "结束", "已完成").await;
    let stub = Stub::new(&ctx).during(move |c| {
        for d in details {
            crate::data::audit::write(c, "manual_status", "", d, &Value::Null, &Value::Null)?;
        }
        Ok(())
    });
    manual(&ctx, &stub).await.unwrap();
    read(&ctx, |c| Ok(kv::get::<bool>(c, crate::data::tasks::PENDING_LEARNING)?.unwrap())).await
}

#[tokio::test]
async fn pending_learning_looks_past_pages_of_other_audits() {
    assert!(pending_learning_after(&["start", "block", "start", "block", "start", "dismiss"]).await);
    assert!(!pending_learning_after(&["start", "block", "start", "block", "start"]).await);
}
