//! Report generation and manual learning orchestration.
use super::*;

#[derive(serde::Serialize)]
pub(crate) struct ReportOutcome<'a> {
    pub(crate) kind: &'static str,
    pub(crate) id: &'a str,
    label: &'a str,
    status: &'a str,
    finished: i64,
    error: &'a str,
}

impl State {
    /// Read committed results only. Archive batch reports are not whole attempts.
    pub(crate) fn report_outcome<'a>(&'a self) -> Option<ReportOutcome<'a>> {
        let mut latest: Option<ReportOutcome<'a>> = None;
        let mut consider = |outcome: ReportOutcome<'a>| {
            if latest.as_ref().is_none_or(|old| {
                (outcome.finished, outcome.status == "succeeded")
                    >= (old.finished, old.status == "succeeded")
            }) {
                latest = Some(outcome);
            }
        };
        for attempt in &self.archive_attempts {
            if !["succeeded", "failed"].contains(&attempt.status.as_str()) {
                continue;
            }
            consider(ReportOutcome {
                kind: "archive",
                id: &attempt.id,
                label: "归档",
                status: &attempt.status,
                finished: attempt.finished.max(attempt.started).max(attempt.created),
                error: &attempt.error,
            });
        }
        for report in self.reports.iter().rev() {
            if report["attempt_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
                || report["remaining_events"].as_u64().unwrap_or(0) > 0
                || report["archive_complete"] == false
            {
                continue;
            }
            let status = match report["summary_status"].as_str() {
                Some("done" | "reused" | "fallback") => "succeeded",
                Some("failed" | "interrupted") => "failed",
                _ => continue,
            };
            consider(ReportOutcome {
                kind: "report",
                id: report["id"].as_str().unwrap_or(""),
                label: report["label"].as_str().unwrap_or("报表"),
                status,
                finished: report["finished"]
                    .as_i64()
                    .filter(|time| *time > 0)
                    .unwrap_or_else(|| report["time"].as_i64().unwrap_or(0)),
                error: report["summary_error"].as_str().unwrap_or(""),
            });
        }
        for job in &self.morning_jobs {
            if !["succeeded", "failed"].contains(&job.status.as_str()) {
                continue;
            }
            // Older jobs have no completion time. Use their original claim,
            // never the upgrade/reload time, so old failures stay historical.
            let finished = if job.finished > 0 {
                job.finished
            } else {
                chrono::NaiveDate::parse_from_str(&job.business_day, "%Y-%m-%d")
                    .ok()
                    .map(|day| {
                        self.work_scheduler
                            .runtime
                            .day_records
                            .get(&day)
                            .and_then(|record| record.morning_claim.as_ref())
                            .filter(|claim| claim.claim_id == job.id)
                            .map(|claim| claim.claimed_at_ms)
                            .unwrap_or_else(|| {
                                day.and_hms_opt(0, 0, 0)
                                    .unwrap()
                                    .and_utc()
                                    .timestamp_millis()
                                    - 8 * 3_600_000
                            })
                    })
                    .unwrap_or(0)
            };
            consider(ReportOutcome {
                kind: "morning",
                id: &job.id,
                label: "早报",
                status: &job.status,
                finished,
                error: &job.error,
            });
        }
        latest
    }

    /// Called under the existing State lock, before the result's atomic save.
    /// Distinguish completions in the same millisecond, including clock rollback.
    pub(crate) fn next_report_finished(&self, at: i64) -> i64 {
        self.report_outcome()
            .map_or(at, |outcome| at.max(outcome.finished.saturating_add(1)))
    }

    pub(crate) fn truncate_reports(&mut self) {
        if self.reports.len() <= 60 {
            return;
        }
        // Pending reports must not evict the result that resolved an older error.
        let keep = self
            .report_outcome()
            .filter(|outcome| outcome.kind == "report")
            .and_then(|outcome| self.reports.iter().position(|row| row["id"] == outcome.id));
        if let Some(index) = keep.filter(|index| *index >= 60) {
            let completed = self.reports.remove(index);
            self.reports.truncate(59);
            self.reports.push(completed);
        } else {
            self.reports.truncate(60);
        }
    }
}

impl App {
    pub fn report(&self, label: &str, mode: AiMode) -> Result<Value, String> {
        let report = self.report_result(label, mode)?;
        if report["summary_error"] == crate::ai_control::CANCELLED {
            return Err(crate::ai_control::CANCELLED.into());
        }
        Ok(report)
    }

    // The scheduler needs the committed completion time even for cancellation.
    pub(crate) fn report_result(&self, label: &str, mode: AiMode) -> Result<Value, String> {
        self.require_ai_purpose("report")?;
        let r = self.commit_state(|s| {
            let start = s.report_cursor.min(s.audit.len());
            let delta = &s.audit[start..];
            let changed = delta
                .iter()
                .map(|a| a.task_id.as_str())
                .collect::<BTreeSet<_>>();
            let previous=s.reports.first().map(|r|json!({"id":r["id"],"time":r["time"],"label":r["label"],"suggestions":r["suggestions"]}));
            let status_changes = StatusChanges::new(&s.audit);
            let at = now();
            let tasks = s
                .tasks
                .iter()
                .filter(|t| {
                    (!t.archived || changed.contains(t.id.as_str()) || status_changes.today(t, at))
                        && t.merged_into.is_empty()
                        && t.generation.is_empty()
                })
                .collect::<Vec<_>>();
            let mut r = json!({"id":id(),"previous_report":previous,"delta":delta,"suggestions":s.suggestions,"time":now(),"label":label,"date":date(),"data_until":s.last_scan,"pending_analysis":crate::context_budget::pending_refs(&s, false).count(),"tasks":tasks,"usage":{"calls":s.calls,"input_tokens":s.input_tokens,"output_tokens":s.output_tokens}});
            let reuse = s.reports.first().filter(|old| {
                delta.is_empty()
                    && old["tasks"] == r["tasks"]
                    && old["suggestions"] == r["suggestions"]
                    && old["ai_summary"].is_object()
            });
            if let Some(old) = reuse {
                r["ai_summary"] = old["ai_summary"].clone();
                r["summary_status"] = json!("reused");
                r["finished"] = json!(s.next_report_finished(now()));
            } else {
                r["summary_status"] = json!("pending");
            }
            s.report_cursor = s.audit.len();
            s.reports.insert(0, r.clone());
            s.truncate_reports();
            Ok(r)
        })?;
        self.write_report_file(&r, "report_file_failed");
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
        let payload = json!({"report_tasks":tasks,"delta":r["delta"],"label":r["label"],"time":stamp(now()),"_trigger":if mode == AiMode::Manual {"manual_report"} else {"scheduled_report"}});
        match self.ask_ai_mode("report", &payload, mode) {
            Ok(v) => {
                r["ai_summary"] = v;
                r["summary_status"] = json!("done");
                r.as_object_mut().unwrap().remove("summary_error");
            }
            Err(e) if e == work_hours::DEFERRED => return Ok(r),
            Err(e) if e == crate::ai_control::CANCELLED => {
                r["summary_status"] = json!("interrupted");
                r["summary_error"] = json!(e);
            }
            Err(e) => {
                r["summary_status"] = json!("failed");
                r["summary_error"] = json!(e);
            }
        }
        self.commit_state(|s| {
            r["finished"] = json!(s.next_report_finished(now()));
            if let Some(old) = s.reports.iter_mut().find(|x| x["id"] == r["id"]) {
                *old = r.clone();
            } else {
                // Concurrent pending reports may have displaced this in-flight row.
                s.reports.insert(0, r.clone());
                s.truncate_reports();
            }
            Ok(())
        })?;
        self.write_report_file(&r, "report_file_failed");
        Ok(r)
    }
    pub fn learn(&self, mode: AiMode) -> Result<Value, String> {
        self.require_ai_purpose("learning")?;
        if !self.ai_allowed(mode) {
            return Ok(json!({"skipped":self.ai_deferred_reason()}));
        }
        process::check_running()?;
        let (frozen, mut eligible_ids) = self.commit_state(|s| {
            let eligible = s
                .audit
                .iter()
                .enumerate()
                .skip(s.learning_cursor)
                .filter(|(_, a)| learning_evidence(a))
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            s.pending_learning = !eligible.is_empty();
            if eligible.is_empty() {
                s.learning_cursor = s.audit.len();
            }
            Ok((s.clone(), eligible))
        })?;
        if eligible_ids.is_empty() {
            return Ok(json!({"skipped":"无新增纠错或归档备注，沿用既有建议"}));
        }
        let call = self.begin_ai_for("learning")?;
        let (count, request, history_ids) = self.learning_request(&frozen, &eligible_ids, mode)?;
        let cutoff = if count < eligible_ids.len() {
            eligible_ids[count - 1] + 1
        } else {
            frozen.audit.len()
        };
        eligible_ids.truncate(count);
        let result = self.invoke_prepared_request(&request, mode, &call, true)?;
        let rows = result["suggestions"].as_array().unwrap();
        let (added, updated) = self.commit_state(|s| {
            let counts = apply_learning(s, rows, &eligible_ids, &history_ids)?;
            s.learning_cursor = cutoff;
            s.pending_learning = s.audit.iter().skip(cutoff).any(learning_evidence);
            Ok(counts)
        })?;
        self.log(
            "learning_result",
            json!({"added":added,"updated":updated,"processed":eligible_ids.len()}),
        );
        Ok(
            json!({"added":added,"updated":updated,"message":if added+updated==0{"本次无新增建议，既有建议继续保留"}else{"建议已更新，未处理建议继续保留"}}),
        )
    }

    fn learning_request(
        &self,
        s: &State,
        eligible: &[usize],
        mode: AiMode,
    ) -> Result<(usize, PreparedAiRequest, Vec<usize>), String> {
        let relevant = eligible
            .iter()
            .map(|i| s.audit[*i].task_id.as_str())
            .collect::<BTreeSet<_>>();
        let kinds = eligible
            .iter()
            .map(|i| s.audit[*i].kind.as_str())
            .collect::<BTreeSet<_>>();
        let history = crate::context_budget::rows(
            s.audit
                .iter()
                .enumerate()
                .take(s.learning_cursor)
                .rev()
                .filter(|(_, a)| {
                    learning_evidence(a)
                        && (relevant.contains(a.task_id.as_str())
                            || kinds.contains(a.kind.as_str()))
                })
                .map(|(i, a)| json!({"audit_index":i,"event":crate::context_budget::audit(a)})),
            2800,
        );
        let history_ids = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["audit_index"].as_u64().unwrap() as usize)
            .collect();
        let tasks = s
            .tasks
            .iter()
            .map(|t| (t.id.as_str(), t))
            .collect::<BTreeMap<_, _>>();
        let audit = eligible.iter().map(|i| {
            let a = &s.audit[*i];
            let messages = tasks.get(a.task_id.as_str()).map(|t| t.evidence.all().into_iter().filter_map(|id|s.messages.get(id)).filter(|m|!m.withdrawn).rev().take(1).map(|m|json!({"id":m.id,"sender":m.sender,"mine":m.mine,"text":m.text.chars().take(400).collect::<String>()})).collect::<Vec<_>>()).unwrap_or_default();
            json!({"audit_index":i,"event":crate::context_budget::audit(a),"messages":messages})
        }).collect::<Vec<_>>();
        let mut payload = json!({"history":history["items"],"omitted_history":history["omitted"],"existing_suggestions":crate::context_budget::suggestions(s,&json!(audit).to_string()),"current_prompt":prompts::content(&s.config,"classify"),"current_tasks":crate::context_budget::rows(s.tasks.iter().filter(|t|relevant.contains(t.id.as_str())).map(crate::context_budget::task),3000)});
        let mut context = self.capture_prepare_context_from(s, "learning", false, json!([]));
        let mut prepare = |count| {
            process::check_running()?;
            payload["audit"] = json!(&audit[..count]);
            self.prepare_ai_candidate_with("learning", &payload, mode, &mut context)
        };
        let all = prepare(eligible.len())?;
        let (count, mut request) = if all.prepared.total <= all.config.max_input_chars {
            (eligible.len(), all)
        } else {
            crate::context_budget::fit_prefix(eligible.len() - 1, |count| {
                let request = prepare(count)?;
                Ok((request.prepared.total <= request.config.max_input_chars).then_some(request))
            })?
            .ok_or_else(|| {
                format!(
                    "{}：单条纠错记录及必需上下文超过单次输入上限，未调用 AI",
                    ai::INPUT_LIMIT_ERROR
                )
            })?
        };
        request.payload["_batch_metrics"] = json!({"selected":count,"initial_selected":eligible.len(),"total_input_chars":request.prepared.total,"max_input_chars":request.config.max_input_chars});
        request.materialize_records();
        Ok((count, request, history_ids))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_uses_the_actual_request_budget_instead_of_six_record_batches() {
        let (_root, app) = crate::test_support::scoped_app("shishi-learning-budget");
        let mut state = app.state.lock().unwrap().clone();
        state.config.model_profiles = None;
        state.config.max_input_chars = 200_000;
        state.learning_cursor = 20;
        for i in 0..50 {
            state.audit.push(Audit {
                time: 1000 + i,
                kind: "manual_edit".into(),
                task_id: "synthetic-task".into(),
                detail: format!("Synthetic correction {i}"),
                change_summary: String::new(),
                before: json!({}),
                after: json!({"title":format!("Synthetic title {i}")}),
            });
        }
        let eligible = (20..50).collect::<Vec<_>>();
        let (count, all, history) = app
            .learning_request(&state, &eligible, AiMode::Manual)
            .unwrap();
        assert_eq!(count, 30);
        assert_eq!(all.payload["audit"].as_array().unwrap().len(), 30);
        assert!(history.len() > 3 && history.iter().all(|i| *i < 20));
        let (_, ten, _) = app
            .learning_request(&state, &eligible[..10], AiMode::Manual)
            .unwrap();
        state.config.max_input_chars = ten.prepared.total;
        let (count, partial, same_history) = app
            .learning_request(&state, &eligible, AiMode::Manual)
            .unwrap();
        assert_eq!(count, 10);
        assert_eq!(same_history, history);
        assert!(partial.prepared.total <= state.config.max_input_chars);
        assert_eq!(partial.payload["audit"].as_array().unwrap().len(), 10);
        assert_eq!(partial.payload["audit"][9]["audit_index"], 29);
        assert_eq!(partial.payload["_batch_metrics"]["initial_selected"], 30);
        state.config.max_input_chars = 1;
        let error = app
            .learning_request(&state, &eligible, AiMode::Manual)
            .err()
            .unwrap();
        assert!(error.starts_with(ai::INPUT_LIMIT_ERROR));
        assert_eq!(state.learning_cursor, 20);
        assert_eq!(app.state.lock().unwrap().calls, 0);
    }
}
