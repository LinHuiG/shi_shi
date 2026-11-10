//! Offline tests of the task domain on synthetic data.

use rusqlite::Connection;
use serde_json::{json, Value};

use super::model::{self, Task};
use super::tags::{self, Tag};
use crate::data::kv;
use crate::data::{rules, schedules as schedule, task_query as query, tasks as edit};
use crate::{messages, time, Config, Ctx, Runtime};

fn conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::data::schema::migrate(&mut conn).unwrap();
    conn
}

fn create(conn: &Connection, task: Value) -> Task {
    let mut task = task;
    task["status"] = task.get("status").cloned().unwrap_or(json!("待处理"));
    let v = crate::data::tasks::edit(conn, &json!({ "task": task })).unwrap();
    crate::data::tasks::get(conn, v["id"].as_str().unwrap()).unwrap().unwrap()
}

fn get(conn: &Connection, id: &str) -> Task {
    crate::data::tasks::get(conn, id).unwrap().unwrap()
}

fn token(conn: &Connection, kind: &str, task_id: &str) -> String {
    conn.query_row(
        "SELECT token FROM task_audit WHERE kind = ?1 AND task_id = ?2 ORDER BY seq DESC LIMIT 1",
        [kind, task_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn err(r: crate::Result<Value>) -> String {
    r.unwrap_err().0
}

#[test]
fn edit_patches_submitted_fields_and_rejects_stale_versions() {
    let c = conn();
    let t = create(&c, json!({"title": "核对报表", "description": "月底前", "blocker": "等数据"}));
    assert_eq!(t.number, 1);
    assert!(t.manual);
    let second = create(&c, json!({"title": "第二件"}));
    assert_eq!(second.number, 2);

    crate::data::tasks::edit(&c, &json!({"task": {"id": t.id, "updated": t.updated, "title": "核对月报"}})).unwrap();
    let edited = get(&c, &t.id);
    assert_eq!((edited.title.as_str(), edited.blocker.as_str()), ("核对月报", "等数据"));
    assert!(edited.updated > t.updated);

    let stale = json!({"task": {"id": t.id, "updated": t.updated, "title": "旧版本"}});
    assert_eq!(err(crate::data::tasks::edit(&c, &stale)), "事项已被更新，请刷新后重新编辑");
    let empty = json!({"task": {"id": t.id, "updated": edited.updated, "title": " "}});
    assert_eq!(err(crate::data::tasks::edit(&c, &empty)), "标题不能为空");
    let due = json!({"task": {"id": t.id, "updated": edited.updated, "due": "明天"}});
    assert_eq!(err(crate::data::tasks::edit(&c, &due)), "截止时间需要完整日期和时间");
    assert_eq!(get(&c, &t.id).title, "核对月报");
}

#[test]
fn action_requires_version_and_deferral_resumes_processing() {
    let c = conn();
    let t = create(&c, json!({"title": "跟进合同"}));
    let no_version = json!({"id": t.id, "action": "start"});
    assert_eq!(err(edit::action(&c, &no_version, "08:00")), "事项已更新，请刷新后重试");
    edit::action(&c, &json!({"id": t.id, "action": "start", "updated": t.updated}), "08:00").unwrap();
    let t = get(&c, &t.id);
    assert_eq!(t.status, "处理中");

    edit::action(&c, &json!({"id": t.id, "action": "defer", "choice": "hour", "updated": t.updated}), "08:00").unwrap();
    let t = get(&c, &t.id);
    assert_eq!(t.status, "待处理");
    assert!(t.resume_processing && t.deferred_until > time::now());
    assert_eq!(edit::next_resume(&c).unwrap(), Some(t.deferred_until));
    assert_eq!(edit::resume_deferred(&c, t.deferred_until - 1).unwrap(), 0);
    assert_eq!(edit::resume_deferred(&c, t.deferred_until).unwrap(), 1);
    let t = get(&c, &t.id);
    assert_eq!((t.status.as_str(), t.deferred_until, t.resume_processing), ("处理中", 0, false));
    assert_eq!(edit::next_resume(&c).unwrap(), None);

    let bad = json!({"id": t.id, "action": "fly", "updated": t.updated});
    assert_eq!(err(edit::action(&c, &bad, "08:00")), "未知操作");
}

#[test]
fn batch_checks_every_item_before_changing_any() {
    let c = conn();
    let a = create(&c, json!({"title": "甲"}));
    let b = create(&c, json!({"title": "乙"}));
    let items = json!([{"id": a.id, "updated": a.updated}, {"id": b.id, "updated": b.updated - 1}]);
    let e = err(edit::batch(&c, &json!({"action": "done", "items": items}), "08:00"));
    assert_eq!(e, "乙：事项已更新，请刷新后重试，本批未修改");
    assert_eq!(get(&c, &a.id).status, "待处理");

    let dup = json!([{"id": a.id, "updated": a.updated}, {"id": a.id, "updated": a.updated}]);
    assert_eq!(err(edit::batch(&c, &json!({"action": "done", "items": dup}), "08:00")), "不能重复选择同一事项");
    assert_eq!(err(edit::batch(&c, &json!({"action": "done", "items": []}), "08:00")), "请先选择事项");
    assert_eq!(err(edit::batch(&c, &json!({"action": "fly", "items": [{}]}), "08:00")), "未知批量操作");

    let items = json!([{"id": a.id, "updated": a.updated}, {"id": b.id, "updated": b.updated}]);
    let e = err(edit::batch(&c, &json!({"action": "reopen", "items": items}), "08:00"));
    assert_eq!(e, "甲：当前状态不支持此操作，本批未修改");
    assert_eq!(err(edit::batch(&c, &json!({"action": "archive", "items": items}), "08:00")), "请选择本批归档结果");
    let r = edit::batch(&c, &json!({"action": "archive", "status": "已完成", "items": items}), "08:00").unwrap();
    assert_eq!(r["updated"], 2);
    assert!(get(&c, &a.id).archived && get(&c, &b.id).archived);
    assert_eq!(get(&c, &b.id).status, "已完成");
}

#[test]
fn merge_combines_evidence_and_undo_restores_both_tasks() {
    let c = conn();
    let at = time::now();
    let m1 = messages::sample("s", "1", at, "第一条");
    let mut m2 = messages::sample("s", "2", at, "第二条");
    crate::data::messages::upsert(&c, &m1).unwrap();
    crate::data::messages::upsert(&c, &m2).unwrap();
    let tag = |v: &str| json!({"kind": "事项类型", "value": v});
    let a = create(&c, json!({"title": "主事项", "evidence": {"direct": [m1.id]}, "tags": [tag("问题排查")]}));
    let b = create(
        &c,
        json!({"title": "重复事项", "description": "细节", "evidence": {"direct": [m2.id]}, "tags": [tag("日常事务")]}),
    );

    let stale = json!({"ids": [a.id, b.id], "versions": {a.id.clone(): a.updated, b.id.clone(): b.updated - 1}});
    assert_eq!(err(edit::merge(&c, &stale)), "所选事项已有修改，请重新打开合并窗口核对");
    assert_eq!(err(edit::merge(&c, &json!({"ids": [a.id, a.id]}))), "请选择至少两个不同事项");
    let versions = json!({a.id.clone(): a.updated, b.id.clone(): b.updated});
    edit::merge(&c, &json!({"ids": [a.id, b.id], "versions": versions})).unwrap();

    let main = get(&c, &a.id);
    let other = get(&c, &b.id);
    assert_eq!(other.merged_into, a.id);
    assert_eq!(main.evidence.all().len(), 2);
    assert_eq!(main.tags.len(), 2);
    assert!(main.description.contains("合并事项：重复事项"));
    let evidence = |c: &Connection| {
        let rows = query::snapshot_rows(c, &crate::state::Scope::from(&json!({"selected_task": main.id}))).unwrap();
        assert_eq!(rows.len(), 1);
        let listed = query::active(c, &json!({})).unwrap();
        let row = listed["rows"].as_array().unwrap().iter().find(|r| r["id"] == main.id).unwrap().clone();
        assert_eq!(row["evidence_count"], rows[0]["evidence_count"]);
        rows[0]["evidence_count"].clone()
    };
    assert_eq!(evidence(&c), 2);
    m2.withdrawn = true;
    crate::data::messages::upsert(&c, &m2).unwrap();
    assert_eq!(evidence(&c), 1);
    assert_eq!(err(edit::merge(&c, &json!({"ids": [a.id, b.id]}))), "归档、已合并或正在生成的事项不能合并");

    let merge = token(&c, "merge", &a.id);
    edit::undo(&c, &json!({"audit_id": merge})).unwrap();
    let main = get(&c, &a.id);
    let other = get(&c, &b.id);
    assert!(other.merged_into.is_empty());
    assert_eq!(main.evidence.direct, vec![m1.id.clone()]);
    assert_eq!(main.tags, vec![Tag { kind: "事项类型".into(), value: "问题排查".into() }]);
    assert_eq!(main.description, "");
    assert_eq!(err(edit::undo(&c, &json!({"audit_id": merge}))), "事项此后已有修改，不能直接撤销，请手动编辑");
}

#[test]
fn undo_round_trips_edits_and_creations() {
    let c = conn();
    let t = create(&c, json!({"title": "原标题"}));
    crate::data::tasks::edit(&c, &json!({"task": {"id": t.id, "updated": t.updated, "title": "新标题"}})).unwrap();
    let edit_token = token(&c, "manual_edit", &t.id);
    edit::undo(&c, &json!({"audit_id": edit_token})).unwrap();
    assert_eq!(get(&c, &t.id).title, "原标题");
    // The undo is itself recorded and the original row can no longer apply.
    assert_eq!(token(&c, "undo", &t.id).len(), 36);
    assert!(edit::undo(&c, &json!({"audit_id": edit_token})).is_err());

    let created = create(&c, json!({"title": "误建"}));
    let create_token = token(&c, "manual_edit", &created.id);
    edit::undo(&c, &json!({"audit_id": create_token})).unwrap();
    assert_eq!(get(&c, &created.id).status, "无需处理");

    let t = get(&c, &t.id);
    edit::archive(&c, &json!({"id": t.id, "updated": t.updated, "status": "已完成", "note": "已交付"})).unwrap();
    assert!(kv::get::<bool>(&c, crate::data::tasks::PENDING_LEARNING).unwrap().unwrap());
    let archive_token = token(&c, "manual_archive", &t.id);
    assert_eq!(err(edit::undo(&c, &json!({"audit_id": archive_token}))), "此记录不支持撤销");
    assert_eq!(err(edit::undo(&c, &json!({}))), "缺少记录索引");
    assert_eq!(err(edit::undo(&c, &json!({"audit_id": "x"}))), "记录不存在或已清理");
}

#[test]
fn change_history_pages_sorts_and_searches() {
    let c = conn();
    let mut t = create(&c, json!({"title": "标题0"}));
    for i in 1..=60 {
        crate::data::tasks::edit(&c, &json!({"task": {"id": t.id, "updated": t.updated, "title": format!("标题{i}")}}))
            .unwrap();
        t = get(&c, &t.id);
    }
    let page = |extra: Value| {
        let mut p = json!({"id": t.id});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        query::changes(&c, &p).unwrap()
    };
    let first = page(json!({}));
    assert_eq!(
        (first["total"].as_u64(), first["all"].as_i64(), first["pages"].as_u64()),
        (Some(61), Some(61), Some(2))
    );
    assert_eq!(first["rows"].as_array().unwrap().len(), 50);
    assert_eq!(first["rows"][0]["after"]["title"], "标题60");
    assert_eq!(first["rows"][0]["kind_label"], "手动编辑事项");
    let second = page(json!({"page": 2}));
    assert_eq!(second["rows"].as_array().unwrap().len(), 11);
    assert_eq!(second["rows"][10]["after"]["title"], "标题0");
    let ascending = page(json!({"sort": "asc"}));
    assert_eq!(ascending["rows"][0]["after"]["title"], "标题0");

    let found = page(json!({"query": "标题59"}));
    assert_eq!((found["total"].as_u64(), found["all"].as_i64()), (Some(2), Some(61)));
    // The task title is searchable on every row; "#1" is the task label.
    assert_eq!(page(json!({"query": "#1 标题60"}))["total"], 61);
    assert_eq!(page(json!({"query": "#1 标题0"}))["total"], 2);
    assert_eq!(page(json!({"query": "#2"}))["total"], 0);
    assert_eq!(err(query::changes(&c, &json!({"id": t.id, "sort": "up"}))), "时间排序无效");
    assert_eq!(err(query::changes(&c, &json!({"id": t.id, "since": 5, "until": 1}))), "开始时间不能晚于结束时间");
    assert_eq!(err(query::changes(&c, &json!({"id": "x"}))), "事项不存在");
}

#[test]
fn schedules_generate_once_per_day() {
    let c = conn();
    let s = schedule::edit(
        &c,
        &json!({"schedule": {"title": "日报", "frequency": "daily", "time": "00:00", "enabled": true,
            "tags": [{"kind": "事项类型", "value": "日常事务"}]}}),
    )
    .unwrap();
    let now = time::now();
    assert_eq!(schedule::generate(&c, now).unwrap(), 0);
    let at = super::schedule::tomorrow_start(now) + time::HOUR;
    assert!(schedule::has_due(&c, at).unwrap());
    assert_eq!(schedule::generate(&c, at).unwrap(), 1);
    assert_eq!(schedule::generate(&c, at).unwrap(), 0);
    assert!(!schedule::has_due(&c, at).unwrap());
    // Even with the day marker lost, the per-day index keeps one task.
    c.execute("UPDATE schedules SET last_generated_on = ''", []).unwrap();
    assert_eq!(schedule::generate(&c, at).unwrap(), 0);
    let tasks = crate::data::tasks::list(&c, "schedule_id <> ''").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "日报");
    assert_eq!(tasks[0].tags.len(), 1);
    assert_eq!(tasks[0].scheduled_for, time::stamp(at)[..10]);

    let stale =
        json!({"schedule": {"id": s["id"], "updated": 1, "title": "日报", "frequency": "daily", "time": "08:00"}});
    assert_eq!(err(schedule::edit(&c, &stale)), "定时事项已更新，请重新打开编辑");
    assert_eq!(err(schedule::delete(&c, &json!({"id": s["id"], "updated": 1}))), "定时事项已更新，请刷新后重试");
    schedule::delete(&c, &json!({"id": s["id"], "updated": s["updated"]})).unwrap();
    assert!(schedule::list(&c).unwrap().is_empty());
    assert_eq!(crate::data::tasks::list(&c, "1").unwrap().len(), 1);
}

#[test]
fn tag_proposals_versions_and_filters() {
    let c = conn();
    let expected = || crate::data::tags::versions(&c).unwrap().last().unwrap().id.clone();
    let initial = expected();
    let tag = Tag { kind: "客户".into(), value: "甲方".into() };
    crate::data::tags::propose(&c, &tag, "多次出现", &[("m1".into(), "甲方来电".into())]).unwrap();
    crate::data::tags::propose(&c, &tag, "多次出现", &[("m2".into(), "甲方邮件".into())]).unwrap();
    let state = crate::data::tags::snapshot(&c).unwrap();
    let proposals = state["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0]["evidence"], json!(["m1", "m2"]));
    let id = proposals[0]["id"].as_str().unwrap().to_string();

    let stale = json!({"expected": "old", "action": "adopt", "id": id});
    assert_eq!(err(crate::data::tags::tag_decide(&c, &stale)), "标签版本已变化，请刷新后重试");
    crate::data::tags::tag_decide(&c, &json!({"expected": initial, "action": "adopt", "id": id})).unwrap();
    let (catalog, kind) = crate::data::tags::catalog(&c).unwrap();
    assert_eq!(kind, "事项等级");
    assert!(tags::known(&catalog, "客户", "甲方"));
    assert_eq!(
        err(crate::data::tags::tag_decide(&c, &json!({"expected": expected(), "action": "adopt", "id": id}))),
        "此建议已处理"
    );

    crate::data::tags::tag_filters(&c, &json!({"filters": {"客户": ["甲方", "甲方"]}, "priorities": ["紧急"]}))
        .unwrap();
    assert_eq!(crate::data::tags::snapshot(&c).unwrap()["filters"], json!({"客户": ["甲方"]}));
    assert_eq!(
        err(crate::data::tags::tag_filters(&c, &json!({"filters": {}, "priorities": ["不存在"]}))),
        "事项等级筛选无效"
    );

    let priority = json!({"expected": expected(), "action": "display_priority", "name": "客户", "display_priority": 5});
    crate::data::tags::tag_decide(&c, &priority).unwrap();
    let rollback = json!({"expected": expected(), "action": "rollback", "id": initial});
    crate::data::tags::tag_decide(&c, &rollback).unwrap();
    let (catalog, _) = crate::data::tags::catalog(&c).unwrap();
    assert!(!tags::known(&catalog, "客户", "甲方"));
    assert_eq!(crate::data::tags::snapshot(&c).unwrap()["filters"], json!({}));
    assert_eq!(crate::data::tags::versions(&c).unwrap().len(), 4);

    crate::data::tags::propose(&c, &Tag { kind: "客户".into(), value: "乙方".into() }, "新客户", &[]).unwrap();
    let id = crate::data::tags::snapshot(&c).unwrap()["proposals"][1]["id"].as_str().unwrap().to_string();
    crate::data::tags::tag_decide(&c, &json!({"expected": expected(), "action": "reject", "id": id})).unwrap();
    assert_eq!(crate::data::tags::snapshot(&c).unwrap()["proposals"][1]["status"], "rejected");
}

fn add_rule(c: &Connection, id: &str, evidence: &[i64]) {
    c.execute(
        "INSERT INTO suggestions(id, title, rule, evidence, created, updated) VALUES (?1, '规则', '先看阻塞', ?2, 1, 1)",
        [id, &json!(evidence).to_string()],
    )
    .unwrap();
}

#[test]
fn rules_decide_toggle_and_feedback() {
    let c = conn();
    let t = create(&c, json!({"title": "样本"}));
    let seq: i64 = c.query_row("SELECT max(seq) FROM task_audit", [], |r| r.get(0)).unwrap();
    add_rule(&c, "r1", &[seq, 9999]);

    let f = rules::feedback_prepare(&c, &json!({"kind": "rule", "id": "r1", "text": "更具体"})).unwrap();
    assert_eq!(f.request["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(f.request["evidence"][0]["task_id"], t.id);
    assert_eq!(
        err(rules::feedback_prepare(&c, &json!({"id": "r1", "text": ""})).map(|_| json!(0))),
        "请输入1–4000字的批复意见"
    );
    let bad = json!({"title": "新规则", "rule": "", "reason": "按批复"});
    assert_eq!(err(rules::feedback_complete(&c, &f, &bad)), "AI 返回的 rule 无效，原建议保留");
    let revised =
        rules::feedback_complete(&c, &f, &json!({"title": "新规则", "rule": "先看阻塞原因", "reason": "按批复"}))
            .unwrap();
    assert_eq!(revised["title"], "新规则");
    let revisions: i64 = c.query_row("SELECT count(*) FROM advice_revisions", [], |r| r.get(0)).unwrap();
    assert_eq!(revisions, 1);
    assert_eq!(err(rules::feedback_complete(&c, &f, &revised)), "AI 分析期间建议已变化，本次结果未覆盖；请重新批复");

    assert_eq!(err(rules::decide(&c, &json!({"id": "r1", "action": "maybe"}))), "请选择启用或拒绝");
    rules::decide(&c, &json!({"kind": "rule", "id": "r1", "action": "enable"})).unwrap();
    assert_eq!(crate::data::rules::active(&c).unwrap().len(), 1);
    assert_eq!(kv::get_i64(&c, crate::data::rules::RULE_VERSION).unwrap(), 1);
    assert_eq!(err(rules::decide(&c, &json!({"id": "r1", "action": "reject"}))), "建议已处理或不存在，请刷新");
    assert_eq!(
        err(rules::decide(&c, &json!({"kind": "prompt", "id": "p1", "action": "enable"}))),
        "建议已处理或不存在，请刷新"
    );

    rules::toggle(&c, &json!({"id": "r1"})).unwrap();
    assert!(crate::data::rules::active(&c).unwrap().is_empty());
    assert_eq!(crate::data::rules::list(&c).unwrap()[0]["decision"], "pending");
    c.execute("UPDATE suggestions SET archived_at = 5", []).unwrap();
    assert_eq!(err(rules::toggle(&c, &json!({"id": "r1"}))), "此建议已归档，请编辑系统提示词或回滚版本");
    assert_eq!(err(rules::toggle(&c, &json!({"id": "nope"}))), "建议不存在");

    crate::data::tags::propose(&c, &Tag { kind: "客户".into(), value: "丙方".into() }, "新", &[]).unwrap();
    let id = crate::data::tags::snapshot(&c).unwrap()["proposals"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(err(rules::decide(&c, &json!({"kind": "tag", "id": id, "action": "enable"}))), "不支持的建议操作");
    let f = rules::feedback_prepare(&c, &json!({"kind": "tag", "id": id, "text": "改名"})).unwrap();
    rules::feedback_complete(&c, &f, &json!({"tag": {"kind": "客户", "value": "丙公司"}, "reason": "统一称呼"}))
        .unwrap();
    assert_eq!(crate::data::tags::snapshot(&c).unwrap()["proposals"][0]["tag"]["value"], "丙公司");

    assert!(super::rules::learning_evidence("manual_status", "dismiss", &Value::Null));
    assert!(!super::rules::learning_evidence("manual_status", "开始处理", &Value::Null));
    assert!(super::rules::learning_evidence("manual_archive", "", &json!({"archive_note": "原因"})));
}

#[test]
fn archive_pages_filters_and_revision() {
    let c = conn();
    let revision = query::tasks_revision(&c, time::now()).unwrap();
    let mut first = None;
    for i in 0..55 {
        let tags = if i == 7 { json!([{"kind": "事项等级", "value": "紧急"}]) } else { json!([]) };
        let t = create(&c, json!({"title": format!("归档{i}"), "tags": tags}));
        edit::archive(&c, &json!({"id": t.id, "updated": t.updated, "status": "已完成"})).unwrap();
        if i == 7 {
            first = Some(t.id);
        }
    }
    let live = create(&c, json!({"title": "进行中"}));
    assert_ne!(query::tasks_revision(&c, time::now()).unwrap(), revision);

    let page1 = query::archived(&c, &json!({})).unwrap();
    assert_eq!(
        (page1["total"].as_u64(), page1["all"].as_u64(), page1["pages"].as_u64()),
        (Some(55), Some(55), Some(2))
    );
    assert_eq!(page1["rows"].as_array().unwrap().len(), 50);
    assert_eq!(page1["rows"][0]["id"].as_str(), first.as_deref());
    assert_eq!(page1["rows"][0]["priority_rank"], 0);
    let page2 = query::archived(&c, &json!({"page": 2})).unwrap();
    assert_eq!(page2["rows"].as_array().unwrap().len(), 5);
    let ids: std::collections::BTreeSet<_> = page1["rows"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["rows"].as_array().unwrap())
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 55);
    assert!(!ids.contains(&live.id));

    assert_eq!(query::archived(&c, &json!({"query": "归档5"})).unwrap()["total"], 6);
    assert_eq!(query::archived(&c, &json!({"priorities": ["紧急"]})).unwrap()["total"], 1);
    assert_eq!(query::archived(&c, &json!({"tag_filters": {"事项等级": ["紧急"]}})).unwrap()["total"], 1);
    assert_eq!(query::archived(&c, &json!({"until": 0})).unwrap()["total"], 0);
    assert_eq!(err(query::archived(&c, &json!({"query": "长".repeat(201)}))), "搜索词最多200字");

    let listed = query::tasks(&c, &json!({"archived": false})).unwrap();
    assert_eq!(listed["total"], 1);
    assert_eq!(query::tasks(&c, &json!({"query": "#56"})).unwrap()["items"][0]["title"], "进行中");
    assert_eq!(err(query::tasks(&c, &json!({"focus": "全部"}))), "关注范围应为待处理、处理中、被阻塞或延后处理");
}

#[test]
fn ai_halves_validate_and_commit() {
    let c = conn();
    let t = create(&c, json!({"title": "写方案"}));
    let request = json!({"task": {"id": t.id, "updated": t.updated, "title": "写方案", "status": "待处理"}, "text": "改成已完成"});
    let prepared = edit::ai_edit_prepare(&c, &request).unwrap();
    assert_eq!(prepared.request["user_instruction"], "改成已完成");
    let result = json!({"changes": [{"field": "status", "value": "已完成"}], "tag_changes": [], "reason": "按说明"});
    edit::ai_edit_complete(&c, prepared, &result).unwrap();
    assert_eq!((get(&c, &t.id).status.as_str(), get(&c, &t.id).reason.as_str()), ("已完成", "按说明"));
    // Without an AI change nothing is saved, the form's own edits included.
    for changes in [json!([]), json!([{"field": "status", "value": "已完成"}])] {
        let mut form = get(&c, &t.id).payload();
        form["description"] = json!("表单里改的描述");
        let prepared = edit::ai_edit_prepare(&c, &json!({"task": form, "text": "查一下进度"})).unwrap();
        let result = json!({"changes": changes, "tag_changes": [], "reason": "没有可用的查询结果"});
        let e = edit::ai_edit_complete(&c, prepared, &result).unwrap_err().0;
        assert_eq!(e, "AI未修改事项，原事项和表单改动均未保存：没有可用的查询结果");
        assert_eq!(get(&c, &t.id).description, "");
    }
    let scope = crate::state::Scope::from(&json!({"selected_task": t.id}));
    assert_eq!(query::snapshot_rows(&c, &scope).unwrap()[0]["origin"], "manual_edit");
    let stale = edit::ai_edit_prepare(&c, &json!({"task": get(&c, &t.id).payload(), "text": "改回"})).unwrap();
    let now = get(&c, &t.id);
    crate::data::tasks::edit(&c, &json!({"task": {"id": t.id, "updated": now.updated, "title": "写新方案"}})).unwrap();
    let e = edit::ai_edit_complete(&c, stale, &json!({"changes": [], "tag_changes": []})).unwrap_err().0;
    assert_eq!(e, "分析期间事项已有新修改，本次结果未覆盖。请重新打开编辑后再试");

    assert_eq!(edit::generation_text(&json!({"text": " "})).unwrap_err().0, "请输入1–6000字的事项描述");
    let draft = edit::generation_start(&c, &json!({"text": "明天开会并整理纪要"})).unwrap();
    assert_eq!(draft.generation, "pending");
    let rows = json!({"tasks": [{"title": "开会"}, {"title": "整理纪要", "due": "明天"}]});
    assert_eq!(edit::generation_complete(&c, &draft, Ok(rows)).unwrap(), 2);
    let made = get(&c, &draft.id);
    assert_eq!((made.title.as_str(), made.generation.as_str()), ("开会", ""));
    assert_eq!(crate::data::tasks::list(&c, "title = '整理纪要' AND due = ''").unwrap().len(), 1);

    let draft = edit::generation_start(&c, &json!({"text": "另一件"})).unwrap();
    assert_eq!(edit::interrupt_generations(&c).unwrap(), 1);
    let failed = get(&c, &draft.id);
    assert_eq!((failed.generation.as_str(), failed.generation_error.as_str()), ("failed", "软件重启，任务已中断"));
    let retry = edit::generation_start(&c, &json!({"id": draft.id})).unwrap();
    assert_eq!(retry.generation, "pending");
    assert_eq!(edit::generation_start(&c, &json!({"id": t.id})).unwrap_err().0, "该事项不需要重试");
}

#[test]
fn runtime_routes_task_methods_and_snapshot() {
    let rt = Runtime::start(Ctx::memory(Config::default()).unwrap(), false).unwrap();
    let t = rt.call("edit", json!({"task": {"title": "通过请求创建", "status": "待处理"}})).unwrap();
    let id = t["id"].as_str().unwrap();
    assert_eq!(rt.call("task_get", json!({"id": id})).unwrap()["title"], "通过请求创建");
    assert_eq!(rt.call("tasks_query", json!({})).unwrap()["total"], 1);

    assert_eq!(rt.call("edit_ai", json!({"text": ""})).unwrap_err().0, "请输入1–6000字的修改说明");
    assert_eq!(rt.call("generate", json!({"text": ""})).unwrap_err().0, "请输入1–6000字的事项描述");
    assert_eq!(rt.call("generate", json!({"id": id})).unwrap_err().0, "该事项不需要重试");
    assert_eq!(
        rt.call("advice_feedback", json!({"id": "x", "text": "好"})).unwrap_err().0,
        "建议已处理或不存在，请刷新"
    );
    assert_eq!(rt.call("tasks_query", json!({})).unwrap()["total"], 1);

    // The snapshot carries only the tasks the page points at; lists page.
    assert!(rt.call("state", json!({"page": "tasks"})).unwrap()["tasks"].as_array().unwrap().is_empty());
    assert_eq!(rt.call("active_tasks_query", json!({})).unwrap()["rows"][0]["id"], id);
    let other = rt.call("edit", json!({"task": {"title": "另一件", "status": "待处理"}})).unwrap();
    assert_eq!(rt.call("merge_candidates", json!({"id": id})).unwrap()["rows"][0]["id"], other["id"]);
    let tasks = rt.call("state", json!({"page": "tasks", "selected_task": id})).unwrap();
    assert_eq!(tasks["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(tasks["tasks"][0]["id"], id);
    assert_eq!(tasks["tasks"][0]["change_count"], 1);
    assert_eq!(tasks["open_task_count"], 2);
    assert_eq!(tasks["task_focus_counts"]["待处理"], 2);
    assert!(tasks["next_deferral_at"].is_null());
    for key in ["tag_state", "rule_version", "tasks_revision", "task_history_revision"] {
        assert!(tasks.get(key).is_some(), "{key}");
    }
    assert!(tasks.get("suggestions").is_none() && tasks.get("schedules").is_none());
    assert!(rt.call("state", json!({"page": "schedules"})).unwrap()["schedules"].is_array());
    assert!(rt.call("state", json!({"page": "learning"})).unwrap()["suggestions"].is_array());
    let logs = rt.call("state", json!({"page": "logs"})).unwrap();
    assert!(logs["tasks"].as_array().unwrap().is_empty());
    assert!(logs.get("schedules").is_none());
    let selected = rt.call("state", json!({"page": "logs", "selected_task": id})).unwrap();
    assert_eq!(selected["tasks"][0]["description"], "");
}

fn reply(value: Value) -> crate::ai::provider::fake::Step {
    crate::ai::provider::fake::Step::Reply(value, json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}))
}

/// Waits (bounded) until the background generation of `id` settles.
fn settled(rt: &Runtime, id: &str) -> Value {
    for _ in 0..500 {
        let t = rt.call("task_get", json!({"id": id})).unwrap();
        if t["generation"] != "pending" {
            return t;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("generation did not settle");
}

#[test]
fn runtime_ai_methods_call_the_model_and_persist() {
    use crate::ai::provider::fake::Fake;
    let rt = Runtime::start(Ctx::memory(Config::default()).unwrap(), false).unwrap();
    let t = rt.call("edit", json!({"task": {"title": "旧标题", "status": "待处理"}})).unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let fake = Fake::new(vec![
        reply(json!({"reason": "按说明改标题", "changes": [{"field": "title", "value": "新标题"}], "tag_changes": [],
            "tag_suggestions": [{"kind": "客户", "value": "甲方", "reason": "说明里提到", "evidence_ids": ["input"]},
                {"kind": "客户", "value": "乙方", "reason": "无据", "evidence_ids": ["audit:9"]}]})),
        reply(
            json!({"schedule": {"title": "日报", "description": "写当天日报", "frequency": "daily", "time": "09:00"}}),
        ),
        reply(json!({"title": "新规则", "rule": "先看截止", "reason": "按批复"})),
        reply(
            json!({"tasks": [{"title": "开会", "description": "明天开会"}, {"title": "写纪要", "description": "会后写纪要"}]}),
        ),
    ]);
    rt.ctx.ai.set_driver(Some(fake.clone()));

    let edited = rt.call("edit_ai", json!({"task": t, "text": "把标题改成新标题"})).unwrap();
    assert_eq!(edited["title"], "新标题", "{edited}");
    assert_eq!(rt.call("task_get", json!({"id": id})).unwrap()["title"], "新标题");
    let (kind, prompt) = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(kind, "edit");
    assert!(prompt.contains("把标题改成新标题") && prompt.contains("旧标题"), "{prompt}");
    // Only the suggestion backed by evidence the request showed is queued.
    let proposals = rt.ctx.db.test_read_blocking(|c| crate::data::tags::snapshot(c)).unwrap()["proposals"].clone();
    let queued: Vec<&str> = proposals.as_array().unwrap().iter().filter_map(|p| p["tag"]["value"].as_str()).collect();
    assert_eq!(queued, ["甲方"], "{proposals}");
    assert!(proposals[0].to_string().contains("input:"), "{proposals}");

    let draft = rt.call("schedule_format", json!({"text": "每天九点写日报"})).unwrap();
    assert_eq!((draft["title"].as_str(), draft["time"].as_str()), (Some("日报"), Some("09:00")), "{draft}");
    assert_eq!(fake.seen.lock().unwrap()[1].0, "schedule");

    rt.ctx.db.test_write_blocking(|c| Ok(add_rule(c, "r1", &[]))).unwrap();
    rt.call("advice_feedback", json!({"id": "r1", "text": "改成先看截止"})).unwrap();
    let (kind, prompt) = fake.seen.lock().unwrap()[2].clone();
    assert_eq!(kind, "advice_revision");
    assert!(prompt.contains("改成先看截止") && prompt.contains("先看阻塞"), "{prompt}");
    let rule: String = rt
        .ctx
        .db
        .test_read_blocking(|c| Ok(c.query_row("SELECT rule FROM suggestions WHERE id = 'r1'", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(rule, "先看截止");

    let pending = rt.call("generate", json!({"text": "明天开会并写纪要"})).unwrap();
    assert_eq!(pending["generation"], "pending");
    let made = settled(&rt, pending["id"].as_str().unwrap());
    assert_eq!((made["title"].as_str(), made["generation"].as_str()), (Some("开会"), Some("")), "{made}");
    assert_eq!(rt.call("tasks_query", json!({})).unwrap()["total"], 3);
    assert_eq!(fake.seen.lock().unwrap()[3].0, "format");

    // A failed call marks the draft failed with the call's error.
    let failing = rt.call("generate", json!({"text": "再来一件"})).unwrap();
    let failed = settled(&rt, failing["id"].as_str().unwrap());
    assert_eq!(
        (failed["generation"].as_str(), failed["generation_error"].as_str()),
        (Some("failed"), Some("假驱动没有更多脚本")),
        "{failed}"
    );
    assert_eq!(fake.calls(), 5);
}

/// The SQL list queries order, filter and count the same way the Rust
/// rules (effective_tags, priority_rank, focus) do on a mixed sample.
#[test]
fn list_queries_match_the_rust_rules() {
    let c = conn();
    let at = time::now();
    let mut seed = 7u64;
    let mut next = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let levels = ["紧急", "高", "普通", "低", "未知"];
    let statuses = ["待处理", "处理中", "已完成", "已取消"];
    for i in 0..130 {
        let mut tags = Vec::new();
        for _ in 0..next(3) {
            tags.push(Tag { kind: "事项等级".into(), value: levels[next(5) as usize].into() });
        }
        if next(3) == 0 {
            tags.push(Tag {
                kind: "事项类型".into(),
                value: ["问题排查", "日常事务", "旧类型"][next(3) as usize].into(),
            });
        }
        let due = match next(4) {
            0 => chrono::DateTime::from_timestamp_millis(at + 3_600_000).unwrap().to_rfc3339(),
            1 => chrono::DateTime::from_timestamp_millis(at + 20 * 3_600_000).unwrap().to_rfc3339(),
            2 => chrono::DateTime::from_timestamp_millis(at + 90 * 3_600_000).unwrap().to_rfc3339(),
            _ => String::new(),
        };
        let mut t = Task {
            id: format!("t{:03}", 130 - i),
            title: format!("事项{i}"),
            description: if i % 7 == 0 { "含Alpha说明".into() } else { String::new() },
            status: statuses[next(4) as usize].into(),
            blocker: if next(5) == 0 {
                "\u{3000}".into()
            } else if next(5) == 0 {
                "等审批".into()
            } else {
                String::new()
            },
            due,
            follow_deadline: next(2) == 0,
            deferred_until: if next(4) == 0 { at + (1 + next(3) as i64) * 3_600_000 } else { 0 },
            archived: next(3) == 0,
            merged_into: if next(10) == 0 { "t001".into() } else { String::new() },
            updated: at - next(4) as i64 * 1000,
            tags,
            ..Task::default()
        };
        model::normalize(&mut t);
        edit::save(&c, &t).unwrap();
    }
    let (catalog, kind) = crate::data::tags::catalog(&c).unwrap();
    let all = crate::data::tasks::list(&c, "1").unwrap();
    let rank = |t: &Task| tags::priority_rank(&tags::effective_tags(t, &catalog, &kind, at), &catalog, &kind);

    // Navigation total: every task with a focus view, once.
    for focus in ["待处理", "处理中", "被阻塞", "延后处理"] {
        assert!(all.iter().any(|t| t.focus(at) == Some(focus)), "{focus}");
    }
    let (counts, next) = query::focus_counts(&c, at).unwrap();
    for (focus, n) in &counts {
        assert_eq!(n.as_u64().unwrap() as usize, all.iter().filter(|t| t.focus(at) == Some(focus.as_str())).count());
    }
    let deferred = all.iter().filter(|t| t.focus(at) == Some("延后处理")).map(|t| t.deferred_until).min();
    assert_eq!(next, deferred);

    // tasks_query: priority order, deferred tasks refilled in return order.
    for p in [
        json!({}),
        json!({"archived": false}),
        json!({"focus": "延后处理"}),
        json!({"focus": "被阻塞"}),
        json!({"status": "处理中"}),
        json!({"query": "alpha"}),
        json!({"query": "#1"}),
    ] {
        let query = p["query"].as_str().unwrap_or("").to_lowercase();
        let mut list: Vec<&Task> = all
            .iter()
            .filter(|t| {
                p["archived"].as_bool().is_none_or(|v| t.archived == v)
                    && p["status"].as_str().is_none_or(|v| t.status == v)
                    && p["focus"].as_str().is_none_or(|v| t.focus(at) == Some(v))
                    && (query.is_empty()
                        || t.title.to_lowercase().contains(&query)
                        || t.description.to_lowercase().contains(&query)
                        || format!("#{}", t.number).contains(&query))
            })
            .collect();
        list.sort_by(|a, b| (rank(a), -a.updated, &a.id).cmp(&(rank(b), -b.updated, &b.id)));
        let mut later: Vec<&Task> = list.iter().copied().filter(|t| t.focus(at) == Some("延后处理")).collect();
        later.sort_by_key(|t| t.deferred_until);
        let mut later = later.into_iter();
        for t in list.iter_mut().filter(|t| t.focus(at) == Some("延后处理")) {
            *t = later.next().unwrap();
        }
        let mut p2 = p.clone();
        p2["limit"] = json!(100);
        let got = query::tasks(&c, &p2).unwrap();
        let ids: Vec<&str> = got["items"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        let want: Vec<&str> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, want.iter().take(100).copied().collect::<Vec<_>>(), "{p}");
        assert_eq!(got["total"], want.len(), "{p}");
        p2["limit"] = json!(3);
        p2["offset"] = json!(2);
        let page = query::tasks(&c, &p2).unwrap();
        let ids: Vec<&str> = page["items"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, want.iter().skip(2).take(3).copied().collect::<Vec<_>>(), "{p}");
        for (row, t) in got["items"].as_array().unwrap().iter().zip(&list) {
            assert_eq!(row["focus"], json!(t.focus(at)));
            assert_eq!(row["tags"], json!(t.tags));
        }
    }

    // archived_tasks_query: effective tags filter, level then recency.
    let has = |tags: &[Tag], k: &str, values: &[&str]| {
        values.is_empty() || tags.iter().any(|t| t.kind == k && values.contains(&t.value.as_str()))
    };
    let filters: [(Value, fn(&[Tag], &dyn Fn(&[Tag], &str, &[&str]) -> bool) -> bool); 4] = [
        (json!({}), |_, _| true),
        (json!({"priorities": ["高", "紧急"]}), |t, has| has(t, "事项等级", &["高", "紧急"])),
        (json!({"tag_filters": {"事项类型": ["问题排查"], "事项等级": []}}), |t, has| {
            has(t, "事项类型", &["问题排查"])
        }),
        (json!({"tag_filters": {"事项等级": ["普通"]}, "priorities": []}), |t, has| {
            has(t, "事项等级", &["普通"])
        }),
    ];
    for (p, keep) in filters {
        let mut want: Vec<&Task> = all
            .iter()
            .filter(|t| t.archived && t.merged_into.is_empty() && t.generation.is_empty())
            .filter(|t| keep(&tags::effective_tags(t, &catalog, &kind, at), &has))
            .collect();
        want.sort_by(|a, b| (rank(a), -a.updated, &a.id).cmp(&(rank(b), -b.updated, &b.id)));
        let ids = every_page(&c, query::archived, &p);
        assert_eq!(ids, want.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), "{p}");
    }

    // active_tasks_query against the task page's former client rules
    // (visibleTasks): tags, quick view, scope and text filter; level then
    // recency, number on ties; deferred slots refilled by return time.
    let day = |ms: i64| (ms + 8 * 3_600_000).div_euclid(86_400_000);
    let due_ms = |t: &Task| chrono::DateTime::parse_from_rfc3339(&t.due).ok().map(|d| d.timestamp_millis());
    let front = |scope: &str, view: &str, text: &str, keep: &dyn Fn(&[Tag]) -> bool| -> Vec<String> {
        let text = text.trim().to_lowercase();
        let mut list: Vec<&Task> = all
            .iter()
            .filter(|t| keep(&tags::effective_tags(t, &catalog, &kind, at)))
            .filter(|t| match view {
                "" => true,
                "today" => t.focus(at).is_some() && due_ms(t).is_some_and(|d| day(d) == day(at)),
                focus => t.focus(at) == Some(focus),
            })
            .filter(|t| !t.archived && t.merged_into.is_empty())
            .filter(|t| match scope {
                "全部" => true,
                "未完成" => model::ACTIVE.contains(&t.status.as_str()),
                "待处理" | "处理中" | "被阻塞" | "延后处理" => t.focus(at) == Some(scope),
                status => t.status == status,
            })
            .filter(|t| {
                text.is_empty()
                    || format!("{} {} {} #{}", t.title, t.description, t.blocker, t.number)
                        .to_lowercase()
                        .contains(&text)
            })
            .collect();
        list.sort_by(|a, b| (rank(a), -a.updated, a.number).cmp(&(rank(b), -b.updated, b.number)));
        let mut later: Vec<&Task> = list.iter().copied().filter(|t| t.focus(at) == Some("延后处理")).collect();
        later.sort_by_key(|t| t.deferred_until);
        let mut later = later.into_iter();
        for t in list.iter_mut().filter(|t| t.focus(at) == Some("延后处理")) {
            *t = later.next().unwrap();
        }
        list.iter().map(|t| t.id.clone()).collect()
    };
    let keeps: [(Value, Box<dyn Fn(&[Tag]) -> bool>); 3] = [
        (json!({}), Box::new(|_: &[Tag]| true)),
        (json!({"priorities": ["高", "紧急"]}), Box::new(move |t: &[Tag]| has(t, "事项等级", &["高", "紧急"]))),
        (
            json!({"tag_filters": {"事项类型": ["问题排查", "日常事务"]}}),
            Box::new(move |t: &[Tag]| has(t, "事项类型", &["问题排查", "日常事务"])),
        ),
    ];
    let mut cases = Vec::new();
    for scope in ["全部", "未完成", "待处理", "处理中", "被阻塞", "延后处理", "已完成", "已取消"]
    {
        for view in ["", "today", "待处理", "被阻塞", "延后处理"] {
            cases.push((scope, view, "", 0));
        }
    }
    for text in ["alpha", " #1", "等审", "事项1"] {
        for k in 0..keeps.len() {
            cases.push(("全部", "", text, k));
        }
    }
    for k in 1..keeps.len() {
        cases.push(("待处理", "", "", k));
        cases.push(("全部", "被阻塞", "", k));
    }
    let mut paged = false;
    for (scope, view, text, k) in cases {
        let (filter, keep) = &keeps[k];
        let mut p = filter.clone();
        p["scope"] = json!(scope);
        p["view"] = json!(view);
        p["query"] = json!(text);
        let want = front(scope, view, text, keep.as_ref());
        assert_eq!(every_page(&c, query::active, &p), want, "{p}");
        let first = query::active(&c, &p).unwrap();
        assert_eq!(first["total"], want.len(), "{p}");
        for row in first["rows"].as_array().unwrap() {
            let t = all.iter().find(|t| t.id == row["id"]).unwrap();
            assert_eq!(row["priority_rank"], rank(t), "{p}");
            assert_eq!(row["effective_tags"], json!(tags::effective_tags(t, &catalog, &kind, at)));
            assert!(row.get("description").is_none());
        }
        // A selected task past this page comes back pinned; one shown or
        // filtered out does not.
        if let Some(late) = want.get(50) {
            paged = true;
            p["selected"] = json!(late);
            assert_eq!(query::active(&c, &p).unwrap()["selected"]["id"], json!(late), "{p}");
            p["selected"] = json!(want[0]);
            assert!(query::active(&c, &p).unwrap()["selected"].is_null());
        }
        if let Some(out) = all.iter().find(|t| !want.contains(&t.id)) {
            p["selected"] = json!(out.id);
            assert!(query::active(&c, &p).unwrap()["selected"].is_null(), "{p}");
        }
    }
    assert!(paged);
    assert_eq!(query::active(&c, &json!({"scope": "无效"})).unwrap_err().0, "事项范围无效");

    // The snapshot carries the referred tasks only, whatever is unarchived.
    let open: Vec<&Task> = all.iter().filter(|t| t.focus(at).is_some()).collect();
    let scope = crate::state::Scope::from(&json!({"page": "tasks", "task": open[0].id, "selected_task": open[1].id}));
    let rows = query::snapshot_rows(&c, &scope).unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [open[0].id.as_str(), open[1].id.as_str()]);
    assert!(rows[0]["description"].is_string() && rows[0]["change_count"].is_i64());
    assert!(query::snapshot_rows(&c, &crate::state::Scope::from(&json!({"page": "tasks"}))).unwrap().is_empty());
}

/// Ids of every page of a paged task list, following its cursor.
fn every_page(c: &Connection, run: fn(&Connection, &Value) -> crate::Result<Value>, p: &Value) -> Vec<String> {
    let mut p = p.clone();
    let mut ids = Vec::new();
    for page in 1.. {
        p["page"] = json!(page);
        let got = run(c, &p).unwrap();
        ids.extend(got["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()));
        if got["next_cursor"].is_null() {
            assert_eq!(got["total"], ids.len(), "{p}");
            return ids;
        }
        p["cursor"] = got["next_cursor"].clone();
    }
    unreachable!()
}

struct Recorder(std::sync::Mutex<Vec<crate::HostEvent>>);

impl crate::Host for Recorder {
    fn event(&self, event: crate::HostEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn console_changes_notify_and_window_edits_stay_silent() {
    let host = std::sync::Arc::new(Recorder(Default::default()));
    let ctx = Ctx::new(
        Default::default(),
        crate::data::Db::memory().unwrap(),
        crate::ConfigStore::memory(Config::default()),
        host.clone(),
    );
    let rt = Runtime::start(ctx, false).unwrap();
    let t = rt.call("edit", json!({"task": {"title": "窗口新建", "status": "待处理"}})).unwrap();
    rt.call("action", json!({"id": t["id"], "updated": t["updated"], "action": "done"})).unwrap();
    assert!(host.0.lock().unwrap().is_empty(), "window edits are silent");
    let c = rt.call_cli("edit", json!({"task": {"title": "CLI 新建", "status": "待处理"}})).unwrap();
    rt.call_cli("action", json!({"id": c["id"], "updated": c["updated"], "action": "done"})).unwrap();
    rt.call_cli("tasks_query", json!({})).unwrap();
    let events = host.0.lock().unwrap();
    let notices = events
        .iter()
        .map(|e| match e {
            crate::HostEvent::Notify { title, body, task_id } => (title.as_str(), body.as_str(), task_id.as_ref()),
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>();
    let id = c["id"].as_str().map(String::from);
    assert_eq!(notices, [("CLI 新建", "新增事项", id.as_ref()), ("CLI 新建", "状态改为已完成", id.as_ref())]);
}
