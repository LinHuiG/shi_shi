use super::*;

fn p(args: &[&str]) -> Result<Args, String> {
    parse(args.iter().map(|s| s.to_string()))
}

#[test]
fn modes_flags_and_errors() {
    let a = p(&[]).unwrap();
    assert_eq!(a.mode, Mode::Desktop);
    assert!(!a.background && a.root.is_none());
    let a = p(&["--background", "--root", "R", "--no-background-jobs", "--no-auto-update"]).unwrap();
    assert!(a.background && a.no_jobs && a.no_auto_update);
    assert_eq!(a.root, Some(PathBuf::from("R")));
    assert_eq!(p(&["--plugin-worker"]).unwrap().mode, Mode::PluginWorker);
    assert_eq!(p(&["--analysis-submit-mcp", "S"]).unwrap().mode, Mode::AnalysisSubmit("S".into()));
    assert_eq!(p(&["--apply-update", "P"]).unwrap().mode, Mode::ApplyUpdate("P".into()));
    assert_eq!(p(&["--data-transfer", "T"]).unwrap().mode, Mode::DataTransfer("T".into()));
    assert_eq!(p(&["--no-background-jobs"]).unwrap_err(), "隔离验证选项必须同时指定 --root");
    assert_eq!(p(&["--x"]).unwrap_err(), "未知启动参数：--x");
    assert_eq!(p(&["--root"]).unwrap_err(), "--root 缺少路径");
}

#[test]
fn restart_keeps_isolation_and_current_owners() {
    let a = p(&["--root", "R", "--no-background-jobs", "--cli-owner"]).unwrap();
    assert_eq!(a.restart(true, false), ["--root", "R", "--no-background-jobs"]);
    assert_eq!(a.restart(true, true), ["--root", "R", "--no-background-jobs", "--cli-owner"]);
    assert_eq!(a.restart(false, true), ["--root", "R", "--no-background-jobs", "--background"]);
}
