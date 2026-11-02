//! ACP transport, sessions and structured submissions for the DSH adapter.
//! Protocol types remain independent of ShiShi application types. Profiles use
//! caller-owned temporary files without changing source configuration or credentials.

pub mod analysis;
pub mod client;
pub mod jsonrpc;
pub mod process;
pub mod providers;
pub mod restricted;
pub mod schema_validate;
pub mod sha256;
pub mod submit;
pub mod transport;
pub mod workspace;

pub use client::{
    AcpClient, AcpEvent, AcpLaunch, ClientError, McpServer, PendingPermission, PermissionPolicy,
    PromptResult, SessionInfo, SessionUpdate, TokenUsage,
};
pub use restricted::{
    check_readiness, materialize_profile, DshLaunch, MaterializedProfile, Readiness,
    ReadinessError, RestrictedError,
};
pub use transport::{TransportConfig, TransportError};

#[cfg(test)]
pub(crate) fn test_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "shishi-acp-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
