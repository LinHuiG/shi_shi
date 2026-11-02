//! Fit one frozen analysis projection to the exact outgoing request budget.
use super::*;
use crate::context_budget::Batch;
use std::time::Instant;

#[derive(Debug)]
pub(super) enum FitFailure {
    Stop(String),
    MessageTooLarge(String),
}
impl From<String> for FitFailure {
    fn from(error: String) -> Self {
        Self::Stop(error)
    }
}

/// Keep room ids, task order and manual history stable across probes. Adding
/// a message now only adds a row (and possibly columns/rooms), so its encoded
/// request size is monotone. Re-running relevance or reply selection for each
/// prefix would not have that property.
fn prefix_payload(
    base: &Value,
    messages: usize,
    tasks: usize,
    context: bool,
    directory: usize,
) -> Value {
    // Large omitted summaries are never cloned for each binary-search probe.
    let all_changes = base["changes"].as_array().unwrap();
    let changes = &all_changes[..messages.min(all_changes.len())];
    let mut payload = Value::Object(
        base.as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                !matches!(key.as_str(), "changes" | "tasks" | "task_index")
                    && (context || !matches!(key.as_str(), "context" | "manual_history"))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    payload["changes"] = Value::Array(changes.to_vec());
    if (!context || changes.len() < all_changes.len())
        && changes
            .iter()
            .any(|row| row["reply"].as_str().is_some_and(|reply| !reply.is_empty()))
    {
        payload["reply_context_incomplete"] = json!(true);
    }
    let rooms = changes
        .iter()
        .chain(
            base["context"]
                .as_array()
                .filter(|_| context)
                .into_iter()
                .flatten(),
        )
        .filter_map(|row| row["room"].as_str())
        .collect::<BTreeSet<_>>();
    if let Some(rows) = payload["rooms"].as_array_mut() {
        rows.retain(|row| row["id"].as_str().is_some_and(|id| rooms.contains(id)));
    }
    let rows = base["tasks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let existing_index = base["task_index"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !rows.is_empty() || !existing_index.is_empty() {
        payload["tasks"] = Value::Array(rows[..tasks.min(rows.len())].to_vec());
        let omitted = rows.len().saturating_sub(tasks) + existing_index.len();
        let index = rows.iter().skip(tasks).map(|task| {
            json!({"id":task["id"],"title":crate::context_budget::clip(task["title"].as_str().unwrap_or(""),80)})
        }).chain(existing_index.iter().cloned())
            .take(directory.saturating_sub(tasks)).collect::<Vec<_>>();
        let unlisted = omitted.saturating_sub(index.len());
        payload["retrieval"]["omitted_tasks"] = json!(omitted);
        payload["retrieval"]["unlisted_tasks"] = json!(unlisted);
        payload["retrieval"]["task_directory_incomplete"] = json!(unlisted > 0);
        payload["task_index"] = Value::Array(index);
    }
    payload
}

impl App {
    pub(super) fn fit_analysis_batch(
        &self,
        mut batch: Batch,
        mode: AiMode,
        context: &mut PrepareContext,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(Batch, PreparedAiRequest), FitFailure> {
        let check = || -> Result<(), String> {
            process::check_running()?;
            if cancel.load(Ordering::SeqCst) {
                return Err(crate::ai_control::CANCELLED.into());
            }
            if Instant::now() >= deadline {
                return Err("分析总时限已到，本批未调用 AI".into());
            }
            Ok(())
        };
        check()?;
        let count = batch.payload["changes"].as_array().unwrap().len();
        let task_count = batch.payload["tasks"].as_array().map_or(0, Vec::len);
        let directory_count =
            task_count + batch.payload["task_index"].as_array().map_or(0, Vec::len);
        let has_context = batch.payload["context"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty());
        let limit = context.config.max_input_chars;
        let full = self.prepare_ai_candidate_with("classify", &batch.payload, mode, context)?;
        // The first preparation captures complete local records, aliases and
        // runtime. Subsequent probes only clone the public projection.
        if let Some(local) = batch
            .payload
            .get_mut("_local_context")
            .and_then(Value::as_object_mut)
        {
            local.insert("records".into(), json!([]));
        }
        let mut probes = 1usize;
        let (selected, summaries, directory, keep_context, mut request) = if full.prepared.total
            <= limit
        {
            (count, task_count, directory_count, true, full)
        } else {
            let mut prepare = |messages: usize, tasks: usize, quoted: bool, directory: usize| {
                check()?;
                probes += 1;
                self.prepare_ai_candidate_with(
                    "classify",
                    &prefix_payload(&batch.payload, messages, tasks, quoted, directory),
                    mode,
                    context,
                )
            };
            let without_quotes = if has_context {
                prepare(count, task_count, false, directory_count)?
            } else {
                full
            };
            if without_quotes.prepared.total <= limit {
                (count, task_count, directory_count, false, without_quotes)
            } else {
                // New messages have priority over optional task summaries.
                // Omitted summaries retain their searchable id/title index;
                // their complete versions stay available in the frozen tool.
                let minimal = if task_count > 0 {
                    prepare(count, 0, false, directory_count)?
                } else {
                    without_quotes
                };
                // A directory is useful context, never a prerequisite for a
                // fresh message. All task records remain in the frozen query
                // snapshot even when their titles cannot fit inline.
                let (minimal, mut directory) =
                    if minimal.prepared.total > limit && directory_count > 0 {
                        (prepare(count, 0, false, 0)?, 0)
                    } else {
                        (minimal, directory_count)
                    };
                let mut smallest_chars = minimal.prepared.total;
                let mut accepted = if minimal.prepared.total <= limit {
                    Some((count, minimal))
                } else {
                    None
                };
                if accepted.is_none() {
                    accepted =
                        crate::context_budget::fit_prefix(count.saturating_sub(1), |middle| {
                            let candidate = prepare(middle, 0, false, directory)?;
                            smallest_chars = smallest_chars.min(candidate.prepared.total);
                            Ok((candidate.prepared.total <= limit).then_some(candidate))
                        })?;
                }
                let Some((selected, mut accepted)) = accepted else {
                    check()?;
                    self.log("analysis_budget_paused", json!({
                        "selected":1,"initial_selected":count,"total_input_chars":smallest_chars,
                        "max_input_chars":limit,"reason":"minimum_request_exceeds_budget"
                    }));
                    let error = format!("{}：单条消息及必需上下文至少为 {} 字符，超过上限 {}；本次未调用 AI，消息仍待处理。请精简配置或提高上限。", ai::INPUT_LIMIT_ERROR, smallest_chars, limit);
                    return Err(if prepare(0, 0, false, 0)?.prepared.total <= limit {
                        FitFailure::MessageTooLarge(error)
                    } else {
                        FitFailure::Stop(error)
                    });
                };
                if directory < directory_count {
                    if let Some((count, request)) =
                        crate::context_budget::fit_prefix(directory_count, |middle| {
                            let candidate = prepare(selected, 0, false, middle)?;
                            Ok((candidate.prepared.total <= limit).then_some(candidate))
                        })?
                    {
                        directory = count;
                        accepted = request;
                    }
                }
                // Fill the remaining space with the highest-ranked summaries.
                // Each summary replaces a smaller id/title index row, keeping
                // this search monotone too. Manual history never changes set.
                let (summaries, mut accepted) =
                    crate::context_budget::fit_prefix(task_count.min(directory), |middle| {
                        let candidate = prepare(selected, middle, false, directory)?;
                        Ok((candidate.prepared.total <= limit).then_some(candidate))
                    })?
                    .unwrap_or((0, accepted));
                // If all messages fit after summary reduction, restore their
                // original quote excerpts when there is space. A shortened
                // prefix retrieves its own parents instead of including quotes
                // belonging to messages that were left for the next batch.
                let quoted = if has_context && selected == count {
                    let candidate = prepare(selected, summaries, true, directory)?;
                    if candidate.prepared.total <= limit {
                        accepted = candidate;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                (selected, summaries, directory, quoted, accepted)
            }
        };
        check()?;
        batch.payload =
            prefix_payload(&batch.payload, selected, summaries, keep_context, directory);
        let sent = batch.payload["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect::<BTreeSet<_>>();
        batch
            .fingerprints
            .retain(|id, _| sent.contains(id.as_str()));
        let mut visible = sent;
        visible.extend(
            batch.payload["context"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["id"].as_str()),
        );
        batch.allowed.retain(|id, _| visible.contains(id.as_str()));
        let mut reduced_by = Vec::new();
        if has_context && !keep_context {
            reduced_by.push("reply_context");
        }
        if summaries < task_count {
            reduced_by.push("task_summaries");
        }
        if directory < directory_count {
            reduced_by.push("task_directory");
        }
        if selected < count {
            reduced_by.push("input_budget");
        }
        let metrics = json!({
            "selected":selected,"initial_selected":count,"max_input_chars":limit,
            "total_input_chars":request.prepared.total,"task_limit":summaries,
            "omitted_tasks":directory_count-summaries,"unlisted_tasks":directory_count-directory,
            "reduced_by":reduced_by,"packing_probes":probes
        });
        request.payload["_batch_metrics"] = metrics.clone();
        batch.payload["_batch_metrics"] = metrics;
        Ok((batch, request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture(count: usize, tasks: usize, driver: &str) -> (App, State, Vec<Message>, PathBuf) {
        let (root, app) = crate::test_support::app("ss-fit-analysis");
        let mut state = app.state.lock().unwrap().clone();
        state.config.codex = root.join("must-not-launch.exe").to_string_lossy().into();
        state
            .config
            .ai_connections
            .push(crate::ai_backend::Connection {
                id: "purpose-source".into(),
                driver: driver.into(),
                ..Default::default()
            });
        state.config.model_profiles = Some(ModelProfiles {
            analysis: Some(ModelChoice {
                connection_id: "purpose-source".into(),
                model: "synthetic-model".into(),
                reasoning_effort: "medium".into(),
            }),
            ..Default::default()
        });
        let at = now() - 10_000;
        let messages = (0..count)
            .map(|i| Message {
                id: format!("original-{}-{i:04}", "identifier".repeat(60)),
                source_id: "source".into(),
                chat: format!("room-{}", i % 3),
                sender: "Synthetic sender".into(),
                text: format!("Synthetic {i} 合成🙂\\\n{}", "材料".repeat(30 + i % 7)),
                time: at + i as i64,
                updated_at: at + i as i64,
                private: true,
                mention_me: i % 11 == 0,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        for message in &messages {
            state.messages.insert(message.id.clone(), message.clone());
        }
        for i in 0..tasks {
            state.tasks.push(Task {
                id: format!("task-{i:03}"),
                title: format!("Synthetic task {i}"),
                description: "合成事项摘要 ".repeat(90),
                status: "待处理".into(),
                updated: at - i as i64,
                ..Default::default()
            });
        }
        (app, state, messages, root)
    }

    #[test]
    fn exact_wire_budget_accepts_all_short_messages_despite_long_original_ids() {
        for driver in ["codex", "dsh", "api"] {
            let (app, state, messages, root) = fixture(80, 0, driver);
            let mut context =
                app.capture_prepare_context_from(&state, "classify", false, json!([]));
            let batch = crate::context_budget::classify(&state, &messages);
            let full = app
                .prepare_ai_candidate_with("classify", &batch.payload, AiMode::Manual, &mut context)
                .unwrap();
            context.config.max_input_chars = full.prepared.total;
            assert_eq!(
                crate::ai_backend::connection(&context.config)
                    .unwrap()
                    .driver,
                driver
            );
            let candidates = crate::context_budget::analysis_candidates(
                messages.iter(),
                context.config.max_input_chars,
            );
            assert_eq!(candidates.len(), 80);
            let (accepted, mut request) = app
                .fit_analysis_batch(
                    batch,
                    AiMode::Manual,
                    &mut context,
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_secs(60),
                )
                .unwrap();
            assert_eq!(accepted.fingerprints.len(), 80);
            assert_eq!(request.prepared.prompt, full.prepared.prompt);
            assert_eq!(request.payload["_batch_metrics"]["packing_probes"], 1);
            request.materialize_records();
            assert_eq!(
                ai::prepare(&request.config, "classify", &request.payload).total,
                request.prepared.total
            );
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn overflow_selects_the_largest_measured_prefix_not_a_fixed_count() {
        for driver in ["codex", "api"] {
            let (app, state, messages, root) = fixture(64, 0, driver);
            let mut context =
                app.capture_prepare_context_from(&state, "classify", false, json!([]));
            let batch = crate::context_budget::classify(&state, &messages);
            let full = app
                .prepare_ai_candidate_with("classify", &batch.payload, AiMode::Manual, &mut context)
                .unwrap();
            // Exhaustive oracle over independently rebuilt task-free prefixes.
            // Runtime and aliases remain frozen exactly as in the real call.
            let sizes = (1..=messages.len())
                .map(|n| {
                    let candidate = crate::context_budget::classify(&state, &messages[..n]);
                    app.prepare_ai_candidate_with(
                        "classify",
                        &candidate.payload,
                        AiMode::Manual,
                        &mut context,
                    )
                    .unwrap()
                    .prepared
                    .total
                })
                .collect::<Vec<_>>();
            assert!(sizes.windows(2).all(|pair| pair[0] < pair[1]));
            context.config.max_input_chars = sizes[36];
            let (accepted, request) = app
                .fit_analysis_batch(
                    batch,
                    AiMode::Manual,
                    &mut context,
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_secs(60),
                )
                .unwrap();
            assert_eq!(accepted.fingerprints.len(), 37);
            assert_eq!(accepted.allowed.len(), 37);
            assert_eq!(request.prepared.total, sizes[36]);
            assert!(sizes[37] > request.config.max_input_chars);
            assert!(
                request.payload["_batch_metrics"]["packing_probes"]
                    .as_u64()
                    .unwrap()
                    < 12
            );
            assert!(Arc::ptr_eq(&request.records, &full.records));
            assert!(Arc::ptr_eq(&request.wire_ids, &full.wire_ids));
            assert!(!accepted.allowed.contains_key(&messages[37].id));
            drop(app);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn background_is_reduced_by_budget_without_forcing_ten_messages_or_summaries() {
        let (app, state, messages, root) = fixture(56, 40, "api");
        let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
        let batch = crate::context_budget::classify(&state, &messages);
        let full = app
            .prepare_ai_candidate_with("classify", &batch.payload, AiMode::Manual, &mut context)
            .unwrap();
        let mut base = batch.payload.clone();
        base["_local_context"]["records"] = json!([]);
        let sizes = (0..=40)
            .map(|n| {
                app.prepare_ai_candidate_with(
                    "classify",
                    &prefix_payload(&base, messages.len(), n, false, 40),
                    AiMode::Manual,
                    &mut context,
                )
                .unwrap()
                .prepared
                .total
            })
            .collect::<Vec<_>>();
        assert!(sizes.windows(2).all(|pair| pair[0] < pair[1]));
        context.config.max_input_chars = sizes[23];
        assert!(full.prepared.total > context.config.max_input_chars);
        let (accepted, request) = app
            .fit_analysis_batch(
                batch,
                AiMode::Manual,
                &mut context,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(accepted.fingerprints.len(), 56);
        assert_eq!(request.initial_tasks.len(), 23);
        assert_eq!(accepted.payload["task_index"].as_array().unwrap().len(), 17);
        let listed = accepted.payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .chain(accepted.payload["task_index"].as_array().unwrap())
            .map(|task| task["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            listed,
            accepted.versions.keys().map(String::as_str).collect()
        );
        assert_eq!(
            request.payload["_batch_metrics"]["reduced_by"],
            json!(["task_summaries"])
        );
        assert_eq!(request.prepared.total, request.config.max_input_chars);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_task_directory_is_bounded_without_removing_retrieval_records() {
        let (app, state, messages, root) = fixture(8, 2000, "api");
        let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
        let batch = crate::context_budget::classify(&state, &messages);
        let full = app
            .prepare_ai_candidate_with("classify", &batch.payload, AiMode::Manual, &mut context)
            .unwrap();
        let mut base = batch.payload.clone();
        base["_local_context"]["records"] = json!([]);
        context.config.max_input_chars = app
            .prepare_ai_candidate_with(
                "classify",
                &prefix_payload(&base, messages.len(), 0, false, 20),
                AiMode::Manual,
                &mut context,
            )
            .unwrap()
            .prepared
            .total;
        assert!(full.prepared.total > context.config.max_input_chars);
        let (accepted, mut request) = app
            .fit_analysis_batch(
                batch,
                AiMode::Manual,
                &mut context,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(accepted.fingerprints.len(), messages.len());
        assert_eq!(accepted.payload["task_index"].as_array().unwrap().len(), 20);
        assert_eq!(accepted.payload["retrieval"]["unlisted_tasks"], 1980);
        assert!(request.prepared.total <= request.config.max_input_chars);
        assert_eq!(
            request
                .records
                .iter()
                .filter(|row| row["kind"] == "task")
                .count(),
            2000
        );
        assert!(Arc::ptr_eq(&request.records, &full.records));
        request.materialize_records();
        let found: Value = serde_json::from_str(&crate::analysis_context::query(
            &request.payload["_local_context"],
            &mut crate::analysis_context::Meter::default(),
            &json!({"kind":"task","query":"Synthetic task 1999"}),
        ))
        .unwrap();
        assert_eq!(found["records"][0]["data"]["title"], "Synthetic task 1999");
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn trimmed_reply_parents_remain_retrievable_but_are_not_initial_evidence() {
        let (app, mut state, mut messages, root) = fixture(45, 0, "codex");
        // An edited older parent is later in update order than its child.
        messages[0].reply = messages[44].id.clone();
        state
            .messages
            .insert(messages[0].id.clone(), messages[0].clone());
        let mut context = app.capture_prepare_context_from(&state, "classify", false, json!([]));
        let batch = crate::context_budget::classify(&state, &messages);
        app.prepare_ai_candidate_with("classify", &batch.payload, AiMode::Manual, &mut context)
            .unwrap();
        let mut base = batch.payload.clone();
        base["_local_context"]["records"] = json!([]);
        context.config.max_input_chars = app
            .prepare_ai_candidate_with(
                "classify",
                &prefix_payload(&base, 32, 0, false, 0),
                AiMode::Manual,
                &mut context,
            )
            .unwrap()
            .prepared
            .total;
        let (accepted, request) = app
            .fit_analysis_batch(
                batch,
                AiMode::Manual,
                &mut context,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(accepted.fingerprints.len(), 32);
        assert_eq!(accepted.payload["reply_context_incomplete"], true);
        assert!(!accepted.allowed.contains_key(&messages[44].id));
        assert!(request
            .records
            .iter()
            .any(|row| row["id"] == messages[44].id));
        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn packing_stops_on_cancellation_or_deadline_before_preparing() {
        let (app, state, messages, root) = fixture(20, 0, "api");
        for cancelled in [true, false] {
            let batch = crate::context_budget::classify(&state, &messages);
            let mut context =
                app.capture_prepare_context_from(&state, "classify", false, json!([]));
            let error = app
                .fit_analysis_batch(
                    batch,
                    AiMode::Manual,
                    &mut context,
                    &AtomicBool::new(cancelled),
                    Instant::now(),
                )
                .err()
                .unwrap();
            let FitFailure::Stop(error) = error else {
                panic!("cancellation must not isolate a message");
            };
            assert!(if cancelled {
                error == crate::ai_control::CANCELLED
            } else {
                error.contains("总时限")
            });
            assert!(context.record_set.is_none());
        }
        assert_eq!(app.state.lock().unwrap().calls, 0);
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
}
