//! Report generation and manual learning orchestration.
use super::*;

impl App {
    pub fn report(&self, label: &str, mode: AiMode) -> Result<Value, String> {
        self.require_ai_purpose("report")?;
        let mut state_lock = self.state.lock().unwrap();
        let mut s = state_lock.clone();
        let start = s.report_cursor.min(s.audit.len());
        let delta = s.audit[start..].to_vec();
        let changed = delta
            .iter()
            .map(|a| a.task_id.clone())
            .collect::<BTreeSet<_>>();
        let previous=s.reports.first().map(|r|json!({"id":r["id"],"time":r["time"],"label":r["label"],"suggestions":r["suggestions"]}));
        let status_changes = StatusChanges::new(&s.audit);
        let at = now();
        let tasks = s
            .tasks
            .iter()
            .filter(|t| {
                (!t.archived || changed.contains(&t.id) || status_changes.today(t, at))
                    && t.merged_into.is_empty()
                    && t.generation.is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut r = json!({"id":id(),"previous_report":previous,"delta":delta,"suggestions":s.suggestions,"time":now(),"label":label,"date":date(),"data_until":s.last_scan,"pending_analysis":crate::context_budget::pending(&s).len(),"tasks":tasks,"usage":{"calls":s.calls,"input_tokens":s.input_tokens,"output_tokens":s.output_tokens}});
        let prior = s.reports.first().cloned();
        let reuse = prior.as_ref().filter(|old| {
            delta.is_empty()
                && old["tasks"] == r["tasks"]
                && old["suggestions"] == r["suggestions"]
                && old["ai_summary"].is_object()
        });
        if let Some(old) = reuse {
            r["ai_summary"] = old["ai_summary"].clone();
            r["summary_status"] = json!("reused");
        } else {
            r["summary_status"] = json!("pending");
        }
        s.report_cursor = s.audit.len();
        s.reports.insert(0, r.clone());
        s.reports.truncate(60);
        self.save(&s)?;
        *state_lock = s;
        drop(state_lock);
        if let Err(error) = self.write_report_file(&r) {
            self.log(
                "report_file_failed",
                json!({"error":error,"report":r["id"]}),
            );
        }
        if r["summary_status"] == "pending" {
            return self.summarize_report(r, mode);
        }
        Ok(r)
    }
    fn summarize_report(&self, mut r: Value, mode: AiMode) -> Result<Value, String> {
        if !self.ai_allowed(mode) {
            return Ok(r);
        }
        if self.summary_busy.swap(true, Ordering::SeqCst) {
            return Ok(r);
        }
        let _summary_guard = Guard(&self.summary_busy);
        let tasks=r["tasks"].as_array().unwrap().iter().map(|t| json!({"id":t["id"],"title":t["title"],"description":t["description"],"status":t["status"],"tags":t["tags"],"due":t["due"],"blocker":t["blocker"],"waiting_for":t["waiting_for"],"archive_note":t["archive_note"]})).collect::<Vec<_>>();
        let prior = {
            let s = self.state.lock().unwrap();
            s.reports
                .iter()
                .find(|x| x["id"] == r["previous_report"]["id"])
                .map(|x| x["ai_summary"].clone())
        };
        let payload = json!({"report_tasks":tasks,"delta":r["delta"],"suggestions":r["suggestions"],"previous_summary":prior,"label":r["label"],"time":stamp(now()),"_trigger":if mode == AiMode::Manual {"manual_report"} else {"scheduled_report"}});
        match self.ask_ai_mode("report", &payload, mode) {
            Ok(v)
                if v["summary"].is_string()
                    && ["completed", "attention", "next_steps"]
                        .iter()
                        .all(|k| v[k].is_array()) =>
            {
                r["ai_summary"] = v;
                r["summary_status"] = json!("done");
                r.as_object_mut().unwrap().remove("summary_error");
            }
            Err(e) if e == work_hours::DEFERRED => return Ok(r),
            Err(e) if e == crate::ai_control::CANCELLED => {
                r["summary_status"] = json!("interrupted");
                r["summary_error"] = json!(e);
            }
            Ok(_) => {
                r["summary_status"] = json!("failed");
                r["summary_error"] = json!("汇总输出格式不正确");
            }
            Err(e) => {
                r["summary_status"] = json!("failed");
                r["summary_error"] = json!(e);
            }
        }
        let mut state_lock = self.state.lock().unwrap();
        let mut s = state_lock.clone();
        if let Some(old) = s.reports.iter_mut().find(|x| x["id"] == r["id"]) {
            *old = r.clone();
        }
        self.save(&s)?;
        *state_lock = s;
        drop(state_lock);
        if let Err(error) = self.write_report_file(&r) {
            self.log(
                "report_file_failed",
                json!({"error":error,"report":r["id"]}),
            );
        }
        if r["summary_error"] == crate::ai_control::CANCELLED {
            return Err(crate::ai_control::CANCELLED.into());
        }
        Ok(r)
    }
    pub fn learn(&self, mode: AiMode) -> Result<Value, String> {
        self.require_ai_purpose("learning")?;
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        process::check_running()?;
        self.learning_last_attempt.store(now(), Ordering::SeqCst);
        let (payload, cutoff, eligible_ids, history_ids) = {
            let mut state_lock = self.state.lock().unwrap();
            let mut s = state_lock.clone();
            let eligible = s
                .audit
                .iter()
                .enumerate()
                .skip(s.learning_cursor)
                .filter(|(_, a)| learning_evidence(a))
                .take(6)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            if eligible.is_empty() {
                s.pending_learning = false;
                s.learning_cursor = s.audit.len();
                self.save(&s)?;
                *state_lock = s;
                return Ok(json!({"skipped":"无新增纠错或归档备注，沿用既有建议"}));
            }
            let cutoff = if eligible.len() == 6 {
                eligible[5] + 1
            } else {
                s.audit.len()
            };
            s.pending_learning = true;
            let history_ids = s
                .audit
                .iter()
                .enumerate()
                .take(s.learning_cursor)
                .filter(|(_, a)| {
                    learning_evidence(a)
                        && eligible
                            .iter()
                            .any(|i| s.audit[*i].task_id == a.task_id || s.audit[*i].kind == a.kind)
                })
                .map(|(i, _)| i)
                .rev()
                .take(3)
                .collect::<Vec<_>>();
            let history = history_ids
                .iter()
                .map(
                    |i| json!({"audit_index":i,"event":crate::context_budget::audit(&s.audit[*i])}),
                )
                .collect::<Vec<_>>();
            let audit=eligible.iter().map(|i|{
                let a=&s.audit[*i];let task=s.tasks.iter().find(|t|t.id==a.task_id);
                let messages=task.map(|t|{let all=t.evidence.all(); all.iter().filter_map(|id|s.messages.get(id)).filter(|m|!m.withdrawn).rev().take(1).map(|m|json!({"id":m.id,"sender":m.sender,"mine":m.mine,"text":m.text.chars().take(400).collect::<String>()})).collect::<Vec<_>>()}).unwrap_or_default();
                json!({"audit_index":i,"event":crate::context_budget::audit(a),"messages":messages})
            }).collect::<Vec<_>>();
            let relevant = eligible
                .iter()
                .map(|i| s.audit[*i].task_id.as_str())
                .collect::<BTreeSet<_>>();
            let prepared = (
                json!({"audit":audit,"history":history,"existing_suggestions":crate::context_budget::suggestions(&s,&json!(audit).to_string()),"current_prompt":prompts::content(&s.config,"classify"),"current_tasks":crate::context_budget::rows(s.tasks.iter().filter(|t|relevant.contains(t.id.as_str())).map(crate::context_budget::task),6,3000)}),
                cutoff,
                eligible,
                history_ids,
            );
            self.save(&s)?;
            *state_lock = s;
            prepared
        };
        let result = self.ask_ai_mode("learning", &payload, mode)?;
        let rows = result["suggestions"].as_array().ok_or("缺少建议数组")?;
        let mut locked = self.state.lock().unwrap();
        let mut s = locked.clone();
        let (added, updated) = apply_learning(&mut s, rows, &eligible_ids, &history_ids)?;
        s.learning_cursor = cutoff;
        s.pending_learning = s.audit.iter().skip(cutoff).any(learning_evidence);
        self.save(&s)?;
        *locked = s;
        drop(locked);
        self.log(
            "learning_result",
            json!({"added":added,"updated":updated,"processed":eligible_ids.len()}),
        );
        Ok(
            json!({"added":added,"updated":updated,"message":if added+updated==0{"本次无新增建议，既有建议继续保留"}else{"建议已更新，未处理建议继续保留"}}),
        )
    }
}
