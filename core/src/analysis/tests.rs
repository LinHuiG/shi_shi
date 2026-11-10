//! Offline tests of the analysis domain with synthetic messages and a stub
//! model. Nothing here reaches a real provider.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::progress::{self, Failure};
use super::*;
use crate::ai::prompt::require_purpose;
use crate::ai::{INPUT_LIMIT_ERROR, UNCONFIGURED_PREFIX, UNKNOWN_EVIDENCE};
use crate::data as store;
use crate::data::kv;
use crate::exec::{Cancel, CANCELLED};
use crate::messages::Message;
use crate::{Config, Ctx, Db};

type Edit = Box<dyn FnOnce(&Connection) -> Result<()> + Send>;
type Answer = Box<dyn FnOnce(&Request) -> Reply + Send>;

/// A model that answers from a queue, records every request, can run an
/// edit while "thinking" and can wait for cancellation.
#[derive(Default)]
struct Stub {
    answers: Mutex<VecDeque<Answer>>,
    seen: Mutex<Vec<Request>>,
    edits: Mutex<VecDeque<Option<Edit>>>,
    /// Characters charged per change; `huge` ids cost a million.
    per_change: usize,
    huge: Vec<String>,
    /// Even an empty request is too large.
    always_huge: bool,
    hang: bool,
    db: Option<Db>,
}

impl Stub {
    fn new(ctx: &Ctx) -> Self {
        Self { db: Some(ctx.db.clone()), ..Default::default() }
    }

    fn ok(self, v: Value) -> Self {
        self.answer(move |_| Reply::ok(v))
    }

    fn answer(self, f: impl FnOnce(&Request) -> Reply + Send + 'static) -> Self {
        self.answers.lock().unwrap().push_back(Box::new(f));
        self
    }

    fn during(self, edit: impl FnOnce(&Connection) -> Result<()> + Send + 'static) -> Self {
        self.edits.lock().unwrap().push_back(Some(Box::new(edit)));
        self
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn arc(self) -> Arc<dyn Model> {
        Arc::new(self)
    }
}

impl Model for Stub {
    fn measure(&self, request: Request) -> Result<Measured<Request>> {
        let changes = request.payload["changes"].as_array().cloned().unwrap_or_default();
        let huge = self.always_huge || changes.iter().any(|c| self.huge.iter().any(|h| c["id"] == h.as_str()));
        let total = if huge { 1_000_000 } else { 100 + changes.len() * self.per_change };
        Ok(Measured::new(request, total))
    }

    fn invoke<'a>(&'a self, request: Measured<Request>, cancel: Cancel) -> ReplyFuture<'a> {
        let request = request.request;
        Box::pin(async move {
            self.seen.lock().unwrap().push(request.clone());
            if let (Some(reserve), Some(db)) = (request.reserve.clone(), &self.db) {
                if let Err(e) = db.test_write(move |tx| crate::data::analysis::take_filter(tx, &reserve)).await {
                    return Reply::err(e.0);
                }
            }
            let edit = self.edits.lock().unwrap().pop_front().flatten();
            if let (Some(edit), Some(db)) = (edit, &self.db) {
                if let Err(e) = db.test_write(move |tx| edit(tx)).await {
                    return Reply::err(e.0);
                }
            }
            if self.hang {
                cancel.cancelled().await;
                return Reply::err(CANCELLED);
            }
            match self.answers.lock().unwrap().pop_front() {
                Some(answer) => answer(&request),
                None => Reply::ok(json!({"actions": [], "no_action_reason": "无"})),
            }
        })
    }
}

fn ctx() -> Arc<Ctx> {
    Ctx::memory(Config::default()).unwrap()
}

fn ctx_with(change: impl FnOnce(&mut Config)) -> Arc<Ctx> {
    let mut config = Config::default();
    change(&mut config);
    Ctx::memory(config).unwrap()
}

async fn read<R: Send + 'static>(ctx: &Ctx, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> R {
    ctx.db.test_read(f).await.unwrap()
}

async fn write<R: Send + 'static>(ctx: &Ctx, f: impl FnOnce(&Connection) -> Result<R> + Send + 'static) -> R {
    ctx.db.test_write(move |tx| f(tx)).await.unwrap()
}

fn message(id: &str, text: &str, at: i64) -> Message {
    Message {
        id: id.into(),
        source_id: "src".into(),
        source_name: "合成来源".into(),
        external_id: id.into(),
        chat: "room".into(),
        sender: "同事甲".into(),
        time: at,
        text: text.into(),
        kind: "text".into(),
        mine: true,
        ..Default::default()
    }
}

async fn put(ctx: &Ctx, m: Message) {
    write(ctx, move |c| crate::data::messages::upsert(c, &m).map(|_| ())).await
}

async fn pending(ctx: &Ctx, id: &'static str) -> bool {
    read(ctx, move |c| {
        let hash: String = c.query_row("SELECT content_hash FROM messages WHERE id = ?1", [id], |r| r.get(0))?;
        store::analysis::still_pending(c, id, &hash)
    })
    .await
}

async fn column(ctx: &Ctx, id: &'static str, col: &'static str) -> Option<String> {
    read(ctx, move |c| Ok(c.query_row(&format!("SELECT {col} FROM messages WHERE id = ?1"), [id], |r| r.get(0))?)).await
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

async fn task_count(ctx: &Ctx) -> i64 {
    read(ctx, |c| Ok(c.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?)).await
}

fn create(source: &str, title: &str) -> Value {
    json!({"actions": [{
        "action": "create", "sources": [source], "confidence": 0.8, "reason": "合成测试",
        "fields": [{"field": "title", "value": title}, {"field": "description", "value": "合成描述"},
            {"field": "status", "value": "待处理"}],
        "tag_changes": [],
    }], "no_action_reason": ""})
}

async fn manual(ctx: &Arc<Ctx>, m: &Arc<dyn Model>) -> Result<Value> {
    round::run(ctx, m, Mode::Manual, false).await
}

// ---- rounds -------------------------------------------------------------------

#[tokio::test]
async fn round_without_pending_messages_does_not_call() {
    let ctx = ctx();
    let stub = Arc::new(Stub::new(&ctx));
    let m: Arc<dyn Model> = stub.clone();
    assert_eq!(manual(&ctx, &m).await.unwrap()["skipped"], NOTHING);
    assert_eq!(stub.calls(), 0);
}

#[tokio::test]
async fn round_applies_actions_and_marks_messages_analyzed() {
    let ctx = ctx();
    put(&ctx, message("m1", "请周五前提交合成报告", time::now())).await;
    let stub = Arc::new(Stub::new(&ctx).ok(create("m1", "提交合成报告")));
    let m: Arc<dyn Model> = stub.clone();
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["applied"], 1);
    assert_eq!(r["processed"], 1);
    assert_eq!(r["batches"], 1);
    assert_eq!(r["remaining"], 0);
    assert_eq!(stub.calls(), 1);
    let sent = &stub.seen.lock().unwrap()[0];
    assert_eq!(sent.purpose, "classify");
    assert_eq!(sent.payload["_trigger"], "manual");
    assert!(sent.reader.is_some());
    assert!(!pending(&ctx, "m1").await);
    assert_eq!(task_count(&ctx).await, 1);
    let result = logs(&ctx, "analysis_result").await;
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["applied"], 1);
    let s = read(&ctx, store::ai::analysis_state).await;
    assert!(s.last_success_at > 0 && !s.pending);
    assert!(ctx.analysis.analyzing().is_empty());
}

#[tokio::test]
async fn unadopted_sources_stay_pending_for_retry() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    // Cites a message the batch never showed: only that action is dropped.
    let m = Stub::new(&ctx).ok(create("unknown", "无关事项")).arc();
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["applied"], 0);
    assert!(!pending(&ctx, "m1").await);
}

#[tokio::test]
async fn authority_change_during_call_rejects_the_batch() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let m = Stub::new(&ctx)
        .during(|c| kv::bump(c, crate::data::rules::RULE_VERSION).map(|_| ()))
        .ok(create("m1", "不应创建"))
        .arc();
    let e = manual(&ctx, &m).await.unwrap_err();
    assert_eq!(e.0, AUTHORITY_CHANGED);
    assert!(pending(&ctx, "m1").await);
    assert_eq!(task_count(&ctx).await, 0);
}

#[tokio::test]
async fn message_edited_during_call_is_not_marked() {
    let ctx = ctx();
    put(&ctx, message("m1", "初版", time::now())).await;
    let edited = message("m1", "改过的版本", time::now());
    let m = Stub::new(&ctx)
        .during(move |c| crate::data::messages::upsert(c, &edited).map(|_| ()))
        .ok(create("m1", "x"))
        .arc();
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["applied"], 0);
    assert_eq!(r["processed"], 0);
    assert!(pending(&ctx, "m1").await);
    assert_eq!(task_count(&ctx).await, 0);
}

#[tokio::test]
async fn oversized_message_is_deferred_and_the_rest_goes_on() {
    let ctx = ctx_with(|c| c.max_input_chars = 5_000);
    let now = time::now();
    put(&ctx, message("big", "大消息", now - 2_000)).await;
    put(&ctx, message("small", "小消息", now - 1_000)).await;
    let stub = Stub { huge: vec!["big".into()], per_change: 10, ..Stub::new(&ctx) }.ok(create("small", "小事项"));
    let m = stub.arc();
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["applied"], 1);
    assert_eq!(r["processed"], 1);
    assert!(pending(&ctx, "big").await);
    assert!(!pending(&ctx, "small").await);
    // Nothing is charged to the message: the next round measures it again.
    assert_eq!(column(&ctx, "big", "retry_reason").await, None);
    assert_eq!(logs(&ctx, "analysis_input_skipped").await[0]["id"], "big");
}

#[tokio::test]
async fn input_limit_fails_only_this_round_and_the_next_round_checks_again() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let huge = Stub { always_huge: true, ..Stub::new(&ctx) }.arc();
    let e = round::run(&ctx, &huge, Mode::Automatic, false).await.unwrap_err();
    assert!(e.0.starts_with(INPUT_LIMIT_ERROR), "{}", e.0);
    assert_eq!(logs(&ctx, "analysis_input_limit").await.len(), 1);
    assert!(pending(&ctx, "m1").await);
    assert_eq!(column(&ctx, "m1", "retry_reason").await, None);
    // The failed round still counts as the last analysis; nothing is locked.
    assert!(read(&ctx, store::ai::analysis_state).await.last_analysis_at > 0);
    let fits = Stub::new(&ctx).ok(create("m1", "合成事项")).arc();
    let r = round::run(&ctx, &fits, Mode::Automatic, false).await.unwrap();
    assert_eq!(r["applied"], 1);
    assert!(!pending(&ctx, "m1").await);
}

#[tokio::test]
async fn partial_round_keeps_committed_batches() {
    let ctx = ctx_with(|c| c.max_input_chars = 1_500);
    let now = time::now();
    put(&ctx, message("a", "第一条", now - 2_000)).await;
    put(&ctx, message("b", "第二条", now - 1_000)).await;
    let m = Stub { per_change: 1_000, ..Stub::new(&ctx) }
        .ok(create("a", "第一件事"))
        .answer(|_| Reply::err("合成失败"))
        .arc();
    let e = manual(&ctx, &m).await.unwrap_err();
    assert!(e.0.starts_with("合成失败；已提交 1 批、处理 1 条消息、采用 1 个动作"), "{}", e.0);
    assert!(!pending(&ctx, "a").await);
    assert!(pending(&ctx, "b").await);
    let partial = logs(&ctx, "analysis_partial").await;
    assert_eq!(partial[0]["batches"], 1);
    assert_eq!(partial[0]["error"], "合成失败");
}

#[tokio::test]
async fn invalid_response_charges_a_failure() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let m = Stub::new(&ctx)
        .answer(|_| Reply {
            result: Err(crate::Error::new(UNKNOWN_EVIDENCE)),
            commands_ran: false,
            invalid_response: true,
            refused: false,
        })
        .arc();
    assert_eq!(manual(&ctx, &m).await.unwrap_err().0, UNKNOWN_EVIDENCE);
    assert!(pending(&ctx, "m1").await);
    assert_eq!(
        column(&ctx, "m1", "retry_reason").await.as_deref(),
        Some(format!("{UNKNOWN_EVIDENCE}；本批未提交，等待后续自动分析").as_str())
    );
}

fn refused() -> Reply {
    Reply {
        result: Err(crate::Error::new("合成拒答")), commands_ran: false, invalid_response: false, refused: true
    }
}

async fn errors(ctx: &Ctx, id: &'static str) -> i64 {
    read(ctx, move |c| Ok(c.query_row("SELECT error_count FROM messages WHERE id = ?1", [id], |r| r.get(0))?)).await
}

#[tokio::test]
async fn only_input_errors_count_and_a_version_at_the_limit_is_not_selected() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    // Output and validation failures never count.
    let m = Stub::new(&ctx)
        .answer(|_| Reply {
            result: Err(crate::Error::new(UNKNOWN_EVIDENCE)),
            commands_ran: false,
            invalid_response: true,
            refused: false,
        })
        .answer(|_| Reply::err("合成网络超时"))
        .arc();
    manual(&ctx, &m).await.unwrap_err();
    manual(&ctx, &m).await.unwrap_err();
    assert_eq!(errors(&ctx, "m1").await, 0);
    // A refused single message counts once per round, never retried within it.
    let stub = Arc::new(Stub::new(&ctx).answer(|_| refused()).answer(|_| refused()).answer(|_| refused()));
    let m: Arc<dyn Model> = stub.clone();
    for n in 1..=progress::MAX_INPUT_ERRORS {
        manual(&ctx, &m).await.unwrap_err();
        assert_eq!(errors(&ctx, "m1").await, n);
        assert_eq!(stub.calls() as i64, n);
    }
    let config = ctx.config.get();
    let (rows, manual_n, auto_n) = read(&ctx, move |c| {
        Ok((
            store::analysis::pending_rows(c, &config, true)?,
            store::analysis::pending_count(c, &config, true)?,
            store::analysis::pending_count(c, &config, false)?,
        ))
    })
    .await;
    assert!(rows.is_empty() && manual_n == 0 && auto_n == 0, "{rows:?}");
    assert_eq!(manual(&ctx, &m).await.unwrap()["skipped"], NOTHING);
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["skipped"].is_string(), true);
    assert_eq!(stub.calls(), 3);
    // The same content scanned again keeps its count; corrected content starts over.
    put(&ctx, message("m1", "合成消息", time::now())).await;
    assert_eq!(errors(&ctx, "m1").await, 3);
    assert_eq!(manual(&ctx, &m).await.unwrap()["skipped"], NOTHING);
    put(&ctx, message("m1", "合成消息（已更正）", time::now())).await;
    let config = ctx.config.get();
    let rows = read(&ctx, move |c| store::analysis::pending_rows(c, &config, true)).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].errors, 0);
}

#[tokio::test]
async fn a_refused_batch_is_not_charged_to_its_messages() {
    let ctx = ctx();
    let now = time::now();
    put(&ctx, message("a", "第一条", now - 2_000)).await;
    put(&ctx, message("b", "第二条", now - 1_000)).await;
    let m = Stub::new(&ctx).answer(|_| refused()).arc();
    manual(&ctx, &m).await.unwrap_err();
    assert_eq!((errors(&ctx, "a").await, errors(&ctx, "b").await), (0, 0));
    assert!(pending(&ctx, "a").await && pending(&ctx, "b").await);
}

#[tokio::test]
async fn command_unconfirmed_stays_manual_only_without_reaching_the_limit() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let m = Stub::new(&ctx)
        .answer(|_| Reply {
            result: Err(crate::Error::new("合成命令失败")),
            commands_ran: true,
            invalid_response: false,
            refused: false,
        })
        .arc();
    manual(&ctx, &m).await.unwrap_err();
    assert_eq!(errors(&ctx, "m1").await, 0);
    let config = ctx.config.get();
    let (manual_n, auto_n, auto_rows) = read(&ctx, move |c| {
        Ok((
            store::analysis::pending_count(c, &config, true)?,
            store::analysis::pending_count(c, &config, false)?,
            store::analysis::pending_rows(c, &config, false)?,
        ))
    })
    .await;
    assert_eq!((manual_n, auto_n, auto_rows.len()), (1, 0, 0));
}

#[tokio::test]
async fn interrupted_round_keeps_messages_and_records_interruption() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let m = Stub { hang: true, ..Stub::new(&ctx) }.arc();
    let (c2, m2) = (ctx.clone(), m.clone());
    let running = tokio::spawn(async move { round::run(&c2, &m2, Mode::Manual, false).await });
    while ctx.analysis.analyzing().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(ctx.exec.interrupt(None));
    let e = running.await.unwrap().unwrap_err();
    assert_eq!(e.0, CANCELLED);
    assert!(pending(&ctx, "m1").await);
    assert!(read(&ctx, store::ai::analysis_state).await.analysis_interrupted_at > 0);
    assert!(read(&ctx, store::ai::analysis_state).await.last_analysis_at > 0, "a started round keeps its trigger time");
    assert!(ctx.analysis.analyzing().is_empty());
}

#[tokio::test]
async fn busy_ai_answers_busy_for_a_plain_manual_round() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let hold = Stub { hang: true, ..Stub::new(&ctx) }.arc();
    let (c2, h2) = (ctx.clone(), hold.clone());
    let running = tokio::spawn(async move { round::run(&c2, &h2, Mode::Manual, false).await });
    while !ctx.exec.ai_busy() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let m = Stub::new(&ctx).arc();
    assert_eq!(manual(&ctx, &m).await.unwrap_err().0, crate::exec::BUSY);
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["skipped"], round::AI_QUEUED);
    ctx.exec.interrupt(None);
    let _ = running.await;
}

#[tokio::test]
async fn automatic_round_waits_for_its_interval() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let stub = Arc::new(Stub::new(&ctx));
    let m: Arc<dyn Model> = stub.clone();
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["skipped"], NOT_DUE);
    assert!(read(&ctx, store::ai::analysis_state).await.pending);
    assert_eq!(stub.calls(), 0);
    let off = ctx_with(|c| c.auto_ai = false);
    assert_eq!(round::automatic(&off, &m).await.unwrap()["skipped"], DISABLED);
}

/// Move the trigger time back so the interval has passed.
async fn rewind(ctx: &Ctx) {
    let interval = ctx.config.get().analysis_interval_minutes.max(1) as i64 * time::MINUTE;
    write(ctx, move |c| {
        store::ai::update_analysis_state(c, |s| {
            s.last_analysis_at -= interval + progress::COOLDOWN;
            s.pending_since -= interval + progress::COOLDOWN;
        })
        .map(|_| ())
    })
    .await
}

#[tokio::test]
async fn a_manual_round_sets_the_trigger_time_and_stale_automatic_triggers_lapse() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let stub = Arc::new(Stub::new(&ctx));
    let m: Arc<dyn Model> = stub.clone();
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["skipped"], NOT_DUE);
    rewind(&ctx).await;
    let before = time::now();
    // A failed manual round still counts as the trigger.
    let failing = Stub::new(&ctx).answer(|_| Reply::err("合成失败")).arc();
    manual(&ctx, &failing).await.unwrap_err();
    let started = read(&ctx, store::ai::analysis_state).await.last_analysis_at;
    assert!(started >= before, "{started} {before}");
    put(&ctx, message("m2", "新合成消息", time::now())).await;
    // The automatic trigger computed before the manual round lapses, both
    // at the check and when the round itself rechecks.
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["skipped"], NOT_DUE);
    assert_eq!(round::run(&ctx, &m, Mode::Automatic, true).await.unwrap()["skipped"], NOT_DUE);
    assert_eq!(stub.calls(), 0);
    assert_eq!(read(&ctx, store::ai::analysis_state).await.last_analysis_at, started);
    // After the normal interval it runs.
    rewind(&ctx).await;
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["processed"], 2);
    assert_eq!(stub.calls(), 1);
}

#[tokio::test]
async fn refused_or_empty_rounds_do_not_touch_the_trigger_time() {
    let ctx = ctx();
    let m = Stub::new(&ctx).arc();
    assert_eq!(manual(&ctx, &m).await.unwrap()["skipped"], NOTHING);
    assert_eq!(read(&ctx, store::ai::analysis_state).await.last_analysis_at, 0);
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let hold = Stub { hang: true, ..Stub::new(&ctx) }.arc();
    let (c2, h2) = (ctx.clone(), hold.clone());
    let running = tokio::spawn(async move { round::run(&c2, &h2, Mode::Manual, false).await });
    while ctx.analysis.analyzing().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = read(&ctx, store::ai::analysis_state).await.last_analysis_at;
    assert!(started > 0);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(manual(&ctx, &m).await.unwrap_err().0, crate::exec::BUSY);
    assert_eq!(read(&ctx, store::ai::analysis_state).await.last_analysis_at, started);
    ctx.exec.interrupt(None);
    assert_eq!(running.await.unwrap().unwrap_err().0, CANCELLED);
    assert_eq!(
        read(&ctx, store::ai::analysis_state).await.last_analysis_at,
        started,
        "a cancellation does not move it back"
    );
    // The busy slot does not hold up the next trigger.
    rewind(&ctx).await;
    write(&ctx, |c| store::ai::update_analysis_state(c, |s| s.analysis_interrupted_at = 0).map(|_| ())).await;
    assert_eq!(round::automatic(&ctx, &m).await.unwrap()["processed"], 1);
}

#[tokio::test]
async fn memory_update_is_written_in_the_accept_transaction() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let mut v = create("m1", "事项");
    v["_memory_update"] = json!({"before": "", "text": "合成笔记"});
    let m = Stub::new(&ctx).ok(v).arc();
    manual(&ctx, &m).await.unwrap();
    let note = read(&ctx, |c| crate::data::prompts::memory(c, "temporary")).await;
    assert_eq!(note.text, "合成笔记");
    assert_eq!(logs(&ctx, "analysis_result").await[0]["memory_updated"], true);
}

#[tokio::test]
async fn stale_memory_update_is_skipped_but_the_batch_commits() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let mut v = create("m1", "事项");
    v["_memory_update"] = json!({"before": "旧的", "text": "新的"});
    let m = Stub::new(&ctx).ok(v).arc();
    assert_eq!(manual(&ctx, &m).await.unwrap()["applied"], 1);
    assert_eq!(logs(&ctx, "memory_skipped").await[0]["error"], apply::MEMORY_CHANGED);
}

#[tokio::test]
async fn tag_suggestions_citing_unknown_evidence_are_dropped() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let n = read(&ctx, |c| {
        let m1 = crate::data::messages::get(c, "m1")?.unwrap();
        let allowed = BTreeMap::from([("m1".to_string(), m1)]);
        let payload = json!({"changes": [{"id": "m1"}]});
        let result = json!({"tag_suggestions": [
            {"tag": "合成标签", "reason": "测试", "evidence_ids": ["outside"]},
            {"tag": "", "reason": "测试", "evidence_ids": ["m1"]},
        ]});
        crate::data::analysis::collect_tags(c, &result, &payload, &allowed, &[])
    })
    .await;
    assert_eq!(n, 0);
}

// ---- filter -------------------------------------------------------------------

async fn filter_state(ctx: &Ctx) -> filter::FilterState {
    read(ctx, crate::data::analysis::filter_state).await
}

async fn reset_cooldown(ctx: &Ctx) {
    write(ctx, |c| Ok(c.execute("UPDATE filter_state SET last_run = 0", []).map(|_| ())?)).await
}

#[tokio::test]
async fn filter_wait_marks_messages_seen_and_analyze_asks_for_early_run() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let stub = Arc::new(Stub::new(&ctx).ok(json!({"decision": "wait", "reason": "无需处理"})));
    let m: &dyn Model = stub.as_ref();
    filter::tick(&ctx, m).await.unwrap();
    assert_eq!(stub.calls(), 1);
    assert_eq!(stub.seen.lock().unwrap()[0].purpose, "message_filter");
    assert!(!filter_state(&ctx).await.early);
    assert_eq!(column(&ctx, "m1", "filter_hash").await, column(&ctx, "m1", "content_hash").await);
    // Already looked at: nothing to filter.
    reset_cooldown(&ctx).await;
    filter::tick(&ctx, m).await.unwrap();
    assert_eq!(stub.calls(), 1);

    put(&ctx, message("m2", "紧急合成消息", time::now())).await;
    let stub = Arc::new(Stub::new(&ctx).ok(json!({"decision": "analyze", "reason": "紧急"})));
    filter::tick(&ctx, stub.as_ref()).await.unwrap();
    let f = filter_state(&ctx).await;
    assert!(f.early);
    assert_eq!(f.trigger, "message_filter");
    assert_eq!(f.reason, "紧急");

    // The early run reports the filter trigger and consumes the decision.
    let full = Arc::new(Stub::new(&ctx));
    let m: Arc<dyn Model> = full.clone();
    let r = round::automatic(&ctx, &m).await.unwrap();
    assert_eq!(r["processed"], 2, "{r}");
    assert_eq!(full.seen.lock().unwrap()[0].payload["_trigger"], "message_filter");
    assert!(!filter_state(&ctx).await.early);
}

#[tokio::test]
async fn filter_failure_falls_back_to_full_analysis() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let stub = Stub::new(&ctx).answer(|_| Reply::err("合成过滤失败"));
    filter::tick(&ctx, &stub).await.unwrap();
    let f = filter_state(&ctx).await;
    assert!(f.early);
    assert_eq!(f.trigger, "message_filter_failed");
    assert!(f.reason.contains("合成过滤失败"));
    assert_eq!(column(&ctx, "m1", "filter_hash").await, None);
}

#[tokio::test]
async fn filter_reset_forgets_decision_and_marks() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    let stub = Stub::new(&ctx).ok(json!({"decision": "analyze", "reason": "x"}));
    filter::tick(&ctx, &stub).await.unwrap();
    write(&ctx, |c| {
        c.execute("UPDATE messages SET filter_hash = content_hash", [])?;
        crate::data::analysis::reset_filter(c)
    })
    .await;
    assert!(!filter_state(&ctx).await.early);
    assert_eq!(column(&ctx, "m1", "filter_hash").await, None);
}

#[tokio::test]
async fn consume_ignores_an_older_decision() {
    let ctx = ctx();
    write(&ctx, |c| {
        c.execute("UPDATE filter_state SET early = 1, last_run = 200, trigger = 'message_filter'", [])?;
        crate::data::analysis::consume_filter(c, Some(100))
    })
    .await;
    assert!(filter_state(&ctx).await.early);
    write(&ctx, |c| crate::data::analysis::consume_filter(c, Some(200))).await;
    assert!(!filter_state(&ctx).await.early);
}

// ---- backlog ------------------------------------------------------------------

#[tokio::test]
async fn backlog_skips_only_previewed_versions() {
    let ctx = ctx();
    let old = time::now() - 3 * time::HOUR;
    put(&ctx, message("old1", "旧消息一", old)).await;
    put(&ctx, message("old2", "旧消息二", old)).await;
    put(&ctx, message("new", "新消息", time::now())).await;
    assert_eq!(backlog::handle(&ctx, json!({"hours": 0})).await.unwrap_err().0, backlog::HOURS_ERROR);
    let mut preview = backlog::handle(&ctx, json!({"hours": 1})).await.unwrap();
    assert_eq!(preview["count"], 2);
    assert_eq!(preview["eligible"], 3);
    assert_eq!(backlog::handle(&ctx, json!({"hours": 1, "apply": true})).await.unwrap_err().0, backlog::PREVIEW_FIRST);
    // Edited after the preview: stays pending.
    put(&ctx, message("old2", "旧消息二（已编辑）", old)).await;
    preview["apply"] = json!(true);
    let r = backlog::handle(&ctx, preview).await.unwrap();
    assert_eq!(r["skipped_count"], 1);
    assert_eq!(r["remaining"], 2);
    assert!(!pending(&ctx, "old1").await);
    assert!(pending(&ctx, "old2").await);
    assert!(pending(&ctx, "new").await);
    assert_eq!(logs(&ctx, "analysis_skipped").await[0]["skipped"], 1);
}

#[tokio::test]
async fn backlog_groups_retry_reasons() {
    let ctx = ctx();
    put(&ctx, message("m1", "合成消息", time::now())).await;
    write(&ctx, |c| {
        let hash: String = c.query_row("SELECT content_hash FROM messages WHERE id = 'm1'", [], |r| r.get(0))?;
        let one = BTreeMap::from([("m1".to_string(), hash)]);
        store::analysis::record_failure(c, &one, Failure::CommandUnconfirmed, "命令未确认")
    })
    .await;
    let preview = backlog::handle(&ctx, json!({})).await.unwrap();
    assert_eq!(preview["hours"], 24);
    assert_eq!(preview["manual_retry_count"], 1);
    assert_eq!(preview["retry_reasons"][0]["automatic_retry"], false);
    assert_eq!(preview["retry_reasons_more"], 0);
    // The SQL counts agree with the listed rows.
    put(&ctx, message("m2", "另一条合成消息", time::now())).await;
    let config = ctx.config.get();
    let (rows, auto, part) = read(&ctx, move |c| {
        let rows = store::analysis::pending_rows(c, &config, true)?;
        let analyzing = BTreeMap::from([("m2".to_string(), rows.iter().find(|r| r.id == "m2").unwrap().hash.clone())]);
        let mut part = serde_json::Map::new();
        store::analysis::part(c, &config, &analyzing, &mut part)?;
        Ok((rows, store::analysis::pending_rows(c, &config, false)?, part))
    })
    .await;
    assert_eq!((part["pending"].as_u64(), part["manual_retry_count"].as_u64()), (Some(1), Some(1)));
    assert_eq!(rows.iter().filter(|r| !r.automatic).count(), 1);
    let (_, backlog, _) = ctx.db.analysis_probe(ctx.config.get()).await.unwrap();
    assert_eq!(backlog, progress::Backlog::of(&auto));
    assert_eq!(backlog.count, 1);
}

// ---- read_context ---------------------------------------------------------------

#[tokio::test]
async fn reader_keeps_only_accepted_reads() {
    let ctx = ctx();
    put(&ctx, message("m1", "可补查的合成消息", time::now())).await;
    let config = ctx.config.get();
    let reader = Arc::new(context::Reader::new(ctx.db.clone(), &config, &json!({})));
    let snapshot = json!({});
    let mut meter = context::Meter::default();
    let read = reader.clone();
    let out =
        tokio::task::spawn_blocking(move || read.query_blocking(&snapshot, &mut meter, &json!({"ids": ["m1"]}), None))
            .await
            .unwrap();
    assert!(out.contains("可补查的合成消息"), "{out}");
    assert!(reader.accepted().messages.is_empty(), "staged reads are not evidence yet");
    reader.accept();
    assert!(reader.accepted().messages.contains_key("m1"));
}

#[test]
fn snapshot_pager_rejects_empty_queries_and_pages_long_bodies() {
    let long: String = "合".repeat(context::PAGE * 2);
    let snapshot =
        json!({"records": [{"id": "r1", "kind": "message", "source": "src", "chat": "room", "data": {"text": long}}]});
    let mut meter = context::Meter::default();
    let out = context::query(&snapshot, &mut meter, &json!({}), None);
    assert!(out.contains(context::INVALID_ARGS), "{out}");
    let first = context::query(&snapshot, &mut meter, &json!({"ids": ["r1"]}), None);
    assert!(first.chars().count() <= context::PAGE);
    assert!(meter.offsets.get("r1").is_some_and(|n| *n > 0), "{meter:?}");
    assert!(!meter.complete.contains("r1"));
}

// ---- helpers kept small ---------------------------------------------------------

#[test]
fn require_purpose_names_the_purpose() {
    let mut config = Config::default();
    config.model_profiles.as_mut().unwrap().analysis = None;
    let e = require_purpose(&config, "classify").unwrap_err();
    assert!(e.0.starts_with(UNCONFIGURED_PREFIX) && e.0.contains("事项更新"));
}

// ---- the AI layer with the fake driver ------------------------------------------

use crate::ai::provider::fake::{Fake, Step};

const USAGE: fn() -> Value =
    || json!({"input_tokens": 10, "output_tokens": 5, "cached_input_tokens": 0, "total_tokens": 15});

async fn layered(ctx: &Arc<Ctx>, steps: Vec<Step>) -> (Arc<Fake>, Arc<dyn Model>) {
    let fake = Fake::new(steps);
    ctx.ai.set_driver(Some(fake.clone()));
    (fake, layer::model(ctx).await.unwrap())
}

#[tokio::test]
async fn layer_sends_aliases_and_applies_the_decoded_action() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "请周五前提交合成报告", time::now())).await;
    let (fake, m) = layered(&ctx, vec![Step::Reply(create("m1", "提交合成报告"), USAGE())]).await;
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["applied"], 1);
    assert_eq!(fake.calls(), 1);
    let (kind, prompt) = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(kind, "classify");
    assert!(prompt.contains("m1") && !prompt.contains("msg-a"), "{prompt}");
    assert!(!pending(&ctx, "msg-a").await);
    assert_eq!(task_count(&ctx).await, 1);
}

#[tokio::test]
async fn layer_reads_context_through_the_reader_in_aliases() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "可补查的合成正文", time::now())).await;
    let steps = vec![
        Step::Read("read_context".into(), json!({"ids": ["m1"]}), None),
        Step::Reply(create("m1", "合成事项"), USAGE()),
    ];
    let (fake, m) = layered(&ctx, steps).await;
    assert_eq!(manual(&ctx, &m).await.unwrap()["applied"], 1);
    let reads = fake.reads.lock().unwrap().clone();
    assert!(reads[0].contains("可补查的合成正文") && reads[0].contains("m1"), "{reads:?}");
    assert!(!reads[0].contains("msg-a"), "{reads:?}");
}

#[tokio::test]
async fn layer_denies_unregistered_tools_and_bounds_images() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成消息", time::now())).await;
    let steps = vec![
        Step::Read("run_command".into(), json!({}), None),
        Step::Read("read_image".into(), json!({"path": "/x.png"}), None),
        Step::Read("read_image".into(), json!({"image_id": "i9"}), None),
        Step::Read("read_image".into(), json!({"image_id": "i9"}), None),
        Step::Reply(create("m1", "合成事项"), USAGE()),
    ];
    let (fake, m) = layered(&ctx, steps).await;
    manual(&ctx, &m).await.unwrap();
    let reads = fake.reads.lock().unwrap().clone();
    assert!(reads[0].starts_with("failure:Denied"), "{reads:?}");
    assert!(reads[1].starts_with("failure:Unavailable"), "{reads:?}");
    assert!(reads[3].starts_with("failure:Unavailable"), "{reads:?}");
}

#[tokio::test]
async fn layer_replaces_the_temporary_memory() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成消息", time::now())).await;
    let mut v = create("m1", "事项");
    v["memory_update"] = json!(["合成笔记"]);
    let (_, m) = layered(&ctx, vec![Step::Reply(v, USAGE())]).await;
    manual(&ctx, &m).await.unwrap();
    let note = read(&ctx, |c| crate::data::prompts::memory(c, "temporary")).await;
    assert_eq!(note.text, "合成笔记");
}

#[tokio::test]
async fn layer_degrades_an_empty_conclusion_without_asking_again() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成闲聊", time::now())).await;
    let v = json!({"actions": [], "no_action_reason": " "});
    let (_, m) = layered(&ctx, vec![Step::Reply(v, USAGE())]).await;
    let r = manual(&ctx, &m).await.unwrap();
    assert_eq!(r["processed"], 1);
    assert_eq!(logs(&ctx, "analysis_result").await[0]["analysis_degraded"], true);
}

#[tokio::test]
async fn layer_sends_the_fitted_render_with_its_batch_metrics() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成消息", time::now())).await;
    let (_, m) = layered(&ctx, vec![Step::Reply(json!({"actions": [], "no_action_reason": "无"}), USAGE())]).await;
    manual(&ctx, &m).await.unwrap();
    let start = &logs(&ctx, "ai_start").await[0];
    // The batch was measured once and sent as measured.
    assert_eq!(start["batch"]["packing_probes"], 1);
    assert_eq!(start["batch"]["total_input_chars"], start["input_sizes"]["total_input_chars"]);
}

#[tokio::test]
async fn layer_rejects_unknown_aliases_as_an_invalid_result() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成消息", time::now())).await;
    let (_, m) = layered(&ctx, vec![Step::Reply(create("m9", "无据事项"), USAGE())]).await;
    let e = manual(&ctx, &m).await.unwrap_err();
    assert!(e.0.contains(UNKNOWN_EVIDENCE), "{e:?}");
    assert!(pending(&ctx, "msg-a").await);
    assert_eq!(task_count(&ctx).await, 0);
    let reason = column(&ctx, "msg-a", "retry_reason").await.unwrap_or_default();
    assert!(reason.contains(UNKNOWN_EVIDENCE) && reason.ends_with("本批未提交，等待后续自动分析"), "{reason}");
}

#[tokio::test]
async fn layer_reports_a_provider_refusal_as_refused() {
    let ctx = ctx();
    put(&ctx, message("msg-a", "合成消息", time::now())).await;
    let refusal = crate::ai::provider::api_error(&shishi_api::ApiError::simple(shishi_api::ApiErrorKind::Refusal));
    let (_, m) = layered(&ctx, vec![Step::Fail("API 分析失败：connection refused".into()), Step::Fail(refusal)]).await;
    manual(&ctx, &m).await.unwrap_err();
    assert_eq!(errors(&ctx, "msg-a").await, 0);
    manual(&ctx, &m).await.unwrap_err();
    assert_eq!(errors(&ctx, "msg-a").await, 1);
    let reason = column(&ctx, "msg-a", "retry_reason").await.unwrap_or_default();
    assert!(reason.contains("供应商拒答") && reason.contains("内容修改后重新计数"), "{reason}");
}

#[test]
fn layer_clears_projects_the_request_did_not_offer() {
    let projects = json!([{"id": "p1"}]);
    let mut kept = json!({"suggested_project_id": "p1", "suggested_project_reason": "相关"});
    layer::check_project(&mut kept, &projects);
    assert_eq!(kept["suggested_project_id"], "p1");
    for mut row in [
        json!({"suggested_project_id": "p2", "suggested_project_reason": "相关"}),
        json!({"suggested_project_id": "p1", "suggested_project_reason": " "}),
    ] {
        layer::check_project(&mut row, &projects);
        assert_eq!(row["suggested_project_id"], "");
        assert_eq!(row["suggested_project_reason"], "");
    }
}

#[test]
fn archive_context_replaces_the_archived_memory_it_showed() {
    let payload =
        json!({"_archived_memory": "旧归档", "_local_context": {"records": [], "archived_memory": {"revision": 3}}});
    let mut c = crate::archive::layer::archive_context("archive", &payload).unwrap();
    let mut v = json!({"archived_memory": ["新归档"], "memory_update": ["不可写"]});
    c.finish(&mut v);
    assert_eq!(v["_archived_memory_update"], json!({"before": "旧归档", "text": "新归档", "expected": 3}));
    assert!(v.get("archived_memory").is_none() && v.get("_memory_update").is_none());
    assert!(v.get("_memory_error").is_none());
    assert_eq!(v["_context_usage"]["calls"], 0);
    assert!(crate::archive::layer::archive_context("report", &payload).is_none());
    let Err(e) = c.read("read_image", &json!({"image_id": "i1"}), None) else { panic!("image read in an archive") };
    assert_eq!(e.kind(), shishi_api::ToolFailureKind::Unavailable);
}

// ---- change notices -----------------------------------------------------------

struct Recorder(Mutex<Vec<crate::HostEvent>>);

impl crate::Host for Recorder {
    fn event(&self, event: crate::HostEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn a_committed_ai_change_notifies_its_task_once() {
    let host = Arc::new(Recorder(Mutex::new(Vec::new())));
    let ctx = Ctx::new(
        Default::default(),
        Db::memory().unwrap(),
        crate::ConfigStore::memory(Config::default()),
        host.clone(),
    );
    put(&ctx, message("m1", "请周五前提交合成报告", time::now())).await;
    let m: Arc<dyn Model> = Arc::new(Stub::new(&ctx).ok(create("m1", "提交合成报告")));
    assert_eq!(manual(&ctx, &m).await.unwrap()["applied"], 1);
    let events = host.0.lock().unwrap();
    let [crate::HostEvent::Notify { title, body, task_id: Some(_) }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!((title.as_str(), body.as_str()), ("提交合成报告", "新增事项"));
}

#[test]
fn notices_keep_one_per_task_and_skip_unchanged_writes() {
    let mut applied = super::apply::Applied::default();
    let task = |status: &str| json!({"title": "合成事项", "status": status, "blocker": ""});
    applied.record("a", &Value::Null, &task("待处理"), "");
    applied.record("a", &task("待处理"), &task("处理中"), "");
    applied.record(
        "b",
        &task("待处理"),
        &task("处理中"),
        "这是一段明显超过三十个字符长度限制的合成变更摘要文字内容需要回退",
    );
    applied.record("c", &task("待处理"), &task("待处理"), "");
    let notices = applied.notices();
    assert_eq!(
        notices,
        [("a".into(), "合成事项".into(), "新增事项".into()), ("b".into(), "合成事项".into(), "状态改为处理中".into()),]
    );
}
