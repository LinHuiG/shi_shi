//! Reversible identity compression and provenance-aware display of leaked aliases.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct IdMap {
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
}
fn collect_evidence_ids(task: &Value, messages: &mut BTreeSet<String>) {
    if let Some(evidence) = task.get("evidence") {
        for role in ["direct", "background", "completion"] {
            for id in evidence[role].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str() {
                    messages.insert(id.to_owned());
                }
            }
        }
    }
    // Background/completion arrays can also appear directly on an action-like
    // row; include them so aliases always have a reverse mapping.
    for key in [
        "background_evidence",
        "completion_evidence",
        "deadline_evidence",
        "sources",
    ] {
        for id in task[key].as_array().into_iter().flatten() {
            if let Some(id) = id.as_str() {
                messages.insert(id.to_owned());
            }
        }
    }
}

impl IdMap {
    /// Only known transport aliases in user-facing prose are resolved. Genuine
    /// source tokens (for example a product code) are never rewritten.
    pub fn humanize_prose(&self, value: &mut Value, records: &[Value]) {
        let mut names = BTreeMap::new();
        for (alias, original) in &self.reverse {
            if records.iter().any(|r| {
                r["data"]["text"].as_str().is_some_and(|t| {
                    t.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .any(|v| v == alias)
                })
            }) {
                continue;
            }
            let record = records
                .iter()
                .find(|r| r["id"] == *original || r["data"]["id"] == *original);
            let name = record
                .and_then(|r| {
                    if alias.starts_with('m') {
                        r["data"]["sender"].as_str()
                    } else {
                        r["data"]["title"].as_str()
                    }
                })
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(if alias.starts_with('m') {
                    "相关消息"
                } else {
                    "相关事项"
                });
            names.insert(alias.clone(), name.to_owned());
        }
        fn walk(v: &mut Value, key: &str, names: &BTreeMap<String, String>) {
            match v {
                Value::String(text)
                    if [
                        "title",
                        "description",
                        "summary",
                        "next_step",
                        "blocker",
                        "waiting_for",
                        "reason",
                    ]
                    .contains(&key) =>
                {
                    let mut out = String::new();
                    let mut token = String::new();
                    for c in text.chars().chain(std::iter::once('\0')) {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            token.push(c);
                        } else {
                            out.push_str(names.get(&token).map(String::as_str).unwrap_or(&token));
                            token.clear();
                            if c != '\0' {
                                out.push(c);
                            }
                        }
                    }
                    *text = out;
                }
                Value::Array(rows) => {
                    for row in rows {
                        walk(row, key, names)
                    }
                }
                Value::Object(obj) => {
                    for (key, row) in obj {
                        walk(row, key, names)
                    }
                }
                _ => {}
            }
        }
        walk(value, "", &names);
    }
    pub fn extend_from(&mut self, newer: Self) {
        for (original, candidate) in newer.forward {
            if self.forward.contains_key(&original) {
                continue;
            }
            let prefix = if candidate.starts_with('t') { "t" } else { "m" };
            let mut n = 1;
            while self.reverse.contains_key(&format!("{prefix}{n}")) {
                n += 1;
            }
            let alias = format!("{prefix}{n}");
            self.forward.insert(original.clone(), alias.clone());
            self.reverse.insert(alias, original);
        }
    }
    pub fn from_payload(payload: &Value) -> Self {
        let mut messages = BTreeSet::new();
        let mut tasks = BTreeSet::new();
        for key in ["changes", "context", "messages"] {
            for row in payload[key].as_array().into_iter().flatten() {
                if let Some(id) = row["id"].as_str() {
                    messages.insert(id.to_owned());
                }
            }
        }
        for key in ["tasks", "task_index"] {
            for row in payload[key].as_array().into_iter().flatten() {
                if let Some(id) = row["id"].as_str() {
                    tasks.insert(id.to_owned());
                }
                collect_evidence_ids(row, &mut messages);
            }
        }
        if let Some(id) = payload["task"]["id"].as_str() {
            tasks.insert(id.to_owned());
        }
        collect_evidence_ids(&payload["task"], &mut messages);
        for record in payload["_local_context"]["records"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = record["id"].as_str() {
                match record["kind"].as_str() {
                    Some("message") => {
                        messages.insert(id.to_owned());
                    }
                    Some("task") => {
                        tasks.insert(id.to_owned());
                    }
                    _ => {}
                }
            }
            if record["kind"] == "task" {
                collect_evidence_ids(&record["data"], &mut messages);
            }
        }
        Self::from_ids(messages, tasks)
    }
    pub fn from_ids(messages: BTreeSet<String>, tasks: BTreeSet<String>) -> Self {
        let all = messages.union(&tasks).cloned().collect::<BTreeSet<_>>();
        let mut result = Self::default();
        for (prefix, ids) in [("m", messages), ("t", tasks)] {
            let mut n = 1;
            for id in ids.into_iter().filter(|id| !id.is_empty()) {
                if result.forward.contains_key(&id) {
                    continue;
                }
                while all.contains(&format!("{prefix}{n}")) {
                    n += 1;
                }
                let alias = format!("{prefix}{n}");
                n += 1;
                result.forward.insert(id.clone(), alias.clone());
                result.reverse.insert(alias, id);
            }
        }
        result
    }
    pub fn encode(&self, value: &mut Value) {
        rewrite(value, "", &self.forward);
    }
    pub fn decode(&self, value: &mut Value) -> Result<(), String> {
        // Creates have no existing target. The application allocates their ID;
        // a model-supplied placeholder is not an evidence reference.
        for action in value["actions"].as_array_mut().into_iter().flatten() {
            if action["action"] == "create" && action["task_id"].is_string() {
                action["task_id"] = json!("");
            }
        }
        if !self.reverse.is_empty() {
            validate_aliases(value, "", &self.reverse)?;
        }
        rewrite(value, "", &self.reverse);
        Ok(())
    }
}
fn validate_aliases(
    value: &Value,
    key: &str,
    map: &BTreeMap<String, String>,
) -> Result<(), String> {
    match value {
        Value::String(s) if reference(key) => {
            if s.len() > 1
                && matches!(s.as_bytes()[0], b'm' | b't')
                && s.as_bytes()[1..].iter().all(u8::is_ascii_digit)
                && !map.contains_key(s)
            {
                return Err("AI 返回未知证据短编号，本次结果未应用".into());
            }
        }
        Value::Array(rows) => {
            for row in rows {
                validate_aliases(row, key, map)?;
            }
        }
        Value::Object(obj) => {
            for (key, row) in obj {
                validate_aliases(row, key, map)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn reference(key: &str) -> bool {
    matches!(
        key,
        "id" | "task_id"
            | "message_id"
            | "reply"
            | "sources"
            | "direct"
            | "background"
            | "completion"
            | "evidence"
            | "background_evidence"
            | "completion_evidence"
            | "deadline_evidence"
            | "ids"
            | "evidence_ids"
            | "_context_seen"
    )
}
fn rewrite(value: &mut Value, key: &str, map: &BTreeMap<String, String>) {
    match value {
        Value::String(s) if reference(key) => {
            if let Some(to) = map.get(s) {
                *s = to.clone();
            }
        }
        Value::Array(rows) => {
            for row in rows {
                rewrite(row, key, map)
            }
        }
        Value::Object(obj) => {
            for (key, row) in obj {
                rewrite(row, key, map)
            }
        }
        _ => {}
    }
}
pub fn compact(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("retrieval");
    }
    clean(payload);
    let mut rooms = Vec::new();
    let mut room_ids = BTreeMap::new();
    if payload.get("rooms").is_none() {
        for key in ["changes", "context", "messages"] {
            for row in payload[key].as_array_mut().into_iter().flatten() {
                if row.get("room").is_none() && row.get("chat").is_some() {
                    let pair = (row["source_id"].clone(), row["chat"].clone());
                    let key = serde_json::to_string(&pair).unwrap();
                    let id = room_ids
                        .entry(key)
                        .or_insert_with(|| {
                            let id = format!("r{}", rooms.len() + 1);
                            rooms.push(json!({"id":id,"source_id":pair.0,"chat":pair.1}));
                            id
                        })
                        .clone();
                    row.as_object_mut().unwrap().remove("source_id");
                    row.as_object_mut().unwrap().remove("chat");
                    row["room"] = json!(id);
                }
            }
        }
        if !rooms.is_empty() {
            payload["rooms"] = json!(rooms);
        }
    }
    let mut columns = vec![
        "id".to_string(),
        "room".into(),
        "sender".into(),
        "time".into(),
        "text".into(),
    ];
    let mut extra = BTreeSet::new();
    let mut any = false;
    for key in ["changes", "context", "messages"] {
        for row in payload[key].as_array().into_iter().flatten() {
            if let Some(obj) = row.as_object() {
                any = true;
                for key in obj.keys() {
                    if !columns.contains(key) {
                        extra.insert(key.clone());
                    }
                }
            }
        }
    }
    if !any {
        return;
    }
    columns.extend(extra);
    // Omit globally unused columns, but retain null cells to preserve alignment.
    columns.retain(|col| {
        ["changes", "context", "messages"].iter().any(|key| {
            payload[*key]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| row.get(col).is_some())
        })
    });
    for key in ["changes", "context", "messages"] {
        if let Some(rows) = payload[key].as_array_mut() {
            for row in rows {
                if row.is_object() {
                    *row = json!(columns.iter().map(|k| row[k].clone()).collect::<Vec<_>>());
                }
            }
        }
    }
    payload["message_cols"] = json!(columns);
}
fn clean(value: &mut Value) {
    match value {
        Value::Array(rows) => {
            for row in rows {
                clean(row)
            }
        }
        Value::Object(obj) => {
            obj.remove("match_rank");

            for (key, row) in obj.iter_mut() {
                // Private snapshots are validation state, not wire data. Empty
                // baselines and arrays have meaning and must survive unchanged.
                if !key.starts_with('_') {
                    clean(row);
                }
            }
            obj.retain(|key, v| {
                key.starts_with('_')
                    || (!v.is_null()
                        && v.as_str() != Some("")
                        && !v.as_array().is_some_and(|a| a.is_empty())
                        && !v.as_object().is_some_and(|a| a.is_empty()))
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prose_aliases_use_real_senders_without_touching_references_or_source_codes() {
        let payload = json!({"changes":[{"id":"msg-a","sender":"示例发言人","text":"请核对资料"},{"id":"msg-b","sender":"另一发言人","text":"m2 是产品代码"}]});
        let ids = IdMap::from_payload(&payload);
        let records = vec![
            json!({"id":"msg-a","data":payload["changes"][0]}),
            json!({"id":"msg-b","data":payload["changes"][1]}),
        ];
        let alias = ids.forward["msg-a"].clone();
        let mut result = json!({"actions":[{"description":format!("{alias} 提供资料；model-m1 保持原样；m2 是产品代码"),"evidence":{"direct":[alias]}}]});
        ids.humanize_prose(&mut result, &records);
        assert!(result["actions"][0]["description"]
            .as_str()
            .unwrap()
            .starts_with("示例发言人"));
        assert!(result["actions"][0]["description"]
            .as_str()
            .unwrap()
            .contains("model-m1"));
        assert!(result["actions"][0]["description"]
            .as_str()
            .unwrap()
            .contains("m2 是产品代码"));
        ids.decode(&mut result).unwrap();
        assert_eq!(result["actions"][0]["evidence"]["direct"][0], "msg-a");
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
        assert_eq!(
            input["_local_context"]["records"][0]["id"],
            input["changes"][0]["id"]
        );
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
        let mut create =
            json!({"actions":[{"action":"create","task_id":"t999","sources":[source]}]});
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
        assert!(map
            .decode(&mut json!({"evidence":{"direct":["m999"]}}))
            .is_err());
    }

    #[test]
    fn reduces_repeated_fields_without_dropping_distinct_evidence() {
        let mut p = json!({"changes":(0..30).map(|i|json!({"id":format!("01234567-89ab-4def-8123-{i:012}"),"room":"r1","sender":"协作者","time":"2026-10-16 10:01:02","text":"未完成，截止17:30","reply":"","match_rank":7})).collect::<Vec<_>>(),"retrieval":{"omitted_candidates":50}});
        let before = p.to_string().len();
        let ids = IdMap::from_payload(&p);
        ids.encode(&mut p);
        compact(&mut p);
        assert_eq!(p["changes"].as_array().unwrap().len(), 30);
        assert!(p.to_string().len() < before * 3 / 4);
        assert!(p.get("retrieval").is_none());
        println!("wire fixture bytes: {before} -> {}", p.to_string().len());
    }
    #[test]
    fn pagination_aliases_remain_stable_and_unknown_proof_is_rejected() {
        let mut ids = IdMap::from_ids(["old".into()].into_iter().collect(), BTreeSet::new());
        let mut first = json!({"messages":[{"id":"old","text":"不可完成"}]});
        ids.encode(&mut first);
        ids.extend_from(IdMap::from_ids(
            ["new".into(), "old".into()].into_iter().collect(),
            BTreeSet::new(),
        ));
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
}
