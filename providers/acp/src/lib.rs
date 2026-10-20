//! Staged, standalone primitives for the DeepSeek Harness ACP analysis adapter.
//!
//! This crate is intentionally free of ShiShi application types. Integration
//! copies these modules into the main crate after review and maps the pure
//! DTOs to `ai.rs`/`analysis_context.rs`/`handling.rs`. It never invokes a
//! model itself. Provider inheritance reads only caller-selected profile configuration,
//! keeps it in caller-owned temporary files and does not modify the source or credential store.

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
    AcpClient, AcpEvent, AcpLaunch, ClientError, ContextUsage, McpServer, PendingPermission,
    PermissionPolicy, PromptResult, ServerCapabilities, SessionInfo, SessionUpdate, TokenUsage,
};
pub use restricted::{
    check_readiness, materialize_profile, DshLaunch, MaterializedProfile, Readiness,
    ReadinessError, RestrictedError,
};
pub use transport::{TransportConfig, TransportError};
