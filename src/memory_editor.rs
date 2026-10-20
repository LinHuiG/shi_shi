//! Private current memory, with optimistic concurrency but no user-facing history.
use crate::{engine::App, model::*};
use serde_json::{json, Value};
use std::collections::BTreeSet;

impl App {
    pub fn memory_read(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"temporary":{"text":s.analysis_memory.temporary,"revision":s.analysis_memory.temporary_version,"updated":s.analysis_memory.temporary_updated,"max_chars":crate::analysis_context::MEMORY_LIMIT},"archived":{"entries":s.analysis_memory.archived_entries,"revision":s.analysis_memory.archived_version,"max_chars":crate::analysis_context::ARCHIVED_MEMORY_LIMIT}})
    }
    pub fn memory_save(&self, p: &Value) -> Result<Value, String> {
        let kind = p["kind"].as_str().ok_or("请选择记忆类型")?;
        let expected = p["expected"].as_u64().ok_or("缺少记忆校验值，请重新读取")?;
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let at = now();
        match kind {
            "temporary" => {
                if expected != s.analysis_memory.temporary_version {
                    return Err("临时记忆已更新，草稿已保留；请重新读取后合并".into());
                }
                let text = p["text"].as_str().ok_or("记忆正文无效")?;
                if text.chars().count() > crate::analysis_context::MEMORY_LIMIT {
                    return Err("临时记忆最多 2000 字符，未保存".into());
                }
                s.analysis_memory.temporary = text.into();
                s.analysis_memory.temporary_version += 1;
                s.analysis_memory.temporary_updated = at;
            }
            "archived" => {
                if expected != s.analysis_memory.archived_version {
                    return Err("归档记忆已更新，草稿已保留；请重新读取后合并".into());
                }
                let rows = p["entries"]
                    .as_array()
                    .filter(|v| v.len() <= 128)
                    .ok_or("归档记忆格式无效或条目过多")?;
                let mut entries = vec![];
                let mut keys = BTreeSet::new();
                let mut ids = BTreeSet::new();
                for row in rows {
                    let key = row["key"].as_str().unwrap_or("").trim();
                    let text = row["text"].as_str().ok_or("记忆正文无效")?.trim();
                    if key.is_empty()
                        || key.chars().count() > 80
                        || text.is_empty()
                        || !keys.insert(key.to_string())
                    {
                        return Err("每条归档记忆需填写唯一主题（最多80字）和正文".into());
                    }
                    let ident = row["id"].as_str().unwrap_or("");
                    let old = s
                        .analysis_memory
                        .archived_entries
                        .iter()
                        .find(|e| e.id == ident);
                    if !ident.is_empty() && old.is_none() {
                        return Err("归档记忆条目已变化，请重新读取")?;
                    }
                    let mut entry = old.cloned().unwrap_or_else(|| MemoryEntry {
                        id: id(),
                        created: at,
                        ..Default::default()
                    });
                    if !ids.insert(entry.id.clone()) {
                        return Err("归档记忆编号重复".into());
                    }
                    if entry.key != key || entry.text != text {
                        entry.key = key.into();
                        entry.text = text.into();
                        entry.source = vec!["manual".into()];
                        entry.updated = at;
                        entry.version = s.analysis_memory.archived_version + 1;
                    }
                    entries.push(entry);
                }
                s.analysis_memory.archived_entries = entries;
                if s.analysis_memory.archived_text().chars().count()
                    > crate::analysis_context::ARCHIVED_MEMORY_LIMIT
                {
                    return Err("归档记忆总计最多4000字符，未保存".into());
                }
                s.analysis_memory.archived_version += 1;
            }
            _ => return Err("未知记忆类型".into()),
        }
        // Log the operation, never a second copy of the private memory body.
        App::audit(
            &mut s,
            "memory_manual",
            "",
            "手动保存 AI 记忆",
            Value::Null,
            json!({"kind":kind}),
        );
        self.save(&s)?;
        *lock = s;
        drop(lock);
        Ok(self.memory_read())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_are_atomic_bounded_persistent_and_reject_stale_ai_and_manual_writes() {
        let app =
            App::load(std::env::temp_dir().join(format!("shishi-memory-editor-{}", id()))).unwrap();
        app.memory_save(&json!({"kind":"temporary","expected":0,"text":"人工笔记"}))
            .unwrap();
        assert!(app
            .memory_save(&json!({"kind":"temporary","expected":0,"text":"旧草稿"}))
            .is_err());
        assert!(app
            .memory_save(&json!({"kind":"temporary","expected":1,"text":"字".repeat(2001)}))
            .is_err());
        app.memory_save(&json!({"kind":"archived","expected":0,"entries":[{"key":"偏好","text":"先看已确认的证据"}]})).unwrap();
        let before = app.memory_read();
        assert!(app
            .memory_save(&json!({"kind":"archived","expected":0,"entries":[]}))
            .is_err());
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .memory_save(&json!({"kind":"temporary","expected":1,"text":"不能提交"}))
            .is_err());
        assert_eq!(app.memory_read(), before);
        std::fs::remove_dir(app.root.join("data/state.next.json")).unwrap();
        let reloaded = App::load(app.root.clone()).unwrap();
        assert_eq!(reloaded.memory_read(), before);
    }
}
