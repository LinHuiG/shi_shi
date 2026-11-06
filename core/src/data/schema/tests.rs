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
