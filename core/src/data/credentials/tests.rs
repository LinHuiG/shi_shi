use crate::ai::credentials::CredentialSource;
use crate::Db;

const KEY: &str = "synthetic-test-key-7d1a9c";

#[tokio::test]
async fn keys_are_stored_encrypted_per_entry_and_never_shown() {
    let db = Db::memory().unwrap();
    let id = || "connection-alpha".to_string();
    assert!(!db.credential_status(id()).await.unwrap().configured);
    assert!(db.credential(id()).await.unwrap().is_none());
    db.set_credential(id(), KEY.into()).await.unwrap();
    let status = db.credential_status(id()).await.unwrap();
    assert_eq!((status.configured, status.source), (true, Some(CredentialSource::Dpapi)));
    assert!(!format!("{status:?}").contains(KEY));
    assert_eq!(db.credential(id()).await.unwrap().unwrap().expose_secret(), KEY);
    let stored: Vec<u8> =
        db.read(|c| Ok(c.query_row("SELECT secret FROM credentials", [], |r| r.get(0))?)).await.unwrap();
    assert!(!stored.windows(KEY.len()).any(|w| w == KEY.as_bytes()));

    // Replacing keeps one row; deleting reports whether one existed.
    db.set_credential(id(), "synthetic-other".into()).await.unwrap();
    assert_eq!(db.credential(id()).await.unwrap().unwrap().expose_secret(), "synthetic-other");
    assert!(db.delete_credential(id()).await.unwrap());
    assert!(!db.delete_credential(id()).await.unwrap());
    assert!(db.credential(id()).await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_input_and_a_corrupt_entry_fail_only_themselves() {
    let db = Db::memory().unwrap();
    for id in ["", ".", "..", "../escape", "with space", "with/slash"] {
        let e = db.set_credential(id.into(), KEY.into()).await.unwrap_err();
        assert!(e.0.contains("invalid connection id"), "{}", e.0);
        assert!(!e.0.contains(KEY));
    }
    for secret in [String::new(), "x".repeat(4097)] {
        let e = db.set_credential("connection-delta".into(), secret).await.unwrap_err();
        assert!(e.0.contains("invalid secret"), "{}", e.0);
    }
    db.set_credential("connection-good".into(), KEY.into()).await.unwrap();
    db.write(|tx| Ok(tx.execute("INSERT INTO credentials VALUES ('connection-bad', x'00', 1)", [])?))
        .await
        .unwrap();
    let e = db.credential("connection-bad".into()).await.err().unwrap();
    assert!(e.0.contains("decryption failed"), "{}", e.0);
    assert_eq!(db.credential("connection-good".into()).await.unwrap().unwrap().expose_secret(), KEY);
    let sealed = crate::ai::credentials::protect(&[0xff]).unwrap();
    db.write(move |tx| Ok(tx.execute("UPDATE credentials SET secret = ?1 WHERE id = 'connection-bad'", [sealed])?))
        .await
        .unwrap();
    let e = db.credential("connection-bad".into()).await.err().unwrap();
    assert_eq!(e.0, "credential error: stored credential is corrupt");
}
