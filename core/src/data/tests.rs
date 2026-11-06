use super::*;

#[tokio::test]
async fn write_commits_and_bumps_revision_but_failure_rolls_back() {
    let db = Db::memory().unwrap();
    let changes = db.subscribe();
    db.write(|tx| {
        tx.execute("INSERT INTO kv(key, value) VALUES ('a', '1')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(db.revision(), 1);
    assert!(changes.has_changed().unwrap());
    let failed: Result<()> = db
        .write(|tx| {
            tx.execute("INSERT INTO kv(key, value) VALUES ('b', '2')", [])?;
            Err(Error::new("业务失败"))
        })
        .await;
    assert_eq!(failed.unwrap_err().0, "业务失败");
    assert_eq!(db.revision(), 1);
    let count: i64 = db.read(|c| Ok(c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn panicking_job_reports_error_and_thread_survives() {
    let db = Db::memory().unwrap();
    let err = db.read::<()>(|_| panic!("boom")).await.unwrap_err();
    assert_eq!(err.0, "数据线程中断");
    let one: i64 = db.read(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(one, 1);
}

#[test]
fn file_database_uses_wal_and_normal_sync() {
    let dir = crate::testing::temp_dir("db-pragma");
    let db = Db::open(&dir.join("shishi.db")).unwrap();
    let (mode, sync): (String, i64) = db
        .read_blocking(|c| {
            let mode = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let sync = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            Ok((mode, sync))
        })
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(sync, 1);
    db.write_blocking(|tx| Ok(tx.execute("INSERT INTO kv(key, value) VALUES ('k', 'v')", [])?)).unwrap();
    let wal = dir.join("shishi.db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    db.checkpoint_blocking().unwrap();
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0, "committed pages moved into shishi.db");
}
