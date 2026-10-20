//! DPAPI-backed credential storage and environment-variable resolution.
//!
//! # Storage
//!
//! The caller supplies a directory. A single encrypted file
//! (`credentials.bin`) holds a JSON map of opaque connection identifiers to
//! secrets and update timestamps. The plaintext JSON exists only in process
//! memory. On disk the whole map is encrypted with Windows DPAPI in the
//! current-user scope using fixed application entropy and
//! `CRYPTPROTECT_UI_FORBIDDEN`; no machine scope is used.
//!
//! Mutations take an exclusive lock on `credentials.lock`; reads use a shared
//! lock. The updated store is written to a temporary file in the same
//! directory and atomically renamed over the old file. A failed encryption,
//! write, or rename leaves the previous store in place. An unreadable,
//! oversized, or tampered store is reported as an error and is never reset
//! automatically. No plaintext temporary file or backup is created.
//!
//! The encrypted file is bounded to 8 MiB and is size-checked before it is
//! read or passed to DPAPI. Its decrypted JSON is bounded to 6 MiB before
//! parsing. A valid store holds at most 1024 entries; each entry still has a
//! 128-byte connection id and a 1–4096-byte secret. Stores outside these
//! limits are rejected as [`CredentialErrorKind::StoreTooLarge`] without
//! modifying the file.
//!
//! # Environment selection
//!
//! [`CredentialStore::resolve`] and [`CredentialStore::status`] take an
//! optional environment variable name. `Some(name)` selects the environment
//! **exclusively**: a missing or empty variable means not configured (status)
//! or no value (resolve), with source reported as
//! [`CredentialSource::Environment`]; it never falls back to DPAPI. A
//! non-Unicode value is an error, not a silent fallback. `None` selects only
//! the DPAPI store. Environment names must match
//! `[A-Za-z_][A-Za-z0-9_]{0,63}`. Environment values are visible to other
//! same-user processes; DPAPI values at rest are not.
//!
//! The public API never includes a secret in `Debug` output, errors, status,
//! or display strings. [`ResolvedCredential`] exposes the secret only through
//! the explicitly named [`ResolvedCredential::expose_secret`] method.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

const STORE_FILE: &str = "credentials.bin";
const LOCK_FILE: &str = "credentials.lock";
const STORE_VERSION: u32 = 1;
const MAX_CONNECTION_ID_BYTES: usize = 128;
const MAX_SECRET_BYTES: usize = 4096;
const MAX_ENV_NAME_BYTES: usize = 64;
const MAX_STORE_ENTRIES: usize = 1024;
const MAX_STORE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STORE_PLAINTEXT_BYTES: usize = 6 * 1024 * 1024;
const DPAPI_ENTROPY: &[u8] = b"ShiShi/credential-store/v1";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A credential store rooted in a caller-supplied directory.
///
/// Construction performs no I/O. The directory is created lazily on the first
/// operation. Do not place this directory on a network share: the lock and
/// atomic replacement are intended for a local filesystem.
#[derive(Clone)]
pub struct CredentialStore {
    root: PathBuf,
}

impl CredentialStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Encrypt and store a synthetic or user credential for an opaque
    /// connection identifier. Returns only non-secret status.
    pub fn set(
        &self,
        connection_id: &str,
        secret: &str,
    ) -> Result<CredentialStatus, CredentialError> {
        validate_connection_id(connection_id)?;
        validate_secret(secret)?;
        let updated_unix = unix_now();
        self.with_exclusive(|store| {
            if !store.entries.contains_key(connection_id)
                && store.entries.len() >= MAX_STORE_ENTRIES
            {
                return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
            }
            store.entries.insert(
                connection_id.to_string(),
                StoredEntry {
                    secret: secret.to_string(),
                    updated_unix,
                },
            );
            self.save(store)?;
            Ok(CredentialStatus {
                configured: true,
                source: Some(CredentialSource::Dpapi),
                updated_unix: Some(updated_unix),
            })
        })
    }

    /// Read a DPAPI-backed credential. Environment variables are not consulted
    /// here; use [`CredentialStore::resolve`] for explicit source selection.
    pub fn get(&self, connection_id: &str) -> Result<Option<ResolvedCredential>, CredentialError> {
        validate_connection_id(connection_id)?;
        self.with_shared(|store| {
            Ok(store
                .entries
                .get(connection_id)
                .map(|entry| ResolvedCredential {
                    secret: entry.secret.clone(),
                    source: CredentialSource::Dpapi,
                    updated_unix: Some(entry.updated_unix),
                }))
        })
    }

    /// Delete a DPAPI-backed credential. Returns whether an entry existed.
    pub fn delete(&self, connection_id: &str) -> Result<bool, CredentialError> {
        validate_connection_id(connection_id)?;
        self.with_exclusive(|store| {
            if store.entries.remove(connection_id).is_some() {
                self.save(store)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    /// Report configured state and source without revealing the value.
    ///
    /// `Some(name)` checks the environment exclusively and never reads or
    /// decrypts DPAPI. A missing or empty variable is reported as not
    /// configured with [`CredentialSource::Environment`]. `None` reports only
    /// the DPAPI entry; an unreadable store is an error in that case.
    pub fn status(
        &self,
        connection_id: &str,
        env_name: Option<&str>,
    ) -> Result<CredentialStatus, CredentialError> {
        validate_connection_id(connection_id)?;
        if let Some(name) = env_name {
            validate_env_name(name)?;
            return Ok(match read_environment(name)? {
                Some(_) => CredentialStatus {
                    configured: true,
                    source: Some(CredentialSource::Environment),
                    updated_unix: None,
                },
                None => CredentialStatus {
                    configured: false,
                    source: Some(CredentialSource::Environment),
                    updated_unix: None,
                },
            });
        }

        self.with_shared(|store| Ok(status_from_store(store, connection_id)))
    }

    /// Resolve a credential from exactly one source.
    ///
    /// `Some(name)` reads only the environment: a present non-empty value is
    /// returned, while a missing or empty one yields `Ok(None)` and never
    /// falls back to DPAPI. `None` reads only the DPAPI entry. Secrets are
    /// returned only inside [`ResolvedCredential`]. A recognized but
    /// non-Unicode environment value is an error and never falls back.
    pub fn resolve(
        &self,
        connection_id: &str,
        env_name: Option<&str>,
    ) -> Result<Option<ResolvedCredential>, CredentialError> {
        validate_connection_id(connection_id)?;
        if let Some(name) = env_name {
            validate_env_name(name)?;
            return Ok(read_environment(name)?.map(|value| ResolvedCredential {
                secret: value,
                source: CredentialSource::Environment,
                updated_unix: None,
            }));
        }
        self.get(connection_id)
    }

    fn with_shared<T>(
        &self,
        operation: impl FnOnce(&StoreData) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        self.with_lock(false, |store| operation(store))
    }

    fn with_exclusive<T>(
        &self,
        operation: impl FnOnce(&mut StoreData) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        self.with_lock(true, operation)
    }

    fn with_lock<T>(
        &self,
        exclusive: bool,
        operation: impl FnOnce(&mut StoreData) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        fs::create_dir_all(&self.root).map_err(CredentialError::io)?;
        let lock_path = self.root.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(CredentialError::io)?;
        if exclusive {
            lock.lock_exclusive().map_err(CredentialError::lock)?;
        } else {
            lock.lock_shared().map_err(CredentialError::lock)?;
        }
        let mut store = self.load()?;
        operation(&mut store)
    }

    fn load(&self) -> Result<StoreData, CredentialError> {
        let path = self.store_path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreData::default());
            }
            Err(error) => return Err(CredentialError::io(error)),
        };
        let metadata = file.metadata().map_err(CredentialError::io)?;
        if metadata.len() == 0 {
            return Err(CredentialError::new(CredentialErrorKind::CorruptStore));
        }
        if metadata.len() > MAX_STORE_BYTES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }

        // Bound the allocation even if the file grows after the metadata check.
        let capacity = usize::try_from(metadata.len())
            .map_err(|_| CredentialError::new(CredentialErrorKind::StoreTooLarge))?;
        let mut encrypted = Vec::with_capacity(capacity);
        file.take(MAX_STORE_BYTES + 1)
            .read_to_end(&mut encrypted)
            .map_err(CredentialError::io)?;
        if encrypted.len() as u64 > MAX_STORE_BYTES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }
        if encrypted.is_empty() {
            return Err(CredentialError::new(CredentialErrorKind::CorruptStore));
        }

        let plaintext = unprotect(&encrypted)?;
        if plaintext.len() > MAX_STORE_PLAINTEXT_BYTES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }
        let store: StoreData = serde_json::from_slice(&plaintext)
            .map_err(|_| CredentialError::new(CredentialErrorKind::CorruptStore))?;
        if store.version != STORE_VERSION {
            return Err(CredentialError::new(
                CredentialErrorKind::UnsupportedStoreVersion,
            ));
        }
        if store.entries.len() > MAX_STORE_ENTRIES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }
        for (connection_id, entry) in &store.entries {
            if validate_connection_id(connection_id).is_err()
                || entry.secret.is_empty()
                || entry.secret.len() > MAX_SECRET_BYTES
            {
                return Err(CredentialError::new(CredentialErrorKind::CorruptStore));
            }
        }
        Ok(store)
    }

    fn save(&self, store: &StoreData) -> Result<(), CredentialError> {
        if store.entries.len() > MAX_STORE_ENTRIES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }
        let plaintext = serde_json::to_vec(store)
            .map_err(|_| CredentialError::new(CredentialErrorKind::StoreWrite))?;
        if plaintext.len() > MAX_STORE_PLAINTEXT_BYTES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }
        let encrypted = protect(&plaintext)?;
        if encrypted.len() as u64 > MAX_STORE_BYTES {
            return Err(CredentialError::new(CredentialErrorKind::StoreTooLarge));
        }

        let mut temp = None;
        for _ in 0..4 {
            let path = self.temp_path();
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    temp = Some((path, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(CredentialError::io(error)),
            }
        }
        let Some((temp_path, mut file)) = temp else {
            return Err(CredentialError::new(CredentialErrorKind::StoreWrite));
        };

        let write_result = (|| -> std::io::Result<()> {
            file.write_all(&encrypted)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, self.store_path())
        })();

        match write_result {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                Err(CredentialError::io(error))
            }
        }
    }

    fn store_path(&self) -> PathBuf {
        self.root.join(STORE_FILE)
    }

    fn temp_path(&self) -> PathBuf {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        self.root.join(format!(
            "{STORE_FILE}.tmp.{}.{}.{}",
            std::process::id(),
            nanos,
            counter
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Dpapi,
    Environment,
}

impl CredentialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dpapi => "dpapi",
            Self::Environment => "environment",
        }
    }
}

/// Non-secret status suitable for UI or logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialStatus {
    pub configured: bool,
    pub source: Option<CredentialSource>,
    pub updated_unix: Option<u64>,
}

/// A resolved secret. This type intentionally implements neither `Debug` nor
/// `Display`; use [`ResolvedCredential::expose_secret`] at the last possible
/// point.
pub struct ResolvedCredential {
    secret: String,
    source: CredentialSource,
    updated_unix: Option<u64>,
}

impl ResolvedCredential {
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }

    pub fn source(&self) -> CredentialSource {
        self.source
    }

    pub fn updated_unix(&self) -> Option<u64> {
        self.updated_unix
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialErrorKind {
    InvalidConnectionId,
    InvalidSecret,
    InvalidEnvName,
    EnvironmentNotUnicode,
    Io,
    Lock,
    Encrypt,
    Decrypt,
    CorruptStore,
    UnsupportedStoreVersion,
    StoreTooLarge,
    StoreWrite,
}

impl CredentialErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConnectionId => "invalid connection id",
            Self::InvalidSecret => "invalid secret",
            Self::InvalidEnvName => "invalid environment variable name",
            Self::EnvironmentNotUnicode => "environment value is not Unicode",
            Self::Io => "I/O error",
            Self::Lock => "file lock error",
            Self::Encrypt => "DPAPI encryption failed",
            Self::Decrypt => "DPAPI decryption failed",
            Self::CorruptStore => "credential store is corrupt",
            Self::UnsupportedStoreVersion => "unsupported credential store version",
            Self::StoreTooLarge => "credential store exceeds documented limits",
            Self::StoreWrite => "credential store write failed",
        }
    }
}

/// Safe credential error. It never contains a path, connection id, environment
/// value, or secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialError {
    kind: CredentialErrorKind,
    os_code: Option<u32>,
}

impl CredentialError {
    fn new(kind: CredentialErrorKind) -> Self {
        Self {
            kind,
            os_code: None,
        }
    }

    fn io(error: std::io::Error) -> Self {
        Self {
            kind: CredentialErrorKind::Io,
            os_code: error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok()),
        }
    }

    fn lock(error: std::io::Error) -> Self {
        Self {
            kind: CredentialErrorKind::Lock,
            os_code: error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok()),
        }
    }

    fn dpapi(kind: CredentialErrorKind, code: u32) -> Self {
        Self {
            kind,
            os_code: (code != 0).then_some(code),
        }
    }

    pub fn kind(&self) -> CredentialErrorKind {
        self.kind
    }

    /// Optional native error code, safe for diagnostics.
    pub fn os_code(&self) -> Option<u32> {
        self.os_code
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.os_code {
            Some(code) => write!(
                f,
                "credential error: {} (code {})",
                self.kind.as_str(),
                code
            ),
            None => write!(f, "credential error: {}", self.kind.as_str()),
        }
    }
}

impl std::error::Error for CredentialError {}

#[derive(Serialize, Deserialize)]
struct StoreData {
    version: u32,
    entries: BTreeMap<String, StoredEntry>,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    secret: String,
    updated_unix: u64,
}

fn validate_connection_id(connection_id: &str) -> Result<(), CredentialError> {
    if connection_id.is_empty()
        || connection_id.len() > MAX_CONNECTION_ID_BYTES
        || connection_id == "."
        || connection_id == ".."
        || !connection_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_'
        })
    {
        return Err(CredentialError::new(
            CredentialErrorKind::InvalidConnectionId,
        ));
    }
    Ok(())
}

fn validate_secret(secret: &str) -> Result<(), CredentialError> {
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(CredentialError::new(CredentialErrorKind::InvalidSecret));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), CredentialError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_ENV_NAME_BYTES
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(CredentialError::new(CredentialErrorKind::InvalidEnvName));
    }
    Ok(())
}

fn read_environment(name: &str) -> Result<Option<String>, CredentialError> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => {
            let value = value
                .into_string()
                .map_err(|_| CredentialError::new(CredentialErrorKind::EnvironmentNotUnicode))?;
            if value.len() > MAX_SECRET_BYTES {
                Err(CredentialError::new(CredentialErrorKind::InvalidSecret))
            } else {
                Ok(Some(value))
            }
        }
    }
}

fn status_from_store(store: &StoreData, connection_id: &str) -> CredentialStatus {
    match store.entries.get(connection_id) {
        Some(entry) => CredentialStatus {
            configured: true,
            source: Some(CredentialSource::Dpapi),
            updated_unix: Some(entry.updated_unix),
        },
        None => CredentialStatus {
            configured: false,
            source: None,
            updated_unix: None,
        },
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn protect(plaintext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    let plaintext_len = u32::try_from(plaintext.len())
        .map_err(|_| CredentialError::new(CredentialErrorKind::Encrypt))?;
    if plaintext_len == 0 {
        return Err(CredentialError::new(CredentialErrorKind::Encrypt));
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext_len,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let entropy = entropy_blob();
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };

    let ok = unsafe {
        CryptProtectData(
            &input,
            null(),
            &entropy,
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(CredentialError::dpapi(
            CredentialErrorKind::Encrypt,
            unsafe { GetLastError() },
        ));
    }
    copy_and_free_blob(&output)
}

fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    let ciphertext_len = u32::try_from(ciphertext.len())
        .map_err(|_| CredentialError::new(CredentialErrorKind::Decrypt))?;
    if ciphertext_len == 0 {
        return Err(CredentialError::new(CredentialErrorKind::Decrypt));
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: ciphertext_len,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let entropy = entropy_blob();
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            null_mut(),
            &entropy,
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(CredentialError::dpapi(
            CredentialErrorKind::Decrypt,
            unsafe { GetLastError() },
        ));
    }
    copy_and_free_blob(&output)
}

fn entropy_blob() -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr() as *mut u8,
    }
}

fn copy_and_free_blob(blob: &CRYPT_INTEGER_BLOB) -> Result<Vec<u8>, CredentialError> {
    let result = if blob.pbData.is_null() || blob.cbData == 0 {
        Err(CredentialError::new(CredentialErrorKind::CorruptStore))
    } else {
        let bytes =
            unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec() };
        Ok(bytes)
    };
    if !blob.pbData.is_null() {
        unsafe {
            LocalFree(blob.pbData as *mut c_void);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;

    const SYNTHETIC_KEY: &str = "synthetic-test-key-7d1a9c";
    const SYNTHETIC_ENV_KEY: &str = "synthetic-env-key-2b8e4f";
    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "shishi-transport-credentials-{label}-{}-{}",
                std::process::id(),
                TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temporary credential directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(&self.path) {
                for entry in entries.flatten() {
                    let mut permissions = match entry.metadata() {
                        Ok(metadata) => metadata.permissions(),
                        Err(_) => continue,
                    };
                    if permissions.readonly() {
                        permissions.set_readonly(false);
                        let _ = fs::set_permissions(entry.path(), permissions);
                    }
                }
            }
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct EnvGuard {
        name: String,
    }

    impl EnvGuard {
        fn set(name: String, value: &str) -> Self {
            std::env::set_var(&name, value);
            Self { name }
        }

        fn name(&self) -> &str {
            &self.name
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(&self.name);
        }
    }

    fn env_name(label: &str) -> String {
        format!(
            "SHISHI_TEST_{label}_{}_{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn files(path: &Path) -> Vec<PathBuf> {
        fs::read_dir(path)
            .expect("read test directory")
            .map(|entry| entry.expect("directory entry").path())
            .collect()
    }

    #[test]
    fn roundtrip_and_status() {
        let temp = TempDir::new("roundtrip");
        let store = CredentialStore::new(temp.path());
        assert_eq!(
            store
                .status("connection-alpha", None)
                .expect("initial status"),
            CredentialStatus {
                configured: false,
                source: None,
                updated_unix: None,
            }
        );

        let status = store
            .set("connection-alpha", SYNTHETIC_KEY)
            .expect("store synthetic key");
        assert!(status.configured);
        assert_eq!(status.source, Some(CredentialSource::Dpapi));
        assert!(status.updated_unix.is_some());

        let resolved = store
            .get("connection-alpha")
            .expect("read synthetic key")
            .expect("credential exists");
        assert_eq!(resolved.expose_secret(), SYNTHETIC_KEY);
        assert_eq!(resolved.source(), CredentialSource::Dpapi);
        assert_eq!(resolved.updated_unix(), status.updated_unix);

        assert_eq!(
            store
                .status("connection-alpha", None)
                .expect("status after set"),
            status
        );
        assert!(store.delete("connection-alpha").expect("delete credential"));
        assert!(store
            .get("connection-alpha")
            .expect("read deleted credential")
            .is_none());
        assert_eq!(
            store
                .status("connection-alpha", None)
                .expect("status after delete"),
            CredentialStatus {
                configured: false,
                source: None,
                updated_unix: None,
            }
        );
        assert!(!store
            .delete("connection-alpha")
            .expect("delete missing credential"));
    }

    #[test]
    fn encrypted_bytes_do_not_contain_plaintext() {
        let temp = TempDir::new("encrypted");
        let store = CredentialStore::new(temp.path());
        store
            .set("connection-alpha", SYNTHETIC_KEY)
            .expect("store synthetic key");

        let paths = files(temp.path());
        let mut names: Vec<String> = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        let mut expected = vec![LOCK_FILE.to_string(), STORE_FILE.to_string()];
        expected.sort();
        assert_eq!(names, expected);

        for path in paths {
            let bytes = fs::read(&path).expect("read stored file");
            assert!(
                find_bytes(&bytes, SYNTHETIC_KEY.as_bytes()).is_none(),
                "plaintext secret appeared in {}",
                path.display()
            );
            assert!(
                find_bytes(&bytes, b"connection-alpha").is_none(),
                "connection id appeared in {}",
                path.display()
            );
        }
    }

    #[test]
    fn dpapi_current_user_roundtrip() {
        let ciphertext = protect(SYNTHETIC_KEY.as_bytes()).expect("DPAPI protect");
        assert!(!ciphertext.is_empty());
        assert!(find_bytes(&ciphertext, SYNTHETIC_KEY.as_bytes()).is_none());
        let plaintext = unprotect(&ciphertext).expect("DPAPI unprotect");
        assert_eq!(plaintext, SYNTHETIC_KEY.as_bytes());
    }

    #[test]
    fn environment_selection_never_falls_back_to_dpapi() {
        let temp = TempDir::new("environment");
        let store = CredentialStore::new(temp.path());
        store
            .set("connection-beta", SYNTHETIC_KEY)
            .expect("store DPAPI value");
        let store_path = temp.path().join(STORE_FILE);

        let name = env_name("EXCLUSIVE");
        let guard = EnvGuard::set(name, SYNTHETIC_ENV_KEY);
        let status = store
            .status("connection-beta", Some(guard.name()))
            .expect("environment status");
        assert_eq!(
            status,
            CredentialStatus {
                configured: true,
                source: Some(CredentialSource::Environment),
                updated_unix: None,
            }
        );
        let resolved = store
            .resolve("connection-beta", Some(guard.name()))
            .expect("resolve environment")
            .expect("environment value exists");
        assert_eq!(resolved.expose_secret(), SYNTHETIC_ENV_KEY);
        assert_eq!(resolved.source(), CredentialSource::Environment);

        std::env::remove_var(guard.name());
        let missing_status = store
            .status("connection-beta", Some(guard.name()))
            .expect("missing selected environment is not an error");
        assert_eq!(
            missing_status,
            CredentialStatus {
                configured: false,
                source: Some(CredentialSource::Environment),
                updated_unix: None,
            }
        );
        assert!(
            store
                .resolve("connection-beta", Some(guard.name()))
                .expect("missing selected environment resolves to no value")
                .is_none(),
            "missing selected environment must not fall back to DPAPI"
        );

        std::env::set_var(guard.name(), "");
        let empty_status = store
            .status("connection-beta", Some(guard.name()))
            .expect("empty selected environment is not an error");
        assert_eq!(empty_status, missing_status);
        assert!(
            store
                .resolve("connection-beta", Some(guard.name()))
                .expect("empty selected environment resolves to no value")
                .is_none(),
            "empty selected environment must not fall back to DPAPI"
        );

        let dpapi_value = store
            .resolve("connection-beta", None)
            .expect("None selects DPAPI")
            .expect("DPAPI value exists");
        assert_eq!(dpapi_value.expose_secret(), SYNTHETIC_KEY);
        assert_eq!(dpapi_value.source(), CredentialSource::Dpapi);

        // Even an unreadable DPAPI store must not affect an explicitly
        // selected environment. This proves the environment path does not
        // read or decrypt the store merely to determine its status.
        let mut bytes = fs::read(&store_path).expect("read DPAPI store");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5a;
        fs::write(&store_path, &bytes).expect("tamper DPAPI store");
        assert_eq!(
            store
                .status("connection-beta", Some(guard.name()))
                .expect("environment status does not decrypt DPAPI"),
            missing_status
        );
        assert!(store
            .resolve("connection-beta", Some(guard.name()))
            .expect("environment resolution does not decrypt DPAPI")
            .is_none());
        let error = store
            .resolve("connection-beta", None)
            .err()
            .expect("None still reads the corrupt DPAPI store");
        let rendered = format!("{empty_status:?} {error} {error:?}");
        assert!(!rendered.contains(SYNTHETIC_KEY));
        assert!(!rendered.contains(SYNTHETIC_ENV_KEY));
        assert!(!rendered.contains("connection-beta"));
    }

    #[test]
    fn oversized_store_and_entry_count_are_rejected_without_reset() {
        let temp = TempDir::new("limits");
        let store = CredentialStore::new(temp.path());
        store
            .set("connection-alpha", SYNTHETIC_KEY)
            .expect("store initial key");
        let store_path = temp.path().join(STORE_FILE);

        let oversized = vec![0u8; MAX_STORE_BYTES as usize + 1];
        fs::write(&store_path, &oversized).expect("write oversized store");
        let error = store
            .get("connection-alpha")
            .err()
            .expect("oversized encrypted store must be rejected");
        assert_eq!(error.kind(), CredentialErrorKind::StoreTooLarge);
        assert_eq!(
            fs::read(&store_path).expect("read oversized store after rejection"),
            oversized
        );
        assert!(!format!("{error} {error:?}").contains(SYNTHETIC_KEY));

        let too_many = entries(MAX_STORE_ENTRIES + 1);
        write_crafted_store(&store_path, too_many);
        let before = fs::read(&store_path).expect("read too-many-entries store");
        let error = store
            .get("connection-0")
            .err()
            .expect("too many entries must be rejected");
        assert_eq!(error.kind(), CredentialErrorKind::StoreTooLarge);
        let error = store
            .set("connection-overflow", SYNTHETIC_ENV_KEY)
            .expect_err("set must not reset a too-large store");
        assert_eq!(error.kind(), CredentialErrorKind::StoreTooLarge);
        assert_eq!(
            fs::read(&store_path).expect("read too-many-entries store after rejection"),
            before
        );

        write_crafted_store(&store_path, entries(MAX_STORE_ENTRIES));
        let resolved = store
            .get("connection-0")
            .expect("store at the entry limit is readable")
            .expect("first entry exists");
        assert_eq!(resolved.expose_secret(), "synthetic-limit-secret");
        let before = fs::read(&store_path).expect("read at-limit store");
        let error = store
            .set("connection-overflow", SYNTHETIC_ENV_KEY)
            .expect_err("a new entry above the limit must be rejected");
        assert_eq!(error.kind(), CredentialErrorKind::StoreTooLarge);
        assert_eq!(
            fs::read(&store_path).expect("read at-limit store after rejection"),
            before,
            "rejected insertion must leave the store byte-identical"
        );

        store
            .set("connection-0", SYNTHETIC_ENV_KEY)
            .expect("overwriting an existing entry at the limit is allowed");
        assert_eq!(
            store
                .get("connection-0")
                .expect("read overwritten entry")
                .expect("entry exists")
                .expose_secret(),
            SYNTHETIC_ENV_KEY
        );
    }

    fn entries(count: usize) -> StoreData {
        let mut map = BTreeMap::new();
        for index in 0..count {
            map.insert(
                format!("connection-{index}"),
                StoredEntry {
                    secret: "synthetic-limit-secret".to_string(),
                    updated_unix: 1,
                },
            );
        }
        StoreData {
            version: STORE_VERSION,
            entries: map,
        }
    }

    fn write_crafted_store(path: &Path, store: StoreData) {
        let plaintext = serde_json::to_vec(&store).expect("serialize crafted store");
        assert!(plaintext.len() <= MAX_STORE_PLAINTEXT_BYTES);
        let encrypted = protect(&plaintext).expect("encrypt crafted store");
        fs::write(path, encrypted).expect("write crafted store");
    }

    #[test]
    fn environment_name_validation_and_failures() {
        let temp = TempDir::new("environment-invalid");
        let store = CredentialStore::new(temp.path());
        for name in ["", "1BAD", "BAD-NAME", "BAD=NAME", "A B"] {
            let error = store
                .status("connection-gamma", Some(name))
                .expect_err("invalid environment name must fail");
            assert_eq!(error.kind(), CredentialErrorKind::InvalidEnvName);
            let error = store
                .resolve("connection-gamma", Some(name))
                .err()
                .expect("invalid environment name must fail");
            assert_eq!(error.kind(), CredentialErrorKind::InvalidEnvName);
        }
        assert!(
            !store
                .status("connection-gamma", None)
                .expect("no environment name")
                .configured
        );
    }

    #[test]
    fn invalid_connection_ids_and_secrets_are_rejected() {
        let temp = TempDir::new("invalid-input");
        let store = CredentialStore::new(temp.path());
        for connection_id in ["", ".", "..", "../escape", "with space", "with/slash"] {
            let error = store
                .set(connection_id, SYNTHETIC_KEY)
                .expect_err("invalid connection id must fail");
            assert_eq!(error.kind(), CredentialErrorKind::InvalidConnectionId);
        }
        let oversized = "x".repeat(MAX_SECRET_BYTES + 1);
        for secret in ["", oversized.as_str()] {
            let error = store
                .set("connection-delta", secret)
                .expect_err("invalid secret must fail");
            assert_eq!(error.kind(), CredentialErrorKind::InvalidSecret);
        }
    }

    #[test]
    fn corrupt_store_is_preserved_and_never_reset() {
        let temp = TempDir::new("corrupt");
        let store = CredentialStore::new(temp.path());
        store
            .set("connection-epsilon", SYNTHETIC_KEY)
            .expect("store synthetic key");
        let store_path = temp.path().join(STORE_FILE);
        let mut bytes = fs::read(&store_path).expect("read encrypted store");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5a;
        fs::write(&store_path, &bytes).expect("tamper encrypted store");
        let before = fs::read(&store_path).expect("reread changed store");

        let error = store
            .get("connection-epsilon")
            .err()
            .expect("tampered store must not decrypt");
        assert!(matches!(
            error.kind(),
            CredentialErrorKind::Decrypt | CredentialErrorKind::CorruptStore
        ));
        assert_eq!(
            fs::read(&store_path).expect("read after failed get"),
            before
        );
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SYNTHETIC_KEY));

        let error = store
            .set("connection-zeta", SYNTHETIC_ENV_KEY)
            .err()
            .expect("set must not reset unreadable store");
        assert!(matches!(
            error.kind(),
            CredentialErrorKind::Decrypt | CredentialErrorKind::CorruptStore
        ));
        assert_eq!(
            fs::read(&store_path).expect("read after failed set"),
            before
        );
    }

    #[test]
    fn failed_save_preserves_previous_store() {
        let temp = TempDir::new("readonly-save");
        let store = CredentialStore::new(temp.path());
        store
            .set("connection-eta", SYNTHETIC_KEY)
            .expect("store initial key");
        let store_path = temp.path().join(STORE_FILE);
        let before = fs::read(&store_path).expect("read initial store");
        let mut permissions = fs::metadata(&store_path)
            .expect("store metadata")
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&store_path, permissions).expect("mark store read-only");

        let result = store.set("connection-eta", SYNTHETIC_ENV_KEY);
        let mut permissions = fs::metadata(&store_path)
            .expect("store metadata after failed save")
            .permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&store_path, permissions).expect("clear read-only");

        let error = result.expect_err("read-only store must fail the save");
        assert_eq!(error.kind(), CredentialErrorKind::Io);
        assert_eq!(
            fs::read(&store_path).expect("read after failed save"),
            before
        );
        let resolved = store
            .get("connection-eta")
            .expect("old store remains readable")
            .expect("old key remains");
        assert_eq!(resolved.expose_secret(), SYNTHETIC_KEY);
    }

    #[test]
    fn concurrent_updates_do_not_lose_keys() {
        let temp = TempDir::new("concurrent");
        let root = Arc::new(temp.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(4));
        let mut workers = Vec::new();
        for worker in 0..4 {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let store = CredentialStore::new(root.as_path());
                barrier.wait();
                for index in 0..4 {
                    let id = format!("connection-{worker}-{index}");
                    let value = format!("synthetic-concurrent-key-{worker}-{index}");
                    store.set(&id, &value).expect("concurrent set");
                }
            }));
        }
        for worker in workers {
            worker.join().expect("join concurrent worker");
        }

        let store = CredentialStore::new(temp.path());
        for worker in 0..4 {
            for index in 0..4 {
                let id = format!("connection-{worker}-{index}");
                let expected = format!("synthetic-concurrent-key-{worker}-{index}");
                let resolved = store
                    .get(&id)
                    .expect("read concurrent key")
                    .expect("concurrent key exists");
                assert_eq!(resolved.expose_secret(), expected);
            }
        }
    }

    #[test]
    fn status_and_errors_do_not_expose_secrets() {
        let temp = TempDir::new("safe-status");
        let store = CredentialStore::new(temp.path());
        let status = store
            .set("connection-theta", SYNTHETIC_KEY)
            .expect("store synthetic key");
        let rendered = format!("{status:?}");
        assert!(!rendered.contains(SYNTHETIC_KEY));

        let error = store
            .set("connection-theta", "")
            .expect_err("empty secret rejected");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SYNTHETIC_KEY));
        assert!(!rendered.contains("connection-theta"));
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
