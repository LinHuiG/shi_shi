//! Bounded, read-only observation of the exact rollout returned by Codex.
//! This never resumes a thread or claims its approvals. Unknown formats fall
//! back to the explicit pending-sync state and normal history reconciliation.
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

pub struct Observation {
    pub turn: String,
    pub status: String,
}
pub fn observe(path: &Path, thread: &str, since: i64, previous: &[String]) -> Option<Observation> {
    let mut file = File::open(path).ok()?;
    let mut header = String::new();
    BufReader::new((&mut file).take(65536))
        .read_line(&mut header)
        .ok()?;
    let meta: Value = serde_json::from_str(&header).ok()?;
    if meta["type"] != "session_meta" || meta["payload"]["id"] != thread {
        return None;
    }
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(256 * 1024).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    let mut current = String::new();
    let mut latest = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(at) = v["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
        else {
            continue;
        };
        if at < since {
            continue;
        }
        let p = &v["payload"];
        if v["type"] == "turn_context" {
            current = p["turn_id"].as_str().unwrap_or("").into();
        }
        if v["type"] != "event_msg" {
            continue;
        }
        let turn = p["turn_id"].as_str().unwrap_or(&current);
        if turn.is_empty() || previous.iter().any(|s| s == turn) {
            continue;
        }
        let status = match p["type"].as_str().unwrap_or("") {
            "task_started" | "turn_started" => "inProgress",
            "task_complete" | "turn_complete" => "completed",
            "turn_aborted" => "interrupted",
            _ => continue,
        };
        current = turn.to_string();
        latest = Some(Observation {
            turn: current.clone(),
            status: status.into(),
        });
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn observes_only_matching_thread_new_turns_and_complete_records() {
        let root =
            std::env::temp_dir().join(format!("shishi-desktop-watch-{}", crate::model::id()));
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("rollout.jsonl");
        let rows = [
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            json!({"timestamp":"2026-10-20T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"timestamp":"2026-10-20T00:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"new"}}),
        ];
        let mut text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&p, &text).unwrap();
        let o = observe(&p, "thread", 0, &["old".into()]).unwrap();
        assert_eq!(o.turn, "new");
        assert_eq!(o.status, "inProgress");
        assert!(observe(&p, "other", 0, &[]).is_none());
        text+="{\"timestamp\":\"2026-10-20T00:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"new\"}}\n{\"partial\":";
        std::fs::write(&p, text).unwrap();
        assert_eq!(observe(&p, "thread", 0, &[]).unwrap().status, "completed");
        assert!(observe(&p, "thread", i64::MAX, &[]).is_none());
    }
}
