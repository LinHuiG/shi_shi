use super::*;
use crate::{Config, Ctx};

async fn seed(ctx: &Ctx, rows: &'static [(&'static str, &'static str)]) {
    ctx.db
        .test_write(move |tx| {
            for (kind, detail) in rows {
                crate::data::activity::log(tx, kind, &serde_json::from_str(detail).unwrap())?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn timeline_filters_hides_finished_starts_and_pages_by_cursor() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    seed(
        &ctx,
        &[
            ("ai_start", r#"{"request_id":"a","kind":"classify"}"#),
            ("ai_start", r#"{"request_id":"b","kind":"report"}"#),
            ("ai_success", r#"{"request_id":"a","kind":"classify"}"#),
            ("status", r#"{}"#),
            ("error", r#"{"message":"x"}"#),
            ("tool_call_finished", r#"{"status":"timed_out"}"#),
        ],
    )
    .await;
    let q = |p: Value| query(&ctx, p);
    let ai = q(json!({})).await.unwrap();
    let kinds: Vec<_> = ai["rows"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(
        kinds,
        ["tool_call_finished", "ai_success", "ai_start"],
        "finished start a is hidden; status is not runtime"
    );
    assert_eq!(q(json!({"purpose": "report"})).await.unwrap()["total"], 1);
    assert_eq!(q(json!({"filter": "errors"})).await.unwrap()["total"], 2);
    assert_eq!(q(json!({"filter": "all"})).await.unwrap()["total"], 4);
    assert_eq!(q(json!({"filter": "all", "query": "TIMED_OUT"})).await.unwrap()["total"], 1);
    assert_eq!(q(json!({"filter": "all", "page": 9, "page_size": 3})).await.unwrap()["page"], 2);
    assert_eq!(q(json!({"filter": "nope"})).await.unwrap_err().0, "日志筛选无效");
    assert_eq!(q(json!({"purpose": "nope"})).await.unwrap_err().0, "AI 用途筛选无效");
    assert_eq!(q(json!({"day": "2026-2-1"})).await.unwrap_err().0, "日志日期无效");

    let first = q(json!({"filter": "all", "page_size": 2})).await.unwrap();
    assert_eq!((first["pages"].clone(), first["rows"].as_array().unwrap().len()), (json!(2), 2));
    let mut seen = first["rows"].as_array().unwrap().clone();
    let mut cursor = first["next_cursor"].clone();
    while !cursor.is_null() {
        let next = q(json!({"filter": "all", "page_size": 2, "cursor": cursor})).await.unwrap();
        seen.extend(next["rows"].as_array().unwrap().iter().cloned());
        cursor = next["next_cursor"].clone();
    }
    assert_eq!(seen.len(), 4);
    assert!(seen.windows(2).all(
        |w| (w[0]["time"].as_i64(), w[0]["position"].as_str()) > (w[1]["time"].as_i64(), w[1]["position"].as_str())
    ));
}

#[tokio::test]
async fn audit_rows_join_the_timeline_with_task_labels() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    ctx.db
        .test_write(|tx| {
            crate::data::audit::write(tx, "settings", "", "修改设置", &json!({}), &json!({"a": 1}))?;
            Ok(())
        })
        .await
        .unwrap();
    let before = day_revision(&ctx, &time::today()).await;
    let changes = query(&ctx, json!({"filter": "changes"})).await.unwrap();
    assert_eq!(changes["total"], 1);
    let row = &changes["rows"][0];
    assert_eq!(row["audit"], true);
    assert!(row["position"].as_str().unwrap().starts_with("1:"));
    assert_eq!(changes["revision"], before);

    let id = ctx
        .db
        .test_write(|tx| {
            let v = crate::data::tasks::edit(tx, &json!({"task": {"title": "日志事项", "status": "待处理"}}))?;
            Ok(v["id"].as_str().unwrap().to_string())
        })
        .await
        .unwrap();
    let changes = query(&ctx, json!({"filter": "changes"})).await.unwrap();
    let row = changes["rows"].as_array().unwrap().iter().find(|r| r["task_id"] == id).unwrap().clone();
    assert_eq!((row["task_title"].as_str(), row["task_label"].as_str()), (Some("日志事项"), Some("#1")));
}
