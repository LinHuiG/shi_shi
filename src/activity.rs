//! Read-only, filtered daily log pagination, independent of the live status ring.
use crate::{engine::App, model::date};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

fn runtime(kind: &str) -> bool {
    kind.starts_with("handling_")
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
                    | "request_error"
                    | "ai_failed"
                    | "ai_rejected"
                    | "generation_failed"
                    | "archive_failed"
            ) || (kind.starts_with("handling_")
                && (row["detail"]["error"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    || matches!(
                        row["detail"]["status"].as_str(),
                        Some("failed" | "uncertain" | "disconnected")
                    )))
        }
        _ => {
            kind.starts_with("handling_")
                || matches!(
                    kind,
                    "advice_auto_decision"
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
impl App {
    pub fn activity_query(&self, p: &Value) -> Result<Value, String> {
        let day = p["day"].as_str().map(str::to_owned).unwrap_or_else(date);
        let parsed =
            chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").map_err(|_| "日志日期无效")?;
        if parsed.to_string() != day {
            return Err("日志日期无效".into());
        }
        let start = parsed
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
            - 8 * 3_600_000;
        let end = start + 86_400_000;
        // Accept the retired UI token while new clients consistently use changes.
        let filter = match p["filter"].as_str().unwrap_or("ai") {
            "tasks" => "changes",
            value => value,
        };
        if !["ai", "all", "errors", "changes"].contains(&filter) {
            return Err("日志筛选无效".into());
        }
        let purpose = p["purpose"].as_str().unwrap_or("all");
        if !["all", "analysis", "message_filter", "report", "archive"].contains(&purpose) {
            return Err("AI 用途筛选无效".into());
        }
        let page = p["page"].as_u64().unwrap_or(1).clamp(1, 1000) as usize;
        let size = p["page_size"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
        let mut paths = Vec::new();
        match fs::read_dir(self.root.join("data/logs")) {
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
        let mut finished = HashSet::new();
        for path in &paths {
            visit(path, |r| {
                if matches!(
                    r["kind"].as_str(),
                    Some("ai_success" | "ai_failed" | "ai_cancelled")
                ) {
                    if let Some(id) = r["detail"]["request_id"].as_str() {
                        finished.insert(id.to_string());
                    }
                }
            })?;
        }
        let mut rows = BTreeMap::new();
        let mut total = 0usize;
        let mut sequence = 0usize;
        let mut insert = |r: Value| {
            sequence += 1;
            let time = r["time"].as_i64().unwrap_or(0);
            if time < start
                || time >= end
                || !matches_filter(&r, filter)
                || (filter == "ai" && !matches_purpose(&r, purpose))
            {
                return;
            }
            if r["kind"] == "ai_start"
                && r["detail"]["request_id"]
                    .as_str()
                    .is_some_and(|id| finished.contains(id))
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
        for path in &paths {
            invalid += visit(path, |r| {
                if runtime(r["kind"].as_str().unwrap_or("")) {
                    insert(r);
                }
            })?;
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
                insert(value);
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
        Ok(json!({"rows":rows,"page":page,"pages":pages,"total":total,"invalid_lines":invalid}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_commit_and_failure_are_visible_with_their_final_status() {
        let root = std::env::temp_dir().join(format!("shishi-archive-log-{}", crate::model::id()));
        let app = App::load(root.clone()).unwrap();
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
        drop(app);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn purpose_filtering_precedes_counts_pagination_and_start_finish_merging() {
        let root = std::env::temp_dir().join(format!("shishi-purpose-{}", crate::model::id()));
        let app = App::load(root.clone()).unwrap();
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
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
    use crate::model::{id, now};
    #[test]
    fn disk_query_keeps_early_ai_after_scan_noise_and_reads_rotations() {
        let root = std::env::temp_dir().join(format!("shishi-log-query-{}", id()));
        let app = App::load(root.clone()).unwrap();
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
        fs::remove_dir_all(root).unwrap();
    }
}
