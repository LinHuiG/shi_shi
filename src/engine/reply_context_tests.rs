//! Explicit reply context and commit boundaries; synthetic data, no model calls.
use super::*;

fn fixture() -> State {
    let mut state = State::default();
    state.config.model_profiles = None;
    let at = now() - 10_000;
    for (i, (id, reply, text)) in [
        ("request", "", "请核对这份合成资料"),
        ("ask", "request", "请提供完整材料"),
        (
            "material",
            "ask",
            r#"{"title":"合成材料","items":[{"code":"A","value":42}],"enabled":true}"#,
        ),
        ("followup", "material", "这些材料是否足够，核对有进展吗？"),
    ]
    .into_iter()
    .enumerate()
    {
        state.messages.insert(
            id.into(),
            Message {
                id: id.into(),
                source_id: "synthetic-source".into(),
                chat: "synthetic-chat".into(),
                text: text.into(),
                reply: reply.into(),
                time: at + i as i64,
                updated_at: at + i as i64,
                private: true,
                ..Default::default()
            },
        );
    }
    state.tasks.push(Task {
        id: "task".into(),
        title: "核对合成资料".into(),
        description: "等待提供完整材料".into(),
        waiting_for: "完整材料".into(),
        status: "待处理".into(),
        updated: at + 1,
        evidence: TaskEvidence {
            direct: vec!["request".into(), "ask".into()],
            ..Default::default()
        },
        ..Default::default()
    });
    state
}

fn app(state: State) -> (App, PathBuf) {
    let (root, app) = crate::test_support::app("ss-reply-context");
    app.commit_state(|s| {
        *s = state;
        Ok(())
    })
    .unwrap();
    (app, root)
}

fn prepare(
    app: &App,
    state: &State,
    change: &str,
) -> (crate::context_budget::Batch, PreparedAiRequest) {
    let batch = crate::context_budget::classify(state, &[state.messages[change].clone()]);
    let mut context = app.capture_prepare_context_from(state, "classify", false, json!([]));
    let mut request = app
        .prepare_ai_request_with("classify", &batch.payload, AiMode::Manual, &mut context)
        .unwrap();
    request.frozen = Some(FrozenView::new(
        batch.allowed.clone(),
        batch.versions.clone(),
        &request.records,
    ));
    (batch, request)
}

fn update(sources: Value, description: &str) -> Value {
    json!({"action":"update","task_id":"task","sources":sources,
        "fields":[{"field":"description","value":description},{"field":"waiting_for","value":""}],
        "tag_changes":[],"confidence":0.99,"reason":"新回复提供了材料和后续进展"})
}

#[test]
fn reply_ancestors_supply_missing_material_without_becoming_delta_messages() {
    let state = fixture();
    let changes = [state.messages["followup"].clone()];
    let batch = crate::context_budget::classify(&state, &changes);
    assert_eq!(
        batch
            .fingerprints
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["followup"]
    );
    assert_eq!(
        batch.payload["context"][0]["text"],
        state.messages["material"].text
    );
    assert_eq!(batch.payload["context"][1]["id"], "ask");
    assert_eq!(batch.payload["context"][2]["id"], "request");
    assert_eq!(batch.payload["tasks"][0]["id"], "task");
    let combined = crate::context_budget::classify(
        &state,
        &[state.messages["material"].clone(), changes[0].clone()],
    );
    assert_eq!(combined.payload["context"].as_array().unwrap().len(), 2);
    assert!(!combined.payload["context"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "material"));
}

#[test]
fn reply_context_stops_at_source_chat_withdrawal_and_cycle_boundaries() {
    for boundary in ["source", "chat", "withdrawn", "missing"] {
        let mut state = fixture();
        match boundary {
            "source" => {
                state.messages.get_mut("material").unwrap().source_id = "other-source".into()
            }
            "chat" => state.messages.get_mut("material").unwrap().chat = "other-chat".into(),
            "withdrawn" => state.messages.get_mut("material").unwrap().withdrawn = true,
            _ => {
                state.messages.remove("material");
            }
        }
        let batch = crate::context_budget::classify(&state, &[state.messages["followup"].clone()]);
        assert!(batch.payload.get("context").is_none(), "{boundary}");
        assert_eq!(batch.allowed.len(), 1, "{boundary}");
        assert_eq!(batch.payload["reply_context_incomplete"], true);
    }
    let mut state = fixture();
    state.messages.get_mut("ask").unwrap().reply = "material".into();
    let batch = crate::context_budget::classify(&state, &[state.messages["followup"].clone()]);
    assert_eq!(batch.payload["context"].as_array().unwrap().len(), 2);
    assert_eq!(batch.payload["reply_context_incomplete"], true);
}

#[test]
fn reply_context_bounds_serialized_content_and_reports_unread_ancestors() {
    for text in ["短材料".to_owned(), "长材料🙂".repeat(800)] {
        let mut state = State::default();
        for i in 0..20 {
            let id = format!("chain-{i}");
            state.messages.insert(
                id.clone(),
                Message {
                    id,
                    source_id: "source".into(),
                    chat: "room".into(),
                    text: text.clone(),
                    reply: if i < 19 {
                        format!("chain-{}", i + 1)
                    } else {
                        String::new()
                    },
                    ..Default::default()
                },
            );
        }
        let batch = crate::context_budget::classify(&state, &[state.messages["chain-0"].clone()]);
        let rows = batch.payload["context"].as_array().unwrap();
        assert!(batch.payload["context"].to_string().chars().count() <= 8000);
        if text.chars().count() > 2000 {
            assert!(rows.len() < 19);
            assert_eq!(batch.payload["reply_context_incomplete"], true);
            assert!(rows
                .iter()
                .all(|m| m["truncated"] == true && m["original_chars"] == text.chars().count()));
        } else {
            assert_eq!(rows.len(), 19);
            assert!(batch.payload.get("reply_context_incomplete").is_none());
            let combined = crate::context_budget::classify(
                &state,
                &[
                    state.messages["chain-0"].clone(),
                    state.messages["chain-1"].clone(),
                ],
            );
            assert_eq!(combined.payload["context"].as_array().unwrap().len(), 18);
            assert!(combined.payload.get("reply_context_incomplete").is_none());
        }
        assert_eq!(batch.fingerprints.len(), 1);
    }
}

#[test]
fn old_analyzed_parent_remains_fully_retrievable_after_wire_preparation() {
    let mut state = fixture();
    let mut earliest = state.messages["request"].clone();
    earliest.id = "earliest".into();
    earliest.text = "更早的合成材料".repeat(400);
    state.messages.get_mut("request").unwrap().reply = earliest.id.clone();
    state.messages.insert(earliest.id.clone(), earliest);
    for id in ["request", "ask"] {
        state.messages.get_mut(id).unwrap().text = "合成历史说明".repeat(400);
    }
    for message in state.messages.values_mut().filter(|m| m.id != "followup") {
        message.time -= 3 * 86_400_000;
        message.updated_at = message.time;
    }
    state.messages.get_mut("material").unwrap().text = "合成完整材料🙂".repeat(1000);
    for message in state.messages.values().filter(|m| m.id != "followup") {
        state
            .analyzed
            .insert(message.id.clone(), fingerprint(message));
    }
    let (app, root) = app(state.clone());
    assert!(!crate::snapshots::records_from_state(&state, now())
        .iter()
        .any(|m| m["id"] == "material"));
    let (batch, request) = prepare(&app, &state, "followup");
    assert!(!batch.payload["context"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "earliest"));
    assert!(request.records.iter().any(|row| row["id"] == "earliest"));
    let mut reference = json!({"id":"material"});
    request.wire_ids.encode(&mut reference);
    let alias = reference["id"].as_str().unwrap();
    let columns = request.payload["message_cols"].as_array().unwrap();
    let id_col = columns.iter().position(|c| c == "id").unwrap();
    let reply_col = columns.iter().position(|c| c == "reply").unwrap();
    let truncated_col = columns.iter().position(|c| c == "truncated").unwrap();
    assert_eq!(request.payload["context"][0][id_col], alias);
    assert_eq!(request.payload["changes"][0][reply_col], alias);
    assert_eq!(request.payload["context"][0][truncated_col], true);
    let snapshot = &request.payload["_local_context"];
    let mut meter = crate::analysis_context::Meter::default();
    let mut text = String::new();
    let mut offset = 0;
    for _ in 0..10 {
        let raw = crate::analysis_context::query(
            snapshot,
            &mut meter,
            &json!({"ids":[alias],"offset":offset}),
        );
        assert!(raw.chars().count() <= crate::analysis_context::PAGE);
        let page: Value = serde_json::from_str(&raw).unwrap();
        let row = &page["records"][0];
        text.push_str(row["data"]["text"].as_str().unwrap());
        match row["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(text, state.messages["material"].text);
    assert!(meter.complete.contains(alias));
    let mut bounded = snapshot.clone();
    bounded["budget_chars"] = json!(crate::analysis_context::PAGE);
    let mut meter = crate::analysis_context::Meter::default();
    let first: Value = serde_json::from_str(&crate::analysis_context::query(
        &bounded,
        &mut meter,
        &json!({"ids":[alias]}),
    ))
    .unwrap();
    let next = first["records"][0]["next_offset"].clone();
    let second: Value = serde_json::from_str(&crate::analysis_context::query(
        &bounded,
        &mut meter,
        &json!({"ids":[alias],"offset":next}),
    ))
    .unwrap();
    assert_eq!(second["budget_exhausted"], true);
    assert!(!meter.complete.contains(alias));
    // Global budget fallback may omit every inline ancestor. Its complete
    // frozen record remains eligible only after an actual retrieval receipt.
    let mut payload = batch.payload.clone();
    payload.as_object_mut().unwrap().remove("context");
    payload["reply_context_incomplete"] = json!(true);
    let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
    let reduced = app
        .prepare_ai_request_with("classify", &payload, AiMode::Manual, &mut context)
        .unwrap();
    let initial = batch
        .allowed
        .into_iter()
        .filter(|(id, _)| batch.fingerprints.contains_key(id))
        .collect();
    let reduced_frozen = FrozenView::new(initial, batch.versions, &reduced.records);
    let mut allowed = reduced_frozen.initial_messages.clone();
    let action = update(json!(["followup", "material"]), "材料齐全，继续核对");
    let mut current = state.clone();
    assert_eq!(
        App::apply_actions(
            &mut current,
            &[action.clone()],
            &allowed,
            &reduced_frozen.task_versions,
            &[]
        )
        .unwrap(),
        0
    );
    admit_retrieved(&mut allowed, &[json!("material")], &reduced_frozen);
    assert_eq!(allowed["material"].text, state.messages["material"].text);
    assert_eq!(
        App::apply_actions(
            &mut current,
            &[action],
            &allowed,
            &reduced_frozen.task_versions,
            &[]
        )
        .unwrap(),
        1
    );
    let frozen = request.frozen.as_ref().unwrap();
    let mut live = state;
    live.messages.get_mut("material").unwrap().text = "并发新版本".into();
    live.tasks[0].updated += 1;
    let mut allowed = frozen.initial_messages.clone();
    let mut versions = frozen.task_versions.clone();
    retain_live_frozen(&mut allowed, &mut versions, frozen, &live);
    assert!(!allowed.contains_key("material"));
    assert!(!versions.contains_key("task"));
    let original = json!(live.tasks);
    assert_eq!(
        App::apply_actions(
            &mut live,
            &[update(json!(["followup", "material"]), "不能采用")],
            &allowed,
            &versions,
            &[]
        )
        .unwrap(),
        0
    );
    assert_eq!(json!(live.tasks), original);
    drop(app);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn multi_round_reply_progress_persists_and_matches_inbox_evidence() {
    let mut state = fixture();
    let followup = state.messages.remove("followup").unwrap();
    let (app, root) = app(state);
    for (id, description) in [
        ("material", "材料已收到，等待核对"),
        ("followup", "材料齐全，继续核对并回复进展"),
    ] {
        if id == "followup" {
            app.commit_state(|s| {
                s.messages.insert(id.into(), followup.clone());
                Ok(())
            })
            .unwrap();
        }
        let (batch, request) = prepare(&app, &app.state.lock().unwrap(), id);
        let sources = if id == "followup" {
            json!(["followup", "material"])
        } else {
            json!(["material"])
        };
        let frozen = request.frozen.as_ref().unwrap();
        app.commit_state(|s| {
            let count = App::apply_actions(
                s,
                &[update(sources, description)],
                &frozen.initial_messages,
                &frozen.task_versions,
                &[],
            )?;
            assert_eq!(count, 1);
            App::finish_message_batch(s, &batch.fingerprints, &BTreeSet::new());
            // Advancing the watermark records older unprocessed messages as
            // pending:<fingerprint> holes in the analyzed map. A map entry is
            // not an acknowledgement; quoted ancestors must remain pending.
            for parent in ["request", "ask"] {
                assert!(!crate::message_progress::analyzed(s, &s.messages[parent]));
                assert!(crate::message_progress::pending_revision(
                    &s.messages[parent],
                    s.analyzed.get(parent)
                ));
                assert!(!crate::message_progress::filtered(s, &s.messages[parent]));
            }
            Ok(())
        })
        .unwrap();
    }
    let before = json!(app.state.lock().unwrap().tasks);
    let (batch, request) = prepare(&app, &app.state.lock().unwrap(), "followup");
    // A confirmation with no action is valid: context does not force a write.
    app.commit_state(|s| {
        assert_eq!(
            App::apply_actions(
                s,
                &[],
                &request.frozen.as_ref().unwrap().initial_messages,
                &batch.versions,
                &[]
            )?,
            0
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(json!(app.state.lock().unwrap().tasks), before);
    drop(app);
    let reopened = App::load(root.clone()).unwrap();
    let state = reopened.state.lock().unwrap();
    assert!(state.tasks[0].waiting_for.is_empty());
    assert!(state.tasks[0].evidence.direct.contains(&"material".into()));
    assert!(state.tasks[0].evidence.direct.contains(&"followup".into()));
    for parent in ["request", "ask"] {
        assert!(!crate::message_progress::analyzed(
            &state,
            &state.messages[parent]
        ));
        assert!(crate::message_progress::pending_revision(
            &state.messages[parent],
            state.analyzed.get(parent)
        ));
        assert!(!crate::message_progress::filtered(
            &state,
            &state.messages[parent]
        ));
    }
    for processed in ["material", "followup"] {
        assert!(crate::message_progress::analyzed(
            &state,
            &state.messages[processed]
        ));
    }
    drop(state);
    let inbox = reopened.query_messages(&json!({"page_size":20})).unwrap();
    let evidence = reopened.evidence_query(&json!({"id":"task"})).unwrap();
    for id in ["material", "followup"] {
        let message = inbox["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        let proof = evidence["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(message["text"], proof["text"]);
    }
    drop(reopened);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn reservation_observes_only_changes_even_when_ancestors_are_inlined() {
    let mut state = fixture();
    let (app, root) = app(state.clone());
    state.config.codex = root
        .join("nonexistent-synthetic-provider.exe")
        .to_string_lossy()
        .into_owned();
    app.state.lock().unwrap().config.codex = state.config.codex.clone();
    let (_, request) = prepare(&app, &state, "followup");
    // The deliberately missing executable fails locally after reservation.
    // No provider process, network connection or real model call can occur.
    assert!(app.invoke_ai_request(&request, AiMode::Manual).is_err());
    let state = app.state.lock().unwrap();
    assert_eq!(state.calls, 1);
    assert!(crate::message_progress::filtered(
        &state,
        &state.messages["followup"]
    ));
    for id in ["material", "ask", "request"] {
        assert!(!crate::message_progress::filtered(
            &state,
            &state.messages[id]
        ));
        assert!(!state.analyzed.contains_key(id));
    }
    drop(state);
    drop(app);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn tight_global_budget_drops_optional_context_before_rejecting_the_delta() {
    let mut state = fixture();
    state.config.daily_calls = 0;
    for message in state.messages.values_mut().filter(|m| m.id != "followup") {
        message.text = "合成历史材料".repeat(500);
    }
    for message in state.messages.values().filter(|m| m.id != "followup") {
        state
            .analyzed
            .insert(message.id.clone(), fingerprint(message));
    }
    let (app, root) = app(state.clone());
    let (mut batch, full) = prepare(&app, &state, "followup");
    batch.payload.as_object_mut().unwrap().remove("context");
    batch.payload["reply_context_incomplete"] = json!(true);
    let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
    let reduced = app
        .prepare_ai_request_with("classify", &batch.payload, AiMode::Manual, &mut context)
        .unwrap();
    let limit = reduced.prepared.total + 300;
    assert!(limit < full.prepared.total);
    app.state.lock().unwrap().config.max_input_chars = limit;
    // Reaching the quota boundary proves the real analysis loop accepted the
    // reduced exact request without allowing any provider invocation.
    let error = app.analyze(AiMode::Manual).unwrap_err();
    assert!(error.contains("每日AI调用上限"), "{error}");
    assert_eq!(app.state.lock().unwrap().calls, 0);
    assert_eq!(app.state.lock().unwrap().analyzed, state.analyzed);
    // Wire compaction may retain a null key; only delivered rows count as
    // inline context, independently of that serialization detail.
    assert!(reduced.payload["context"]
        .as_array()
        .is_none_or(Vec::is_empty));
    assert_eq!(reduced.payload["reply_context_incomplete"], true);
    drop(app);
    let _ = fs::remove_dir_all(root);
}
