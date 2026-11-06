//! DSH over ACP: the stdio JSON-RPC client, the restricted analysis profile
//! and its readiness check, structured submissions, and process-tree
//! ownership. Profiles live in caller-owned directories; the user's DSH home
//! and credentials are never changed (except the explicitly installed bridge).

pub mod client;
pub mod dsh;
pub mod process;
pub mod submit;
pub mod workspace;

pub use client::{
    AcpClient, AcpEvent, AcpLaunch, ClientError, McpServer, PendingPermission, PermissionPolicy, PromptResult,
    SessionInfo, SessionUpdate, TokenUsage,
};
pub use dsh::{check_readiness, materialize_profile, MaterializedProfile, Readiness, ReadinessError};

#[cfg(test)]
pub(crate) fn test_path(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("shishi-acp-{label}-{}-{nanos}", std::process::id()))
}
