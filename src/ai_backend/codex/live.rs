//! Progress from the fresh CLI invocation's published events only. Never inspect
//! raw reasoning items, encrypted content, other conversations or reused history.
use crate::ai_backend::AnalysisEvent;
use serde_json::{json, Value};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    time::{Duration, Instant},
};

pub(super) struct PublishedThread {
    pub stored: Value,
    pub error: Option<String>,
}
pub(super) fn publish_thread(
    thread: &str,
    project: &str,
    title: &str,
    stop: &AtomicBool,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, String>,
    timeout: Duration,
) -> Result<PublishedThread, String> {
    let start = Instant::now();
    let mut stored = Value::Null;
    let mut issues = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(crate::ai_control::CANCELLED.into());
        }
        match call(
            "thread/read",
            json!({"threadId":thread,"includeTurns":false}),
        ) {
            Ok(value) if value["thread"]["id"] == thread => {
                stored = value;
                break;
            }
            Ok(_) => return Err("Codex 返回的会话编号不匹配，未关联".into()),
            Err(error) => {
                let pending =
                    error.contains("no rollout found") || error.contains("thread not loaded");
                // An exec-created thread is not loaded in this separate server.
                // Discover its durable summary without resuming the live writer.
                if pending {
                    if let Ok(page) = call(
                        "thread/list",
                        json!({"limit":100,"sortKey":"created_at","sourceKinds":["exec"],"modelProviders":[]}),
                    ) {
                        if let Some(row) = page["data"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|row| row["id"] == thread)
                        {
                            stored = json!({"thread":row});
                            break;
                        }
                    }
                    if start.elapsed() < timeout {
                        std::thread::sleep(Duration::from_millis(150));
                        continue;
                    }
                }
                issues.push(format!("会话摘要暂不可读：{error}"));
                break;
            }
        }
    }
    if stop.load(Ordering::SeqCst) {
        return Err(crate::ai_control::CANCELLED.into());
    }
    // Metadata publication is background-only. Opening a desktop window is a
    // separate, explicit user action.
    if !project.is_empty() && !stored.is_null() {
        for (method, params) in [
            (
                "thread/metadata/update",
                json!({"threadId":thread,"projectId":project}),
            ),
            ("thread/name/set", json!({"threadId":thread,"name":title})),
        ] {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = call(method, params) {
                issues.push(format!("{method}：{error}"));
            }
        }
    }
    Ok(PublishedThread {
        stored,
        error: (!issues.is_empty()).then(|| issues.join("；")),
    })
}

fn public_event(record: &Value) -> Option<AnalysisEvent> {
    if record["type"] != "event_msg" {
        return None;
    }
    let payload = &record["payload"];
    let (kind, text) = match payload["type"].as_str()? {
        "agent_reasoning" => ("reasoning", payload["text"].as_str()?),
        "agent_message" => ("content", payload["message"].as_str()?),
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| AnalysisEvent::Stream {
        kind: kind.into(),
        text: text
            .chars()
            .skip(text.chars().count().saturating_sub(12000))
            .collect(),
        append: false,
    })
}

pub(super) fn watch_public_rollout(
    stored: &Value,
    thread: &str,
    stop: &AtomicBool,
    tx: &SyncSender<AnalysisEvent>,
) {
    let Some(path) = stored["thread"]["path"].as_str() else {
        return;
    };
    // The exact id was checked before publication. Refuse paths for other sessions.
    if !std::path::Path::new(path)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(thread))
    {
        return;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let mut pending = Vec::new();
    let mut oversized = false;
    loop {
        let mut bytes = Vec::new();
        if file.by_ref().take(65536).read_to_end(&mut bytes).is_err() {
            return;
        }
        for byte in bytes {
            if byte == b'\n' {
                if !oversized {
                    if let Ok(record) = serde_json::from_slice::<Value>(&pending) {
                        if let Some(event) = public_event(&record) {
                            let _ = tx.try_send(event);
                        }
                    }
                }
                pending.clear();
                oversized = false;
            } else if !oversized {
                if pending.len() < 131072 {
                    pending.push(byte);
                } else {
                    pending.clear();
                    oversized = true;
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visibility_waits_for_rollout_and_never_retries_unknown_errors() {
        let mut methods = vec![];
        let mut reads = 0;
        publish_thread(
            "fresh",
            "p",
            "title",
            &AtomicBool::new(false),
            &mut |method, _| {
                methods.push(method.to_string());
                if method == "thread/list" {
                    return Ok(json!({"data":[]}));
                }
                if method == "thread/read" {
                    reads += 1;
                    if reads == 1 {
                        return Err("no rollout found for thread id fresh".into());
                    }
                    return Ok(json!({"thread":{"id":"fresh"}}));
                }
                Ok(json!({}))
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            methods,
            [
                "thread/read",
                "thread/list",
                "thread/read",
                "thread/metadata/update",
                "thread/name/set"
            ]
        );
        let mut calls = 0;
        let unknown = publish_thread(
            "fresh",
            "p",
            "t",
            &AtomicBool::new(false),
            &mut |_, _| {
                calls += 1;
                Err("-32603: Internal error".into())
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(unknown.error.unwrap().contains("Internal error"));
        assert_eq!(calls, 1);
        assert!(publish_thread(
            "fresh",
            "p",
            "t",
            &AtomicBool::new(true),
            &mut |_, _| panic!("cancelled"),
            Duration::from_secs(1)
        )
        .is_err());
    }
    #[test]
    fn unloaded_exec_thread_is_discovered_without_resume_and_metadata_failure_is_reported() {
        let mut methods = vec![];
        let published = publish_thread(
            "fresh",
            "p",
            "title",
            &AtomicBool::new(false),
            &mut |method, args| {
                methods.push(method.to_string());
                match method {
                    "thread/read" => {
                        assert_eq!(args["includeTurns"], false);
                        Err("-32600: thread not loaded: fresh".into())
                    }
                    "thread/list" => {
                        Ok(json!({"data":[{"id":"other"},{"id":"fresh","path":"fresh.jsonl"}]}))
                    }
                    _ => Err("optional metadata unavailable".into()),
                }
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(published.stored["thread"]["id"], "fresh");
        assert!(published
            .error
            .unwrap()
            .contains("optional metadata unavailable"));
        assert_eq!(
            methods,
            [
                "thread/read",
                "thread/list",
                "thread/metadata/update",
                "thread/name/set"
            ]
        );
        publish_thread(
            "fresh",
            "p",
            "t",
            &AtomicBool::new(false),
            &mut |_, _| Err("thread not loaded".into()),
            Duration::ZERO,
        )
        .unwrap();
        assert!(publish_thread(
            "fresh",
            "p",
            "t",
            &AtomicBool::new(false),
            &mut |_, _| Ok(json!({"thread":{"id":"other"}})),
            Duration::ZERO
        )
        .is_err());
    }

    #[test]
    fn only_public_event_text_is_previewed() {
        assert!(public_event(&json!({"type":"response_item","payload":{"type":"reasoning","text":"hidden","encrypted_content":"opaque"}})).is_none());
        assert!(public_event(&json!({"type":"event_msg","payload":{"type":"agent_reasoning_raw_content","text":"hidden"}})).is_none());
        assert_eq!(
            public_event(
                &json!({"type":"event_msg","payload":{"type":"agent_reasoning","text":"公开摘要"}})
            ),
            Some(AnalysisEvent::Stream {
                kind: "reasoning".into(),
                text: "公开摘要".into(),
                append: false
            })
        );
        assert_eq!(
            public_event(
                &json!({"type":"event_msg","payload":{"type":"agent_message","message":"公开响应"}})
            ),
            Some(AnalysisEvent::Stream {
                kind: "content".into(),
                text: "公开响应".into(),
                append: false
            })
        );
    }
    #[test]
    fn public_rollout_streams_before_exit_and_retains_split_utf8_lines() {
        use std::io::Write;
        let thread = uuid::Uuid::new_v4().to_string();
        let path = std::env::temp_dir().join(format!("shishi-rollout-{thread}.jsonl"));
        let mut file = std::fs::File::create(&path).unwrap();
        let stored = json!({"thread":{"id":thread,"path":path}});
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let worker_stop = stop.clone();
        let worker =
            std::thread::spawn(move || watch_public_rollout(&stored, &thread, &worker_stop, &tx));
        let row =
            json!({"type":"event_msg","payload":{"type":"agent_reasoning","text":"公开核对中"}})
                .to_string()
                + "\n";
        let split = row.find('公').unwrap() + 1;
        file.write_all(&row.as_bytes()[..split]).unwrap();
        file.flush().unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(180)).is_err());
        file.write_all(&row.as_bytes()[split..]).unwrap();
        file.flush().unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            AnalysisEvent::Stream {
                kind: "reasoning".into(),
                text: "公开核对中".into(),
                append: false
            }
        );
        assert!(
            !worker.is_finished(),
            "preview must arrive before invocation ends"
        );
        file.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}}\n").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(180)).is_err());
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
