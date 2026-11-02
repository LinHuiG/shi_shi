//! Private current memory, with optimistic concurrency but no user-facing history.
use crate::{engine::App, model::*};
use serde_json::{json, Value};

impl App {
    pub fn memory_read(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"temporary":{"text":s.analysis_memory.temporary,"revision":s.analysis_memory.temporary_version,"updated":s.analysis_memory.temporary_updated,"max_chars":crate::analysis_context::MEMORY_LIMIT},"archived":{"text":s.analysis_memory.archived_text(),"revision":s.analysis_memory.archived_version,"updated":s.analysis_memory.archived_updated,"max_chars":crate::analysis_context::ARCHIVED_MEMORY_LIMIT}})
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
                let text = p["text"].as_str().ok_or("记忆正文无效")?;
                if text.chars().count() > crate::analysis_context::ARCHIVED_MEMORY_LIMIT {
                    return Err("归档记忆最多 4000 字符，未保存".into());
                }
                s.analysis_memory.replace_archived(text.into(), at);
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
        app.memory_save(&json!({"kind":"archived","expected":0,"text":"先看已确认的证据"}))
            .unwrap();
        let before = app.memory_read();
        assert!(app
            .memory_save(&json!({"kind":"archived","expected":0,"text":""}))
            .is_err());
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        assert!(app
            .memory_save(&json!({"kind":"temporary","expected":1,"text":"不能提交"}))
            .is_err());
        assert_eq!(app.memory_read(), before);
        crate::test_support::persistence_failure(&app.root, false).unwrap();
        let reloaded = App::load(app.root.clone()).unwrap();
        assert_eq!(reloaded.memory_read(), before);
    }
}
