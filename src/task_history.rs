//! Stable human-facing numbers and scoped queries over retained task audits.
use crate::{engine::App, model::State};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn numbers(s: &State) -> BTreeMap<String, u64> {
    let mut result = s.task_numbers.clone();
    let mut pending = s
        .tasks
        .iter()
        .map(|t| (t.created, t.id.clone()))
        .chain(
            s.audit
                .iter()
                .filter(|a| !a.task_id.is_empty())
                .map(|a| (a.time, a.task_id.clone())),
        )
        .collect::<Vec<_>>();
    pending.sort();
    let mut next = result.values().copied().max().unwrap_or(0);
    for (_, id) in pending {
        if !result.contains_key(&id) && !id.is_empty() {
            next += 1;
            result.insert(id, next);
        }
    }
    result
}
pub fn reserve(s: &mut State, id: &str) {
    if !id.is_empty() && !s.task_numbers.contains_key(id) {
        let next = s.task_numbers.values().copied().max().unwrap_or(0) + 1;
        s.task_numbers.insert(id.into(), next);
    }
}
pub fn label(kind: &str) -> &str {
    match kind {
        "manual_edit" => "手动编辑事项",
        "manual_tools" => "修改事项工具名单",
        "manual_ai_edit" => "AI 辅助编辑事项",
        "manual_status" => "手动修改状态",
        "manual_create" | "manual_new" => "手动创建事项",
        "ai_create" => "AI 新增事项",
        "ai_update" => "AI 更新事项",
        "merge" => "合并事项",
        "undo" => "撤销操作",
        "archive" | "archive_task" => "归档事项",
        "deadline_update" => "更新截止时间",
        "schedule_generate" => "定时生成事项",
        "ai_start" => "AI 分析开始",
        "ai_success" => "AI 分析完成",
        "ai_failed" => "AI 分析失败",
        "ai_cancelled" => "AI 分析已中断",
        "scan" => "消息读取完成",
        "request_error" | "error" => "操作未完成 异常",
        "analysis_result" => "事项已更新",
        _ => kind,
    }
}
pub fn terms(p: &Value) -> Result<Vec<String>, String> {
    let query = p.get("query").and_then(Value::as_str).unwrap_or("");
    if query.chars().count() > 200 {
        return Err("搜索内容不能超过 200 字".into());
    }
    Ok(query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}
pub fn matches(row: &Value, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let kind = row["kind"].as_str().unwrap_or("");
    let mut caption = label(kind).to_string();
    if kind.starts_with("ai_") {
        let purpose = match row["detail"]["kind"].as_str().unwrap_or("") {
            "message_filter" => "消息过滤",
            "report" => "报表生成",
            "archive" => "归档",
            _ => "AI 分析",
        };
        let status = match kind {
            "ai_start" => "开始",
            "ai_success" => "完成",
            "ai_failed" => "失败",
            "ai_cancelled" => "已中断",
            "ai_rejected" => "输入超限",
            _ => kind,
        };
        caption = format!("{purpose}{status}");
    }
    for (key, name) in [
        ("title", "标题"),
        ("description", "描述"),
        ("status", "状态"),
        ("priority", "优先级"),
        ("tags", "标签"),
        ("blocker", "阻塞"),
        ("waiting_for", "等待对象"),
        ("due", "截止时间"),
        ("archive_note", "归档备注"),
    ] {
        if row["before"][key] != row["after"][key] {
            caption.push_str(name);
        }
    }
    // Search visible content and identifiers, rather than unrelated timestamps,
    // pagination indices or internal request bookkeeping.
    let fields = [
        "title",
        "description",
        "status",
        "priority",
        "tags",
        "blocker",
        "waiting_for",
        "due",
        "archive_note",
        "merged_into",
    ];
    let visible_task = |v: &Value| {
        fields
            .iter()
            .filter_map(|key| v.get(*key).map(|value| ((*key).to_owned(), value.clone())))
            .collect::<serde_json::Map<_, _>>()
    };
    let content = json!({"task_id":row["task_id"],"task_number":row["task_number"],"task_title":row["task_title"],"detail":row["detail"],"before":visible_task(&row["before"]),"after":visible_task(&row["after"])});
    let text = format!("{content} {caption}").to_lowercase();
    terms.iter().all(|term| {
        if term.starts_with('#') && term.len() > 1 {
            row["task_label"]
                .as_str()
                .is_some_and(|label| label.to_lowercase().contains(term))
                || !term[1..].bytes().all(|b| b.is_ascii_digit())
                    && row["task_id"]
                        .as_str()
                        .is_some_and(|id| id.to_lowercase().contains(&term[1..]))
        } else {
            text.contains(term)
        }
    })
}
impl App {
    pub fn evidence_query(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("缺少事项编号")?;
        let terms = terms(p)?;
        let (since, until) = record_range(p)?;
        let ascending = record_ascending(p)?;
        if !self.state.lock().unwrap().tasks.iter().any(|t| t.id == id) {
            return Err("事项不存在".into());
        }
        let mut rows = self.evidence(id).as_array().cloned().unwrap_or_default();
        let all = rows.len();
        rows.retain(|m| {
            let time = m["time"].as_i64().unwrap_or(0);
            let text = ["text", "sender", "group", "chat", "source_name"]
                .iter()
                .filter_map(|k| m[*k].as_str())
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            time >= since && time <= until && terms.iter().all(|t| text.contains(t))
        });
        rows.sort_by(|a, b| {
            let order = a["time"]
                .as_i64()
                .cmp(&b["time"].as_i64())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()));
            if ascending {
                order
            } else {
                order.reverse()
            }
        });
        let total = rows.len();
        let pages = total.div_ceil(50).max(1);
        let page = (p["page"].as_u64().unwrap_or(1) as usize).clamp(1, pages);
        let rows: Vec<_> = rows.into_iter().skip((page - 1) * 50).take(50).collect();
        Ok(json!({"rows":rows,"all":all,"total":total,"page":page,"pages":pages}))
    }
    pub fn task_changes(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("缺少事项编号")?;
        let terms = terms(p)?;
        let ascending = record_ascending(p)?;
        let range = record_range(p)?;
        let s = self.state.lock().unwrap();
        if !s.tasks.iter().any(|t| t.id == id) {
            return Err("事项不存在".into());
        }
        let mut store = self.store.lock().unwrap();
        store
            .as_mut()
            .ok_or("业务数据库尚未初始化")?
            .query_task_history(
                id,
                terms,
                ascending,
                range,
                p["page"].as_u64().unwrap_or(1).max(1) as usize,
            )
    }
}

fn record_ascending(p: &Value) -> Result<bool, String> {
    let sort = match p.get("sort") {
        None | Some(Value::Null) => "desc",
        Some(value) => value.as_str().ok_or("时间排序无效")?,
    };
    match sort {
        "asc" => Ok(true),
        "desc" => Ok(false),
        _ => Err("时间排序无效".into()),
    }
}
fn record_range(p: &Value) -> Result<(i64, i64), String> {
    let endpoint = |key: &str, default: i64| match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or("时间区间无效".to_string()),
    };
    let since = endpoint("since", 0)?;
    let until = endpoint("until", i64::MAX)?;
    if since > until {
        return Err("开始时间不能晚于结束时间".into());
    }
    Ok((since, until))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Audit, Task};
    use std::collections::BTreeSet;
    #[test]
    fn numbers_survive_reordering_deletion_and_new_tasks() {
        let mut s = State::default();
        *s.tasks = vec![
            Task {
                id: "b".into(),
                created: 20,
                ..Default::default()
            },
            Task {
                id: "a".into(),
                created: 10,
                ..Default::default()
            },
        ];
        s.task_numbers = numbers(&s);
        assert_eq!(s.task_numbers["a"], 1);
        assert_eq!(s.task_numbers["b"], 2);
        s.tasks.remove(1);
        s.tasks.reverse();
        reserve(&mut s, "c");
        assert_eq!(numbers(&s)["c"], 3);
        assert_eq!(numbers(&s)["a"], 1);
        assert_eq!(s.task_numbers.values().collect::<BTreeSet<_>>().len(), 3);
    }
    #[test]
    fn saved_numbers_survive_restart_and_ui_task_edits() {
        let root = std::env::temp_dir().join(format!("shishi-task-number-{}", crate::model::id()));
        let app = App::load(root.clone()).unwrap();
        let task = app
            .task_edit(
                &json!({"task":{"title":"Synthetic first","description":"body","status":"待处理"}}),
            )
            .unwrap();
        let id = task["id"].as_str().unwrap().to_string();
        let mut displayed = app.snapshot()["tasks"][0].clone();
        assert_eq!(displayed["number"], 1);
        displayed["title"] = json!("Synthetic edited");
        app.task_edit(&json!({"task":displayed})).unwrap();
        let second=app.task_edit(&json!({"task":{"title":"Synthetic second","description":"body","status":"待处理"}})).unwrap();
        let second_id = second["id"].as_str().unwrap().to_string();
        drop(app);
        let app = App::load(root.clone()).unwrap();
        let state = app.state.lock().unwrap();
        assert_eq!(state.task_numbers[&id], 1);
        assert_eq!(state.task_numbers[&second_id], 2);
        drop(state);
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn scoped_search_sorts_and_pages_retained_records() {
        let root = std::env::temp_dir().join(format!("shishi-history-{}", crate::model::id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.tasks.push(Task {
                id: "a".into(),
                ..Default::default()
            });
            for i in 0..125 {
                s.audit.push(Audit {
                    time: i,
                    task_id: if i % 2 == 0 { "a" } else { "b" }.into(),
                    kind: "manual_edit".into(),
                    detail: format!("synthetic {i}"),
                    ..Default::default()
                });
            }
        }
        app.persist().unwrap();
        let rows = app
            .task_changes(&json!({"id":"a","query":"SYNTHETIC","sort":"asc","page":2}))
            .unwrap();
        assert_eq!(rows["total"], 63);
        assert_eq!(rows["rows"][0]["time"], 100);
        let rows = app
            .task_changes(&json!({"id":"a","query":"synthetic 124"}))
            .unwrap();
        assert_eq!(rows["total"], 1);
        assert_eq!(rows["rows"][0]["time"], 124);
        assert!(app.task_changes(&json!({"id":"missing"})).is_err());
        let range = app
            .task_changes(
                &json!({"id":"a","since":100,"until":104,"query":"synthetic","sort":"asc"}),
            )
            .unwrap();
        assert_eq!(range["total"], 3);
        assert_eq!(range["rows"][0]["time"], 100);
        assert_eq!(range["rows"][2]["time"], 104);
        assert_eq!(range["all"], 63);
        for invalid in [
            json!({"since":12,"until":11}),
            json!({"since":"bad"}),
            json!({"until":-1}),
        ] {
            let mut request = invalid;
            request["id"] = json!("a");
            assert!(app.task_changes(&request).is_err());
            assert!(app.evidence_query(&request).is_err());
        }
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn evidence_window_is_scoped_filtered_bounded_and_inclusive() {
        let root =
            std::env::temp_dir().join(format!("shishi-record-window-{}", crate::model::id()));
        let app = App::load(root.clone()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            let mut task = Task {
                id: "task".into(),
                ..Default::default()
            };
            for index in 0..120 {
                let id = format!("message-{index:03}");
                task.evidence.direct.push(id.clone());
                task.evidence.background.push(id.clone());
                s.messages.insert(
                    id.clone(),
                    crate::model::Message {
                        id,
                        text: "Synthetic needle".into(),
                        time: index,
                        withdrawn: index == 10,
                        ..Default::default()
                    },
                );
            }
            s.messages.insert(
                "other".into(),
                crate::model::Message {
                    id: "other".into(),
                    text: "Synthetic needle".into(),
                    time: 15,
                    ..Default::default()
                },
            );
            s.tasks.push(task);
        }
        let all = app
            .evidence_query(&json!({"id":"task","sort":"asc"}))
            .unwrap();
        assert_eq!(all["all"], 119);
        assert_eq!(all["rows"].as_array().unwrap().len(), 50);
        let rows = app
            .evidence_query(
                &json!({"id":"task","since":10,"until":12,"query":"SYNTHETIC needle","sort":"asc"}),
            )
            .unwrap();
        assert_eq!(rows["total"], 2);
        assert_eq!(rows["rows"][0]["time"], 11);
        assert_eq!(rows["rows"][1]["time"], 12);
        assert!(app.evidence_query(&json!({"id":"other"})).is_err());
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}
