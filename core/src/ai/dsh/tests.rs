use super::*;

#[test]
fn refusal_is_explicit_but_unknown_internal_errors_stay_unknown() {
    assert!(error_message(
        "ACP error -32603: Internal error: DeepSeek Messages stream: unsupported stop reason refusal"
    )
    .starts_with("供应商拒答"));
    assert!(error_message("analysis prompt stopped without an acceptable result: refusal").starts_with("供应商拒答"));
    assert_eq!(error_message("ACP error -32603: Internal error"), "ACP error -32603: Internal error");
}

#[test]
fn model_routes_keep_opaque_values_and_default_plain_ids() {
    assert_eq!(model_route("[\"fixture\",\"model\"]"), ("fixture".into(), "model".into()));
    assert_eq!(model_route("deepseek-v9"), ("deepseek-account".into(), "deepseek-v9".into()));
    assert_eq!(usage(None), Value::Null);
}

#[test]
fn missing_launcher_is_reported_without_spawning() {
    let root = crate::testing::temp_dir("dsh-missing");
    let c = AiConnection {
        driver: "dsh".into(),
        executable: root.join("absent.cmd").to_string_lossy().into(),
        ..Default::default()
    };
    let status = status(&c, &root).unwrap();
    assert_eq!(status["ready"], false);
    assert_eq!(status["status"], "未找到 Harness 启动器；请选择新版 dsh.cmd 或 dsh.exe");
    assert_eq!(validate(&c, "m").unwrap_err(), "未找到桌面 Harness 启动器，请选择新版 dsh.cmd");
    assert_eq!(models(&c, &root).unwrap_err(), "未找到 Harness 启动器；请选择新版 dsh.cmd 或 dsh.exe");
    assert_eq!(workspace_plugin(&c, false, false).unwrap_err(), "请先选择有效的 DSH 启动器");
    assert_eq!(initialize_acp(&c, &root, false).unwrap_err(), "请明确确认初始化 DSH 内置 ACP 环境");
    let launcher = root.join("dsh.cmd");
    fs::write(&launcher, "").unwrap();
    let c = AiConnection { executable: launcher.to_string_lossy().into(), ..c };
    assert_eq!(validate(&c, "").unwrap_err(), "请填写 Harness 模型编号");
    assert!(validate(&c, "m").is_ok());
}
