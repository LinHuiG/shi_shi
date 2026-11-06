use rusqlite::Connection;

use super::*;
use crate::data::messages::{allowed, delete, get, prune, query, upsert, Change};
use crate::data::retention;

fn conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::data::schema::migrate(&mut conn).unwrap();
    conn
}

#[test]
fn identity_is_scoped_by_source_chat_and_external_id() {
    assert_ne!(key("a:1", "b", "c"), key("a", "1:b", "c"));
    assert_ne!(key("s", "c1", "x"), key("s", "c2", "x"));
}

#[test]
fn upsert_keeps_inserted_at_and_moves_updated_at_only_on_change() {
    let conn = conn();
    let now = time::now();
    let mut m = sample("s", "1", now, "你好");
    m.images = ["b", "a"].map(|i| ImageRef { id: i.into(), path: format!("/{i}.jpg") }).to_vec();
    assert_eq!(upsert(&conn, &m).unwrap(), Change::Added);
    let first = get(&conn, &m.id).unwrap().unwrap();
    assert_eq!(first.images, m.images);
    assert_eq!(upsert(&conn, &m).unwrap(), Change::Unchanged);
    m.sender = "李四".into();
    assert_eq!(upsert(&conn, &m).unwrap(), Change::Unchanged);
    assert_eq!(get(&conn, &m.id).unwrap().unwrap().updated_at, first.updated_at);
    m.text = "改了".into();
    assert_eq!(upsert(&conn, &m).unwrap(), Change::Updated);
    let second = get(&conn, &m.id).unwrap().unwrap();
    assert_eq!(second.inserted_at, first.inserted_at);
    assert!(second.updated_at > first.updated_at);
    assert_eq!(second.sender, "李四");
}

#[test]
fn query_filters_searches_case_insensitively_and_pages_by_cursor() {
    let conn = conn();
    let now = time::now();
    for i in 0..25 {
        upsert(&conn, &sample("s", &i.to_string(), now - i, if i == 3 { "Ärger REPORT" } else { "x" })).unwrap();
    }
    upsert(&conn, &sample("s", "old", now - 200 * time::DAY, "old")).unwrap();
    let first = query(&conn, &json!({"page_size": 20}), 90).unwrap();
    assert_eq!(first["total"], 25);
    assert_eq!(first["rows"].as_array().unwrap().len(), 20);
    let next = query(&conn, &json!({"page_size": 20, "cursor": first["next_cursor"]}), 90).unwrap();
    assert_eq!(next["rows"].as_array().unwrap().len(), 5);
    assert_eq!(next["page"], 2);
    let hit = query(&conn, &json!({"query": "ärger report"}), 90).unwrap();
    assert_eq!(hit["total"], 1);
    for (p, e) in [
        (json!({"page_size": 30}), "分页参数无效"),
        (json!({"since": 5, "until": 1}), "开始时间应早于结束时间"),
        (json!({"since": -1}), "无效的since"),
        (json!({"sort": "x"}), "时间排序参数无效"),
        (json!({"query": "长".repeat(201)}), "搜索词最多200字"),
    ] {
        assert_eq!(query(&conn, &p, 90).unwrap_err().0, e);
    }
}

/// The task record dialog's two feeds through dispatch, with the exact
/// params task-history.js and page-feed.js send.
#[test]
fn task_record_feeds_page_with_ui_params() {
    let rt = crate::Runtime::start(crate::Ctx::memory(crate::Config::default()).unwrap(), false).unwrap();
    let task = rt.call("edit", json!({"task": {"title": "核对合同", "status": "待处理"}})).unwrap();
    let (id, mut updated) = (task["id"].as_str().unwrap().to_string(), task["updated"].clone());
    let base = time::now() - time::DAY;
    let messages: Vec<Message> = (0..60)
        .map(|i| sample("s", &i.to_string(), base + i * 1000, if i % 10 == 0 { "合同 Alpha" } else { "普通" }))
        .collect();
    let task_id = id.clone();
    rt.ctx
        .db
        .test_write_blocking(move |tx| {
            for (i, m) in messages.iter().enumerate() {
                upsert(tx, m)?;
                tx.execute(
                    "INSERT INTO task_evidence(task_id, message_id, role, position) VALUES (?1, ?2, 'direct', ?3)",
                    rusqlite::params![task_id, m.id, i as i64],
                )?;
            }
            Ok(())
        })
        .unwrap();
    for i in 1..=60 {
        let t =
            rt.call("edit", json!({"task": {"id": id, "updated": updated, "title": format!("核对合同{i}")}})).unwrap();
        updated = t["updated"].clone();
    }
    // loadTaskRecord: {[task_id|id], query, sort, since, until}, then
    // pageFeed adds {page, page_size: 50, cursor?} until next_cursor is null.
    let feed = |method: &str, key: &str, view: Value| {
        let mut params = json!({ key: id, "query": "", "sort": "desc", "since": null, "until": null });
        params.as_object_mut().unwrap().extend(view.as_object().unwrap().clone());
        let (mut rows, mut cursor, mut page) = (Vec::new(), Value::Null, 1);
        loop {
            let mut args = params.clone();
            args["page"] = json!(page);
            args["page_size"] = json!(50);
            if !cursor.is_null() {
                args["cursor"] = cursor;
            }
            let r = rt.call(method, args).unwrap();
            assert_eq!(r["page"], page);
            rows.extend(r["rows"].as_array().unwrap().iter().cloned());
            cursor = r["next_cursor"].clone();
            if cursor.is_null() {
                return (rows, r["total"].as_i64().unwrap(), r["all"].as_i64().unwrap());
            }
            page += 1;
        }
    };
    let times = |rows: &[Value]| rows.iter().map(|r| r["time"].as_i64().unwrap()).collect::<Vec<_>>();

    let (rows, total, all) = feed("evidence_query", "task_id", json!({}));
    assert_eq!((rows.len(), total, all), (60, 60, 60));
    assert_eq!(rows.iter().map(|r| r["id"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>().len(), 60);
    assert!(times(&rows).windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(rows[0]["evidence_role"], "direct");
    let (rows, ..) = feed("evidence_query", "task_id", json!({"sort": "asc"}));
    assert_eq!(rows[0]["time"], base);
    assert_eq!(feed("evidence_query", "task_id", json!({"query": "alpha 合同"})).1, 6);
    let range = json!({"since": base + 10_000, "until": base + 19_000});
    let (rows, total, all) = feed("evidence_query", "task_id", range.clone());
    assert_eq!((rows.len(), total, all), (10, 10, 60));
    // The key the dialog used to send is not the one the backend reads.
    let old = rt.call("evidence_query", json!({"id": id, "page": 1, "page_size": 50})).unwrap_err();
    assert_eq!(old.0, "缺少事项编号");

    let (rows, total, all) = feed("task_changes", "id", json!({}));
    assert_eq!((rows.len(), total, all), (61, 61, 61));
    assert_eq!(rows[0]["after"]["title"], "核对合同60");
    assert!(rows.iter().all(|r| r["id"].is_string() && r["kind_label"].is_string()));
    let (rows, ..) = feed("task_changes", "id", json!({"sort": "asc"}));
    assert_eq!(rows[60]["after"]["title"], "核对合同60");
    assert_eq!(feed("task_changes", "id", json!({"query": "核对合同59"})).1, 2);
    let (rows, ..) = feed("task_changes", "id", json!({"since": 0, "until": 1}));
    assert!(rows.is_empty());
}

#[test]
fn deletion_leaves_a_tombstone_and_retention_keeps_referenced_messages() {
    let conn = conn();
    let now = time::now();
    let keep = sample("s", "keep", now - 200 * time::DAY, "引用");
    let gone = sample("s", "gone", now - 200 * time::DAY, "过期");
    let fresh = sample("s", "fresh", now, "新");
    for m in [&keep, &gone, &fresh] {
        upsert(&conn, m).unwrap();
    }
    let mut cited = crate::tasks::model::Task {
        id: "t".into(),
        title: "引用".into(),
        status: "待处理".into(),
        ..Default::default()
    };
    cited.evidence.direct.push(keep.id.clone());
    crate::data::tasks::save(&conn, &cited).unwrap();
    assert_eq!(delete(&conn, &json!({"ids": [fresh.id, "missing"]})).unwrap()["count"], 1);
    assert!(!allowed(&conn, &fresh.id, now, cutoff(90)).unwrap());
    assert!(allowed(&conn, &keep.id, keep.time, cutoff(90)).unwrap());
    assert!(!allowed(&conn, &gone.id, gone.time, cutoff(90)).unwrap());
    assert!(prune(&conn, 90).unwrap());
    assert!(get(&conn, &gone.id).unwrap().is_none());
    assert!(get(&conn, &keep.id).unwrap().is_some());
    assert_eq!(retention::load(&conn).unwrap()["messages"]["count"], 1);
    assert!(!prune(&conn, 90).unwrap());
    assert_eq!(delete(&conn, &json!({"ids": []})).unwrap_err().0, "每次请选择1–1000条消息");
    assert_eq!(delete(&conn, &json!({})).unwrap_err().0, "请选择消息");
    assert_eq!(delete(&conn, &json!({"ids": [1]})).unwrap_err().0, "无效消息编号");
}
