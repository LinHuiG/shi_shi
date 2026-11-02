//! Stable human-facing numbers and scoped queries over retained task audits.
use crate::{
    engine::App,
    model::{Message, State},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn numbers(s: &State) -> BTreeMap<String, u64> {
    let mut result = s.task_numbers.clone();
    let mut pending = s
        .tasks
        .iter()
        .map(|t| (t.created, &t.id))
        .chain(s.audit.iter().map(|a| (a.time, &a.task_id)))
        .filter(|(_, id)| !id.is_empty() && !s.task_numbers.contains_key(*id))
        .collect::<Vec<_>>();
    pending.sort();
    let mut next = result.values().copied().max().unwrap_or(0);
    for (_, id) in pending {
        if !result.contains_key(id) {
            next += 1;
            result.insert(id.clone(), next);
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
        "defer_resumed" => "延后到期恢复处理",
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
// Keep one scoped selection for the full IPC response and the paged reader.
// Insertion order preserves completion > background > direct for shared IDs.
fn evidence_rows<'a>(s: &'a State, id: &str) -> Option<Vec<(&'a Message, &'static str)>> {
    let task = s.tasks.iter().find(|t| t.id == id)?;
    let roles: BTreeMap<_, _> = [
        (&task.evidence.direct, "direct"),
        (&task.evidence.background, "background"),
        (&task.evidence.completion, "completion"),
    ]
    .into_iter()
    .flat_map(|(ids, role)| ids.iter().map(move |id| (id, role)))
    .collect();
    Some(
        roles
            .into_iter()
            .filter_map(|(id, role)| {
                s.messages
                    .get(id)
                    .filter(|m| !m.withdrawn)
                    .map(|m| (m, role))
            })
            .collect(),
    )
}
fn evidence_value((message, role): (&Message, &str)) -> Value {
    let mut value = json!(message);
    value["evidence_role"] = json!(role);
    value
}
impl App {
    pub fn evidence(&self, id: &str) -> Value {
        let s = self.state.lock().unwrap();
        Value::Array(
            evidence_rows(&s, id)
                .unwrap_or_default()
                .into_iter()
                .map(evidence_value)
                .collect(),
        )
    }
    pub fn evidence_query(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("缺少事项编号")?;
        let terms = terms(p)?;
        let (since, until) = record_range(p)?;
        let ascending = record_ascending(p)?;
        let s = self.state.lock().unwrap();
        let mut rows = evidence_rows(&s, id).ok_or("事项不存在")?;
        let all = rows.len();
        rows.retain(|(m, _)| {
            if m.time < since || m.time > until {
                return false;
            }
            if terms.is_empty() {
                return true;
            }
            let text = [&m.text, &m.sender, &m.group, &m.chat, &m.source_name]
                .map(String::as_str)
                .join(" ")
                .to_lowercase();
            terms.iter().all(|term| text.contains(term))
        });
        rows.sort_by(|a, b| {
            let order = a.0.time.cmp(&b.0.time).then_with(|| a.0.id.cmp(&b.0.id));
            if ascending {
                order
            } else {
                order.reverse()
            }
        });
        let total = rows.len();
        let pages = total.div_ceil(50).max(1);
        let page = (p["page"].as_u64().unwrap_or(1) as usize).clamp(1, pages);
        let rows: Vec<_> = rows
            .into_iter()
            .skip((page - 1) * 50)
            .take(50)
            .map(evidence_value)
            .collect();
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
        let (root, app) = crate::test_support::scoped_app("shishi-task-number");
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
    }
    #[test]
    fn scoped_search_sorts_and_pages_retained_records() {
        let (_root, app) = crate::test_support::scoped_app("shishi-history");
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
    }

    #[test]
    fn evidence_window_is_scoped_filtered_bounded_and_inclusive() {
        let (_root, app) = crate::test_support::scoped_app("shishi-record-window");
        {
            let mut s = app.state.lock().unwrap();
            let mut task = Task {
                id: "task".into(),
                ..Default::default()
            };
            for index in 0..120 {
                let id = format!("message-{index:03}");
                task.evidence.direct.push(id.clone());
                if index < 119 {
                    task.evidence.background.push(id.clone());
                }
                if index == 0 || index == 11 {
                    task.evidence.completion.push(id.clone());
                }
                s.messages.insert(
                    id.clone(),
                    crate::model::Message {
                        id,
                        text: "Synthetic needle ".repeat(1000),
                        time: index,
                        withdrawn: index == 10,
                        ..Default::default()
                    },
                );
            }
            s.messages.insert(
                "other".into(),
                crate::test_support::message("other", "Synthetic needle", 15),
            );
            s.tasks.push(task);
        }
        let all = app
            .evidence_query(&json!({"id":"task","sort":"asc"}))
            .unwrap();
        assert_eq!(all["all"], 119);
        assert_eq!(all["rows"].as_array().unwrap().len(), 50);
        assert_eq!(all["rows"][0]["evidence_role"], "completion");
        assert_eq!(all["rows"][1]["evidence_role"], "background");
        let last = app
            .evidence_query(&json!({"id":"task","sort":"asc","page":3}))
            .unwrap();
        assert_eq!(last["rows"].as_array().unwrap().len(), 19);
        assert_eq!(last["rows"][18]["id"], "message-119");
        assert_eq!(last["rows"][18]["evidence_role"], "direct");
        let reverse = app
            .evidence_query(&json!({"id":"task","sort":"desc"}))
            .unwrap();
        assert_eq!(reverse["rows"][0], last["rows"][18]);
        let rows = app
            .evidence_query(
                &json!({"id":"task","since":10,"until":12,"query":"SYNTHETIC needle","sort":"asc"}),
            )
            .unwrap();
        assert_eq!(rows["total"], 2);
        assert_eq!(rows["rows"][0]["time"], 11);
        assert_eq!(rows["rows"][0]["evidence_role"], "completion");
        assert_eq!(rows["rows"][1]["time"], 12);
        assert!(app.evidence_query(&json!({"id":"other"})).is_err());
    }
}
