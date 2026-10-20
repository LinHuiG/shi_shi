//! Short IPC acceptance followed by an explicit, queryable terminal result.
//! Polling never resubmits the original operation.
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
};
type ResultMap = BTreeMap<String, Option<Result<Value, String>>>;
fn jobs() -> &'static Mutex<ResultMap> {
    static JOBS: OnceLock<Mutex<ResultMap>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub fn allowed(method: &str) -> bool {
    matches!(
        method,
        "scan"
            | "analyze"
            | "format"
            | "schedule_format"
            | "edit_ai"
            | "report"
            | "archive"
            | "learn"
            | "prompt_retry"
            | "handling_learn"
            | "source_range"
    )
}
pub fn start(app: &Arc<crate::engine::App>, p: &Value) -> Result<Value, String> {
    let method = p["method"]
        .as_str()
        .filter(|m| allowed(m))
        .ok_or("不支持的后台操作")?
        .to_owned();
    let id = crate::model::id();
    {
        let mut map = jobs().lock().unwrap();
        if map.values().filter(|r| r.is_none()).count() >= 8 {
            return Err("后台操作繁忙，请等待当前操作结束".into());
        }
        while map.len() >= 32 {
            if let Some(key) = map
                .iter()
                .find(|(_, r)| r.is_some())
                .map(|(k, _)| k.clone())
            {
                map.remove(&key);
            } else {
                break;
            }
        }
        map.insert(id.clone(), None);
    }
    let worker = app.clone();
    let request = json!({"method":method,"params":p["params"]});
    let key = id.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::dispatch(&worker, &request)
        }))
        .unwrap_or_else(|_| Err("后台操作异常终止，请查看日志".into()));
        if let Err(ref error) = result {
            if error != crate::ai_control::CANCELLED {
                worker.log(
                    "request_error",
                    json!({"method":request["method"],"job_id":key,"error":error}),
                );
            }
        }
        jobs().lock().unwrap().insert(key, Some(result));
    });
    Ok(json!({"job_id":id}))
}
pub fn result(p: &Value) -> Result<Value, String> {
    let map = jobs().lock().unwrap();
    let job = map
        .get(p["job_id"].as_str().ok_or("缺少操作编号")?)
        .ok_or("操作记录已结束或程序已重启，请查看状态与日志；不要重复提交")?;
    Ok(match job {
        None => json!({"done":false}),
        Some(Ok(data)) => json!({"done":true,"ok":true,"data":data}),
        Some(Err(error)) => json!({"done":true,"ok":false,"error":error}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_operation_has_a_stable_result_and_queries_do_not_resubmit() {
        let root = std::env::temp_dir().join(format!("shishi-jobs-{}", crate::model::id()));
        let app = Arc::new(crate::engine::App::load(root.clone()).unwrap());
        assert!(start(&app, &json!({"method":"ai_job_start"})).is_err());
        let accepted = start(&app, &json!({"method":"analyze","params":{}})).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let terminal = loop {
            let value = result(&accepted).unwrap();
            if value["done"] == true {
                break value;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(terminal, result(&accepted).unwrap());
        assert_eq!(app.state.lock().unwrap().calls, 0);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn one_global_deadline_defaults_to_600_and_persists_without_connection_overrides() {
        let root = std::env::temp_dir().join(format!("shishi-timeout-{}", crate::model::id()));
        let app = crate::engine::App::load(root.clone()).unwrap();
        assert_eq!(app.state.lock().unwrap().config.ai_timeout_seconds, 600);
        app.configure_patch(&json!({"ai_timeout_seconds":900}))
            .unwrap();
        for value in [json!(0), json!(1801), json!(30.5), json!("600")] {
            assert!(app
                .configure_patch(&json!({"ai_timeout_seconds":value}))
                .is_err());
        }
        assert_eq!(app.state.lock().unwrap().config.ai_timeout_seconds, 900);
        drop(app);
        let reloaded = crate::engine::App::load(root.clone()).unwrap();
        assert_eq!(
            reloaded.state.lock().unwrap().config.ai_timeout_seconds,
            900
        );
        drop(reloaded);
        std::fs::remove_dir_all(root).unwrap();
    }
}
