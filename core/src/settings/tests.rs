use super::*;

fn ctx() -> Arc<Ctx> {
    Ctx::memory(Config::default()).unwrap()
}

#[tokio::test]
async fn patch_merges_fields_and_profiles_and_records_audit() {
    let ctx = ctx();
    let p = json!({"daily_calls": 20, "model_profiles": {"review": {"model": "m", "reasoning_effort": "low"}}});
    let out = patch(&ctx, &p).await.unwrap();
    assert_eq!(out["config"]["daily_calls"], 20);
    assert_eq!(out["config"]["model_profiles"]["review"]["model"], "m");
    assert_eq!(out["config"]["model_profiles"]["analysis"]["reasoning_effort"], "low");
    assert!(out["config"]["model_profiles"].get("handling").is_none());
    assert_eq!(ctx.config.get().daily_calls, 20);
    let kinds: Vec<String> = ctx
        .db
        .test_read(|c| {
            Ok(c.prepare("SELECT kind FROM task_audit")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(kinds, ["settings"]);
    // An unchanged patch writes nothing.
    let revision = ctx.db.revision();
    patch(&ctx, &json!({"daily_calls": 20})).await.unwrap();
    assert_eq!(ctx.db.revision(), revision);
}

#[tokio::test]
async fn patch_rejects_unknown_fields_and_bad_values_without_saving() {
    let ctx = ctx();
    let e = |p: Value| {
        let ctx = ctx.clone();
        async move { patch(&ctx, &p).await.unwrap_err().0 }
    };
    assert_eq!(e(json!([])).await, "设置补丁必须是对象");
    assert_eq!(e(json!({"tools": []})).await, "设置补丁包含不可修改字段");
    assert_eq!(e(json!({"model_profiles": {"chat": {}}})).await, "模型用途无效");
    assert_eq!(e(json!({"model_profiles": {"handling": {}}})).await, "模型用途无效");
    assert_eq!(e(json!({"model_profiles": 1})).await, "模型用途配置无效");
    assert_eq!(e(json!({"daily_calls": 501})).await, "每日调用上限不能超过500");
    assert_eq!(e(json!({"max_input_chars": 100})).await, "AI 输入上限应为5000–200000字符");
    assert_eq!(e(json!({"retrieval_budget_chars": 10})).await, "补查预算应为 4000–1000000 字符");
    assert_eq!(e(json!({"analysis_interval_minutes": 0})).await, "AI 分析间隔应为1–1440分钟");
    assert_eq!(e(json!({"analysis_lookback_hours": 87601})).await, "待分析消息时间范围应为1–87600小时");
    assert_eq!(e(json!({"message_retention_days": 0})).await, "消息保留期限应为1–3650天");
    assert_eq!(e(json!({"interval_minutes": 121})).await, "扫描间隔应在 1–120 分钟之间");
    assert_eq!(e(json!({"ai_timeout_seconds": 10})).await, "AI 总时限须为 30—1800 秒");
    assert_eq!(e(json!({"analysis_workdir": "../x"})).await, "分析工作目录应填写普通目录名或完整绝对路径，不能使用 ..");
    assert!(e(json!({"work_start": "8点"})).await.starts_with("上下班时间无效："));
    assert!(e(json!({"work_end": "08:00"})).await.starts_with("上下班时间无效："));
    assert_eq!(
        e(json!({"model_profiles": {"analysis": {"connection_id": "missing"}}})).await,
        "所选 AI 连接不存在，请重新选择接入"
    );
    assert_eq!(*ctx.config.get(), Config::default());
}

#[tokio::test]
async fn message_budget_must_leave_room_for_messages() {
    let ctx = ctx();
    let budget = |c: Config| {
        let ctx = ctx.clone();
        async move { crate::ai::prompt::message_budget(&c, "classify", &ctx.db.ai_policy("classify".into()).await?) }
    };
    let fixed = Config::default().max_input_chars - budget(Config::default()).await.unwrap().unwrap();
    let tight = Config { max_input_chars: fixed, ..Config::default() };
    assert_eq!(budget(tight.clone()).await.unwrap_err().0, "总输入上限不足以容纳规则、输出格式和上下文，请提高上限");
    let e = check(&tight, budget(tight.clone()).await);
    // The input range or the budget itself rejects it.
    assert!(e.is_err());
    // An unconfigured purpose has no route to measure; saving still works.
    let mut unconfigured = Config { max_input_chars: fixed, ..Config::default() };
    unconfigured.model_profiles.as_mut().unwrap().analysis = None;
    assert_eq!(budget(unconfigured.clone()).await.unwrap(), None);
    assert!(check(&unconfigured, budget(unconfigured.clone()).await).is_ok());
}

#[tokio::test]
async fn full_config_save_replaces_and_reports_type_errors() {
    let ctx = ctx();
    let mut c = serde_json::to_value(Config::default()).unwrap();
    c["leaders"] = json!(["甲"]);
    assert_eq!(handle(&ctx, "config", c.clone()).await.unwrap(), json!(true));
    assert_eq!(ctx.config.get().leaders, ["甲"]);
    c["unknown"] = json!(1);
    c["daily_calls"] = json!("many");
    assert!(handle(&ctx, "config", c.clone()).await.unwrap_err().0.contains("invalid type"));
    c["daily_calls"] = json!(20);
    assert_eq!(handle(&ctx, "config", c).await.unwrap(), json!(true));
    assert!(serde_json::to_value(&*ctx.config.get()).unwrap().get("unknown").is_none());
}
