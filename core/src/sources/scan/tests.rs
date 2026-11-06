use rusqlite::params;

use super::super::testing::Fixture;
use super::*;
use crate::data::kv;
use crate::data::sources::reconcile_withdrawals;

fn rows(items: &[(&str, &str)]) -> Value {
    let at = now() - MINUTE;
    let messages: Vec<Value> = items
        .iter()
        .map(|(id, text)| json!({"id": id, "chat": "c", "time": at, "text": text, "private": true}))
        .collect();
    json!(messages)
}

#[tokio::test]
async fn scan_counts_rows_and_a_failed_source_keeps_its_progress() {
    let f = Fixture::new("scan-counts", false);
    let a = f.source("工作群", "a").await;
    let b = f.source("家庭群", "b").await;
    f.host.reply("fetch_incremental@a", Ok(json!({"messages": rows(&[("1", "x"), ("1", "x")]), "cursor": {"n": 1}})));
    f.host.reply("fetch_incremental@b", Err(Error::new("读取失败")));
    let result = f.call("scan_only", json!({})).await.unwrap();
    assert_eq!((result["rows"].clone(), result["added"].clone()), (json!(2), json!(1)));
    assert_eq!(result["warnings"], json!(["家庭群：读取失败"]));
    assert_eq!(result["analysis"], json!({"queued": true}));
    let sources = f.sources().await;
    let (sa, sb) = (sources.iter().find(|s| s.id == a.id).unwrap(), sources.iter().find(|s| s.id == b.id).unwrap());
    assert_eq!(sa.cursor, json!({"n": 1}));
    assert!(sa.last_scan > 0 && sa.last_error.is_empty());
    assert_eq!((sb.cursor.clone(), sb.last_scan, sb.last_error.as_str()), (Value::Null, 0, "读取失败"));
    let last = f.ctx.db.test_read(|c| kv::get::<String>(c, "last_error")).await.unwrap().unwrap();
    assert_eq!(last, "家庭群：读取失败");

    f.host
        .reply("fetch_incremental@a", Ok(json!({"messages": rows(&[("1", "改了"), ("2", "y")]), "cursor": {"n": 2}})));
    let result = f.call("scan_only", json!({})).await.unwrap();
    assert_eq!((result["added"].clone(), result["updated"].clone()), (json!(1), json!(1)));
    let revision = f.ctx.db.test_read(|c| kv::get_i64(c, messages::REVISION)).await.unwrap();
    assert_eq!(revision, 2);
    let last = f.ctx.db.test_read(|c| kv::get::<String>(c, "last_error")).await.unwrap().unwrap();
    assert_eq!(last, "", "a source that recovers clears the error");
}

#[tokio::test]
async fn scan_fails_without_sources_or_when_every_source_fails() {
    let f = Fixture::new("scan-fail", false);
    assert_eq!(f.call("scan_only", json!({})).await.unwrap_err().0, "请先配置并启用一个消息来源");
    f.source("工作群", "a").await;
    f.host.reply("fetch_incremental", Ok(json!({"messages": [], "cursor": {}, "until": 1})));
    assert_eq!(f.call("scan_only", json!({})).await.unwrap_err().0, "工作群：插件返回的增量检查点无效");
    f.host.reply("fetch_incremental", Ok(json!({"messages": []})));
    assert_eq!(f.call("scan_only", json!({})).await.unwrap_err().0, "工作群：插件未返回增量游标");
    let finished = f.ctx.db.test_read(|c| kv::get_i64(c, "last_scan_finished_at")).await.unwrap();
    assert!(finished > 0);
}

#[tokio::test]
async fn a_checkpoint_after_now_is_reset_and_that_source_skips_one_scan() {
    let f = Fixture::new("scan-clock", false);
    let a = f.source("工作群", "a").await;
    let ahead = now() + DAY;
    f.ctx
        .db
        .test_write(move |c| Ok(c.execute("UPDATE sources SET last_scan = ?1", [ahead]).map(|_| ())?))
        .await
        .unwrap();
    let result = f.call("scan_only", json!({})).await.unwrap();
    assert_eq!(result["rows"], json!(0));
    let source = f.sources().await.into_iter().find(|s| s.id == a.id).unwrap();
    assert!(source.last_scan <= now() && source.last_scan > now() - MINUTE);
    let logged = f
        .ctx
        .db
        .test_read(|c| {
            Ok(c.query_row("SELECT COUNT(*) FROM activity WHERE kind = 'scan_clock'", [], |r| r.get::<_, i64>(0))?)
        })
        .await
        .unwrap();
    assert_eq!(logged, 1);

    f.host.reply("fetch_incremental", Ok(json!({"messages": rows(&[("1", "x")]), "cursor": {}})));
    assert_eq!(f.call("scan_only", json!({})).await.unwrap()["added"], json!(1), "the next scan reads again");
}

#[tokio::test]
async fn deleted_and_expired_messages_are_not_stored_again() {
    let f = Fixture::new("scan-deleted", false);
    f.source("工作群", "a").await;
    f.host.reply("fetch_incremental", Ok(json!({"messages": rows(&[("1", "x")]), "cursor": {}})));
    f.call("scan_only", json!({})).await.unwrap();
    let id = f
        .ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT id FROM messages", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    f.call("messages_delete", json!({"ids": [id]})).await.unwrap();
    let old = json!([{"id": "9", "chat": "c", "time": now() - 400 * DAY, "text": "旧"}]);
    let mut fresh = rows(&[("1", "x")]);
    fresh.as_array_mut().unwrap().extend(old.as_array().unwrap().clone());
    f.host.reply("fetch_incremental", Ok(json!({"messages": fresh, "cursor": {}})));
    let result = f.call("scan_only", json!({})).await.unwrap();
    assert_eq!(result["added"], json!(0));
    let count = f
        .ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn tracked_messages_missing_from_the_increment_are_looked_up() {
    let f = Fixture::new("scan-lookup", false);
    f.source("工作群", "a").await;
    let image = json!([{"id": "7", "chat": "c", "time": now() - MINUTE, "kind": "image", "images": [{"path": ""}]}]);
    f.host.reply("fetch_incremental", Ok(json!({"messages": image, "cursor": {}})));
    f.call("scan_only", json!({})).await.unwrap();
    let path = if cfg!(windows) { "C:\\a.png" } else { "/a.png" };
    let found = json!([{"id": "7", "chat": "c", "time": now() - MINUTE, "kind": "image", "images": [{"path": path}]},
        {"id": "8", "chat": "c", "time": now() - MINUTE, "text": "未请求"}]);
    f.host.reply("lookup_messages", Ok(json!({"messages": found})));
    f.call("scan_only", json!({})).await.unwrap();
    let lookup = f.host.calls.lock().unwrap().iter().find(|c| c["method"] == "lookup_messages").cloned().unwrap();
    assert_eq!(lookup["params"]["ids"], json!(["7"]));
    let stored = f
        .ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT path FROM message_images", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    assert_eq!(stored, path);
    let count = f
        .ctx
        .db
        .test_read(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn changed_source_results_are_ignored() {
    let f = Fixture::new("scan-changed", false);
    let a = f.source("工作群", "a").await;
    let (host, root) = (f.host.clone(), f.ctx.root.clone());
    let read = a.clone();
    let fetched = fetch(&*host, &root, &read, vec![], now())
        .map(|(_, c, t)| (vec![messages::sample(&a.id, "1", now(), "x")], c, t));
    f.ctx
        .db
        .test_write(move |tx| {
            tx.execute("UPDATE sources SET updated = updated + 1 WHERE id = ?1", [&a.id])?;
            Ok(())
        })
        .await
        .unwrap();
    let outcome = f.ctx.db.commit_scan(vec![(read, fetched)], now(), 30).await.unwrap();
    assert_eq!(outcome.warnings, vec!["工作群配置已变化，本次结果已忽略".to_string()]);
    assert_eq!(outcome.counts["ignored"], json!(1));
    assert_eq!(outcome.successful, 0);
}

#[tokio::test]
async fn range_and_control_validate_their_input() {
    let f = Fixture::new("scan-range", false);
    assert_eq!(f.call("source_range", json!({"until": 1})).await.unwrap_err().0, "缺少开始时间");
    assert_eq!(f.call("source_range", json!({"since": 1})).await.unwrap_err().0, "缺少结束时间");
    let wide = json!({"id": "x", "since": now() - 40 * DAY, "until": now() - DAY});
    assert_eq!(f.call("source_range", wide).await.unwrap_err().0, "请选择过去31天以内的有效时间段（单次最多31天）");
    let missing = json!({"id": "x", "since": now() - 2 * DAY, "until": now() - DAY});
    assert_eq!(f.call("source_range", missing).await.unwrap_err().0, "来源不存在");

    let on = json!({"enabled": true});
    assert_eq!(f.call("scan_control", on.clone()).await.unwrap_err().0, "请先在消息来源中配置并启用一个插件");
    let a = f.source("工作群", "a").await;
    assert_eq!(f.call("scan_control", on.clone()).await.unwrap(), on);
    assert!(f.ctx.config.get().enabled);

    f.host.reply("fetch_range", Ok(json!({"messages": rows(&[("5", "补读")])})));
    let fake = crate::ai::provider::fake::Fake::reply(json!({"actions": [], "no_action_reason": "只是闲聊"}));
    f.ctx.ai.set_driver(Some(fake.clone()));
    let p = json!({"id": a.id, "since": now() - 2 * DAY, "until": now()});
    let result = f.call("source_range", p).await.unwrap();
    assert_eq!(result["rows"], json!(1));
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn withdrawals_cancel_or_reopen_tasks() {
    let f = Fixture::new("scan-withdraw", false);
    f.ctx
        .db
        .test_write(|tx| {
            let mut gone = messages::sample("s", "1", now(), "x");
            gone.withdrawn = true;
            crate::data::messages::upsert(tx, &gone)?;
            crate::data::messages::upsert(tx, &messages::sample("s", "2", now(), "y"))?;
            for (id, number, status, manual) in [("auto", 1, "待处理", 0), ("hand", 2, "待处理", 1), ("done", 3, "已完成", 0)] {
                tx.execute(
                    "INSERT INTO tasks (id, number, title, status, manual, status_changed_at, created, updated) VALUES (?1,?2,'t',?3,?4,0,0,0)",
                    params![id, number, status, manual],
                )?;
                tx.execute("INSERT INTO task_evidence (task_id, message_id, role) VALUES (?1,?2,'direct')", params![id, gone.id])?;
            }
            tx.execute(
                "INSERT INTO task_evidence (task_id, message_id, role) VALUES ('done',?1,'direct')",
                [messages::key("s", "c", "2")],
            )?;
            reconcile_withdrawals(tx)?;
            let row = |id: &str| -> Result<(String, String)> {
                Ok(tx.query_row("SELECT status, reason FROM tasks WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?)
            };
            assert_eq!(row("auto")?, ("已取消".into(), "唯一消息来源已撤回".into()));
            assert_eq!(row("hand")?, ("待处理".into(), "消息来源已撤回；保留人工事项，请确认".into()));
            assert_eq!(row("done")?, ("待处理".into(), "来源发生撤回，重新核对完成依据".into()));
            Ok(())
        })
        .await
        .unwrap();
}
