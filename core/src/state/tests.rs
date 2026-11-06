use super::*;
use crate::Config;

const RUNTIME: [&str; 11] = [
    "ai_busy",
    "busy",
    "ai_request_id",
    "ai_cancelling",
    "ai_source",
    "ai_started",
    "ai_progress",
    "ai_live",
    "ai_edits",
    "updates",
    "activity",
];

#[tokio::test]
async fn unchanged_snapshot_has_only_runtime_keys() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    let first = snapshot(&ctx, &json!({"page": "tasks"})).await.unwrap();
    assert_eq!(first["_page"], "tasks");
    assert!(first.get("config").is_some());
    let rev = first["core_revision"].as_str().unwrap().to_string();
    let small = snapshot(&ctx, &json!({"page": "tasks", "core_revision": rev})).await.unwrap();
    let mut keys: Vec<&str> = small.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    let mut expected: Vec<&str> = RUNTIME.to_vec();
    expected.extend(["_unchanged", "_page", "core_revision", "ai_interrupted_at"]);
    expected.sort();
    assert_eq!(keys, expected);
}

#[tokio::test]
async fn revision_follows_commits_and_scope_but_not_runtime_output() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    let scope = Scope::from(&json!({"page": "tasks"}));
    let before = core_revision(&ctx, &scope);
    ctx.db.record_activity("scan", json!({})).await;
    assert_ne!(core_revision(&ctx, &scope), before, "a business write changes it");
    let before = core_revision(&ctx, &scope);
    ctx.flags.set_updates(&ctx, json!({"status": "downloading", "progress": 40}));
    assert_eq!(core_revision(&ctx, &scope), before);
    let other = Scope::from(&json!({"page": "tasks", "selected_task": "t1"}));
    assert_ne!(core_revision(&ctx, &other), before);
    {
        let _busy = ctx.flags.working(&ctx);
        assert_ne!(core_revision(&ctx, &scope), before);
    }
}

#[tokio::test]
async fn logs_page_gets_day_revision() {
    let ctx = Ctx::memory(Config::default()).unwrap();
    ctx.db.record_activity("scan", json!({"count": 1})).await;
    let logs = snapshot(&ctx, &json!({"page": "logs"})).await.unwrap();
    assert_eq!(logs["activity_day_revision"]["day"], time::today());
    assert_eq!(logs["activity"][0]["kind"], "scan");
}
