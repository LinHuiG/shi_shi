//! OpenAI-compatible analysis client: Chat Completions over an API key, or
//! the Responses backend of a ChatGPT sign-in, with the shared SS tool
//! loop, SSE assembly and the JSON Schema subset ShiShi validates against.
//! Callers resolve credentials; nothing here reads storage or logs bodies.

pub mod http_client;
pub mod json;
pub mod openai;
pub mod schema_validate;
pub mod streaming;

pub use openai::{
    tool_definitions_from_mcp, AnalysisOutput, AnalyzeRequest, ApiError, ApiErrorKind, Client, Endpoint, ModelInfo,
    ToolContent, ToolDefinition, ToolFailure, ToolFailureKind, ToolHandler, ToolInvocation, ToolResult, UsageAggregate,
    Wire,
};
pub use schema_validate::ValidationError;
pub use streaming::ApiEvent;
