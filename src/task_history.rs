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
        ("next_step", "下一步"),
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
        "next_step",
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
    pub fn task_changes(&self, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("缺少事项编号")?;
        let terms = terms(p)?;
        let ascending = match p["sort"].as_str().unwrap_or("desc") {
            "asc" => true,
            "desc" => false,
            _ => return Err("时间排序无效".into()),
        };
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
                p["page"].as_u64().unwrap_or(1).max(1) as usize,
            )
    }
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
        drop(app);
        let _ = std::fs::remove_dir_all(root);
    }
}
