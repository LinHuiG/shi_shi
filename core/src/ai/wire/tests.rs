use super::*;
#[test]
fn large_prose_lookup_preserves_first_record_fallbacks_and_parallel_isolation() {
    let ids =
        IdMap::from_ids((0..5000).map(|i| format!("message-{i:04}")).collect(), ["task".into()].into_iter().collect());
    let records = (0..5000).map(|i| json!({"id":format!("message-{i:04}"),"data":{"sender":format!("Sender {i}"),"text":"Synthetic body ".repeat(20)}})).collect::<Vec<_>>();
    let mut records = records;
    records.push(json!({"data":{"id":"task","title":"Task title"}}));
    records.push(
        json!({"id":"message-0000","data":{"sender":"Later duplicate","text":"m2-x and m3 are real source codes"}}),
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let mut empty = json!({"actions":[],"sources":["m1"]});
                let before = empty.clone();
                ids.humanize_prose(&mut empty, &records);
                assert_eq!(empty, before);
                let mut result = json!({"summary":["m1 / m2 / m5000 / t1 / model-m1 / m1_x"],"sources":["m1"],"actions":[{"description":"m1 / m3 / model-m1 / m1_x","evidence":{"direct":["m1"]}}]});
                ids.humanize_prose(&mut result, &records);
                assert_eq!(result["summary"][0], "Sender 0 / m2 / Sender 4999 / Task title / model-m1 / m1_x");
                assert_eq!(result["sources"][0], "m1");
                assert_eq!(result["actions"][0]["description"], "Sender 0 / m3 / model-m1 / m1_x");
                assert_eq!(result["actions"][0]["evidence"]["direct"][0], "m1");
                ids.decode(&mut result).unwrap();
                assert_eq!(result["actions"][0]["evidence"]["direct"][0], "message-0000");
            });
        }
    });
    let mut fallback = json!({"reason":"m1 t1"});
    ids.humanize_prose(&mut fallback, &[]);
    assert_eq!(fallback["reason"], "相关消息 相关事项");
}

#[test]
fn large_alias_extensions_preserve_existing_ids_and_fill_gaps_per_prefix() {
    let mut ids =
        IdMap::from_ids((0..4000).map(|i| format!("old-{i:04}")).collect(), ["old-task".into()].into_iter().collect());
    let before = ids.forward.clone();
    ids.extend_from(IdMap::from_ids(
        (0..4000).map(|i| format!("new-{i:04}")).collect(),
        ["new-task".into()].into_iter().collect(),
    ));
    for (original, alias) in before {
        assert_eq!(ids.forward[&original], alias);
    }
    assert_eq!(ids.forward["new-0000"], "m4001");
    assert_eq!(ids.forward["new-3999"], "m8000");
    assert_eq!(ids.forward["new-task"], "t2");
    assert_eq!(ids.forward.len(), ids.reverse.len());
}

#[test]
fn identities_roundtrip_without_rewriting_facts_or_reply_text() {
    let id = "01234567-89ab-4def-8123-456789abcdef";
    let task = "11234567-89ab-4def-8123-456789abcdef";
    let mut input = json!({"changes":[{"id":id,"text":format!("账号000123，节点2；17:30前，不可结单。{id}"),"reply":"m1","time":"2026-10-16 17:29:59"},{"id":"m1","text":"否"}],"tasks":[{"id":task,"status":"待处理"}],"_local_context":{"records":[{"id":id,"kind":"message","data":{"id":id,"text":"原文","reply":"m1"}}]}});
    let original = input.clone();
    let map = IdMap::from_payload(&input);
    map.encode(&mut input);
    assert_eq!(input["changes"][0]["text"], original["changes"][0]["text"]);
    assert_ne!(input["changes"][0]["id"], "m1");
    assert_eq!(input["_local_context"]["records"][0]["id"], input["changes"][0]["id"]);
    let mut output = json!({"actions":[{"task_id":input["tasks"][0]["id"],"sources":[input["changes"][0]["id"]],"completion_evidence":[input["changes"][1]["id"]]}],"_context_seen":[input["changes"][0]["id"]]});
    map.decode(&mut output).unwrap();
    assert_eq!(output["actions"][0]["task_id"], task);
    assert_eq!(output["actions"][0]["sources"][0], id);
    assert_eq!(output["actions"][0]["completion_evidence"][0], "m1");
    assert_eq!(output["_context_seen"][0], id);
    compact(&mut input);
    let cols = input["message_cols"].as_array().unwrap();
    let text = cols.iter().position(|c| c == "text").unwrap();
    let time = cols.iter().position(|c| c == "time").unwrap();
    assert_eq!(input["changes"][0][text], original["changes"][0]["text"]);
    assert_eq!(input["changes"][0][time], original["changes"][0]["time"]);
}
#[test]
fn create_placeholders_are_not_existing_targets_but_evidence_is_still_checked() {
    let payload = json!({"changes":[{"id":"message-real","text":"Synthetic request"}],"tasks":[{"id":"task-real"}]});
    let ids = IdMap::from_payload(&payload);
    let mut wire = payload.clone();
    ids.encode(&mut wire);
    let source = wire["changes"][0]["id"].clone();
    let mut create = json!({"actions":[{"action":"create","task_id":"t999","sources":[source]}]});
    ids.decode(&mut create).unwrap();
    assert_eq!(create["actions"][0]["task_id"], "");
    assert_eq!(create["actions"][0]["sources"][0], "message-real");
    for result in [
        json!({"actions":[{"action":"update","task_id":"t999","sources":[source]}]}),
        json!({"actions":[{"action":"create","task_id":"t999","sources":["m999"]}]}),
        json!({"actions":[{"action":"create","task_id":"t999","sources":[source],"background_evidence":["m999"]}]}),
    ] {
        assert!(ids.decode(&mut result.clone()).is_err());
    }
}
#[test]
fn evidence_roles_and_memory_sources_use_the_same_reversible_aliases() {
    let message = "01234567-89ab-4def-8123-456789abcdef";
    let task = "11234567-89ab-4def-8123-456789abcdef";
    let payload = json!({
        "changes":[{"id":message,"text":"原文"}],
        "tasks":[{"id":task,"evidence":{"direct":[message],"background":[],"completion":[]}}],
        "_local_context":{"records":[
            {"id":message,"kind":"message","data":{"text":"原文"}},
            {"id":task,"kind":"task","data":{"id":task,"evidence":{"direct":[message]}}}
        ]}
    });
    let map = IdMap::from_payload(&payload);
    let mut input = payload.clone();
    map.encode(&mut input);
    let alias_message = input["changes"][0]["id"].as_str().unwrap().to_string();
    let alias_task = input["tasks"][0]["id"].as_str().unwrap().to_string();
    assert_ne!(alias_message, message);
    let mut output = json!({
        "actions":[{
            "task_id":alias_task,
            "sources":[alias_message],
            "background_evidence":[alias_message],
            "completion_evidence":[alias_message],
            "evidence":{"direct":[alias_message],"background":[alias_message],"completion":[alias_message]}
        }]
    });
    map.decode(&mut output).unwrap();
    assert_eq!(output["actions"][0]["task_id"], task);
    assert_eq!(output["actions"][0]["evidence"]["direct"][0], message);
    assert_eq!(output["actions"][0]["evidence"]["completion"][0], message);
    assert!(map.decode(&mut json!({"evidence":{"direct":["m999"]}})).is_err());
}

#[test]
fn reduces_repeated_fields_without_dropping_distinct_evidence() {
    let mut p = json!({"changes":(0..30).map(|i|json!({"id":format!("01234567-89ab-4def-8123-{i:012}"),"room":"r1","sender":"协作者","time":"2026-10-16 10:01:02","text":"未完成，截止17:30","reply":"","match_rank":7})).collect::<Vec<_>>(),"retrieval":{"omitted_candidates":50}});
    p["context"] = json!([{"id":"context","extra":{"key":"value"}}, {}]);
    let before = p.to_string().len();
    let ids = IdMap::from_payload(&p);
    ids.encode(&mut p);
    compact(&mut p);
    assert_eq!(p["changes"].as_array().unwrap().len(), 30);
    assert_eq!(p["message_cols"], json!(["id", "room", "sender", "time", "text", "extra"]));
    assert_eq!(p["context"][0][5], json!({"key":"value"}));
    assert_eq!(p["context"][1], json!([null, null, null, null, null, null]));
    assert_eq!(p["changes"][0][5], Value::Null);
    assert!(p.to_string().len() < before * 3 / 4);
    assert!(p.get("retrieval").is_none());
    println!("wire fixture bytes: {before} -> {}", p.to_string().len());
}
#[test]
fn pagination_aliases_remain_stable_and_unknown_proof_is_rejected() {
    let mut ids = IdMap::from_ids(["old".into()].into_iter().collect(), BTreeSet::new());
    let mut first = json!({"messages":[{"id":"old","text":"不可完成"}]});
    ids.encode(&mut first);
    ids.extend_from(IdMap::from_ids(["new".into(), "old".into()].into_iter().collect(), BTreeSet::new()));
    let mut second = json!({"messages":[{"id":"old","text":"不可完成"}]});
    ids.encode(&mut second);
    assert_eq!(first, second);
    assert!(ids.decode(&mut json!({"sources":["m999"]})).is_err());
    let mut task = json!({"category":"日常事务","environment":"未指定","tags":[{"kind":"事项类型","value":"日常事务"},{"kind":"适用环境","value":"未指定"},{"kind":"自定义","value":"保留"}],"due":"17:30","blocker":"缺少材料，不可完成"});
    compact(&mut task);
    assert_eq!(task["tags"].as_array().unwrap().len(), 3);
    assert_eq!(task["due"], "17:30");
    assert_eq!(task["blocker"], "缺少材料，不可完成");
}
