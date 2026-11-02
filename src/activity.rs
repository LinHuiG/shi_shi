//! Read-only, filtered daily log pagination, independent of the live status ring.
use crate::{engine::App, model::date};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

fn runtime(kind: &str) -> bool {
    (kind.starts_with("handling_") || kind.starts_with("tool_call_"))
        || matches!(
            kind,
            "scan"
                | "ai_start"
                | "ai_success"
                | "ai_failed"
                | "ai_cancelled"
                | "ai_rejected"
                | "ai_resumed"
                | "error"
                | "request_error"
                | "analysis_result"
                | "analysis_rejected"
                | "archive_result"
                | "archive_failed"
                | "analysis_skipped"
                | "resume_pending"
                | "codex_visibility"
                | "generation_failed"
        )
}
fn matches_filter(row: &Value, filter: &str) -> bool {
    let kind = row["kind"].as_str().unwrap_or("");
    match filter {
        "all" => true,
        "changes" => row["audit"] == true,
        "errors" => {
            matches!(
                kind,
                "error"
                    | "analysis_rejected"
                    | "request_error"
                    | "ai_failed"
                    | "ai_rejected"
                    | "generation_failed"
                    | "archive_failed"
            ) || ((kind.starts_with("handling_") || kind.starts_with("tool_call_"))
                && (row["detail"]["error"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    || matches!(
                        row["detail"]["status"].as_str(),
                        Some(
                            "failed"
                                | "uncertain"
                                | "disconnected"
                                | "rejected"
                                | "nonzero_exit"
                                | "start_failed"
                                | "output_error"
                                | "timed_out"
                        )
                    )))
        }
        _ => {
            (kind.starts_with("handling_") || kind.starts_with("tool_call_"))
                || matches!(
                    kind,
                    "advice_auto_decision"
                        | "analysis_rejected"
                        | "codex_visibility"
                        | "ai_start"
                        | "ai_success"
                        | "ai_failed"
                        | "ai_cancelled"
                        | "ai_rejected"
                        | "ai_resumed"
                        | "archive_result"
                        | "archive_failed"
                        | "resume_pending"
                )
        }
    }
}
fn matches_purpose(row: &Value, purpose: &str) -> bool {
    let kind = row["detail"]["kind"].as_str().unwrap_or("");
    match purpose {
        "all" => true,
        "analysis" => {
            kind == "classify"
                || (kind.is_empty() && row["kind"].as_str().is_some_and(|s| s.starts_with("ai_")))
        }
        "tool_call" => {
            kind == "tool_call"
                || kind == "tool_test"
                || row["kind"]
                    .as_str()
                    .is_some_and(|k| k.starts_with("tool_call_"))
        }
        _ => kind == purpose,
    }
}
fn visit(path: &Path, mut row: impl FnMut(Value)) -> Result<usize, String> {
    let file = fs::File::open(path).map_err(|e| format!("日志读取失败：{e}"))?;
    let mut invalid = 0;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("日志读取失败：{e}"))?;
        match serde_json::from_str(&line) {
            Ok(value) => row(value),
            Err(_) => invalid += 1,
        }
    }
    Ok(invalid)
}

fn day_range(day: Option<&str>) -> Result<(String, i64, i64), String> {
    let day = day
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(date);
    let parsed = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").map_err(|_| "日志日期无效")?;
    if parsed.to_string() != day {
        return Err("日志日期无效".into());
    }
    let start = parsed
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
        - 28_800_000;
    Ok((day, start, start + 86_400_000))
}
fn day_paths(root: &Path, day: &str) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    match fs::read_dir(crate::storage::data_dir(root).join("logs")) {
        Ok(entries) => {
            for item in entries {
                let item = item.map_err(|e| e.to_string())?;
                let name = item.file_name().to_string_lossy().into_owned();
                let rotated = name
                    .strip_prefix(&format!("{day}-"))
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
                if (name == format!("{day}.jsonl") || rotated)
                    && item.file_type().map_err(|e| e.to_string())?.is_file()
                {
                    paths.push(item.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("日志目录读取失败：{e}")),
    }
    paths.sort();
    Ok(paths)
}
impl App {
    /// Only metadata is read here. The caller may reuse the audit token when
    /// the business revision is unchanged; runtime progress cannot edit audit.
    pub(crate) fn activity_day_revision(
        &self,
        day: Option<&str>,
        known_audit: Option<&Value>,
    ) -> Value {
        let Ok((day, start, end)) = day_range(day) else {
            return Value::Null;
        };
        let files = day_paths(&self.root, &day).map(|paths| {
            paths
                .iter()
                .map(|path| match fs::metadata(path) {
                    Ok(meta) => json!([
                        path.file_name().map(|n| n.to_string_lossy()),
                        meta.len(),
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos().to_string())
                    ]),
                    Err(e) => json!([path.file_name().map(|n| n.to_string_lossy()), e.to_string()]),
                })
                .collect::<Vec<_>>()
        });
        let audit = known_audit
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or_else(|| {
                let state = self.state.lock().unwrap();
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                // Committed audit rows are immutable. Retention changes indices;
                // new decisions append rows, including a late row for an older day.
                for (index, row) in state
                    .audit
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.time >= start && r.time < end)
                {
                    (
                        index,
                        row.time,
                        &row.kind,
                        &row.task_id,
                        &row.detail,
                        &row.change_summary,
                    )
                        .hash(&mut hash);
                }
                // Log labels and search use current task names/numbers.
                for task in &state.tasks {
                    (&task.id, &task.title, state.task_numbers.get(&task.id)).hash(&mut hash);
                }
                json!(hash.finish().to_string())
            });
        json!({"day":day,"files":files.unwrap_or_else(|error|vec![json!({"error":error})]),"audit":audit,
            "live":if day == date() { self.activity_revision.load(Ordering::SeqCst) } else { 0 }})
    }
    pub fn activity_query(&self, p: &Value) -> Result<Value, String> {
        self.log_writer.flush()?;
        let (day, start, end) = day_range(p["day"].as_str())?;
        let revision = self.activity_day_revision(Some(&day), None);
        // Accept the retired UI token while new clients consistently use changes.
        let filter = match p["filter"].as_str().unwrap_or("ai") {
            "tasks" => "changes",
            value => value,
        };
        if !["ai", "all", "errors", "changes"].contains(&filter) {
            return Err("日志筛选无效".into());
        }
        let purpose = p["purpose"].as_str().unwrap_or("all");
        if ![
            "all",
            "analysis",
            "message_filter",
            "report",
            "archive",
            "tool_call",
            "format",
            "edit",
            "tool_test",
        ]
        .contains(&purpose)
        {
            return Err("AI 用途筛选无效".into());
        }
        let page = p["page"].as_u64().unwrap_or(1).clamp(1, 1000) as usize;
        let terms = crate::task_history::terms(p)?;
        let (task_labels, task_numbers) = {
            let s = self.state.lock().unwrap();
            let numbers = &s.task_numbers;
            let labels = s
                .tasks
                .iter()
                .map(|t| (t.id.clone(), (t.title.clone(), numbers.get(&t.id).copied())))
                .collect::<BTreeMap<_, _>>();
            (labels, s.task_numbers.clone())
        };
        let size = p["page_size"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
        let paths = day_paths(&self.root, &day)?;
        let mut rows = BTreeMap::new();
        let mut total = 0usize;
        let mut insert = |mut r: Value, sequence: usize| {
            let task_id = r["task_id"]
                .as_str()
                .or_else(|| r["detail"]["task_id"].as_str())
                .unwrap_or("")
                .to_string();
            if !task_id.is_empty() {
                r["task_id"] = json!(task_id);
            }
            if let Some((title, number)) = task_labels.get(&task_id) {
                r["task_title"] = json!(title);
                r["task_number"] = json!(number);
                r["task_label"] = json!(format!("#{}", number.unwrap_or(0)));
            } else if let Some(number) = task_numbers.get(&task_id) {
                r["task_number"] = json!(number);
                r["task_label"] = json!(format!("#{number}"));
                r["task_title"] = r["after"]["title"]
                    .as_str()
                    .or_else(|| r["before"]["title"].as_str())
                    .map(|s| json!(s))
                    .unwrap_or(Value::Null);
            }
            let time = r["time"].as_i64().unwrap_or(0);
            if time < start
                || time >= end
                || !matches_filter(&r, filter)
                || (filter == "ai" && !matches_purpose(&r, purpose))
                || !crate::task_history::matches(&r, &terms)
            {
                return;
            }
            total += 1;
            rows.insert((time, sequence), r);
            if rows.len() > page * size {
                rows.pop_first();
            }
        };
        let mut invalid = 0;
        let mut sequence = 0usize;
        let mut finished = HashSet::new();
        let mut starts = Vec::new();
        for path in &paths {
            invalid += visit(path, |r| {
                sequence += 1;
                if matches!(
                    r["kind"].as_str(),
                    Some("ai_success" | "ai_failed" | "ai_cancelled")
                ) {
                    if let Some(id) = r["detail"]["request_id"].as_str() {
                        finished.insert(id.to_owned());
                    }
                }
                // Only starts need to wait for later outcomes. Every line is
                // parsed once and ordinary rows retain bounded pagination.
                if r["kind"] == "ai_start" && r["detail"]["request_id"].is_string() {
                    starts.push((r, sequence));
                    return;
                }
                if runtime(r["kind"].as_str().unwrap_or("")) {
                    insert(r, sequence);
                }
            })?;
        }
        for (row, sequence) in starts {
            if !finished.contains(row["detail"]["request_id"].as_str().unwrap_or("")) {
                insert(row, sequence);
            }
        }
        {
            let state = self.state.lock().unwrap();
            for (index, audit) in state.audit.iter().enumerate() {
                if audit.time < start || audit.time >= end {
                    continue;
                }
                let mut value = json!(audit);
                value["audit"] = json!(true);
                value["index"] = json!(index);
                sequence += 1;
                insert(value, sequence);
            }
        }
        let pages = total.div_ceil(size).max(1);
        let page = page.min(pages);
        let rows = rows
            .into_values()
            .rev()
            .skip((page - 1) * size)
            .take(size)
            .collect::<Vec<_>>();
        Ok(
            json!({"day":day,"rows":rows,"page":page,"pages":pages,"total":total,"invalid_lines":invalid,
                "revision":revision}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_day_revision_ignores_today_but_detects_rotations_and_late_audit() {
        let (root, app) = crate::test_support::scoped_app("shishi-log-revision");
        let day = "2026-01-02";
        let (_, start, _) = day_range(Some(day)).unwrap();
        let before = app.activity_day_revision(Some(day), None);
        app.log("ai_start", json!({"request_id":"today"}));
        app.log_writer.flush().unwrap();
        assert_eq!(before, app.activity_day_revision(Some(day), None));
        let path = root.join(format!("data/logs/{day}-1.jsonl"));
        let row = json!({"time":start+100,"kind":"ai_start","detail":{"request_id":"historical"}});
        fs::write(&path, format!("{row}\n")).unwrap();
        let rotation = app.activity_day_revision(Some(day), None);
        assert_ne!(before, rotation);
        app.state.lock().unwrap().audit.push(crate::model::Audit {
            time: start + 200,
            kind: "manual_edit".into(),
            detail: "late historical audit".into(),
            ..Default::default()
        });
        let audit = app.activity_day_revision(Some(day), None);
        assert_ne!(rotation["audit"], audit["audit"]);
        fs::remove_file(path).unwrap();
        assert_ne!(audit, app.activity_day_revision(Some(day), None));
    }
    #[test]
    fn single_pass_keeps_tie_order_and_merges_outcomes_across_rotations() {
        let (root, app) = crate::test_support::scoped_app("shishi-log-single-pass");
        let at = crate::model::now();
        let records = [
            json!({"time":at,"kind":"ai_start","detail":{"request_id":"finished","text":"search token"}}),
            json!({"time":at,"kind":"ai_start","detail":{"request_id":"pending","text":"search token"}}),
            json!({"time":at,"kind":"error","detail":{"text":"search token"}}),
        ];
        fs::write(
            root.join(format!("data/logs/{}-1.jsonl", date())),
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        fs::write(root.join(format!("data/logs/{}.jsonl",date())), json!({"time":at,"kind":"ai_success","detail":{"request_id":"finished","text":"search token"}}).to_string()).unwrap();
        let rows = app
            .activity_query(&json!({"filter":"all","query":"search token","page_size":1,"page":2}))
            .unwrap();
        assert_eq!(rows["total"], 3);
        assert_eq!(rows["rows"][0]["kind"], "error");
        let last = app
            .activity_query(&json!({"filter":"all","query":"search token","page_size":1,"page":3}))
            .unwrap();
        assert_eq!(last["rows"][0]["detail"]["request_id"], "pending");
    }
    #[test]
    fn fuzzy_search_combines_scope_and_matches_task_number_title_and_body_before_paging() {
        let (_root, app) = crate::test_support::scoped_app("shishi-log-search");
        {
            let mut s = app.state.lock().unwrap();
            s.tasks.push(crate::test_support::task(
                "synthetic-task",
                "合成事项 Alpha",
            ));
            s.task_numbers.insert("synthetic-task".into(), 42);
        }
        for i in 0..65 {
            app.log("handling_status",json!({"task_id":"synthetic-task","status":"completed","text":format!("Synthetic detail {i}")}));
        }
        app.log("error", json!({"error":"other row"}));
        let rows = app
            .activity_query(&json!({"filter":"all","query":"#42 ALPHA","page":2}))
            .unwrap();
        assert_eq!(rows["total"], 65);
        assert_eq!(rows["rows"].as_array().unwrap().len(), 15);
        assert_eq!(rows["rows"][0]["task_number"], 42);
        assert_eq!(
            app.activity_query(&json!({"filter":"all","query":"synthetic detail 64"}))
                .unwrap()["total"],
            1
        );
        assert_eq!(
            app.activity_query(&json!({"filter":"errors","query":"Alpha"}))
                .unwrap()["total"],
            0
        );
        assert_eq!(
            app.activity_query(&json!({"filter":"all","query":"#999"}))
                .unwrap()["total"],
            0
        );
        assert!(app
            .activity_query(&json!({"query":"a".repeat(201)}))
            .is_err());
    }
    #[test]
    fn blank_dates_resolve_to_shanghai_today_without_changing_explicit_dates() {
        let (_root, app) = crate::test_support::scoped_app("shishi-log-date");
        for params in [
            json!({}),
            json!({"day":null}),
            json!({"day":""}),
            json!({"day":"  "}),
        ] {
            assert_eq!(app.activity_query(&params).unwrap()["day"], date());
        }
        assert_eq!(
            app.activity_query(&json!({"day":"2026-10-01"})).unwrap()["day"],
            "2026-10-01"
        );
        for day in ["2026-99-01", "2026/10/01", "../state"] {
            assert!(app.activity_query(&json!({"day":day})).is_err());
        }
    }
    #[test]
    fn archive_commit_and_failure_are_visible_with_their_final_status() {
        let (_root, app) = crate::test_support::scoped_app("shishi-archive-log");
        app.log(
            "ai_success",
            json!({"kind":"archive","request_id":"synthetic"}),
        );
        app.log(
            "archive_failed",
            json!({"kind":"archive","error":"synthetic invalid source"}),
        );
        app.log(
            "archive_result",
            json!({"kind":"archive","processed":1,"remaining":0}),
        );
        let rows = app
            .activity_query(&json!({"filter":"ai","purpose":"archive"}))
            .unwrap();
        assert_eq!(rows["total"], 3);
        let errors = app.activity_query(&json!({"filter":"errors"})).unwrap();
        assert_eq!(errors["total"], 1);
        assert_eq!(errors["rows"][0]["kind"], "archive_failed");
    }
    #[test]
    fn purpose_filtering_precedes_counts_pagination_and_start_finish_merging() {
        let (root, app) = crate::test_support::scoped_app("shishi-purpose");
        let at = crate::model::now();
        let mut rows = Vec::new();
        for (purpose, count) in [
            ("classify", 2),
            ("message_filter", 60),
            ("report", 2),
            ("archive", 2),
        ] {
            for i in 0..count {
                rows.push(json!({"time":at-1000+rows.len() as i64,"kind":if i==count-1 {"ai_failed"} else {"ai_success"},"detail":{"kind":purpose,"request_id":format!("{purpose}-{i}")}}));
            }
        }
        rows.push(json!({"time":at-2000,"kind":"ai_start","detail":{"kind":"message_filter","request_id":"message_filter-0"}}));
        let path = root.join(format!("data/logs/{}.jsonl", date()));
        let text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, &text).unwrap();
        assert_eq!(
            app.activity_query(&json!({"filter":"ai"})).unwrap()["total"],
            66
        );
        for (purpose, count) in [
            ("analysis", 2),
            ("message_filter", 60),
            ("report", 2),
            ("archive", 2),
        ] {
            let result = app
                .activity_query(&json!({"filter":"ai","purpose":purpose,"page_size":50}))
                .unwrap();
            assert_eq!(result["total"], count, "{purpose}");
            assert!(result["rows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| matches_purpose(r, purpose)));
        }
        let second = app
            .activity_query(
                &json!({"filter":"ai","purpose":"message_filter","page_size":50,"page":2}),
            )
            .unwrap();
        assert_eq!(second["rows"].as_array().unwrap().len(), 10);
        assert_eq!(second["pages"], 2);
        assert_eq!(
            app.activity_query(&json!({"filter":"errors","purpose":"all"}))
                .unwrap()["total"],
            4
        );
        assert!(app.activity_query(&json!({"purpose":"unknown"})).is_err());
        app.state.lock().unwrap().audit.push(crate::model::Audit {
            time: at,
            kind: "manual_edit".into(),
            task_id: "synthetic-task".into(),
            detail: "合成事项变更".into(),
            change_summary: String::new(),
            before: json!({"title":"原值"}),
            after: json!({"title":"新值"}),
        });
        let changes = app.activity_query(&json!({"filter":"changes"})).unwrap();
        assert_eq!(changes["total"], 1);
        assert_eq!(
            app.activity_query(&json!({"filter":"tasks"})).unwrap(),
            changes
        );
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
    use crate::model::now;
    #[test]
    fn disk_query_keeps_early_ai_after_scan_noise_and_reads_rotations() {
        let (root, app) = crate::test_support::scoped_app("shishi-log-query");
        let at = (now() + 28_800_000).div_euclid(86_400_000) * 86_400_000 - 28_800_000 + 43_200_000;
        let early = json!({"time":at-10000,"kind":"ai_success","detail":{"request_id":"early"}});
        fs::write(
            root.join(format!("data/logs/{}-1.jsonl", date())),
            format!("{early}\ninvalid fixture\n"),
        )
        .unwrap();
        let mut lines = (0..100)
            .map(|i| json!({"time":at-1000+i,"kind":"scan","detail":{}}).to_string())
            .collect::<Vec<_>>();
        lines.push(
            json!({"time":at-11000,"kind":"ai_start","detail":{"request_id":"early"}}).to_string(),
        );
        lines.push(
            json!({"time":at,"kind":"ai_start","detail":{"request_id":"pending"}}).to_string(),
        );
        fs::write(
            root.join(format!("data/logs/{}.jsonl", date())),
            lines.join("\n"),
        )
        .unwrap();
        let first = app
            .activity_query(&json!({"filter":"ai","page_size":1}))
            .unwrap();
        assert_eq!(first["total"], 2);
        assert_eq!(first["pages"], 2);
        assert_eq!(first["rows"][0]["detail"]["request_id"], "pending");
        let second = app
            .activity_query(&json!({"filter":"ai","page_size":1,"page":2}))
            .unwrap();
        assert_eq!(second["rows"][0]["detail"]["request_id"], "early");
        assert_eq!(second["invalid_lines"], 1);
        assert_eq!(
            app.activity_query(&json!({"filter":"all"})).unwrap()["total"],
            102
        );
        assert_eq!(
            app.activity_query(&json!({"filter":"errors"})).unwrap()["total"],
            0
        );
        assert!(app.activity_query(&json!({"day":"../state"})).is_err());
        assert!(app.activity_query(&json!({"filter":"unknown"})).is_err());
        assert_eq!(
            fs::read_to_string(root.join(format!("data/logs/{}.jsonl", date()))).unwrap(),
            lines.join("\n")
        );
    }
}
