//! Full-document archived memory uses the same delivery and commit boundaries
//! as a real request, with synthetic results and no provider calls.
use super::*;
use crate::analysis_context::{self, Meter};

fn app() -> App {
    let app =
        App::load(std::env::temp_dir().join(format!("shishi-archive-text-{}", id()))).unwrap();
    {
        let mut state = app.state.lock().unwrap();
        state.config.model_profiles = None;
        state.analysis_memory.temporary = "保留合成临时笔记".into();
        state.analysis_memory.temporary_version = 4;
        app.save(&state).unwrap();
    }
    app
}

fn prepare(app: &App) -> (Candidate, engine::PreparedAiRequest) {
    let state = app.state.lock().unwrap().clone();
    let candidate = batch(
        &state,
        AiMode::Manual,
        now(),
        "memory-attempt",
        "synthetic-day",
    );
    let mut context = app.capture_archive_context(&state, vec![]);
    let request = app
        .prepare_ai_request_with("archive", &candidate.payload, AiMode::Manual, &mut context)
        .unwrap();
    (candidate, request)
}

fn finish(request: &engine::PreparedAiRequest, replacement: Value) -> Value {
    let mut value = json!({
        "report":{"summary":"合成归档结论","completed":[],"attention":[],"next_steps":[]},
        "archived_memory":replacement
    });
    crate::ai_backend::api::attach_result(
        &mut value,
        &request.payload["_local_context"],
        &Meter::default(),
    );
    value
}

#[test]
fn legacy_entries_remain_complete_until_a_valid_document_replaces_them() {
    let app = app();
    let legacy: AnalysisMemory = serde_json::from_value(json!({
        "archived_entries":[
            {"id":"legacy-a","key":"范围","text":"甲".repeat(2400),"source":["manual"]},
            {"id":"legacy-b","key":"例外","text":"乙".repeat(2200),"source":["manual"]}
        ],
        "archived_version":7,
        "temporary":"保留合成临时笔记",
        "temporary_version":4
    }))
    .unwrap();
    assert!(legacy.archived.is_none());
    let full = legacy.archived_text();
    assert!(full.chars().count() > 4000);
    assert!(full.contains(&"甲".repeat(2400)));
    assert!(full.ends_with(&"乙".repeat(2200)));
    {
        let mut state = app.state.lock().unwrap();
        state.analysis_memory = legacy;
        app.save(&state).unwrap();
    }
    let path = app.root.clone();
    drop(app);
    let app = App::load(path.clone()).unwrap();
    let original = app.memory_read();
    assert_eq!(original["archived"]["text"], full);
    assert_eq!(original["archived"]["max_chars"], 4000);
    assert!(app
        .memory_save(&json!({"kind":"archived","expected":7,"text":full}))
        .is_err());
    assert_eq!(app.memory_read(), original);

    let (candidate, request) = prepare(&app);
    assert_eq!(
        request.payload["_local_context"]["archived_memory"]["inline_text"],
        full
    );
    assert!(request.prepared.prompt.contains(&full));
    let output = finish(&request, json!(["合并后的范围与例外说明"]));
    assert!(output.get("_archived_memory_error").is_none());
    app.commit(candidate, output, now()).unwrap();
    let saved = app.memory_read();
    assert_eq!(saved["archived"]["text"], "合并后的范围与例外说明");
    assert_eq!(saved["archived"]["revision"], 8);
    assert_eq!(saved["temporary"], original["temporary"]);
    assert!(app
        .state
        .lock()
        .unwrap()
        .analysis_memory
        .archived_entries
        .is_empty());
    drop(app);
    let app = App::load(path).unwrap();
    assert_eq!(app.memory_read(), saved);
}

#[test]
fn full_documents_support_first_write_replace_noop_and_explicit_clear() {
    let app = app();
    for (replacement, expected, revision, changed) in [
        (json!(["整段经验"]), "整段经验", 1, 1),
        (json!(["整段经验"]), "整段经验", 1, 0),
        (
            json!(["统一改写后的经验\n保留适用条件"]),
            "统一改写后的经验\n保留适用条件",
            2,
            1,
        ),
        (json!([]), "统一改写后的经验\n保留适用条件", 2, 0),
        (json!([""]), "", 3, 1),
        (json!([]), "", 3, 0),
    ] {
        let (candidate, request) = prepare(&app);
        let report = app
            .commit(candidate, finish(&request, replacement), now())
            .unwrap();
        assert_eq!(report["memory_changes"], changed);
        assert_eq!(app.memory_read()["archived"]["text"], expected);
        assert_eq!(app.memory_read()["archived"]["revision"], revision);
    }
    let state = app.state.lock().unwrap();
    assert_eq!(state.analysis_memory.archived.as_deref(), Some(""));
    assert!(state.analysis_memory.archived_entries.is_empty());
    assert_eq!(state.analysis_memory.temporary, "保留合成临时笔记");
    assert_eq!(
        state
            .audit
            .iter()
            .filter(|a| a.kind == "archive_memory")
            .count(),
        3
    );
    // The audit retains the operation and revision, not a second private body.
    assert!(state
        .audit
        .iter()
        .filter(|a| a.kind == "archive_memory")
        .all(|a| { a.before.is_null() && a.after.get("text").is_none() }));
}

#[test]
fn malformed_or_oversized_updates_never_replace_or_truncate_memory() {
    let app = app();
    app.memory_save(&json!({"kind":"archived","expected":0,"text":"保留原文"}))
        .unwrap();
    let before = app.memory_read();
    for replacement in [
        json!("不是数组"),
        json!(["一", "二"]),
        json!([42]),
        json!(["😀".repeat(4001)]),
    ] {
        let (candidate, request) = prepare(&app);
        let result = finish(&request, replacement);
        assert!(result.get("_archived_memory_error").is_some());
        assert!(app.commit(candidate, result, now()).is_err());
        assert_eq!(app.memory_read(), before);
    }
    let (candidate, _) = prepare(&app);
    let raw = json!({"report":{"summary":"合成结论","completed":[],"attention":[],"next_steps":[]},"archived_memory":["未经完整读取校验"]});
    assert!(app.commit(candidate, raw, now()).is_err());
    assert_eq!(app.memory_read(), before);

    let boundary = "😀".repeat(4000);
    let (candidate, request) = prepare(&app);
    app.commit(candidate, finish(&request, json!([boundary])), now())
        .unwrap();
    assert_eq!(app.memory_read()["archived"]["text"], boundary);
    assert_eq!(app.memory_read()["archived"]["revision"], 2);
    assert!(app
        .memory_save(&json!({"kind":"archived","expected":2,"text":"😀".repeat(4001)}))
        .is_err());
    assert_eq!(app.memory_read()["archived"]["text"], boundary);
}

#[test]
fn whitespace_only_memory_can_be_replaced_without_an_unavailable_record() {
    let app = app();
    let blank = " \r\n\t ";
    app.memory_save(&json!({"kind":"archived","expected":0,"text":blank}))
        .unwrap();
    let (candidate, request) = prepare(&app);
    assert!(request.payload["_local_context"]["records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["id"] != "archived-memory"));
    let output = finish(&request, json!(["有效归档正文"]));
    assert!(output.get("_archived_memory_error").is_none());
    app.commit(candidate, output, now()).unwrap();
    assert_eq!(app.memory_read()["archived"]["text"], "有效归档正文");
    assert_eq!(app.memory_read()["archived"]["revision"], 2);

    // Temporary memory shares the empty-content check but keeps its own limit.
    let mut payload = json!({});
    analysis_context::attach_memory_text(&mut payload, "", blank);
    let request = crate::ai::prepare(&Config::default(), "classify", &payload);
    analysis_context::acknowledge_inline_memory(&mut payload, "classify", &request.prompt);
    let at_limit = analysis_context::memory_update(
        &payload["_local_context"],
        &Meter::default(),
        &json!({"memory_update":["😀".repeat(2000)]}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(at_limit["before"], blank);
    assert_eq!(at_limit["text"].as_str().unwrap().chars().count(), 2000);
    assert!(analysis_context::memory_update(
        &payload["_local_context"],
        &Meter::default(),
        &json!({"memory_update":["😀".repeat(2001)]}),
    )
    .is_err());
}

#[test]
fn archived_memory_requires_complete_delivery_and_archive_write_permission() {
    let old = "\"".repeat(9000);
    let mut payload = json!({});
    analysis_context::attach_memory_text(&mut payload, &old, "临时笔记");
    payload["_local_context"]["archived_memory"]["revision"] = json!(6);
    let config = Config::default();
    let request = crate::ai::prepare(&config, "archive", &payload);
    let update = json!({"archived_memory":["整理后的短正文"]});

    analysis_context::acknowledge_inline_memory(&mut payload, "archive", &request.prompt);
    assert!(analysis_context::archived_memory_update(
        &payload["_local_context"],
        &Meter::default(),
        &update
    )
    .unwrap()
    .is_some());
    analysis_context::acknowledge_inline_memory(
        &mut payload,
        "archive",
        &request.prompt.replace(&old, "省略后的摘要"),
    );
    let snapshot = &payload["_local_context"];
    assert!(
        analysis_context::archived_memory_update(snapshot, &Meter::default(), &update).is_err()
    );
    let mut skipped = Meter::default();
    analysis_context::query(
        snapshot,
        &mut skipped,
        &json!({"ids":["archived-memory"],"offset":7000}),
    );
    assert!(analysis_context::archived_memory_update(snapshot, &skipped, &update).is_err());

    let mut meter = Meter::default();
    let mut offset = 0;
    loop {
        let page: Value = serde_json::from_str(&analysis_context::query(
            snapshot,
            &mut meter,
            &json!({"ids":["archived-memory"],"offset":offset}),
        ))
        .unwrap();
        if let Some(next) = page["records"][0]["next_offset"].as_u64() {
            assert!(next > offset);
            assert!(analysis_context::archived_memory_update(snapshot, &meter, &update).is_err());
            offset = next;
        } else {
            break;
        }
    }
    let replacement = analysis_context::archived_memory_update(snapshot, &meter, &update)
        .unwrap()
        .unwrap();
    assert_eq!(replacement["before"], old);
    assert_eq!(replacement["expected"], 6);
    analysis_context::acknowledge_inline_memory(&mut payload, "classify", &request.prompt);
    assert!(
        analysis_context::archived_memory_update(&payload["_local_context"], &meter, &update)
            .is_err()
    );
}

#[test]
fn manual_edits_and_failed_commits_preserve_memory_and_archive_effects() {
    let app = app();
    {
        let mut state = app.state.lock().unwrap();
        state.tasks.push(Task {
            id: "completed-fixture".into(),
            title: "合成已完成事项".into(),
            status: "已完成".into(),
            created: 1,
            updated: 2,
            ..Default::default()
        });
        app.save(&state).unwrap();
    }
    let (stale, request) = prepare(&app);
    let stale_result = finish(&request, json!(["模型旧结果"]));
    app.memory_save(&json!({"kind":"archived","expected":0,"text":"人工最新正文"}))
        .unwrap();
    let manual = app.memory_read();
    assert!(app
        .commit(stale, stale_result, now())
        .unwrap_err()
        .contains("人工编辑"));
    assert_eq!(app.memory_read(), manual);
    assert!(!app.state.lock().unwrap().tasks[0].archived);

    let (candidate, request) = prepare(&app);
    let result = finish(&request, json!(["已充分读取的更新"]));
    let before = app.state.lock().unwrap().clone();
    crate::test_support::persistence_failure(&app.root, true).unwrap();
    let failed = app.commit(candidate.clone(), result.clone(), now());
    crate::test_support::persistence_failure(&app.root, false).unwrap();
    assert!(failed.is_err());
    {
        let after = app.state.lock().unwrap();
        assert_eq!(json!(after.analysis_memory), json!(before.analysis_memory));
        assert_eq!(json!(after.tasks), json!(before.tasks));
        assert_eq!(after.reports, before.reports);
        assert_eq!(after.archive_ai_cursor, before.archive_ai_cursor);
        assert_eq!(json!(after.audit), json!(before.audit));
    }
    app.commit(candidate, result, now()).unwrap();
    assert_eq!(app.memory_read()["archived"]["text"], "已充分读取的更新");
    assert!(app.state.lock().unwrap().tasks[0].archived);
    let reloaded = App::load(app.root.clone()).unwrap();
    assert_eq!(reloaded.memory_read(), app.memory_read());
    assert!(reloaded.state.lock().unwrap().tasks[0].archived);
}

#[test]
fn manual_archive_can_consolidate_existing_memory_without_new_operations() {
    let app = app();
    app.memory_save(&json!({"kind":"archived","expected":0,"text":"同一经验。\n同一经验。"}))
        .unwrap();
    let mut calls = 0;
    app.run_archive_with("manual_archive", AiMode::Manual, None, &mut |request, _| {
        calls += 1;
        assert!(request.payload["evidence"].as_array().unwrap().is_empty());
        assert_eq!(
            request.prepared.schema["properties"]["archived_memory"]["maxItems"],
            1
        );
        assert_eq!(
            request.payload["_local_context"]["archived_memory"]["before"],
            "同一经验。\n同一经验。"
        );
        Ok(finish(request, json!(["同一经验。"])))
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(app.memory_read()["archived"]["text"], "同一经验。");
    let state = app.state.lock().unwrap();
    let automatic = batch(&state, AiMode::Nightly, now(), "automatic", "synthetic-day");
    assert!(!automatic.has_ai_input());
}
