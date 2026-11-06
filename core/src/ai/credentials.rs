//! Credential encryption and environment-variable resolution. Stored secrets live in the `credentials` table, written
//! and read only by [`crate::data::credentials`]; each entry is encrypted
//! on its own with DPAPI (current user, fixed entropy, UI forbidden). A
//! connection with `key_env` reads only that environment variable. Secrets
//! never appear in `Debug`, errors or status; [`ResolvedCredential::expose_secret`]
//! is the only accessor. Other platforms cannot protect a secret at rest and
//! refuse to store one (tests use a reversible marker).

#[cfg(windows)]
use std::ffi::c_void;
use std::fmt;
#[cfg(windows)]
use std::ptr::{null, null_mut};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
#[cfg(windows)]
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

const MAX_CONNECTION_ID_BYTES: usize = 128;
const MAX_SECRET_BYTES: usize = 4096;
const MAX_ENV_NAME_BYTES: usize = 64;
/// The entropy of 1.1.68's store, kept so its file can still be decrypted.
#[cfg_attr(not(windows), allow(dead_code))]
const DPAPI_ENTROPY: &[u8] = b"ShiShi/credential-store/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Dpapi,
    Environment,
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
}

impl ResolvedCredential {
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialErrorKind {
    InvalidConnectionId,
    InvalidSecret,
    InvalidEnvName,
    EnvironmentNotUnicode,
    Encrypt,
    Decrypt,
    CorruptEntry,
}

impl CredentialErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConnectionId => "invalid connection id",
            Self::InvalidSecret => "invalid secret",
            Self::InvalidEnvName => "invalid environment variable name",
            Self::EnvironmentNotUnicode => "environment value is not Unicode",
            Self::Encrypt => "DPAPI encryption failed",
            Self::Decrypt => "DPAPI decryption failed",
            Self::CorruptEntry => "stored credential is corrupt",
        }
    }
}

/// Safe credential error. It never contains a connection id, environment
/// value, or secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialError {
    kind: CredentialErrorKind,
    os_code: Option<u32>,
}

impl CredentialError {
    fn new(kind: CredentialErrorKind) -> Self {
        Self { kind, os_code: None }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn dpapi(kind: CredentialErrorKind, code: u32) -> Self {
        Self { kind, os_code: (code != 0).then_some(code) }
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.os_code {
            Some(code) => write!(f, "credential error: {} (code {})", self.kind.as_str(), code),
            None => write!(f, "credential error: {}", self.kind.as_str()),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<CredentialError> for crate::Error {
    fn from(e: CredentialError) -> Self {
        crate::Error(e.to_string())
    }
}

/// Check and encrypt `secret` for connection `connection_id`.
pub(crate) fn seal(connection_id: &str, secret: &str) -> Result<Vec<u8>, CredentialError> {
    validate_connection_id(connection_id)?;
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(CredentialError::new(CredentialErrorKind::InvalidSecret));
    }
    protect(secret.as_bytes())
}

/// Decrypt one stored entry. A bad entry fails only its own connection.
pub(crate) fn open(sealed: &[u8]) -> Result<ResolvedCredential, CredentialError> {
    let plaintext = unprotect_bytes(sealed)?;
    let secret = String::from_utf8(plaintext).map_err(|_| CredentialError::new(CredentialErrorKind::CorruptEntry))?;
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(CredentialError::new(CredentialErrorKind::CorruptEntry));
    }
    Ok(ResolvedCredential { secret })
}

/// Decrypt bytes protected by 1.1.68's credential store (the install
/// migration reads `credentials.bin` with it).
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    unprotect_bytes(ciphertext).map_err(|e| e.to_string())
}

pub(crate) fn validate_connection_id(connection_id: &str) -> Result<(), CredentialError> {
    if connection_id.is_empty()
        || connection_id.len() > MAX_CONNECTION_ID_BYTES
        || connection_id == "."
        || connection_id == ".."
        || !connection_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_')
    {
        return Err(CredentialError::new(CredentialErrorKind::InvalidConnectionId));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), CredentialError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_ENV_NAME_BYTES
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes[1..].iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(CredentialError::new(CredentialErrorKind::InvalidEnvName));
    }
    Ok(())
}

/// The secret in environment variable `name`; a missing or empty variable
/// is `None`. The stored entry is never consulted.
pub fn environment(name: &str) -> Result<Option<ResolvedCredential>, CredentialError> {
    validate_env_name(name)?;
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => {
            let secret =
                value.into_string().map_err(|_| CredentialError::new(CredentialErrorKind::EnvironmentNotUnicode))?;
            if secret.len() > MAX_SECRET_BYTES {
                return Err(CredentialError::new(CredentialErrorKind::InvalidSecret));
            }
            Ok(Some(ResolvedCredential { secret }))
        }
    }
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>, CredentialError> {
    let kind = if encrypt { CredentialErrorKind::Encrypt } else { CredentialErrorKind::Decrypt };
    let len = u32::try_from(input.len()).map_err(|_| CredentialError::new(kind))?;
    if len == 0 {
        return Err(CredentialError::new(kind));
    }
    let input = CRYPT_INTEGER_BLOB { cbData: len, pbData: input.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: DPAPI_ENTROPY.len() as u32, pbData: DPAPI_ENTROPY.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(&input, null(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, null_mut(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
    };
    if ok == 0 {
        return Err(CredentialError::dpapi(kind, unsafe { GetLastError() }));
    }
    let result = if output.pbData.is_null() || output.cbData == 0 {
        Err(CredentialError::new(kind))
    } else {
        Ok(unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() })
    };
    if !output.pbData.is_null() {
        unsafe {
            LocalFree(output.pbData as *mut c_void);
        }
    }
    result
}

#[cfg(windows)]
pub(crate) fn protect(plaintext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    dpapi(plaintext, true)
}

#[cfg(windows)]
fn unprotect_bytes(ciphertext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    dpapi(ciphertext, false)
}

// Only Windows protects secrets at rest. Development hosts refuse to store
// a secret; tests use a reversible marker that is never a release build.
#[cfg(all(not(windows), test))]
pub(crate) fn protect(plaintext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    Ok([b"TEST:".as_slice(), &plaintext.iter().map(|b| b ^ 0x5a).collect::<Vec<_>>()].concat())
}

#[cfg(all(not(windows), test))]
fn unprotect_bytes(ciphertext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    ciphertext
        .strip_prefix(b"TEST:")
        .map(|b| b.iter().map(|b| b ^ 0x5a).collect())
        .ok_or(CredentialError::new(CredentialErrorKind::Decrypt))
}

#[cfg(all(not(windows), not(test)))]
fn protect(_: &[u8]) -> Result<Vec<u8>, CredentialError> {
    Err(CredentialError::new(CredentialErrorKind::Encrypt))
}

#[cfg(all(not(windows), not(test)))]
fn unprotect_bytes(_: &[u8]) -> Result<Vec<u8>, CredentialError> {
    Err(CredentialError::new(CredentialErrorKind::Decrypt))
}
