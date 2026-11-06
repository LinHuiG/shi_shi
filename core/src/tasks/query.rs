//! The pure halves of the task queries: request ranges and terms, audit
//! labels and the history search the logs page and change dialog share.

use serde_json::{json, Map, Value};

use crate::Result;

pub fn record_ascending(p: &Value) -> Result<bool> {
    let sort = match p.get("sort") {
        None | Some(Value::Null) => "desc",
        Some(v) => v.as_str().ok_or("时间排序无效")?,
    };
    match sort {
        "asc" => Ok(true),
        "desc" => Ok(false),
        _ => crate::bail!("时间排序无效"),
    }
}

pub fn record_range(p: &Value) -> Result<(i64, i64)> {
    let endpoint = |key: &str, default: i64| -> Result<i64> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => Ok(v.as_i64().filter(|v| *v >= 0).ok_or("时间区间无效")?),
        }
    };
    let (since, until) = (endpoint("since", 0)?, endpoint("until", i64::MAX)?);
    if since > until {
        crate::bail!("开始时间不能晚于结束时间");
    }
    Ok((since, until))
}

pub fn terms(p: &Value) -> Result<Vec<String>> {
    let query = p["query"].as_str().unwrap_or("");
    if query.chars().count() > 200 {
        crate::bail!("搜索内容不能超过 200 字");
    }
    Ok(query.to_lowercase().split_whitespace().map(str::to_owned).collect())
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

const CHANGED: [(&str, &str); 9] = [
    ("title", "标题"),
    ("description", "描述"),
    ("status", "状态"),
    ("priority", "优先级"),
    ("tags", "标签"),
    ("blocker", "阻塞"),
    ("waiting_for", "等待对象"),
    ("due", "截止时间"),
    ("archive_note", "归档备注"),
];

/// Whether a decorated audit row (with `task_number`, `task_title` and
/// `task_label`) matches every search term, over its visible content only.
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
    for (key, name) in CHANGED {
        if row["before"][key] != row["after"][key] {
            caption.push_str(name);
        }
    }
    let visible = |v: &Value| -> Map<String, Value> {
        CHANGED
            .iter()
            .map(|(k, _)| *k)
            .chain(["merged_into"])
            .filter_map(|k| v.get(k).map(|value| (k.to_string(), value.clone())))
            .collect()
    };
    let content = json!({
        "task_id": row["task_id"], "task_number": row["task_number"], "task_title": row["task_title"],
        "detail": row["detail"], "before": visible(&row["before"]), "after": visible(&row["after"]),
    });
    let text = format!("{content} {caption}").to_lowercase();
    terms.iter().all(|term| {
        if term.starts_with('#') && term.len() > 1 {
            row["task_label"].as_str().is_some_and(|l| l.to_lowercase().contains(term.as_str()))
                || !term[1..].bytes().all(|b| b.is_ascii_digit())
                    && row["task_id"].as_str().is_some_and(|id| id.to_lowercase().contains(&term[1..]))
        } else {
            text.contains(term.as_str())
        }
    })
}

pub(crate) fn snapshot_json(text: Option<String>) -> Value {
    text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// Audit rows in `[start, end)`, oldest first: the task part of the logs
/// page (`activity_query`).
const UPDATE_KINDS: [&str; 13] = [
    "ai_create",
    "ai_update",
    "ai_evidence",
    "ai_merge_duplicate",
    "manual_edit",
    "manual_ai_edit",
    "manual_status",
    "manual_archive",
    "merge",
    "undo",
    "archive_item",
    "deadline_update",
    "generation_done",
];

/// Whether the logs page lists an audit row as a task update.
pub fn task_update_kind(kind: &str, task_id: &str) -> bool {
    !task_id.is_empty() && UPDATE_KINDS.contains(&kind)
}
