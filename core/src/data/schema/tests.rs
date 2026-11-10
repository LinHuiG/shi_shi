use super::*;

#[test]
fn migrates_once_and_rejects_newer_files() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    migrate(&mut conn).unwrap();
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v as usize, STEPS.len());
    conn.pragma_update(None, "user_version", 99).unwrap();
    assert!(migrate(&mut conn).unwrap_err().0.contains("高于"));
}

#[test]
fn built_in_prompts_follow_the_release_unless_the_user_changed_them() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    // As left by an older release: one target still built in, one saved by the user.
    conn.execute("UPDATE prompt_versions SET content = '旧内置' WHERE target IN ('format', 'report')", []).unwrap();
    super::super::prompts::record(&conn, "report", "人工改的汇总规则", "manual", "手动保存系统提示词").unwrap();
    migrate(&mut conn).unwrap();
    migrate(&mut conn).unwrap();
    let current = |target| super::super::prompts::current(&conn, target).unwrap().unwrap();
    let format = current("format");
    assert_eq!(
        (format.action.as_str(), format.content.as_str(), format.reason.as_str()),
        ("upgrade", crate::prompts::default("format"), "内置提示词随版本更新")
    );
    assert_eq!(current("report").content, "人工改的汇总规则");
    let versions: i64 = conn.query_row("SELECT count(*) FROM prompt_versions", [], |r| r.get(0)).unwrap();
    assert_eq!(versions as usize, crate::prompts::TARGETS.len() + 2, "only format was upgraded, once");
}
