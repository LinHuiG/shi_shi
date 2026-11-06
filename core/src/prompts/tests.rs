//! Offline tests with synthetic prompts, reviews and audit rows.

use std::sync::Arc;

use serde_json::{json, Value};

use super::learning::{self, Evidence, Suggestion};
use super::review::{self, Review};
use super::*;
use crate::data::kv;
use crate::{dispatch, exec, Config, Ctx};

fn ctx() -> Arc<Ctx> {
    Ctx::memory(Config::default()).unwrap()
}

async fn current_id(ctx: &Arc<Ctx>, target: &'static str) -> String {
    ctx.db.test_read(move |c| Ok(crate::data::prompts::current(c, target)?.unwrap().id)).await.unwrap()
}

async fn audits(ctx: &Arc<Ctx>, kind: &'static str) -> Vec<(String, String)> {
    ctx.db
        .test_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT coalesce(before, ''), coalesce(after, '') FROM task_audit WHERE kind = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn seed_versions_save_and_history() {
    let ctx = ctx();
    let all = dispatch(&ctx, "prompts", json!({})).await.unwrap();
    assert_eq!(all.as_object().unwrap().len(), TARGETS.len());
    for t in TARGETS {
        assert!(!default(t).is_empty(), "{t}");
    }
    let h = dispatch(&ctx, "prompt_history", json!({"target": "format"})).await.unwrap();
    assert_eq!(h["versions"].as_array().unwrap().len(), 1);
    assert_eq!(
        (h["current"]["action"].as_str(), h["current"]["content"].as_str()),
        (Some("initial"), Some(default("format")))
    );
    assert!(h["versions"][0].get("content").is_none(), "list carries no text");

    let base = current_id(&ctx, "format").await;
    for (p, msg) in [
        (json!({"text": "x"}), "缺少提示词类型"),
        (json!({"target": "format"}), "缺少提示词"),
        (json!({"target": "format", "text": "长".repeat(20001), "expected": base}), "每套提示词最多 20000 字"),
        (json!({"target": "format", "text": "x", "expected": "old"}), "提示词已更新，请重新打开编辑"),
        (json!({"target": "nope", "text": "x"}), "未知提示词类型"),
    ] {
        assert_eq!(dispatch(&ctx, "prompt_save", p).await.unwrap_err().0, msg);
    }
    // Saving the effective text again adds nothing.
    dispatch(&ctx, "prompt_save", json!({"target": "format", "text": default("format"), "expected": base}))
        .await
        .unwrap();
    assert_eq!(current_id(&ctx, "format").await, base);
    dispatch(&ctx, "prompt_save", json!({"target": "format", "text": "自定义格式", "expected": base})).await.unwrap();
    let edited = current_id(&ctx, "format").await;
    assert_ne!(edited, base);
    assert_eq!(ctx.db.test_read(|c| crate::data::prompts::effective(c, "format")).await.unwrap(), "自定义格式");
    // Empty text returns to the factory text as a new version.
    dispatch(&ctx, "prompt_save", json!({"target": "format", "text": " ", "expected": edited})).await.unwrap();
    assert_eq!(ctx.db.test_read(|c| crate::data::prompts::effective(c, "format")).await.unwrap(), default("format"));
    let h = dispatch(&ctx, "prompt_history", json!({"target": "format"})).await.unwrap();
    assert_eq!(h["versions"].as_array().unwrap().len(), 3);
    assert_eq!(h["versions"][1]["previous_id"], json!(base));
    let one = dispatch(&ctx, "prompt_history", json!({"target": "format", "version": edited})).await.unwrap();
    assert_eq!(one["content"], "自定义格式");
    let err = dispatch(&ctx, "prompt_history", json!({"target": "classify", "version": edited})).await.unwrap_err();
    assert_eq!(err.0, "提示词版本不存在");
    let err = dispatch(&ctx, "prompt_history", json!({"target": "x"})).await.unwrap_err();
    assert_eq!(err.0, "未知提示词用途");
    // Non-editable kinds always use the factory text.
    assert_eq!(
        ctx.db.test_read(|c| crate::data::prompts::effective(c, "analysis")).await.unwrap(),
        default("analysis")
    );
}

#[tokio::test]
async fn rollback_restores_as_a_new_version_and_bumps_rules() {
    let ctx = ctx();
    let first = current_id(&ctx, "classify").await;
    dispatch(&ctx, "prompt_save", json!({"target": "classify", "text": "新规则", "expected": first})).await.unwrap();
    let second = current_id(&ctx, "classify").await;
    let err = dispatch(&ctx, "prompt_rollback", json!({"id": first, "expected": first})).await.unwrap_err();
    assert_eq!(err.0, "提示词已更新，请刷新版本列表");
    let err = dispatch(&ctx, "prompt_rollback", json!({"id": second, "expected": second})).await.unwrap_err();
    assert_eq!(err.0, "当前已是此版本");
    assert_eq!(
        dispatch(&ctx, "prompt_rollback", json!({"id": "x", "expected": second})).await.unwrap_err().0,
        "版本不存在"
    );
    let out = dispatch(&ctx, "prompt_rollback", json!({"id": first, "expected": second})).await.unwrap();
    let v = current_id(&ctx, "classify").await;
    assert_eq!(out["version"], json!(v));
    assert_eq!(
        ctx.db.test_read(|c| crate::data::prompts::effective(c, "classify")).await.unwrap(),
        default("classify")
    );
    assert_eq!(ctx.db.test_read(|c| kv::get_i64(c, RULE_VERSION)).await.unwrap(), 1);
    let log = audits(&ctx, "prompt_rollback").await;
    assert_eq!(log.len(), 1);
    assert!(log[0].1.contains(&first));
}

fn pending(ctx: &Arc<Ctx>, id: &str) -> Review {
    let id = id.to_string();
    ctx.db
        .test_write_blocking(move |tx| {
            let r = Review {
                id,
                day: "2026-01-02".into(),
                status: "pending".into(),
                base_version: crate::data::prompts::current(tx, "classify")?.unwrap().id,
                base_prompt: crate::data::prompts::effective(tx, "classify")?,
                evidence: json!({"audit": [{"evidence_id": "a1"}], "logs": [{"evidence_id": "l1"}]}),
                ..Default::default()
            };
            crate::data::prompts::insert_review(tx, &r)?;
            Ok(r)
        })
        .unwrap()
}

#[tokio::test]
async fn retry_asks_prompt_review_and_a_failure_stores_nothing() {
    use crate::ai::provider::fake::{Fake, Step};
    let ctx = ctx();
    let shared = ctx.clone();
    exec::blocking(move || Ok(pending(&shared, "r1"))).await.unwrap();
    let fake = Fake::new(vec![
        Step::Fail("连接断开".into()),
        Step::Reply(
            json!({"improved": true, "reason": "更准", "proposed_prompt": "改进版", "evidence_ids": ["l1"]}),
            json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}),
        ),
    ]);
    ctx.ai.set_driver(Some(fake.clone()));
    assert!(dispatch(&ctx, "prompt_retry", json!({"id": "r1"})).await.is_err());
    let rows = ctx.db.test_read(crate::data::prompts::reviews).await.unwrap();
    assert_eq!((rows.len(), rows[0].archived_at), (1, 0));
    assert!(!ctx.prompts.busy.load(std::sync::atomic::Ordering::SeqCst));

    let out = dispatch(&ctx, "prompt_retry", json!({"id": "r1"})).await.unwrap();
    assert_eq!(out["status"], "ready", "{out}");
    let (kind, prompt) = fake.seen.lock().unwrap()[1].clone();
    assert_eq!(kind, "prompt_review");
    assert!(prompt.contains("2026-01-02") && prompt.contains("l1"), "{prompt}");
}

#[tokio::test]
async fn learn_asks_with_new_corrections_and_stores_pending_rules() {
    use crate::ai::provider::fake::{Fake, Step};
    let ctx = ctx();
    let usage = || json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15});
    let (a, b) = ctx
        .db
        .test_write(|tx| {
            crate::data::audit::write(tx, "ai_update", "t1", "自动更新", &json!(null), &json!(null))?;
            let a = crate::data::audit::write(
                tx,
                "manual_edit",
                "t1",
                "改截止时间",
                &json!({"due": ""}),
                &json!({"due": "周五"}),
            )?;
            let b = crate::data::audit::write(
                tx,
                "manual_edit",
                "t2",
                "改截止时间",
                &json!({"due": ""}),
                &json!({"due": "周一"}),
            )?;
            Ok((a, b))
        })
        .await
        .unwrap();
    let fake = Fake::new(vec![Step::Reply(
        json!({"suggestions": [
            {"suggestion_id": "", "title": "截止", "rule": "消息提到日期时写入截止时间", "evidence": [a, b]},
            {"suggestion_id": "", "title": "无据", "rule": "只凭一条", "evidence": [a]},
        ]}),
        usage(),
    )]);
    ctx.ai.set_driver(Some(fake.clone()));
    let out = dispatch(&ctx, "learn", json!({})).await.unwrap();
    assert_eq!(out, json!({"added": 1, "updated": 0, "message": learning::message(1, 0)}));
    let (kind, prompt) = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(kind, "learning");
    assert!(prompt.contains("改截止时间") && !prompt.contains("自动更新"), "{prompt}");
    let rules = ctx.db.test_read(crate::data::rules::list).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!((rules[0]["active"].as_bool(), rules[0]["decision"].as_str()), (Some(false), Some("pending")));
    assert_eq!(rules[0]["evidence"], json!([a, b]));
    let (cursor, pending): (i64, bool) = ctx
        .db
        .test_read(|c| {
            Ok((
                kv::get_i64(c, crate::archive::LEARNING_CURSOR)?,
                kv::get::<bool>(c, crate::data::tasks::PENDING_LEARNING)?.unwrap_or(true),
            ))
        })
        .await
        .unwrap();
    assert_eq!((cursor, pending), (b, false));
    let logged: String = ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT detail FROM activity WHERE kind = 'learning_result'", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&logged).unwrap(), json!({"added": 1, "updated": 0, "processed": 2}));

    // Nothing new: no call, the existing rule stays.
    assert_eq!(dispatch(&ctx, "learn", json!({})).await.unwrap(), json!({"skipped": learning::NOTHING_NEW}));
    assert_eq!(fake.calls(), 1);

    let mut config = Config::default();
    config.model_profiles.as_mut().unwrap().review = None;
    let ctx = Ctx::memory(config).unwrap();
    let e = dispatch(&ctx, "learn", json!({})).await.unwrap_err().0;
    assert!(e.starts_with(crate::ai::UNCONFIGURED_PREFIX), "{e}");
}

#[tokio::test]
async fn retry_validates_answers_and_adopt_needs_the_same_base() {
    let ctx = ctx();
    let shared = ctx.clone();
    exec::blocking(move || Ok(pending(&shared, "r1"))).await.unwrap();
    let invalid = [
        (json!({"reason": "r", "proposed_prompt": "p", "evidence_ids": []}), "缺少是否改进的判断"),
        (json!({"improved": true, "reason": " ", "proposed_prompt": "p", "evidence_ids": []}), "修改原因无效"),
        (json!({"improved": true, "reason": "r", "evidence_ids": []}), "缺少完整提示词"),
        (
            json!({"improved": true, "reason": "r", "proposed_prompt": "", "evidence_ids": ["a1"]}),
            "新提示词应为 1–20000 字",
        ),
        (json!({"improved": true, "reason": "r", "proposed_prompt": "p"}), "缺少日志依据"),
        (json!({"improved": true, "reason": "r", "proposed_prompt": "p", "evidence_ids": [1]}), "日志依据格式无效"),
        (
            json!({"improved": true, "reason": "r", "proposed_prompt": "p", "evidence_ids": ["zz"]}),
            "新提示词没有有效的当日日志依据",
        ),
    ];
    for (answer, msg) in invalid {
        let err = review::retry(&ctx, "r1", |_| async move { Ok(answer) }).await.unwrap_err();
        assert_eq!(err.0, msg);
    }
    assert_eq!(ctx.db.test_read(crate::data::prompts::reviews).await.unwrap().len(), 1, "failures store nothing");

    // The AI sees the current prompt and the stored evidence.
    let seen = Arc::new(std::sync::Mutex::new(Value::Null));
    let probe = seen.clone();
    let out = review::retry(&ctx, "r1", |input| async move {
        *probe.lock().unwrap() = input;
        Ok(json!({"improved": true, "reason": "更准", "proposed_prompt": "改进版", "evidence_ids": ["l1"]}))
    })
    .await
    .unwrap();
    assert_eq!(out["status"], "ready");
    assert_eq!(seen.lock().unwrap()["daily_evidence"]["audit"][0]["evidence_id"], "a1");
    assert_eq!(seen.lock().unwrap()["current_prompt"].as_str(), Some(default("classify")));
    let rows = ctx.db.test_read(crate::data::prompts::reviews).await.unwrap();
    assert!(rows[0].archived_at > 0 && rows[1].archived_at == 0);
    let err = review::retry(&ctx, "r1", |_| async { Ok(Value::Null) }).await.unwrap_err();
    assert_eq!(err.0, "建议已处理，请刷新后选择可重新评估的记录");

    let state = dispatch(&ctx, "state", json!({"page": "prompts"})).await.unwrap();
    assert_eq!(state["prompt_pending"], 1);
    let shown = &state["evolution"]["prompts"];
    assert_eq!(shown.as_array().unwrap().len(), 1);
    assert!(shown[0].get("evidence").is_none());
    let tasks_page = dispatch(&ctx, "state", json!({"page": "tasks"})).await.unwrap();
    assert!(tasks_page["evolution"].get("prompts").is_none());

    // Never adopted by itself; a changed base blocks the user's adoption.
    let new_id = rows[1].id.clone();
    assert_eq!(
        ctx.db.test_read(|c| crate::data::prompts::effective(c, "classify")).await.unwrap(),
        default("classify")
    );
    assert_eq!(dispatch(&ctx, "prompt_adopt", json!({"id": rows[0].id})).await.unwrap_err().0, "该建议尚不能采用");
    let base = current_id(&ctx, "classify").await;
    dispatch(&ctx, "prompt_save", json!({"target": "classify", "text": "别处改过", "expected": base})).await.unwrap();
    let err = dispatch(&ctx, "prompt_adopt", json!({"id": new_id})).await.unwrap_err();
    assert_eq!(err.0, "当前提示词已变化，请重新生成建议，避免覆盖新版本");
    let latest = current_id(&ctx, "classify").await;
    let id = latest.clone();
    ctx.db.rollback_prompt(base, id).await.unwrap();
    // A rollback restores the same text but a new version id; still blocked.
    assert!(dispatch(&ctx, "prompt_adopt", json!({"id": new_id})).await.is_err());
}

#[tokio::test]
async fn adopt_records_version_audit_and_rule_version() {
    let ctx = ctx();
    let shared = ctx.clone();
    exec::blocking(move || Ok(pending(&shared, "r1"))).await.unwrap();
    let out = review::retry(&ctx, "r1", |_| async {
        Ok(json!({"improved": true, "reason": "更准", "proposed_prompt": "改进版", "evidence_ids": ["a1"]}))
    })
    .await
    .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let adopted = dispatch(&ctx, "prompt_adopt", json!({"id": id})).await.unwrap();
    assert_eq!(adopted["version"], json!(current_id(&ctx, "classify").await));
    assert_eq!(ctx.db.test_read(|c| crate::data::prompts::effective(c, "classify")).await.unwrap(), "改进版");
    assert_eq!(ctx.db.test_read(|c| kv::get_i64(c, RULE_VERSION)).await.unwrap(), 1);
    assert_eq!(audits(&ctx, "prompt_adopt").await.len(), 1);
    assert_eq!(dispatch(&ctx, "prompt_adopt", json!({"id": id})).await.unwrap_err().0, "该建议尚不能采用");
    assert_eq!(dispatch(&ctx, "prompt_adopt", json!({"id": "x"})).await.unwrap_err().0, "建议不存在");
}

#[tokio::test]
async fn unchanged_answers_and_concurrent_changes() {
    let ctx = ctx();
    let shared = ctx.clone();
    exec::blocking(move || Ok(pending(&shared, "r1"))).await.unwrap();
    let out = review::retry(&ctx, "r1", |input| async move {
        Ok(json!({"improved": true, "reason": "无需改", "proposed_prompt": input["current_prompt"], "evidence_ids": ["a1"]}))
    })
    .await
    .unwrap();
    assert_eq!(out["status"], "unchanged");
    let id = out["id"].as_str().unwrap().to_string();
    // The prompt changes while the AI is thinking: nothing is stored.
    let racer = ctx.clone();
    let err = review::retry(&ctx, &id, |_| async move {
        let base = current_id(&racer, "classify").await;
        dispatch(&racer, "prompt_save", json!({"target": "classify", "text": "并发修改", "expected": base}))
            .await
            .unwrap();
        Ok(json!({"improved": false, "reason": "r", "proposed_prompt": "", "evidence_ids": []}))
    })
    .await
    .unwrap_err();
    assert_eq!(err.0, "建议或提示词版本已变化，本次结果未保存");
    assert_eq!(ctx.db.test_read(crate::data::prompts::reviews).await.unwrap().len(), 2);
    // A second retry while one runs is refused.
    let inner = ctx.clone();
    let id2 = id.clone();
    let err = review::retry(&ctx, &id, |_| async move {
        let e = review::retry(&inner, &id2, |_| async { Ok(Value::Null) }).await.unwrap_err();
        Err(e)
    })
    .await
    .unwrap_err();
    assert_eq!(err.0, "提示词建议正在生成");
}

#[tokio::test]
async fn memory_saves_are_optimistic_bounded_and_private() {
    let ctx = ctx();
    let m = dispatch(&ctx, "memory_read", json!({})).await.unwrap();
    assert_eq!((m["temporary"]["max_chars"].as_u64(), m["archived"]["max_chars"].as_u64()), (Some(2000), Some(4000)));
    let rev = m["temporary"]["revision"].as_u64().unwrap();
    for (p, msg) in [
        (json!({"expected": 0}), "请选择记忆类型"),
        (json!({"kind": "temporary"}), "缺少记忆校验值，请重新读取"),
        (json!({"kind": "x", "expected": 0}), "未知记忆类型"),
        (
            json!({"kind": "temporary", "expected": rev + 1, "text": "t"}),
            "临时记忆已更新，草稿已保留；请重新读取后合并",
        ),
        (json!({"kind": "temporary", "expected": rev}), "记忆正文无效"),
        (json!({"kind": "temporary", "expected": rev, "text": "字".repeat(2001)}), "临时记忆最多 2000 字符，未保存"),
        (json!({"kind": "archived", "expected": 0, "text": "字".repeat(4001)}), "归档记忆最多 4000 字符，未保存"),
    ] {
        assert_eq!(dispatch(&ctx, "memory_save", p).await.unwrap_err().0, msg);
    }
    let saved =
        dispatch(&ctx, "memory_save", json!({"kind": "temporary", "expected": rev, "text": "私密备注"})).await.unwrap();
    assert_eq!(saved["temporary"]["text"], "私密备注");
    assert_eq!(saved["temporary"]["revision"].as_u64(), Some(rev + 1));
    let log = audits(&ctx, "memory_manual").await;
    assert_eq!(log.len(), 1);
    assert!(!log[0].0.contains("私密") && !log[0].1.contains("私密"));
    // Analysis commits use the same revision check.
    let stale = ctx
        .db
        .test_write(move |tx| crate::data::prompts::replace_memory(tx, "temporary", rev as i64, "旧"))
        .await
        .unwrap();
    assert!(!stale);
    let state = dispatch(&ctx, "state", json!({"page": "tasks"})).await.unwrap();
    assert_eq!(state["memory_state"], json!([rev + 1, 0]));
    let count: i64 =
        ctx.db.test_read(|c| Ok(c.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(count, 2, "no history rows");
}

#[test]
fn learning_counts_only_user_corrections() {
    let none = Value::Null;
    let note = json!({"archive_note": "以后这样归档"});
    let blank = json!({"archive_note": " "});
    let e = |kind, detail, after| Evidence { kind, detail, after };
    assert!(learning::is_evidence(&e("manual_edit", "", &none)));
    assert!(learning::is_evidence(&e("merge", "", &none)));
    assert!(learning::is_evidence(&e("manual_status", "dismiss", &none)));
    assert!(!learning::is_evidence(&e("manual_status", "done", &none)));
    assert!(learning::is_evidence(&e("manual_archive", "", &note)));
    assert!(!learning::is_evidence(&e("manual_archive", "", &blank)));
    assert!(!learning::is_evidence(&e("ai_classify", "", &none)));
}

#[test]
fn learning_adds_pending_rules_and_never_enables_them() {
    let none = Value::Null;
    let note = json!({"archive_note": "备注"});
    let audit = |id: i64| match id {
        1 | 2 | 3 => Some(Evidence { kind: "manual_edit", detail: "", after: &none }),
        4 => Some(Evidence { kind: "manual_archive", detail: "", after: &note }),
        5 => Some(Evidence { kind: "ai_classify", detail: "", after: &none }),
        _ => None,
    };
    let mut list = vec![];
    let rows = vec![
        json!({"title": "规则A", "rule": "做 A", "evidence": [1, 2], "active": true, "decision": "enabled"}),
        json!({"title": "单条", "rule": "做 B", "evidence": [1]}),
        json!({"title": "备注", "rule": "做 C", "evidence": [4]}),
        json!({"title": "非纠错", "rule": "做 D", "evidence": [2, 5]}),
        json!({"title": "第五条", "rule": "做 E", "evidence": [1, 2]}),
    ];
    assert_eq!(learning::apply(&mut list, &rows, &[1, 2, 4, 5], &[], audit, 10), (2, 0));
    assert!(list.iter().all(|s| !s.active && s.decision() == "pending"));
    assert_eq!(list[0].evidence, [1, 2]);

    // A pending rule may grow; an enabled or archived one stays put.
    let update = vec![json!({"suggestion_id": list[0].id, "title": "规则A2", "rule": "做 A2", "evidence": [3]})];
    assert_eq!(learning::apply(&mut list, &update, &[3], &[], audit, 20), (0, 1));
    assert_eq!((list[0].evidence.clone(), list[0].updated), (vec![1, 2, 3], 20));
    list[1].active = true;
    list[1].decision.clear();
    let update = vec![json!({"suggestion_id": list[1].id, "title": "x", "rule": "y", "evidence": [3]})];
    assert_eq!(learning::apply(&mut list, &update, &[3], &[], audit, 30), (0, 0));
    // Evidence outside this batch, the history and the rule's own is refused.
    let foreign = vec![json!({"title": "外来", "rule": "做 F", "evidence": [3, 1]})];
    assert_eq!(learning::apply(&mut list, &foreign, &[3], &[], audit, 40), (0, 0));
    assert_eq!(learning::apply(&mut list, &foreign, &[3], &[1], audit, 40), (1, 0));
    // An unknown suggestion id never creates a rule.
    let ghost = vec![json!({"suggestion_id": "gone", "title": "t", "rule": "r", "evidence": [1, 2]})];
    assert_eq!(learning::apply(&mut list, &ghost, &[1, 2], &[], audit, 50), (0, 0));
    assert_eq!(learning::message(0, 0), "本次无新增建议，既有建议继续保留");
    assert_eq!(learning::message(1, 0), "建议已更新，未处理建议继续保留");
    let s = Suggestion { decision: "rejected".into(), ..Default::default() };
    assert_eq!(s.decision(), "rejected");
}

#[tokio::test]
async fn accuracy_counts_corrections_until_the_ai_touches_the_task_again() {
    let ctx = ctx();
    ctx.db
        .test_write(|tx| {
            let w = |kind, task, before: Value, after: Value| {
                crate::data::audit::write(tx, kind, task, "", &before, &after).map(drop)
            };
            let t = |title: &str, due: &str| json!({"title": title, "due": due, "status": "待处理"});
            // a: created, corrected twice (only the first counts), then updated and left alone.
            w("ai_create", "a", Value::Null, t("甲", ""))?;
            w("manual_edit", "a", t("甲", ""), t("甲改", "2026-11-06"))?;
            w("manual_edit", "a", t("甲改", "2026-11-06"), t("甲再改", "2026-11-06"))?;
            w("ai_update", "a", t("甲再改", "2026-11-06"), t("甲再改", "2026-11-07"))?;
            w("manual_status", "a", Value::Null, Value::Null)?;
            // b: created then merged by hand; c: created and kept.
            w("ai_create", "b", Value::Null, t("乙", ""))?;
            w("merge", "b", Value::Null, Value::Null)?;
            w("ai_create", "c", Value::Null, t("丙", ""))?;
            Ok(())
        })
        .await
        .unwrap();
    let r = dispatch(&ctx, "ai_accuracy", json!({"days": 7})).await.unwrap();
    assert_eq!(
        r,
        json!([
            {"kind": "ai_create", "total": 3, "corrected": 2, "fields": {"due": 1, "merge": 1, "title": 1}},
            {"kind": "ai_update", "total": 1, "corrected": 0, "fields": {}},
        ])
    );
}
