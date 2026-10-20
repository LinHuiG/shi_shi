//! Rebuildable, read-only analysis snapshots derived from the authoritative
//! `state.json`. They are a retrieval accelerator, never a second source of
//! truth: any missing, stale or corrupt snapshot is rebuilt from source.
use crate::model::{Audit, Message, State, Task};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 1;
pub const RETENTION_MS: i64 = 86_400_000;
pub const DIR: &str = "data/analysis-snapshots";
#[cfg(test)]
pub const KINDS: [&str; 3] = ["messages", "tasks", "updates"];

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Header {
    pub kind: String,
    pub schema_version: u32,
    pub generated_at: i64,
    pub data_until: i64,
    pub source_revision: u64,
    pub count: usize,
    pub checksum: String,
    /// Digest of the rows rebuilt from the live source. A snapshot whose file
    /// checksum was recalculated after an external edit still fails this check
    /// and is rebuilt before it can become archive material.
    pub source_digest: String,
}

#[derive(Debug)]
pub struct SnapshotSet {
    #[allow(dead_code)]
    pub headers: Vec<Header>,
    /// Records projected from the verified rows, ready for the read-only tool.
    /// Callers should reuse these instead of re-reading the files.
    pub records: Vec<Value>,
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

pub fn path(root: &Path, kind: &str) -> PathBuf {
    dir(root).join(format!("{kind}.jsonl"))
}

/// FNV-1a over the exact body bytes. This is an integrity check for rebuild
/// detection, not a cryptographic signature.
fn checksum(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn message_row(m: &Message) -> Value {
    let mut row = json!(m);
    row["truncated"] = json!(false);
    row["original_chars"] = json!(m.text.chars().count());
    row
}
fn task_row(t: &Task) -> Value {
    crate::context_budget::task_detail(t)
}
fn task_update_kind(a: &Audit) -> bool {
    !a.task_id.is_empty()
        && [
            "ai_create",
            "ai_update",
            "ai_evidence",
            "ai_merge_duplicate",
            "manual_edit",
            "manual_ai_edit",
            "manual_status",
            "manual_archive",
            "merge",
            "undo",
            "archive_item",
            "deadline_update",
            "generation_done",
        ]
        .contains(&a.kind.as_str())
}

/// Only fields that exist in the current task schema may enter a derived update
/// snapshot. General settings, prompt bodies, logs and credentials never do.
const TASK_UPDATE_FIELDS: [&str; 36] = [
    "decision_context",
    "suggested_project_id",
    "suggested_project_reason",
    "tags",
    "id",
    "title",
    "description",
    "summary",
    "content_updated_at",
    "summary_updated_at",
    "summary_fallback",
    "evidence",
    "priority",
    "environment",
    "category",
    "generation",
    "generation_error",
    "status",
    "blocker",
    "waiting_for",
    "next_step",
    "due",
    "created",
    "updated",
    "archived",
    "archive_note",
    "archived_at",
    "manual",
    "locked",
    "reason",
    "merged_into",
    "follow_deadline",
    "deadline_changed",
    "deferred_until",
    "schedule_id",
    "scheduled_for",
];

fn whitelisted_task(value: &Value, task_id: &str) -> Value {
    let selected = match value {
        Value::Array(rows) => rows
            .iter()
            .find(|row| row["id"].as_str() == Some(task_id))
            .cloned(),
        Value::Object(_) => Some(value.clone()),
        _ => None,
    };
    let Some(object) = selected.as_ref().and_then(Value::as_object) else {
        return json!({});
    };
    let mut result = serde_json::Map::new();
    for field in TASK_UPDATE_FIELDS {
        if let Some(value) = object.get(field) {
            result.insert(field.into(), value.clone());
        }
    }
    Value::Object(result)
}

fn update_row(index: usize, a: &Audit) -> Value {
    // A long note, description or other whitelisted field must remain fully
    // retrievable even when the task itself has been archived. The pageable
    // document holds the complete whitelisted before/after projection; only
    // small field/identity metadata accompanies it on every page.
    let before = whitelisted_task(&a.before, &a.task_id);
    let after = whitelisted_task(&a.after, &a.task_id);
    let fields = TASK_UPDATE_FIELDS
        .iter()
        .filter(|field| before.get(*field) != after.get(*field))
        .map(|field| json!(*field))
        .collect::<Vec<_>>();
    let document = json!({
        "task_id":a.task_id,
        "audit_kind":a.kind,
        "time":a.time,
        "before":before,
        "after":after,
    });
    json!({
        "index":index,
        "evidence_id":format!("audit:{index}"),
        "time":a.time,
        "task_id":a.task_id,
        "kind":a.kind,
        "fields":fields,
        "detail":document.to_string(),
    })
}

fn referenced_messages(s: &State) -> BTreeSet<String> {
    s.tasks
        .iter()
        .filter(|t| !t.archived)
        .flat_map(|t| t.evidence.all())
        .collect()
}

/// Snapshot scope:
/// - messages: recent (24h), referenced by an unarchived task, or not yet analyzed.
/// - tasks: every unarchived task, including old completed tasks.
/// - updates: recent records and every record after the shared archive cursor.
pub fn freeze(s: &State) -> State {
    State {
        messages: s.messages.clone(),
        tasks: s.tasks.iter().filter(|t| !t.archived).cloned().collect(),
        analyzed: s.analyzed.clone(),
        audit: s.audit.clone(),
        archive_ai_cursor: s.archive_ai_cursor,
        last_scan: s.last_scan,
        revision: s.revision,
        ..State::default()
    }
}
pub fn build(s: &State, at: i64) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let cutoff = at - RETENTION_MS;
    let referenced = referenced_messages(s);
    let messages = s
        .messages
        .values()
        .filter(|m| {
            !m.withdrawn
                && (m.time >= cutoff
                    || referenced.contains(&m.id)
                    || s.analyzed.get(&m.id) != Some(&crate::model::fingerprint(m)))
        })
        .map(message_row)
        .collect::<Vec<_>>();
    let tasks = s
        .tasks
        .iter()
        .filter(|t| !t.archived)
        .map(task_row)
        .collect::<Vec<_>>();
    let archive_start = s.archive_ai_cursor.unwrap_or(0).min(s.audit.len());
    let updates = s
        .audit
        .iter()
        .enumerate()
        .filter(|(index, a)| task_update_kind(a) && (a.time >= cutoff || *index >= archive_start))
        .map(|(index, a)| update_row(index, a))
        .collect::<Vec<_>>();
    (messages, tasks, updates)
}

fn serialize(kind: &str, rows: &[Value], s: &State, at: i64) -> Result<(Header, Vec<u8>), String> {
    let body = rows
        .iter()
        .map(|row| {
            serde_json::to_string(row)
                .map(|line| format!("{line}\n"))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<String, String>>()?;
    let digest = checksum(body.as_bytes());
    let header = Header {
        kind: kind.into(),
        schema_version: SCHEMA_VERSION,
        generated_at: at,
        data_until: s.last_scan,
        source_revision: s.revision,
        count: rows.len(),
        checksum: digest.clone(),
        source_digest: digest,
    };
    Ok((header, body.into_bytes()))
}

fn write_rows(
    root: &Path,
    s: &State,
    at: i64,
    rows: [(&str, Vec<Value>); 3],
) -> Result<SnapshotSet, String> {
    let directory = dir(root);
    fs::create_dir_all(&directory).map_err(|e| format!("分析快照目录不可写：{e}"))?;
    let index = s
        .messages
        .iter()
        .map(|(id, m)| (id.clone(), (m.source_id.clone(), m.chat.clone())))
        .collect::<BTreeMap<_, _>>();
    let records = project_records(&rows[0].1, &rows[1].1, &rows[2].1, &index);
    let mut headers = Vec::new();
    for (kind, rows) in rows {
        let (header, body) = serialize(kind, &rows, s, at)?;
        let header_line =
            serde_json::to_string(&header).map_err(|e| format!("分析快照元数据无效：{e}"))?;
        let target = path(root, kind);
        let temp = directory.join(format!("{kind}.jsonl.next"));
        let mut data = header_line.into_bytes();
        data.push(b'\n');
        data.extend_from_slice(&body);
        fs::write(&temp, data).map_err(|e| format!("分析快照写入失败：{e}"))?;
        fs::rename(&temp, &target).map_err(|e| {
            let _ = fs::remove_file(&temp);
            format!("分析快照提交失败：{e}")
        })?;
        headers.push(header);
    }
    // Verify that what the model can read matches the committed headers. A
    // failed verification is an incomplete rebuild, never a silent partial set.
    for header in &headers {
        let loaded = read_header(root, &header.kind)
            .ok_or_else(|| format!("分析快照 {} 重建后不可读", header.kind))?;
        if loaded != *header {
            return Err(format!("分析快照 {} 重建校验失败", header.kind));
        }
    }
    Ok(SnapshotSet { headers, records })
}

pub fn read_header(root: &Path, kind: &str) -> Option<Header> {
    let text = fs::read_to_string(path(root, kind)).ok()?;
    let first = text.lines().next()?;
    let header: Header = serde_json::from_str(first).ok()?;
    (header.kind == kind && header.schema_version == SCHEMA_VERSION).then_some(header)
}

fn read_verified(root: &Path, kind: &str) -> Result<(Header, Vec<Value>), String> {
    let text =
        fs::read_to_string(path(root, kind)).map_err(|e| format!("分析快照 {kind} 不可读：{e}"))?;
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| format!("分析快照 {kind} 缺少元数据"))?;
    let header: Header = serde_json::from_str(header_line)
        .map_err(|e| format!("分析快照 {kind} 元数据损坏：{e}"))?;
    if header.kind != kind || header.schema_version != SCHEMA_VERSION {
        return Err(format!("分析快照 {kind} 版本不匹配"));
    }
    let body = lines.map(|line| format!("{line}\n")).collect::<String>();
    let digest = checksum(body.as_bytes());
    if digest != header.checksum || digest != header.source_digest {
        return Err(format!("分析快照 {kind} 校验失败"));
    }
    let mut rows = Vec::with_capacity(header.count);
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str::<Value>(line)
                .map_err(|e| format!("分析快照 {kind} 行损坏：{e}"))?,
        );
    }
    if rows.len() != header.count {
        return Err(format!("分析快照 {kind} 行数不完整"));
    }
    Ok((header, rows))
}

#[cfg(test)]
pub fn read_rows(root: &Path, kind: &str) -> Result<Vec<Value>, String> {
    read_verified(root, kind).map(|(_, rows)| rows)
}

fn valid(s: &State, header: &Header) -> bool {
    header.schema_version == SCHEMA_VERSION
        && header.source_revision == s.revision
        && header.data_until == s.last_scan
        && header.kind.len() <= 32
}

/// Ensure all three snapshots match the current source content. A partial,
/// stale or corrupt set is rebuilt as a whole. Source digests are compare
/// against freshly rebuilt rows, so an externally edited snapshot with a
/// recalculated file checksum is still rejected. If the rebuild cannot be
/// verified, callers must fail their operation.
pub fn ensure(root: &Path, s: &State, at: i64) -> Result<SnapshotSet, String> {
    let (messages, tasks, updates) = build(s, at);
    let live = [
        ("messages", &messages),
        ("tasks", &tasks),
        ("updates", &updates),
    ];
    let mut headers = Vec::new();
    let mut verified: Vec<(&str, Vec<Value>)> = Vec::new();
    let mut rebuild = false;
    for (kind, rows) in live {
        let expected = serialize(kind, rows, s, at)?.0;
        match read_verified(root, kind) {
            Ok((header, file_rows))
                if valid(s, &header)
                    && header.source_digest == expected.source_digest
                    && header.count == expected.count =>
            {
                headers.push(header);
                verified.push((kind, file_rows));
            }
            _ => {
                rebuild = true;
                break;
            }
        }
    }
    if rebuild {
        write_rows(
            root,
            s,
            at,
            [
                ("messages", messages),
                ("tasks", tasks),
                ("updates", updates),
            ],
        )
    } else {
        let index = s
            .messages
            .iter()
            .map(|(id, m)| (id.clone(), (m.source_id.clone(), m.chat.clone())))
            .collect::<BTreeMap<_, _>>();
        let records = project_records(&verified[0].1, &verified[1].1, &verified[2].1, &index);
        Ok(SnapshotSet { headers, records })
    }
}

fn record_id(kind: &str, row: &Value, position: usize) -> String {
    if kind == "updates" {
        format!("audit:{}", row["index"].as_u64().unwrap_or(position as u64))
    } else {
        row["id"].as_str().unwrap_or_default().to_string()
    }
}

/// Materialise the read-only records served by the per-request MCP tool. Full
/// bodies stay in this local file; only explicitly requested pages reach the
/// model, so the prompt itself does not grow with history size. This helper is
/// test-only: production validation uses the request's frozen record copy.
#[cfg(test)]
pub fn records(root: &Path) -> Result<Vec<Value>, String> {
    let messages = read_rows(root, "messages")?;
    let tasks = read_rows(root, "tasks")?;
    let updates = read_rows(root, "updates")?;
    let mut index = BTreeMap::new();
    for row in &messages {
        if let Some(id) = row["id"].as_str() {
            index.insert(
                id.to_string(),
                (
                    row["source_id"].as_str().unwrap_or("").to_string(),
                    row["chat"].as_str().unwrap_or("").to_string(),
                ),
            );
        }
    }
    Ok(project_records(&messages, &tasks, &updates, &index))
}

/// Build the exact read-only records for an already-frozen state without
/// touching snapshot files. Archive uses this so the model cannot observe
/// messages or tasks that arrived after the candidate cutoff.
pub fn records_from_state(s: &State, at: i64) -> Vec<Value> {
    let (messages, tasks, updates) = build(s, at);
    let index = s
        .messages
        .iter()
        .map(|(id, m)| (id.clone(), (m.source_id.clone(), m.chat.clone())))
        .collect::<BTreeMap<_, _>>();
    project_records(&messages, &tasks, &updates, &index)
}

fn project_records(
    messages: &[Value],
    tasks: &[Value],
    updates: &[Value],
    message_sources: &BTreeMap<String, (String, String)>,
) -> Vec<Value> {
    let mut result = Vec::new();
    for row in messages {
        let id = row["id"].as_str().unwrap_or("").to_string();
        let (source, chat) = message_sources.get(&id).cloned().unwrap_or_default();
        result.push(json!({
            "id":id,
            "kind":"message",
            "source":source,
            "chat":chat,
            "time":row["time"].as_i64().unwrap_or(0),
            "data":row
        }));
    }
    for (position, row) in tasks.iter().enumerate() {
        let id = record_id("tasks", row, position);
        let mut time = row["updated"].as_i64().unwrap_or(0);
        if time <= 0 {
            time = row["created"].as_i64().unwrap_or(0);
        }
        // A task can span several conversations. Source/chat filters are only
        // meaningful when every referenced message agrees; otherwise the
        // record exposes empty values instead of pretending a single origin.
        let references = row["evidence"]
            .as_object()
            .into_iter()
            .flat_map(|_| ["direct", "background", "completion"])
            .flat_map(|key| row["evidence"][key].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .filter_map(|mid| message_sources.get(mid))
            .cloned()
            .collect::<BTreeSet<_>>();
        let (source, chat) = if references.len() == 1 {
            references.into_iter().next().unwrap_or_default()
        } else {
            (String::new(), String::new())
        };
        result.push(json!({
            "id":id,
            "kind":"task",
            "source":source,
            "chat":chat,
            "time":time,
            "data":row
        }));
    }
    for (position, row) in updates.iter().enumerate() {
        // Keep the record metadata small; the full whitelisted before/after
        // document lives only in the pageable `detail` text. Duplicating it in
        // metadata would consume the per-page bound on every continuation.
        let data = json!({
            "task_id":row["task_id"],
            "audit_kind":row["kind"],
            "audit_index":row["index"],
            "evidence_id":row["evidence_id"],
            "fields":row["fields"],
            "detail":row["detail"],
            "note":"detail 为完整白名单变更文档，可用 next_offset 续读；省略字段不代表清空",
        });
        result.push(json!({
            "id":record_id("updates", row, position),
            "kind":"update",
            // Audit records carry no source/chat identity by construction.
            "source":"",
            "chat":"",
            "time":row["time"].as_i64().unwrap_or(0),
            "data":data
        }));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{id, now};

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("shishi-snapshots-{}", id()))
    }

    fn message(id: &str, time: i64, text: &str) -> Message {
        Message {
            id: id.into(),
            source_id: "source".into(),
            chat: "room".into(),
            text: text.into(),
            time,
            private: true,
            ..Default::default()
        }
    }

    #[test]
    fn snapshots_keep_recent_tasks_and_unarchived_old_evidence_but_not_unrelated_old_rows() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.last_scan = at - 1000;
        s.messages
            .insert("recent".into(), message("recent", at - 1000, "近期材料"));
        s.messages.insert(
            "old-linked".into(),
            message("old-linked", at - 3 * RETENTION_MS, "旧但未归档依据"),
        );
        s.messages.insert(
            "old-unrelated".into(),
            message("old-unrelated", at - 3 * RETENTION_MS, "旧且无关"),
        );
        s.analyzed.insert(
            "old-unrelated".into(),
            crate::model::fingerprint(&s.messages["old-unrelated"]),
        );
        s.messages.insert(
            "pending".into(),
            message("pending", at - 3 * RETENTION_MS, "尚未分析"),
        );
        s.tasks.push(Task {
            id: "task".into(),
            title: "未归档事项".into(),
            description: "未归档事项".into(),
            status: "已完成".into(),
            evidence: crate::model::TaskEvidence {
                direct: vec!["old-linked".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        s.audit.push(crate::model::Audit {
            time: at - 3 * RETENTION_MS,
            kind: "manual_archive".into(),
            task_id: "task".into(),
            detail: "旧备注".into(),
            before: json!({}),
            after: json!({"archive_note":"长期要求"}),
        });
        s.archive_ai_cursor = Some(0);
        let set = ensure(&root, &s, at).unwrap();
        assert_eq!(set.headers.len(), 3);
        let messages = read_rows(&root, "messages").unwrap();
        let ids = messages
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect::<BTreeSet<_>>();
        assert!(ids.contains("recent"));
        assert!(ids.contains("old-linked"));
        assert!(ids.contains("pending"));
        assert!(!ids.contains("old-unrelated"));
        let tasks = read_rows(&root, "tasks").unwrap();
        assert_eq!(tasks[0]["summary"], "未归档事项");
        let updates = read_rows(&root, "updates").unwrap();
        assert_eq!(updates[0]["evidence_id"], "audit:0");
        let document: Value = serde_json::from_str(updates[0]["detail"].as_str().unwrap()).unwrap();
        assert_eq!(document["after"]["archive_note"], "长期要求");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_or_stale_snapshot_is_rebuilt_and_checksum_mismatch_is_rejected() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.messages.insert("m".into(), message("m", at, "完整正文"));
        ensure(&root, &s, at).unwrap();
        let file = path(&root, "messages");
        let text = fs::read_to_string(&file).unwrap();
        let body = text.split_once('\n').unwrap().1;
        fs::write(
            &file,
            format!(
                "{}\n{}",
                text.lines().next().unwrap(),
                body.replace("完整", "损坏")
            ),
        )
        .unwrap();
        assert!(read_rows(&root, "messages").is_err());
        // A source revision change or unreadable body triggers a whole rebuild.
        ensure(&root, &s, at).unwrap();
        let rows = read_rows(&root, "messages").unwrap();
        assert_eq!(rows[0]["text"], "完整正文");
        let headers = KINDS
            .iter()
            .map(|kind| read_header(&root, kind).unwrap())
            .collect::<Vec<_>>();
        assert!(headers.iter().all(|h| h.generated_at == at));
        assert_eq!(headers[0].schema_version, SCHEMA_VERSION);
        assert!(!headers[0].checksum.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn externally_edited_snapshot_with_recalculated_checksum_is_rebuilt_from_source() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.messages.insert("m".into(), message("m", at, "权威正文"));
        ensure(&root, &s, at).unwrap();
        let file = path(&root, "messages");
        let text = fs::read_to_string(&file).unwrap();
        let (header_line, _) = text.split_once('\n').unwrap();
        let mut header: Header = serde_json::from_str(header_line).unwrap();
        let body = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id":"m","source_id":"source","chat":"room","text":"被外部改写","time":at
            }))
            .unwrap()
        );
        let digest = checksum(body.as_bytes());
        header.checksum = digest.clone();
        header.source_digest = digest;
        fs::write(
            &file,
            format!("{}\n{}", serde_json::to_string(&header).unwrap(), body),
        )
        .unwrap();
        // The file checksum is internally consistent, but does not match the
        // rows rebuilt from source, so ensure must rebuild.
        ensure(&root, &s, at).unwrap();
        let rows = read_rows(&root, "messages").unwrap();
        assert_eq!(rows[0]["text"], "权威正文");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_snapshot_projection_never_copies_settings_or_secret_bodies() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.audit.push(Audit {
            time: at,
            kind: "manual_edit".into(),
            task_id: "t".into(),
            detail: "修改任务".into(),
            before: json!([{"id":"t","title":"旧标题","config":{"api_key":"TOP-SECRET"}}]),
            after: json!([{"id":"t","title":"新标题","config":{"api_key":"TOP-SECRET-2"}}]),
        });
        // Settings/log audits and unrelated secret bodies are not task updates.
        s.audit.push(Audit {
            time: at,
            kind: "settings".into(),
            task_id: String::new(),
            detail: "修改设置".into(),
            before: json!({"config":{"api_key":"TOP-SECRET"}}),
            after: json!({"config":{"api_key":"TOP-SECRET-2"}}),
        });
        ensure(&root, &s, at).unwrap();
        for kind in KINDS {
            let text = fs::read_to_string(path(&root, kind)).unwrap();
            assert!(!text.contains("TOP-SECRET"), "{kind}");
            assert!(!text.contains("私有提示词"), "{kind}");
        }
        let updates = read_rows(&root, "updates").unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["kind"], "manual_edit");
        assert!(updates[0].get("audit").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_archive_note_is_retained_in_updates_and_pageable_by_read_context() {
        let root = root();
        let at = now();
        let note = format!("{}尾部可检索", "长".repeat(4_500));
        let mut s = State::default();
        s.audit.push(Audit {
            time: at,
            kind: "manual_archive".into(),
            task_id: "t".into(),
            detail: "归档".into(),
            before: json!([]),
            after: json!([{"id":"t","archive_note":note,"status":"已完成"}]),
        });
        ensure(&root, &s, at).unwrap();
        let rows = crate::snapshots::records(&root).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["data"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("尾部可检索")));
        let snapshot = json!({"records":rows,"expires":crate::model::now()+300_000});
        let mut meter = crate::analysis_context::Meter::default();
        let mut offset = 0;
        let mut joined = String::new();
        loop {
            let page: Value = serde_json::from_str(
                &crate::analysis_context::query(
                    &snapshot,
                    &mut meter,
                    &json!({"ids":["audit:0"],"offset":offset}),
                )
                .unwrap(),
            )
            .unwrap();
            if page["records"]
                .as_array()
                .is_none_or(|rows| rows.is_empty())
            {
                break;
            }
            joined.push_str(page["records"][0]["data"]["detail"].as_str().unwrap_or(""));
            match page["records"][0]["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert!(joined.contains("尾部可检索"));
        let document: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(document["after"]["archive_note"], note);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archived_task_long_note_and_description_are_fully_pageable_from_updates() {
        let root = root();
        let at = now();
        let note = format!("{}归档备注尾部", "备".repeat(3_000));
        let description = format!("{}事项描述尾部", "述".repeat(3_500));
        let mut task = Task {
            id: "archived".into(),
            title: "已归档长文".into(),
            description: description.clone(),
            status: "已完成".into(),
            archived: true,
            archive_note: note.clone(),
            updated: at,
            ..Default::default()
        };
        let mut before = task.clone();
        before.description = "旧描述".into();
        before.archive_note.clear();
        task.archive_note = note.clone();
        task.steps.push(crate::model::TaskStep {
            id: "verified".into(),
            title: "人工验证事实".into(),
            status: "completed".into(),
            confirmed: true,
            ..Default::default()
        });
        let mut s = State::default();
        s.tasks.push(task.clone());
        s.audit.push(Audit {
            time: at,
            kind: "manual_archive".into(),
            task_id: task.id.clone(),
            detail: "归档完整长文".into(),
            before: json!([before]),
            after: json!([task]),
        });
        ensure(&root, &s, at).unwrap();
        // The archived task is intentionally absent from the task snapshot.
        assert!(read_rows(&root, "tasks").unwrap().is_empty());
        let rows = records(&root).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "update");
        let document: Value =
            serde_json::from_str(rows[0]["data"]["detail"].as_str().unwrap()).unwrap();
        assert!(document["after"].get("steps").is_none());
        assert!(rows[0]["data"]["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("archive_note")));
        assert!(rows[0]["data"]["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("description")));
        let snapshot = json!({"records":rows,"expires":crate::model::now()+300_000});
        let mut meter = crate::analysis_context::Meter::default();
        let mut offset = 0u64;
        let mut joined = String::new();
        loop {
            let page: Value = serde_json::from_str(
                &crate::analysis_context::query(
                    &snapshot,
                    &mut meter,
                    &json!({"ids":["audit:0"],"offset":offset}),
                )
                .unwrap(),
            )
            .unwrap();
            assert!(page.to_string().chars().count() <= crate::analysis_context::PAGE);
            let Some(row) = page["records"].as_array().and_then(|rows| rows.first()) else {
                break;
            };
            joined.push_str(row["data"]["detail"].as_str().unwrap_or(""));
            // The repeated metadata is small; the whole page is not a second
            // full copy of the document.
            assert!(
                row["data"]["detail"].as_str().unwrap().chars().count()
                    <= crate::analysis_context::PAGE
            );
            match row["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert!(joined.contains(&note));
        assert!(joined.contains(&description));
        assert!(joined.contains("归档备注尾部"));
        assert!(joined.contains("事项描述尾部"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_records_have_time_and_unambiguous_source_chat_semantics() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.messages.insert(
            "m1".into(),
            Message {
                id: "m1".into(),
                source_id: "source".into(),
                chat: "room".into(),
                text: "依据".into(),
                time: at,
                ..Default::default()
            },
        );
        s.tasks.push(Task {
            id: "t".into(),
            title: "任务".into(),
            description: "完整内容".into(),
            status: "待处理".into(),
            updated: at + 5,
            evidence: crate::model::TaskEvidence {
                direct: vec!["m1".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        ensure(&root, &s, at).unwrap();
        let rows = records(&root).unwrap();
        let task = rows.iter().find(|r| r["kind"] == "task").unwrap();
        assert_eq!(task["time"], at + 5);
        assert_eq!(task["source"], "source");
        assert_eq!(task["chat"], "room");
        let update = rows.iter().find(|r| r["kind"] == "update");
        assert!(update.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_rebuild_is_reported_and_does_not_pretend_snapshots_are_complete() {
        let root = root();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(dir(&root), b"not a directory").unwrap();
        let s = State::default();
        let error = ensure(&root, &s, now()).unwrap_err();
        assert!(error.contains("不可写") || error.contains("重建失败"));
        fs::remove_file(dir(&root)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_expose_full_bodies_and_stable_update_ids() {
        let root = root();
        let at = now();
        let mut s = State::default();
        s.messages
            .insert("m".into(), message("m", at, &"长".repeat(5000)));
        s.tasks.push(Task {
            id: "t".into(),
            title: "任务".into(),
            description: "完整内容".into(),
            status: "待处理".into(),
            ..Default::default()
        });
        s.audit.push(crate::model::Audit {
            time: at,
            kind: "manual_edit".into(),
            task_id: "t".into(),
            detail: "修改".into(),
            before: json!({}),
            after: json!({}),
        });
        ensure(&root, &s, at).unwrap();
        let rows = records(&root).unwrap();
        assert!(rows.iter().any(|r| r["kind"] == "message"
            && r["data"]["text"].as_str().unwrap().chars().count() == 5000));
        assert!(rows
            .iter()
            .any(|r| r["kind"] == "task" && r["data"]["description"] == "完整内容"));
        assert!(rows
            .iter()
            .any(|r| r["id"] == "audit:0" && r["kind"] == "update"));
        fs::remove_dir_all(root).unwrap();
    }
}
