use crate::{engine::App, model::*};
use serde_json::{json, Value};
use std::collections::BTreeSet;
/// Referenced task/memory evidence can outlive the display retention window.
/// Being unanalyzed alone never extends a message's retention.
pub fn protected(s: &State) -> BTreeSet<String> {
    let mut protected = s
        .tasks
        .iter()
        .filter(|t| !t.archived)
        .flat_map(|t| t.evidence.all())
        .collect::<BTreeSet<_>>();
    // Private archived memory and archive-group rollback material may cite
    // message ids directly. Keep those messages alive so retention cannot
    // leave dangling evidence.
    for entry in s.analysis_memory.archived_entries.iter().chain(
        s.archive_versions
            .iter()
            .flat_map(|group| group.archived_entries.iter()),
    ) {
        for source in &entry.source {
            if !source.starts_with("audit:") && !source.starts_with("source:") {
                protected.insert(source.clone());
            }
        }
    }
    protected
}
pub fn allowed_with(s: &State, m: &Message, keep: &BTreeSet<String>) -> bool {
    allowed_at(s, m, keep, now())
}
fn allowed_at(s: &State, m: &Message, keep: &BTreeSet<String>, at: i64) -> bool {
    !s.deleted_messages.contains_key(&m.id)
        && (m.time >= at - i64::from(s.config.message_retention_days.max(1)) * 86_400_000
            || keep.contains(&m.id))
}
pub fn prune(s: &mut State) {
    let cutoff = now() - i64::from(s.config.message_retention_days.max(1)) * 86_400_000;
    let keep = protected(s);
    let expired = s
        .messages
        .values()
        .filter(|message| message.time < cutoff && !keep.contains(&message.id))
        .cloned()
        .collect::<Vec<_>>();
    if !expired.is_empty() {
        crate::retention::add_messages(s, &expired);
    }
    s.messages.retain(|id, m| {
        m.time >= cutoff && !s.deleted_messages.contains_key(id) || keep.contains(id)
    });
    s.analyzed.retain(|id, _| s.messages.contains_key(id));
    s.analysis_retries
        .retain(|id, _| s.messages.contains_key(id));
    s.analysis_retry_counts
        .retain(|id, _| s.messages.contains_key(id));
    if !expired.is_empty() {
        crate::retention::compact_digest(s);
    }
}
impl App {
    pub fn query_messages(&self, v: &Value) -> Result<Value, String> {
        let number = |key: &str, default: u64| -> Result<u64, String> {
            match v.get(key) {
                None => Ok(default),
                Some(x) => x.as_u64().ok_or(format!("无效的{key}")),
            }
        };
        let requested = number("page", 1)?;
        let size = number("page_size", 50)?;
        if requested == 0 || requested > 1_000_000 || ![20, 50, 100].contains(&size) {
            return Err("分页参数无效".into());
        }
        let since = number("since", 0)?;
        let until = number("until", i64::MAX as u64)?;
        if since > until || until > i64::MAX as u64 {
            return Err("开始时间应早于结束时间".into());
        }
        let q = v["query"].as_str().unwrap_or("").trim().to_lowercase();
        let ascending = match v.get("sort").and_then(Value::as_str) {
            None if v.get("sort").is_none() => false,
            Some("desc") => false,
            Some("asc") => true,
            _ => return Err("时间排序参数无效".into()),
        };
        if q.chars().count() > 200 {
            return Err("搜索词最多200字".into());
        }
        let s = self.state.lock().unwrap();
        let keep = protected(&s);
        let mut matches = s
            .messages
            .values()
            .filter(|m| {
                !m.withdrawn
                    && m.time >= since as i64
                    && m.time <= until as i64
                    && allowed_with(&s, m, &keep)
            })
            .filter(|m| {
                q.is_empty()
                    || [&m.text, &m.sender, &m.group, &m.chat, &m.source_name]
                        .iter()
                        .any(|x| x.to_lowercase().contains(&q))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.id.cmp(&a.id)));
        if ascending {
            matches.reverse();
        }
        let total = matches.len();
        let pages = total.div_ceil(size as usize).max(1);
        let page = (requested as usize).min(pages);
        let rows = matches
            .into_iter()
            .skip((page - 1) * size as usize)
            .take(size as usize)
            .collect::<Vec<_>>();
        Ok(json!({"rows":rows,"page":page,"page_size":size,"total":total,"pages":pages}))
    }
    pub fn prune_messages(&self) -> Result<(), String> {
        // Archive candidate audit indices must stay stable until commit.
        if self.archive_busy.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let mut lock = self.state.lock().unwrap();
        let days = lock.config.message_retention_days.max(1);
        let marker_path = crate::storage::data_dir(&self.root).join("retention-pruned.txt");
        let marker = format!("{}:{}", date(), days);
        let due = std::fs::read_to_string(&marker_path)
            .map(|v| v != marker)
            .unwrap_or(true);
        if !due {
            let at = now();
            let keep = protected(&lock);
            if !lock
                .messages
                .values()
                .any(|m| !allowed_at(&lock, m, &keep, at))
            {
                return Ok(());
            }
        }
        let mut s = lock.clone();
        if due {
            crate::retention::prune(&mut s, now());
            crate::ai::prune_traces(&self.root, days);
        }
        prune(&mut s);
        self.save(&s)?;
        *lock = s;
        if due {
            let _ = std::fs::write(marker_path, marker);
        }
        Ok(())
    }
    pub fn delete_messages(&self, v: &Value) -> Result<Value, String> {
        let ids = v["ids"].as_array().ok_or("请选择消息")?;
        if ids.is_empty() || ids.len() > 1000 {
            return Err("每次请选择1–1000条消息".into());
        }
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let mut count = 0;
        for ident in ids {
            let ident = ident.as_str().ok_or("无效消息编号")?;
            if s.messages.remove(ident).is_some() {
                s.deleted_messages.insert(ident.into(), now());
                s.analyzed.remove(ident);
                count += 1;
            }
        }
        Self::audit(
            &mut s,
            "messages_delete",
            "",
            "手动删除消息，防止扫描重新导入，保留关联事项",
            Value::Null,
            json!({"count":count}),
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!({"count":count}))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_expires_unprocessed_messages_including_paused_retries() {
        let mut s = State::default();
        for id in ["pending", "processed"] {
            s.messages.insert(
                id.into(),
                Message {
                    id: id.into(),
                    time: now() - 366 * 86_400_000i64,
                    text: "请核对".into(),
                    private: true,
                    ..Default::default()
                },
            );
        }
        s.analyzed
            .insert("processed".into(), fingerprint(&s.messages["processed"]));
        s.analysis_retry_counts.insert("pending".into(), 3);
        prune(&mut s);
        assert!(!s.messages.contains_key("pending"));
        assert!(!s.messages.contains_key("processed"));
        assert!(!s.analyzed.contains_key("pending"));
        assert!(s.analysis_retry_counts.is_empty());
    }
    #[test]
    fn queries_search_entire_cache_page_stably_and_clamp_after_deletion() {
        let app = App::load(std::env::temp_dir().join(format!("shishi-pages-{}", id()))).unwrap();
        let at = now();
        {
            let mut s = app.state.lock().unwrap();
            for i in 0..237 {
                s.messages.insert(
                    format!("m{i:03}"),
                    Message {
                        id: format!("m{i:03}"),
                        time: at,
                        text: if i < 7 {
                            "历史目标".into()
                        } else {
                            "普通消息".into()
                        },
                        ..Default::default()
                    },
                );
            }
        }
        let a = app
            .query_messages(&json!({"page":1,"page_size":20}))
            .unwrap();
        let b = app
            .query_messages(&json!({"page":2,"page_size":20}))
            .unwrap();
        assert_eq!(a["total"], 237);
        assert_eq!(a["rows"][0]["id"], "m236");
        assert_eq!(b["rows"][0]["id"], "m216");
        let asc = app
            .query_messages(&json!({"page":2,"page_size":20,"sort":"asc"}))
            .unwrap();
        assert_eq!(asc["rows"][0]["id"], "m020");
        let asc_search = app
            .query_messages(&json!({"query":"历史目标","sort":"asc"}))
            .unwrap();
        assert_eq!(asc_search["rows"][0]["id"], "m000");
        assert!(app.query_messages(&json!({"sort":"invalid"})).is_err());
        let found = app
            .query_messages(
                &json!({"query":"历史目标","page":8,"page_size":20,"since":at-1,"until":at+1}),
            )
            .unwrap();
        assert_eq!(found["total"], 7);
        assert_eq!(found["page"], 1);
        assert_eq!(
            app.query_messages(&json!({"since":at+1})).unwrap()["total"],
            0
        );
        assert!(app.query_messages(&json!({"page_size":1000})).is_err());
        assert!(app.query_messages(&json!({"since":20,"until":10})).is_err());
        assert_eq!(Config::default().message_retention_days, 90);
    }
    #[test]
    fn retention_changes_apply_immediately_and_failed_deletion_is_atomic() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-messages-{}", id()))).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            for (id, days) in [("recent", 0), ("older", 10)] {
                s.messages.insert(
                    id.into(),
                    Message {
                        id: id.into(),
                        time: now() - days * 86_400_000,
                        ..Default::default()
                    },
                );
                s.analyzed.insert(id.into(), "fingerprint".into());
            }
        }
        let mut c = app.state.lock().unwrap().config.clone();
        c.message_retention_days = 5;
        app.configure(json!(c)).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert!(s.messages.contains_key("recent"));
            assert!(!s.messages.contains_key("older"));
            assert!(!s.analyzed.contains_key("older"));
        }
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app.delete_messages(&json!({"ids":["recent"]})).is_err());
        let s = app.state.lock().unwrap();
        assert!(s.messages.contains_key("recent"));
        assert!(!s.deleted_messages.contains_key("recent"));
    }
}
