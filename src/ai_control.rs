use crate::{engine::App, model::id};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub const CANCELLED: &str = "本次 AI 任务已中断，可手动重新执行";

/// Stable diagnostic category for a failed run. The original message is kept;
/// this only adds a machine-readable class so the UI/logs can distinguish a
/// cancellation, a local resource limit, a temporary provider failure and a
/// source/plugin failure without repeating the same row every tick.
pub fn failure_class(error: &str) -> &'static str {
    let text = error.to_lowercase();
    if error == CANCELLED || text.contains("中断") || text.contains("cancel") {
        return "cancelled";
    }
    if error.starts_with(crate::ai::INPUT_LIMIT_ERROR)
        || text.contains("上限")
        || text.contains("超限")
        || text.contains("quota")
        || text.contains("rate limit")
        || text.contains("429")
        || text.contains("capacity")
        || text.contains("overloaded")
    {
        return "resource_limit";
    }
    if text.contains("供应商拒答")
        || text.contains("refused")
        || text.contains("stop reason refusal")
    {
        return "provider_refusal";
    }
    if text.contains("http 504") {
        return "upstream_timeout";
    }
    if text.contains("tool arguments") {
        return "tool_arguments";
    }
    if text.contains("timeout")
        || text.contains("timed out")
        || text.contains("超时")
        || text.contains("deadline")
        || text.contains("temporar")
        || text.contains("unavailable")
        || text.contains("connection reset")
        || text.contains("eof")
    {
        return "temporary_failure";
    }
    if text.contains("来源")
        || text.contains("插件")
        || text.contains("source")
        || text.contains("plugin")
        || text.contains("scan")
    {
        return "source_failure";
    }
    "unknown"
}

pub struct ActiveAi {
    pub source: Value,
    pub id: String,
    pub cancel: Arc<AtomicBool>,
}

pub struct AiCall<'a> {
    app: &'a App,
    pub id: String,
    pub cancel: Arc<AtomicBool>,
}

impl AiCall<'_> {
    /// Keep one cancellation token/reservation across archive batches, but give
    /// each provider request an independent trace and live-preview identity.
    pub fn next_request(&mut self) -> Result<(), String> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        let mut active = self.app.active_ai.lock().unwrap();
        let call = active
            .as_mut()
            .filter(|a| a.id == self.id)
            .ok_or(CANCELLED)?;
        self.id = id();
        call.id = self.id.clone();
        Ok(())
    }

    // Closing acceptance and cancellation under the same lock prevents a late
    // click from accepting a cancelled result or cancelling the next request.
    pub fn finish(&self) -> bool {
        let mut active = self.app.active_ai.lock().unwrap();
        let cancelled = self.cancel.load(Ordering::SeqCst);
        if active.as_ref().is_some_and(|a| a.id == self.id) {
            *active = None;
            self.app.ai_busy.store(false, Ordering::SeqCst);
        }
        cancelled
    }
}
impl Drop for AiCall<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn begin_ai(&self) -> Result<AiCall<'_>, String> {
        self.begin_ai_for("classify")
    }

    pub(crate) fn begin_ai_for(&self, kind: &str) -> Result<AiCall<'_>, String> {
        let mut active = self.active_ai.lock().unwrap();
        if kind != "archive"
            && (self.archive_busy.load(Ordering::SeqCst)
                || self
                    .state
                    .lock()
                    .unwrap()
                    .archive_attempts
                    .iter()
                    .any(|a| a.status == "queued"))
        {
            return Err("归档已排队或正在执行，请稍后重试".into());
        }
        if self.ai_busy.swap(true, Ordering::SeqCst) {
            return Err("Codex 正在处理其他请求，请稍后重试".into());
        }
        let id = id();
        let cancel = Arc::new(AtomicBool::new(false));
        // Reset under the same lock as interruption, before exposing this request.
        self.state.lock().unwrap().ai_interrupted_at = 0;
        *active = Some(ActiveAi {
            source: Value::Null,
            id: id.clone(),
            cancel: cancel.clone(),
        });
        Ok(AiCall {
            app: self,
            id,
            cancel,
        })
    }

    pub fn interrupt_ai(&self, request_id: &str) -> Result<Value, String> {
        let active = self.active_ai.lock().unwrap();
        let Some(call) = active.as_ref().filter(|a| a.id == request_id) else {
            return Ok(json!({"skipped":"该次 AI 分析已结束"}));
        };
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        next.ai_interrupted_at = crate::model::now();
        self.save(&next)?;
        *state = next;
        call.cancel.store(true, Ordering::SeqCst);
        Ok(json!({"interrupted":true}))
    }

    pub(crate) fn ai_deferred_reason(&self) -> &'static str {
        crate::work_hours::DEFERRED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_input_is_rejected_before_cli_start_or_call_reservation() {
        let root = std::env::temp_dir().join(format!("shishi-input-limit-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        {
            let mut s = app.state.lock().unwrap();
            s.config.max_input_chars = 5_000;
            s.config.codex = "nonexistent-must-not-launch.exe".into();
            s.pending_analysis = true;
        }
        let result = app.ask_ai(
            "classify",
            &json!({"changes":[{"text":"中🙂".repeat(4_000)}]}),
        );
        let error = result.unwrap_err();
        assert!(error.starts_with(crate::ai::INPUT_LIMIT_ERROR), "{error}");
        assert!(error.contains("changes"));
        let s = app.state.lock().unwrap();
        assert_eq!((s.calls, s.unreported_calls), (0, 0));
        assert!(s.pending_analysis && s.analyzed.is_empty());
        drop(s);
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        assert!(app
            .activity
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "ai_rejected"));
        assert!(!root.join("data/ai-work").exists());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_or_running_archive_reserves_priority_over_new_analysis() {
        let root = std::env::temp_dir().join(format!("shishi-ai-priority-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.state
            .lock()
            .unwrap()
            .archive_attempts
            .push(crate::model::ArchiveAttempt {
                status: "queued".into(),
                ..Default::default()
            });
        assert!(app.begin_ai_for("classify").is_err());
        assert!(!app.ai_busy.load(Ordering::SeqCst));
        app.state.lock().unwrap().archive_attempts.clear();
        app.archive_busy.store(true, Ordering::SeqCst);
        assert!(app.begin_ai_for("message_filter").is_err());
        let archive = app.begin_ai_for("archive").unwrap();
        assert!(app.begin_ai_for("classify").is_err());
        drop(archive);
        app.archive_busy.store(false, Ordering::SeqCst);
        drop(app.begin_ai_for("classify").unwrap());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interruption_only_cancels_current_request_and_preserves_settings() {
        let root = std::env::temp_dir().join(format!("shishi-ai-interrupt-{}", id()));
        let app = App::load(root.clone()).unwrap();
        app.state.lock().unwrap().config.model_profiles = None;
        app.test_force_working();
        {
            let mut s = app.state.lock().unwrap();
            s.config.auto_ai = false;
            s.pending_analysis = true;
        }
        let call = app.begin_ai().unwrap();
        assert!(app.begin_ai().is_err());
        assert!(app.interrupt_ai("old-request").unwrap()["skipped"].is_string());
        assert!(!call.cancel.load(Ordering::SeqCst));
        app.interrupt_ai(&call.id).unwrap();
        // A stop arriving during prompt preparation must survive call reservation.
        app.reserve_ai(crate::work_hours::AiMode::Manual, None)
            .unwrap();
        assert!(app.state.lock().unwrap().ai_interrupted_at > 0);
        assert!(call.finish());
        let next = app.begin_ai().unwrap();
        drop(call);
        assert!(app.ai_busy.load(Ordering::SeqCst));
        assert!(!next.finish());
        let reloaded = App::load(root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        assert!(s.ai_interrupted_at > 0 && s.pending_analysis);
        assert!(!s.config.auto_ai);
        drop(s);
        drop(next);
        drop(reloaded);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
