use crate::{engine::App, model::*, prompts};
use serde_json::{json, Value};

pub fn decision(r: &Suggestion) -> &str {
    if r.decision.is_empty() {
        if r.active {
            "enabled"
        } else {
            "pending"
        }
    } else {
        &r.decision
    }
}
// Archive-time prompt updates are whole-prompt replacements validated by the
// prompt review path. Nothing is appended to the current prompt here; enabled
// rules stay active and are injected into the system segment at request time.
pub fn sync_handling_prompt(s: &mut State, text: &str, reason: &str) {
    if let Some(old) = s.handling.versions.last().cloned() {
        if old.policy.prompt != text {
            let mut next = old.clone();
            next.id = id();
            next.previous = old.id;
            next.created = now();
            next.action = "manual".into();
            next.reason = reason.into();
            next.policy.prompt = text.into();
            s.handling.versions.push(next);
        }
    }
}
fn target(s: &State, kind: &str, id: &str) -> Result<Value, String> {
    let mut value = match kind {
        "rule" => s
            .suggestions
            .iter()
            .find(|r| r.id == id && r.archived_at == 0 && decision(r) == "pending")
            .map(|r| json!(r)),
        "tag" => s
            .tag_state
            .proposals
            .iter()
            .find(|r| r.id == id && r.archived_at == 0 && r.status == "pending")
            .map(|r| json!(r)),
        "prompt" => s
            .prompt_reviews
            .iter()
            .find(|r| r.id == id && r.archived_at == 0 && r.status == "ready")
            .map(|r| json!(r)),
        "handling" => s
            .handling
            .proposals
            .iter()
            .find(|r| r.id == id && r.archived_at == 0 && !r.adopted && !r.rejected)
            .map(|r| json!(r)),
        _ => None,
    }
    .ok_or("建议已处理或不存在，请刷新".to_string())?;
    value["current_prompt_version"] = json!(if kind == "handling" {
        s.handling.versions.last().map(|v| v.id.clone())
    } else {
        prompts::current(s, "classify").map(|v| v.id.clone())
    });
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> App {
        App::load(std::env::temp_dir().join(format!("shishi-evolution-{}", id()))).unwrap()
    }
    fn seed(app: &App) {
        let mut s = app.state.lock().unwrap();
        for ident in ["accepted", "rejected", "pending"] {
            s.suggestions.push(Suggestion {
                id: ident.into(),
                title: ident.into(),
                rule: format!("{ident}判断规则"),
                created: now(),
                ..Default::default()
            });
        }
    }
    fn archive(app: &App) -> Result<Value, String> {
        let t = app
            .task_edit(&json!({"task":{"title":"归档样例","status":"待处理"}}))
            .unwrap();
        app.archive_task(&json!({"id":t["id"],"updated":t["updated"],"status":"已完成"}))
    }
    #[test]
    fn manual_archive_keeps_enabled_rules_and_does_not_queue_prompt_rewrites() {
        let app = fixture();
        seed(&app);
        let before = prompts::content(&app.state.lock().unwrap().config, "classify");
        app.advice_decide(&json!({"kind":"rule","id":"accepted","action":"enable"}))
            .unwrap();
        app.advice_decide(&json!({"kind":"rule","id":"rejected","action":"reject"}))
            .unwrap();
        archive(&app).unwrap();
        archive(&app).unwrap();
        let reloaded = App::load(app.root.clone()).unwrap();
        let s = reloaded.state.lock().unwrap();
        // Manual archive notes no longer append enabled rules to the base
        // prompt; the rule is injected at request time instead.
        assert_eq!(prompts::content(&s.config, "classify"), before);
        assert_eq!(s.suggestions.len(), 3);
        assert!(s.suggestions[0].active);
        assert_eq!(s.suggestions[0].decision, "enabled");
        assert_eq!(s.suggestions[1].decision, "rejected");
        assert_eq!(s.suggestions[2].archived_at, 0);
        assert!(!s.prompt_versions.iter().any(|v| v.action == "consolidate"));
        assert!(s.prompt_reviews.is_empty());
    }
    #[test]
    fn archive_failure_does_not_hide_suggestions_or_change_prompt() {
        let app = fixture();
        seed(&app);
        app.advice_decide(&json!({"id":"accepted","action":"enable"}))
            .unwrap();
        let t = app.task_edit(&json!({"task":{"title":"例子"}})).unwrap();
        let before = json!(*app.state.lock().unwrap());
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .archive_task(&json!({"id":t["id"],"updated":t["updated"],"status":"已完成"}))
            .is_err());
        assert_eq!(json!(*app.state.lock().unwrap()), before);
    }
    #[test]
    fn feedback_is_reviewable_and_rejects_stale_results_and_failed_save() {
        let app = fixture();
        seed(&app);
        let before = target(&app.state.lock().unwrap(), "rule", "pending").unwrap();
        let result =
            json!({"reason":"按人工批复修正","title":"改进判断","rule":"明确证据后再判断"});
        app.finish_advice_feedback("rule", "pending", "保留不确定状态", &before, result.clone())
            .unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.advice_revisions.len(), 1);
            assert!(!s.suggestions[2].active);
            assert_eq!(s.suggestions[2].rule, "明确证据后再判断");
        }
        assert!(app
            .finish_advice_feedback("rule", "pending", "旧批复", &before, result.clone())
            .is_err());
        let updated = target(&app.state.lock().unwrap(), "rule", "pending").unwrap();
        let mut c = app.state.lock().unwrap().config.clone();
        c.classify_prompt = "新的用户规则".into();
        app.configure(json!(c)).unwrap();
        assert!(app
            .finish_advice_feedback("rule", "pending", "过期基准", &updated, result.clone())
            .is_err());
        let latest = target(&app.state.lock().unwrap(), "rule", "pending").unwrap();
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .finish_advice_feedback("rule", "pending", "保存失败", &latest, result)
            .is_err());
        assert_eq!(app.state.lock().unwrap().advice_revisions.len(), 1);
    }
    #[test]
    fn handling_prompt_edits_and_rollbacks_share_the_live_policy() {
        let app = fixture();
        let initial = prompts::current(&app.state.lock().unwrap(), "handling")
            .unwrap()
            .id
            .clone();
        let mut c = app.state.lock().unwrap().config.clone();
        c.handling_prompt = "先核对证据和环境，再提出建议".into();
        c.review_prompt = "按增量记录审校".into();
        app.configure(json!(c)).unwrap();
        let edited = prompts::current(&app.state.lock().unwrap(), "handling")
            .unwrap()
            .id
            .clone();
        assert_ne!(initial, edited);
        assert_eq!(
            app.state
                .lock()
                .unwrap()
                .handling
                .versions
                .last()
                .unwrap()
                .policy
                .prompt,
            "先核对证据和环境，再提出建议"
        );
        app.rollback_prompt(&initial, &edited).unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(
            s.handling.versions.last().unwrap().policy.prompt,
            prompts::content(&s.config, "handling")
        );
        assert_eq!(
            prompts::content(&s.config, "prompt_review"),
            "按增量记录审校"
        );
        assert_eq!(s.handling.versions.len(), 3);
    }
    #[test]
    fn model_profiles_dispatch_independently_and_partial_config_uses_base_model() {
        let mut c = Config {
            model: "base-model".into(),
            model_profiles: None,
            reasoning_effort: "low".into(),
            ..Default::default()
        };
        assert_eq!(c.for_kind("handling").model, "base-model");
        c.model_profiles = Some(ModelProfiles {
            filter: ModelChoice {
                connection_id: String::new(),
                model: "filter-model".into(),
                reasoning_effort: "low".into(),
            },
            analysis: ModelChoice {
                connection_id: String::new(),
                model: "analysis-model".into(),
                reasoning_effort: "medium".into(),
            },
            handling: ModelChoice {
                connection_id: String::new(),
                model: "handling-model".into(),
                reasoning_effort: "high".into(),
            },
            review: ModelChoice {
                connection_id: String::new(),
                model: "review-model".into(),
                reasoning_effort: "low".into(),
            },
        });
        for kind in ["classify", "format", "edit", "schedule"] {
            assert_eq!(c.for_kind(kind).model, "analysis-model");
        }
        assert_eq!(c.for_kind("handling").reasoning_effort, "high");
        for kind in [
            "prompt_review",
            "learning",
            "report",
            "handling_learning",
            "advice_revision",
        ] {
            assert_eq!(c.for_kind(kind).model, "review-model");
        }
    }
    #[test]
    fn manual_merge_combines_generic_labels_without_environment_exception() {
        let app = fixture();
        let primary = app
            .task_edit(&json!({"task":{"title":"主事项","tags":[{"kind":"所属项目","value":"示例项目A"}]}}))
            .unwrap();
        let other=app.task_edit(&json!({"task":{"title":"另一事项","tags":[{"kind":"所属项目","value":"示例项目B"}],"description":"关联背景"}})).unwrap();
        let ids = json!([primary["id"], other["id"]]);
        app.merge(&json!({"ids":ids})).unwrap();
        let s = app.state.lock().unwrap();
        assert!(s.tasks[0].environment.is_empty());
        assert_eq!(s.tasks[0].tags.len(), 2);
        assert!(s.tasks[0].description.contains("关联背景"));
        assert_eq!(s.tasks[1].merged_into, s.tasks[0].id);
    }
}
impl App {
    pub fn advice_decide(&self, v: &Value) -> Result<Value, String> {
        let kind = v["kind"].as_str().unwrap_or("rule");
        let ident = v["id"].as_str().ok_or("缺少建议编号")?;
        let action = v["action"].as_str().unwrap_or("");
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let before = target(&s, kind, ident)?;
        match kind {
            "rule" => {
                if !["enable", "reject"].contains(&action) {
                    return Err("请选择启用或拒绝".into());
                }
                let r = s.suggestions.iter_mut().find(|r| r.id == ident).unwrap();
                r.active = action == "enable";
                r.decision = if r.active { "enabled" } else { "rejected" }.into();
                r.updated = now();
                s.rule_version += 1;
            }
            "prompt" if action == "reject" => {
                s.prompt_reviews
                    .iter_mut()
                    .find(|r| r.id == ident)
                    .unwrap()
                    .status = "rejected".into()
            }
            "handling" if action == "reject" => {
                s.handling
                    .proposals
                    .iter_mut()
                    .find(|r| r.id == ident)
                    .unwrap()
                    .rejected = true
            }
            _ => return Err("不支持的建议操作".into()),
        }
        Self::audit(
            &mut s,
            "advice_decision",
            "",
            "明确处理迭代建议，下次归档收起",
            before,
            json!({"kind":kind,"id":ident,"action":action}),
        );
        self.save(&s)?;
        *lock = s;
        Ok(json!(true))
    }
    pub fn advice_feedback(&self, v: &Value) -> Result<Value, String> {
        let kind = v["kind"].as_str().unwrap_or("rule");
        let ident = v["id"].as_str().ok_or("缺少建议编号")?;
        let feedback = v["text"].as_str().unwrap_or("").trim();
        if feedback.is_empty() || feedback.chars().count() > 4000 {
            return Err("请输入1–4000字的批复意见".into());
        }
        let (before, evidence, current) = {
            let s = self.state.lock().unwrap();
            let before = target(&s, kind, ident)?;
            let evidence = if kind == "rule" {
                json!(before["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i.as_u64().and_then(|i| s.audit.get(i as usize)))
                    .collect::<Vec<_>>())
            } else if kind == "handling" {
                json!(s.handling.runs.iter().filter(|run|before["evidence"].as_array().map(|ids|ids.contains(&json!(run.id))||run.rounds.iter().any(|v|ids.contains(&json!(v.id)))).unwrap_or(false)).map(|run|json!({"id":run.id,"project":run.project_id,"instructions":run.instructions,"rounds":run.rounds.iter().rev().take(5).map(|r|json!({"question":r.question.chars().take(3000).collect::<String>(),"conclusion":r.conclusion.chars().take(4000).collect::<String>()})).collect::<Vec<_>>()})).collect::<Vec<_>>())
            } else {
                before
                    .get("evidence")
                    .or(before.get("excerpts"))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            (
                before,
                evidence,
                prompts::content(
                    &s.config,
                    if kind == "handling" {
                        "handling"
                    } else {
                        "classify"
                    },
                ),
            )
        };
        let result=self.ask_ai("advice_revision",&json!({"kind":kind,"suggestion":before,"evidence":evidence,"current_prompt":current,"user_feedback":feedback}))?;
        self.finish_advice_feedback(kind, ident, feedback, &before, result)
    }
    pub(crate) fn finish_advice_feedback(
        &self,
        kind: &str,
        ident: &str,
        feedback: &str,
        before: &Value,
        result: Value,
    ) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if target(&s, kind, ident)? != *before {
            return Err("AI 分析期间建议已变化，本次结果未覆盖；请重新批复".into());
        }
        let bounded = |key: &str, max: usize| -> Result<String, String> {
            let text = result[key].as_str().unwrap_or("").trim();
            if text.is_empty() || text.chars().count() > max {
                return Err(format!("AI 返回的 {key} 无效，原建议保留"));
            }
            Ok(text.into())
        };
        let reason = bounded("reason", 4000)?;
        match kind {
            "rule" => {
                let r = s.suggestions.iter_mut().find(|r| r.id == ident).unwrap();
                r.title = bounded("title", 200)?;
                r.rule = bounded("rule", 6000)?;
                r.updated = now();
            }
            "tag" => {
                let tag: crate::tags::Tag =
                    serde_json::from_value(result["tag"].clone()).map_err(|e| e.to_string())?;
                if !crate::tags::valid(&tag) {
                    return Err("AI 返回的标签名称无效".into());
                }
                let r = s
                    .tag_state
                    .proposals
                    .iter_mut()
                    .find(|r| r.id == ident)
                    .unwrap();
                r.tag = tag;
                r.reason = reason.clone();
            }
            "prompt" => {
                let base = prompts::current(&s, "classify").unwrap().id.clone();
                let current = prompts::content(&s.config, "classify");
                let r = s.prompt_reviews.iter_mut().find(|r| r.id == ident).unwrap();
                r.proposed_prompt = bounded("prompt", 20000)?;
                r.base_prompt = current;
                r.base_version = base;
                r.reason = reason.clone();
            }
            "handling" => {
                let base = s.handling.versions.last().unwrap().id.clone();
                let r = s
                    .handling
                    .proposals
                    .iter_mut()
                    .find(|r| r.id == ident)
                    .unwrap();
                r.policy.prompt = bounded("prompt", 20000)?;
                r.base = base;
                r.reason = reason.clone();
            }
            _ => return Err("未知建议类型".into()),
        }
        let after = target(&s, kind, ident)?;
        s.advice_revisions.push(json!({"id":id(),"kind":kind,"suggestion_id":ident,"time":now(),"feedback":feedback,"reason":reason,"before":before,"after":after}));
        Self::audit(
            &mut s,
            "advice_feedback",
            "",
            "按用户批复修改建议，仍待用户启用",
            before.clone(),
            after.clone(),
        );
        self.save(&s)?;
        *lock = s;
        Ok(after)
    }
}
