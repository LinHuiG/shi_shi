//! Self-contained staging crate for the OpenAI Chat Completions analysis
//! provider and its local structured-output validator.
//!
//! The HTTP module extends the finalized WinHTTP transport with incremental
//! body callbacks while retaining its deadline, cancellation and HTTPS rules. No credentials, provider
//! storage, engine state, or UI code is duplicated here: callers resolve a
//! borrowed credential and pass it to the client for one analysis run.
//!
//! Production endpoints are HTTPS-only. A plaintext loopback helper exists only
//! under `cfg(test)` for deterministic local integration tests; there is no
//! production insecure switch.

pub mod endpoint;
pub mod http_client;
pub mod openai;
pub mod schema_validate;
pub mod streaming;
pub mod transport;
pub use streaming::ApiEvent;

pub use endpoint::{Endpoint, EndpointError, EndpointErrorKind};
pub use openai::{
    tool_definitions_from_mcp, AnalysisOutput, AnalyzeRequest, ApiError, ApiErrorKind,
    ChatCompletionsClient, FnToolHandler, ModelInfo, ToolContent, ToolDefinition, ToolFailure,
    ToolFailureKind, ToolHandler, ToolInvocation, ToolResult, UsageAggregate, USER_INSTRUCTION,
};
pub use schema_validate::{check_schema, validate as validate_schema, ValidationError};
pub use transport::{
    HttpMethod, SystemTransport, Transport, TransportError, TransportErrorKind, TransportRequest,
    TransportResponse,
};
