use crate::{ai, engine::App, model::*, work_hours::AiMode};
use serde_json::{json, Value};
#[cfg(test)]
use std::fs;
use std::sync::atomic::Ordering;

pub const TARGETS: [&str; 7] = [
    "classify",
    "format",
    "learning",
    "report",
    "handling",
    "prompt_review",
    "archive",
];
pub fn content(config: &Config, target: &str) -> String {
    ai::effective_policy(config, target).to_owned()
}
pub(crate) fn set_content(config: &mut Config, target: &str, text: String) -> Result<(), String> {
    let field = match target {
        "classify" => &mut config.classify_prompt,
        "format" => &mut config.format_prompt,
        "learning" => &mut config.learning_prompt,
        "report" => &mut config.report_prompt,
        "handling" => &mut config.handling_prompt,
        "prompt_review" => &mut config.review_prompt,
        "archive" => &mut config.archive_prompt,
        _ => return Err("未知提示词类型".into()),
    };
    *field = text;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> App {
        App::load(std::env::temp_dir().join(format!("shishi-prompts-{}", id()))).unwrap()
    }
    fn archive(app: &App) -> PromptReview {
        let task = app
            .task_edit(&json!({"task":{"title":"验证归档","status":"待处理"}}))
            .unwrap();
        app.archive_task(&json!({"id":task["id"],"updated":task["updated"],"status":"已完成","note":"普通确认不表示完成"})).unwrap();
        queue(&mut app.state.lock().unwrap(), &[]);
        app.state
            .lock()
            .unwrap()
            .prompt_reviews
            .last()
            .unwrap()
            .clone()
    }
    fn ready(app: &App, r: &PromptReview, text: &str) {
        app.finish_prompt_review(&r.id,json!({"improved":true,"reason":"根据人工归档修正完成判断","proposed_prompt":text,"evidence_ids":[r.evidence["audit"][0]["evidence_id"]]})).unwrap();
    }
    #[test]
    fn archive_policy_is_manually_configurable_and_fixed_permissions_survive_edit_and_restart() {
        let app = fixture();
        let initial = current(&app.state.lock().unwrap(), "archive")
            .unwrap()
            .id
            .clone();
        app.save_prompt(
            &json!({"target":"archive","text":"用户手动设置的归档表达风格","expected":initial}),
        )
        .unwrap();
        let next = current(&app.state.lock().unwrap(), "archive")
            .unwrap()
            .id
            .clone();
        assert_ne!(initial, next);
        let reloaded = App::load(app.root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        assert_eq!(s.config.archive_prompt, "用户手动设置的归档表达风格");
        let prompt = ai::build_prompt(&s.config, "archive", &json!({}));
        assert!(prompt.contains(&s.config.archive_prompt));
        assert!(prompt.contains(crate::prompt_config::get("archive_permissions")));
        drop(s);
        app.rollback_prompt(&initial, &next).unwrap();
        assert_eq!(
            content(&app.state.lock().unwrap().config, "archive"),
            ai::default_policy("archive")
        );
        drop(reloaded);
        let path = app.root.clone();
        drop(app);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn archive_preview_adoption_rollback_and_restart_preserve_every_version() {
        let app = fixture();
        let initial = app.state.lock().unwrap().prompt_versions.clone();
        assert_eq!(initial.len(), TARGETS.len());
        let review = archive(&app);
        ready(&app, &review, "改进后的规则");
        assert_eq!(
            content(&app.state.lock().unwrap().config, "classify"),
            review.base_prompt
        );
        let adopted = app.adopt_prompt(&review.id).unwrap()["version"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            content(&app.state.lock().unwrap().config, "classify"),
            "改进后的规则"
        );
        app.rollback_prompt(&review.base_version, &adopted).unwrap();
        let reloaded = App::load(app.root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        assert_eq!(s.prompt_versions.len(), TARGETS.len() + 2);
        assert_eq!(content(&s.config, "classify"), review.base_prompt);
        assert_eq!(s.prompt_reviews[0].status, "adopted");
        assert_eq!(current(&s, "classify").unwrap().previous_id, adopted);
    }
    #[test]
    fn stale_suggestions_invalid_evidence_and_failed_save_cannot_change_active_prompt() {
        let app = fixture();
        let review = archive(&app);
        assert!(app.finish_prompt_review(&review.id,json!({"improved":true,"reason":"无依据","proposed_prompt":"错误","evidence_ids":["invented"]})).is_err());
        ready(&app, &review, "建议规则");
        let mut config = app.state.lock().unwrap().config.clone();
        config.classify_prompt = "手动新规则".into();
        app.configure(json!(config)).unwrap();
        assert!(app.adopt_prompt(&review.id).is_err());
        let latest = current(&app.state.lock().unwrap(), "classify")
            .unwrap()
            .id
            .clone();
        fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app.rollback_prompt(&review.base_version, &latest).is_err());
        assert_eq!(
            content(&app.state.lock().unwrap().config, "classify"),
            "手动新规则"
        );
        assert_eq!(
            app.state.lock().unwrap().prompt_versions.len(),
            TARGETS.len() + 1
        );
    }

    #[test]
    fn retry_publishes_only_a_validated_result_and_failure_leaves_no_placeholder() {
        let app = fixture();
        let old = archive(&app);
        app.persist().unwrap();
        let before = json!(*app.state.lock().unwrap());
        assert!(!has_result(&old));
        let result = json!({"improved":true,"reason":"合成纠正依据","proposed_prompt":"先确认材料","evidence_ids":[old.evidence["audit"][0]["evidence_id"]]});
        for failure in [
            Err(crate::ai_control::CANCELLED.to_string()),
            Ok(json!({"improved":true,"reason":"缺少依据","proposed_prompt":"","evidence_ids":[]})),
        ] {
            assert!(app.retry_prompt_review_with(&old.id, |_| failure).is_err());
            assert_eq!(json!(*app.state.lock().unwrap()), before);
        }
        app.retry_prompt_review_with(&old.id, |_| {
            let disk: Value =
                serde_json::from_slice(&fs::read(app.root.join("data/state.json")).unwrap())
                    .unwrap();
            assert_eq!(disk["prompt_reviews"].as_array().unwrap().len(), 1);
            assert!(!app
                .state
                .lock()
                .unwrap()
                .prompt_reviews
                .iter()
                .any(has_result));
            Ok(result)
        })
        .unwrap();
        let restored = App::load(app.root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        assert_eq!(s.prompt_reviews.len(), 2);
        assert!(s.prompt_reviews[0].archived_at > 0);
        assert_eq!(s.prompt_reviews[1].status, "ready");
        assert!(has_result(&s.prompt_reviews[1]));
        assert_eq!(content(&s.config, "classify"), old.base_prompt);
        let current = s.prompt_reviews[1].id.clone();
        drop(s);
        restored.retry_prompt_review_with(&current, |_|Ok(json!({"improved":false,"reason":"无需修改","proposed_prompt":"","evidence_ids":[]}))).unwrap();
        let snapshot = restored.snapshot_page(Some("prompts"));
        assert_eq!(snapshot["prompt_pending"], 0);
        assert_eq!(
            snapshot["evolution"]["prompts"].as_array().unwrap().len(),
            1
        );
        assert_eq!(snapshot["evolution"]["prompts"][0]["status"], "unchanged");
    }

    #[test]
    fn retry_preserves_original_on_save_failure_and_rejects_a_changed_baseline() {
        let app = fixture();
        let old = archive(&app);
        app.persist().unwrap();
        let before = json!(*app.state.lock().unwrap());
        let result =
            json!({"improved":false,"reason":"无需修改","proposed_prompt":"","evidence_ids":[]});
        fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .retry_prompt_review_with(&old.id, |_| Ok(result.clone()))
            .is_err());
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        fs::remove_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .retry_prompt_review_with(&old.id, |_| {
                app.state.lock().unwrap().prompt_reviews[0].status = "rejected".into();
                Ok(result)
            })
            .is_err());
        assert_eq!(app.state.lock().unwrap().prompt_reviews.len(), 1);
    }
}
pub fn current<'a>(s: &'a State, target: &str) -> Option<&'a PromptVersion> {
    s.prompt_versions.iter().rev().find(|v| v.target == target)
}
pub fn has_result(r: &PromptReview) -> bool {
    ["ready", "adopted", "rejected", "unchanged", "superseded"].contains(&r.status.as_str())
        && !r.reason.trim().is_empty()
        && (r.status == "unchanged" || !r.proposed_prompt.trim().is_empty())
}
pub fn record(s: &mut State, target: &str, text: String, action: &str, reason: &str) -> String {
    let parent = current(s, target).map(|v| v.id.clone()).unwrap_or_default();
    let version = PromptVersion {
        id: id(),
        target: target.into(),
        content: text,
        previous_id: parent,
        action: action.into(),
        reason: reason.into(),
        created: now(),
    };
    let id = version.id.clone();
    s.prompt_versions.push(version);
    id
}
pub fn initialize(s: &mut State) {
    for target in TARGETS {
        let text = content(&s.config, target);
        if current(s, target).map(|v| v.content.as_str()) != Some(text.as_str()) {
            record(s, target, text, "initial", "迁移现有提示词，保留生效内容");
        }
    }
}
pub fn record_config_changes(s: &mut State, next: &Config) {
    for target in TARGETS {
        let text = content(next, target);
        if content(&s.config, target) != text {
            record(s, target, text, "manual", "手动保存系统提示词");
        }
    }
}
#[cfg(test)]
pub fn queue(s: &mut State, _logs: &[Value]) {
    let eligible = s
        .audit
        .iter()
        .enumerate()
        .skip(s.archive_review_cursor)
        .filter(|(_, a)| crate::engine::learning_evidence(a))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    for batch in eligible.chunks(6) {
        let audit=batch.iter().map(|i|json!({"evidence_id":format!("audit:{i}"),"event":crate::context_budget::audit(&s.audit[*i])})).collect::<Vec<_>>();
        let evidence = json!({"audit":audit,"logs":[]});
        if s.prompt_reviews
            .iter()
            .any(|r| r.status == "pending" && r.evidence == evidence)
        {
            continue;
        }
        s.prompt_reviews.push(PromptReview {
            id: id(),
            day: date(),
            created: now(),
            status: "pending".into(),
            base_version: current(s, "classify")
                .map(|v| v.id.clone())
                .unwrap_or_default(),
            base_prompt: content(&s.config, "classify"),
            evidence,
            ..Default::default()
        });
    }
    s.archive_review_cursor = s.audit.len();
}

impl App {
    pub fn prompt_history(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"versions":s.prompt_versions,"reviews":s.prompt_reviews.iter().rev().map(|r|{let mut v=json!(r);v.as_object_mut().unwrap().remove("evidence");v}).collect::<Vec<_>>(),"current":TARGETS.iter().map(|k|((*k).to_owned(),current(&s,k).map(|v|v.id.clone()).unwrap_or_default())).collect::<std::collections::BTreeMap<_,_>>()})
    }
    #[cfg(test)]
    pub(crate) fn finish_prompt_review(&self, id: &str, result: Value) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let value = apply_review(&mut s, id, result)?;
        self.save(&s)?;
        *lock = s;
        Ok(value)
    }
    pub fn adopt_prompt(&self, review_id: &str) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let index = s
            .prompt_reviews
            .iter()
            .position(|r| r.id == review_id)
            .ok_or("建议不存在")?;
        let r = s.prompt_reviews[index].clone();
        if r.status != "ready" {
            return Err("该建议尚不能采用".into());
        }
        if current(&s, "classify").map(|v| v.id.as_str()) != Some(r.base_version.as_str())
            || content(&s.config, "classify") != r.base_prompt
        {
            return Err("当前提示词已变化，请重新生成建议，避免覆盖新版本".into());
        }
        set_content(&mut s.config, "classify", r.proposed_prompt.clone())?;
        let version = record(
            &mut s,
            "classify",
            r.proposed_prompt.clone(),
            "adopt",
            &r.reason,
        );
        s.prompt_reviews[index].status = "adopted".into();
        s.rule_version += 1;
        App::audit(
            &mut s,
            "prompt_adopt",
            "",
            &r.reason,
            json!({"version":r.base_version}),
            json!({"version":version}),
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!({"version":version}))
    }
    pub fn rollback_prompt(&self, id: &str, expected: &str) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let old = s
            .prompt_versions
            .iter()
            .find(|v| v.id == id)
            .cloned()
            .ok_or("版本不存在")?;
        if current(&s, &old.target).map(|v| v.id.as_str()) != Some(expected) {
            return Err("提示词已更新，请刷新版本列表".into());
        }
        if old.id == expected {
            return Err("当前已是此版本".into());
        }
        set_content(&mut s.config, &old.target, old.content.clone())?;
        if old.target == "handling" {
            crate::evolution::sync_handling_prompt(&mut s, &old.content, "从系统提示词历史回滚");
        }
        let version = record(
            &mut s,
            &old.target,
            old.content,
            "rollback",
            &format!("回滚到 {}", old.id),
        );
        s.rule_version += 1;
        App::audit(
            &mut s,
            "prompt_rollback",
            "",
            "回滚系统提示词",
            json!({"version":expected}),
            json!({"version":version,"restored_from":id}),
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!({"version":version}))
    }
    pub fn retry_prompt_review(&self, id: &str) -> Result<Value, String> {
        self.retry_prompt_review_with(id, |payload| {
            self.ask_ai_mode("prompt_review", payload, AiMode::Manual)
        })
    }
    fn retry_prompt_review_with(
        &self,
        id: &str,
        invoke: impl FnOnce(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        if self.prompt_busy.swap(true, Ordering::SeqCst) {
            return Err("提示词建议正在生成".into());
        }
        struct Busy<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Busy<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _busy = Busy(&self.prompt_busy);
        let (old, mut next) = {
            let s = self.state.lock().unwrap();
            let old = s
                .prompt_reviews
                .iter()
                .find(|r| r.id == id)
                .cloned()
                .ok_or("建议不存在")?;
            if old.archived_at > 0 || ["adopted", "rejected"].contains(&old.status.as_str()) {
                return Err("建议已处理，请刷新后选择可重新评估的记录".into());
            }
            let mut next = old.clone();
            next.id = crate::model::id();
            next.status = "pending".into();
            next.base_version = current(&s, "classify")
                .map(|v| v.id.clone())
                .unwrap_or_default();
            next.base_prompt = content(&s.config, "classify");
            next.created = now();
            next.last_attempt = now();
            next.proposed_prompt.clear();
            next.reason.clear();
            next.error.clear();
            next.evidence_ids.clear();
            (old, next)
        };
        let result = invoke(
            &json!({"day":next.day,"current_prompt":next.base_prompt,"daily_evidence":next.evidence}),
        )?;
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if s.prompt_reviews
            .iter()
            .find(|r| r.id == old.id)
            .map(|r| json!(r))
            != Some(json!(old))
            || current(&s, "classify").map(|v| v.id.as_str()) != Some(next.base_version.as_str())
            || content(&s.config, "classify") != next.base_prompt
        {
            return Err("建议或提示词版本已变化，本次结果未保存".into());
        }
        next.archived_at = 0;
        let next_id = next.id.clone();
        s.prompt_reviews.push(next);
        let value = apply_review(&mut s, &next_id, result)?;
        s.prompt_reviews
            .iter_mut()
            .find(|r| r.id == old.id)
            .unwrap()
            .archived_at = now();
        self.save(&s)?;
        *lock = s;
        Ok(value)
    }
}

pub(crate) fn apply_review(s: &mut State, id: &str, result: Value) -> Result<Value, String> {
    let r = s
        .prompt_reviews
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or("建议不存在")?;
    if r.status != "pending" {
        return Err("建议状态已变化".into());
    }
    let improved = result["improved"].as_bool().ok_or("缺少是否改进的判断")?;
    let reason = result["reason"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 4000)
        .ok_or("修改原因无效")?;
    let proposed = result["proposed_prompt"].as_str().ok_or("缺少完整提示词")?;
    if improved && (proposed.trim().is_empty() || proposed.chars().count() > 20000) {
        return Err("新提示词应为 1–20000 字".into());
    }
    let evidence = result["evidence_ids"]
        .as_array()
        .ok_or("缺少日志依据")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("日志依据格式无效".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let allowed = r.evidence["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(r.evidence["logs"].as_array().into_iter().flatten())
        .filter_map(|v| v["evidence_id"].as_str())
        .collect::<Vec<_>>();
    if improved
        && (evidence.is_empty() || evidence.iter().any(|id| !allowed.contains(&id.as_str())))
    {
        return Err("新提示词没有有效的当日日志依据".into());
    }
    r.status = if improved && proposed != r.base_prompt {
        "ready"
    } else {
        "unchanged"
    }
    .into();
    r.reason = reason.into();
    r.proposed_prompt = proposed.into();
    r.evidence_ids = evidence;
    r.error.clear();
    let value = json!(r);
    Ok(value)
}
