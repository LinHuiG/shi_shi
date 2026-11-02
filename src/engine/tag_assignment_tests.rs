//! Synthetic classification and editing results; no provider calls.
use super::*;
use crate::tags::{Group, Tag};

fn tag(kind: &str, value: &str) -> Tag {
    Tag {
        kind: kind.into(),
        value: value.into(),
    }
}

fn catalog() -> Vec<Group> {
    vec![Group {
        name: "类型".into(),
        values: vec!["核对".into(), "沟通".into()],
        ..Default::default()
    }]
}

fn fixture() -> State {
    let mut state = State::default();
    state.tag_state.unified = true;
    state.tag_state.versions = vec![crate::tags::Version {
        id: "synthetic-catalog".into(),
        groups: catalog(),
        ..Default::default()
    }];
    for (id, time) in [("old", 10), ("new", 20)] {
        state.messages.insert(
            id.into(),
            Message {
                private: true,
                ..crate::test_support::message(id, "合成核对进展", time)
            },
        );
    }
    let mut task = Task {
        id: "task".into(),
        title: "核对合成资料".into(),
        description: "等待结果".into(),
        status: "待处理".into(),
        updated: 10,
        tags: vec![tag("类型", "核对"), tag("旧目录", "保留值")],
        evidence: TaskEvidence {
            direct: vec!["old".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    normalize_task(&mut task);
    state.tasks.push(task);
    state
}

fn action(changes: Value) -> Value {
    json!({"action":"update","task_id":"task","sources":["new"],
        "fields":[],"tag_changes":changes,"confidence":0.99,
        "reason":"合成消息明确分类变化","change_summary":"分类已更新"})
}

fn apply(state: &mut State, actions: &[Value]) -> Result<usize, String> {
    let allowed = state.messages.clone();
    let versions = state
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.updated))
        .collect();
    App::apply_actions(state, actions, &allowed, &versions, &catalog())
}

#[test]
fn sparse_analysis_keeps_all_tags_when_description_and_deadline_change() {
    let mut state = fixture();
    // More tags than fit in the eight-tag context summary, including old values
    // no longer present in the catalog. An empty patch must retain every one.
    state.tasks[0]
        .tags
        .extend((0..10).map(|i| tag("旧目录", &format!("合成标签{i}"))));
    normalize_task(&mut state.tasks[0]);
    let tags = state.tasks[0].tags.clone();
    let mut update = action(json!([]));
    update["fields"] = json!([
        {"field":"description","value":"已有合成结果，安排下一次核对"},
        {"field":"due","value":"2030-01-02T09:00:00+08:00"}
    ]);
    update["deadline_evidence"] = json!(["new"]);
    update["change_summary"] = json!("已安排下一次核对");
    assert_eq!(apply(&mut state, &[update]).unwrap(), 1);
    assert_eq!(state.tasks[0].tags, tags);
    assert_eq!(state.tasks[0].due, "2030-01-02T09:00:00+08:00");
    assert_eq!(state.tasks[0].description, "已有合成结果，安排下一次核对");
    assert_eq!(
        state.audit.last().unwrap().change_summary,
        "已安排下一次核对"
    );
}

#[test]
fn new_and_existing_untagged_tasks_receive_catalog_tags() {
    for create in [true, false] {
        let mut state = fixture();
        state.tasks[0].tags.clear();
        let mut result = action(json!([{"action":"add","kind":"类型","value":"核对"}]));
        if create {
            state.tasks.clear();
            result["action"] = json!("create");
            result["task_id"] = json!("");
            result["fields"] = json!([
                {"field":"title","value":"核对合成资料"},
                {"field":"description","value":"确认合成结果"}
            ]);
        }
        assert_eq!(apply(&mut state, &[result]).unwrap(), 1);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].tags, [tag("类型", "核对")]);
        assert_eq!(state.audit.last().unwrap().change_summary, "分类已更新");
    }
}

#[test]
fn explicit_tag_replacement_preserves_unrelated_tags_and_records_summary() {
    let mut state = fixture();
    let update = action(json!([
        {"action":"remove","kind":"类型","value":"核对"},
        {"action":"add","kind":"类型","value":"沟通"}
    ]));
    assert_eq!(apply(&mut state, &[update]).unwrap(), 1);
    assert_eq!(state.tasks[0].tags.len(), 2);
    assert!(state.tasks[0].tags.contains(&tag("类型", "沟通")));
    assert!(state.tasks[0].tags.contains(&tag("旧目录", "保留值")));
    assert_eq!(state.audit.last().unwrap().change_summary, "分类已更新");
}

#[test]
fn duplicate_create_preserves_tags_and_cannot_remove_them() {
    let original = fixture();
    for changes in [
        json!([]),
        json!([{"action":"remove","kind":"类型","value":"核对"}]),
    ] {
        let mut state = original.clone();
        let mut duplicate = action(changes.clone());
        duplicate["action"] = json!("create");
        duplicate["sources"] = json!(["old", "new"]);
        duplicate["fields"] = json!([{"field":"description","value":"新的合成进展"}]);
        let result = apply(&mut state, &[duplicate]);
        assert_eq!(result.is_ok(), changes.as_array().unwrap().is_empty());
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].tags, original.tasks[0].tags);
        assert!(state
            .audit
            .iter()
            .all(|entry| entry.change_summary.is_empty()));
    }
}

#[test]
fn invalid_later_action_cannot_partially_commit_the_analysis_batch() {
    let (root, app) = crate::test_support::scoped_app("shishi-tag-batch");
    app.commit_state(|s| {
        *s = fixture();
        Ok(())
    })
    .unwrap();
    let before = json!(*app.state.lock().unwrap());
    let valid = action(json!([{"action":"add","kind":"类型","value":"沟通"}]));
    for invalid in [
        action(json!([{"action":"add","kind":"类型","value":"目录中不存在"}])),
        action(Value::Null),
        {
            let mut legacy = action(json!([]));
            legacy["tags"] = json!([]);
            legacy
        },
    ] {
        assert!(app
            .commit_state(|s| apply(s, &[valid.clone(), invalid]))
            .is_err());
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        assert_eq!(
            App::load(root.clone()).unwrap().state.lock().unwrap().tasks[0].tags,
            fixture().tasks[0].tags
        );
    }
}

#[test]
fn flat_ai_edit_patch_persists_and_undo_restores_all_tags() {
    let (root, app) = crate::test_support::scoped_app("shishi-tag-edit");
    app.commit_state(|s| {
        *s = fixture();
        Ok(())
    })
    .unwrap();
    let old = app.state.lock().unwrap().tasks[0].clone();
    let result = json!({"changes":[],"reason":"按明确要求调整分类","tag_changes":[
        {"action":"remove","kind":"旧目录","value":"保留值"},
        {"action":"add","kind":"类型","value":"沟通"}
    ]});
    app.complete_ai_edit(&old, old.clone(), "添加沟通并删除旧标签", result)
        .unwrap();
    let reopened = App::load(root.clone()).unwrap();
    let tags = reopened.state.lock().unwrap().tasks[0].tags.clone();
    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&tag("类型", "核对")) && tags.contains(&tag("类型", "沟通")));
    reopened.undo(0).unwrap();
    assert_eq!(reopened.state.lock().unwrap().tasks[0].tags, old.tags);
    let before = json!(*reopened.state.lock().unwrap());
    let current = reopened.state.lock().unwrap().tasks[0].clone();
    assert!(reopened.complete_ai_edit(&current, current.clone(), "错误格式", json!({
        "changes":[],"tag_changes":[{"action":"remove","tag":{"kind":"类型","value":"核对"}}]
    })).is_err());
    assert_eq!(json!(*reopened.state.lock().unwrap()), before);
}
