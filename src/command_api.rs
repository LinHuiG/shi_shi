//! Small query projections for console clients; mutations use the existing UI dispatcher.
use crate::engine::App;
use serde_json::{json, Value};

pub fn skill(root: &std::path::Path) -> Value {
    let cli = std::env::current_exe()
        .map(|path| path.with_file_name("ss.exe"))
        .unwrap_or_else(|_| root.join("ss.exe"));
    let path = cli.to_string_lossy().replace('\'', "''");
    let root = root.to_string_lossy().replace('\'', "''");
    json!({"text":format!("{}\n\n## 当前实例的 CLI\n\n以下路径和根目录对应当前打开的拾事。后续命令沿用此入口和 `--root`，将 `--help` 替换为业务命令。\n\n```powershell\n& '{}' --root '{}' --help\n```\n", include_str!("../docs/skills/shishi-cli/SKILL.md"), path, root)})
}

pub fn task_get(app: &App, p: &Value) -> Result<Value, String> {
    let state = app.state.lock().unwrap();
    let id = p["id"].as_str().ok_or("缺少事项编号")?;
    state
        .tasks
        .iter()
        .find(|t| t.id == id)
        .map(|t| json!(t))
        .ok_or_else(|| "事项不存在".into())
}
pub fn tasks_query(app: &App, p: &Value) -> Result<Value, String> {
    let limit = p["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
    let offset = p["offset"].as_u64().unwrap_or(0) as usize;
    let query = p["query"].as_str().unwrap_or("").to_lowercase();
    let focus = p["focus"].as_str();
    if p.get("focus").is_some()
        && !matches!(focus, Some("待处理" | "处理中" | "被阻塞" | "延后处理"))
    {
        return Err("关注范围应为待处理、处理中、被阻塞或延后处理".into());
    }
    let state = app.state.lock().unwrap();
    let at = crate::model::now();
    let mut tasks = state
        .tasks
        .iter()
        .filter(|t| {
            p["archived"].as_bool().is_none_or(|v| t.archived == v)
                && p["status"].as_str().is_none_or(|v| t.status == v)
                && focus.is_none_or(|v| t.focus(at) == Some(v))
                && (query.is_empty()
                    || t.title.to_lowercase().contains(&query)
                    || t.description.to_lowercase().contains(&query)
                    || state
                        .task_numbers
                        .get(&t.id)
                        .is_some_and(|number| format!("#{number}").contains(&query))
                    || t.id == query)
        })
        .collect::<Vec<_>>();
    let catalog = crate::tags::groups(&state);
    let kind = crate::tags::priority_kind(&state);
    tasks.sort_by_cached_key(|t| {
        (
            crate::tags::priority_rank(
                &crate::tags::effective_tags(t, &catalog, &kind, at),
                &catalog,
                &kind,
            ),
            std::cmp::Reverse(t.updated),
            t.id.clone(),
        )
    });
    // Reorder only deferred slots; mixed lists keep other tasks in place.
    let mut deferred = tasks
        .iter()
        .copied()
        .filter(|t| t.focus(at) == Some("延后处理"))
        .collect::<Vec<_>>();
    if deferred.len() > 1 {
        // Stable sorting retains the existing priority/update order for ties.
        deferred.sort_by_key(|t| t.deferred_until);
        let mut ordered = deferred.into_iter();
        for task in &mut tasks {
            if task.focus(at) == Some("延后处理") {
                *task = ordered.next().unwrap();
            }
        }
    }
    Ok(
        json!({"total":tasks.len(),"offset":offset,"limit":limit,"items":tasks.into_iter().skip(offset).take(limit).map(|t|json!({"id":t.id,"number":state.task_numbers.get(&t.id),"title":t.title,"status":t.status,"focus":t.focus(at),"due":t.due,"deferred_until":t.deferred_until,"updated":t.updated,"archived":t.archived,"merged_into":t.merged_into,"tags":t.tags})).collect::<Vec<_>>()}),
    )
}
pub fn methods() -> Value {
    let names = crate::COMMAND_METHODS
        .iter()
        .copied()
        .chain(["storage_change"])
        .collect::<std::collections::BTreeSet<_>>();
    json!({"protocol_version":1,"methods":names,"gui_only":["pick"],"native_progress_window":["storage_change"],
        "examples":{
            "tasks_query":{"focus":"待处理","limit":20,"offset":0},
            "task_get":{"id":"<task-id>"},
            "edit_create":{"method":"edit","params":{"task":{"title":"示例事项","status":"待处理"}}},
            "edit_update":{"method":"edit","params":{"task":{"id":"<task-id>","updated":"<从 task_get 读取的整数>","title":"更新标题"}}},
            "action":{"id":"<task-id>","action":"done"},
            "memory_save":{"kind":"temporary","expected":"<从 memory_read 读取的整数>","text":"内容"},
            "ai_job_start":{"method":"analyze","params":{}},
            "ai_job_result":{"job_id":"<job-id>"}},
        "notes":["所有变更复用图形界面的业务校验与保存；事项修改必须携带读取到的 updated，记忆保存必须携带 revision 作为 expected。","查询长操作的 job_id 不会重复提交。后台重启后查不到的操作，请核对当前数据和日志，不自动重发。","CLI 的文件输入使用 --input 或 --params；业务方法的参数与客户端 IPC 一致。"]})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_contains_shared_mutations_and_versioned_queries() {
        let catalogue = methods();
        for name in [
            "edit",
            "action",
            "memory_read",
            "memory_save",
            "tasks_query",
            "task_get",
            "storage_change",
            "connection_save",
            "tool_save",
            "schedule_edit",
            "cli_skill",
        ] {
            assert!(
                catalogue["methods"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(name)),
                "{name}"
            );
        }
    }
}
