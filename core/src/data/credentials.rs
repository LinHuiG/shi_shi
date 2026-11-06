//! Stored API keys ([`crate::ai::credentials`] encrypts and decrypts them).
//! Encryption runs before the write is queued and decryption after the
//! read returns, so the data thread only moves ciphertext.

use rusqlite::{params, Connection, OptionalExtension};

use super::Db;
use crate::ai::credentials::{self, CredentialSource, CredentialStatus, ResolvedCredential};
use crate::{time, Result};

fn upsert(tx: &Connection, id: &str, sealed: &[u8]) -> Result<()> {
    tx.execute(
        "INSERT INTO credentials(id, secret, updated) VALUES (?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET secret = excluded.secret, updated = excluded.updated",
        params![id, sealed, time::now()],
    )?;
    Ok(())
}

impl Db {
    /// Encrypt and store `secret` under reference `id`, replacing an entry.
    pub async fn set_credential(&self, id: String, secret: String) -> Result<()> {
        let sealed = credentials::seal(&id, &secret)?;
        self.write(move |tx| upsert(tx, &id, &sealed)).await
    }

    /// [`Self::set_credential`] from synchronous code (install migration).
    pub fn set_credential_blocking(&self, id: String, secret: String) -> Result<()> {
        let sealed = credentials::seal(&id, &secret)?;
        self.write_blocking(move |tx| upsert(tx, &id, &sealed))
    }

    /// The decrypted key under `id`; a bad entry fails only this reference.
    pub async fn credential(&self, id: String) -> Result<Option<ResolvedCredential>> {
        credentials::validate_connection_id(&id)?;
        let sealed: Option<Vec<u8>> = self
            .read(move |c| {
                Ok(c.query_row("SELECT secret FROM credentials WHERE id = ?1", [id], |r| r.get(0)).optional()?)
            })
            .await?;
        Ok(sealed.map(|s| credentials::open(&s)).transpose()?)
    }

    /// Whether a key is stored under `id`, without decrypting it.
    pub async fn credential_status(&self, id: String) -> Result<CredentialStatus> {
        credentials::validate_connection_id(&id)?;
        let updated: Option<i64> = self
            .read(move |c| {
                Ok(c.query_row("SELECT updated FROM credentials WHERE id = ?1", [id], |r| r.get(0)).optional()?)
            })
            .await?;
        Ok(CredentialStatus {
            configured: updated.is_some(),
            source: updated.map(|_| CredentialSource::Dpapi),
            updated_unix: updated.map(|u| (u / 1000) as u64),
        })
    }

    /// Delete the entry under `id`; whether one existed.
    pub async fn delete_credential(&self, id: String) -> Result<bool> {
        self.write(move |tx| Ok(tx.execute("DELETE FROM credentials WHERE id = ?1", [id])? > 0)).await
    }
}

#[cfg(test)]
mod tests;
