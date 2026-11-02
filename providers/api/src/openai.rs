//! OpenAI Chat Completions-compatible analysis client.
//!
//! Analysis only. Integration must reject `handling`/`handling_learning` and
//! any agent/session purpose for this provider with an explicit unsupported
//! error; this module intentionally exposes no agent or execution API.
//!
//! The caller supplies the exact prepared prompt, model, JSON schema, borrowed
//! credential, cancellation flag, overall timeout, tool definitions, and a
//! caller-owned tool handler. This module never reads a credential store, never
//! writes prompts or keys to disk, and never logs provider bodies.
//!
//! The system message is the exact prompt string. A fixed short user message
//! follows it; no timestamp or runtime text is prepended. If a provider reports
//! that `response_format` is explicitly unsupported, the request is retried once
//! without it and the schema is placed in the user message so strict local
//! validation still has a contract. A received output that fails local JSON or
//! schema validation gets one bounded correction. Recoverable tool rejections
//! share one separate correction without executing an invalid batch.
//! Other failures are never retried.
//!
//! The tool loop supports any caller-advertised tools (including
//! `read_context`/`read_image`), validates names and arguments against those
//! definitions, and calls only the caller's handler. It stops on the overall
//! deadline or cancellation. Tool exhaustion or lack of progress closes tools
//! for one final answer using the evidence already received. Initial input and
//! shared continuation text have separate caller-supplied allowances.

use crate::endpoint::{Endpoint, EndpointError};
pub use crate::json::canonical_json;
use crate::json::chars as json_chars;
use crate::schema_validate::{CheckedSchema, ValidationError};
use crate::streaming::{ApiEvent, StreamCollector};
use crate::transport::{
    HttpMethod, SystemTransport, Transport, TransportError, TransportErrorKind, TransportRequest,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const USER_INSTRUCTION: &str =
    "Return only the JSON object required by the system message. Do not include prose or Markdown.";

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MIN_TIMEOUT: Duration = Duration::from_millis(1);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(1800);
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MODELS_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_MODELS: usize = 500;
pub const MAX_TOOLS: usize = 32;
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 16;
/// A bounded oversized reply can still receive a refusal for every call.
pub const MAX_TOOL_CALLS_IN_RESPONSE: usize = 128;
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CREDENTIAL_BYTES: usize = 4096;
pub const MAX_MODEL_CHARS: usize = 256;
pub const MAX_TOOL_NAME_CHARS: usize = 64;
pub const MAX_TOOL_DESCRIPTION_CHARS: usize = 1024;

const RESPONSE_FORMAT_NAME: &str = "shishi_analysis";
const MAX_CORRECTION_PATH_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorKind {
    InvalidConfig,
    InvalidEndpoint,
    MissingCredential,
    Cancelled,
    Timeout,
    Transport,
    HttpStatus,
    ResponseTooLarge,
    InputLimit,
    EmptyResponse,
    Refusal,
    Truncated,
    MalformedResponse,
    InvalidJson,
    SchemaInvalid,
    UnsupportedResponseFormat,
    ToolDefinitionInvalid,
    UnknownTool,
    InvalidToolCall,
    InvalidToolArguments,
    ToolFailed,
    NoProgress,
    EmptyModels,
    ModelsTooMany,
}

/// Safe analysis error. It stores only a kind, an optional HTTP status, a
/// schema/constraint error, and safe transport/endpoint classes. Provider body
/// text, URLs, headers, and credentials are never retained or formatted.
#[derive(Clone, Debug)]
pub struct ApiError {
    usage: Option<UsageAggregate>,
    partial_usage: Option<Value>,
    parameter: Option<&'static str>,
    kind: ApiErrorKind,
    status: Option<u16>,
    schema: Option<ValidationError>,
    endpoint: Option<EndpointError>,
    transport: Option<TransportErrorKind>,
}

impl ApiError {
    pub fn simple(kind: ApiErrorKind) -> Self {
        Self {
            usage: None,
            partial_usage: None,
            parameter: None,
            kind,
            status: None,
            schema: None,
            endpoint: None,
            transport: None,
        }
    }

    fn from_endpoint(error: EndpointError) -> Self {
        Self {
            kind: ApiErrorKind::InvalidEndpoint,
            endpoint: Some(error),
            ..Self::simple(ApiErrorKind::InvalidEndpoint)
        }
    }

    fn from_transport(error: TransportError) -> Self {
        let kind = match error.kind() {
            TransportErrorKind::Cancelled => ApiErrorKind::Cancelled,
            TransportErrorKind::Timeout => ApiErrorKind::Timeout,
            TransportErrorKind::ResponseTooLarge => ApiErrorKind::ResponseTooLarge,
            TransportErrorKind::InvalidRequest => ApiErrorKind::InvalidConfig,
            TransportErrorKind::Network
            | TransportErrorKind::Tls
            | TransportErrorKind::Protocol => ApiErrorKind::Transport,
        };
        Self {
            kind,
            transport: Some(error.kind()),
            ..Self::simple(kind)
        }
    }

    fn with_schema(kind: ApiErrorKind, error: ValidationError) -> Self {
        Self {
            schema: Some(error),
            ..Self::simple(kind)
        }
    }

    fn http_status(status: u16) -> Self {
        Self {
            kind: ApiErrorKind::HttpStatus,
            status: Some(status),
            ..Self::simple(ApiErrorKind::HttpStatus)
        }
    }

    pub fn kind(&self) -> ApiErrorKind {
        self.kind
    }
    pub fn usage(&self) -> Option<UsageAggregate> {
        self.usage
    }
    pub fn parameter(&self) -> Option<&'static str> {
        self.parameter
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ApiErrorKind::InvalidConfig => match &self.schema {
                Some(error) => write!(f, "invalid analysis configuration: {error}"),
                None => f.write_str("invalid analysis configuration"),
            },
            ApiErrorKind::InvalidEndpoint => match self.endpoint {
                Some(error) => write!(f, "invalid API endpoint: {error}"),
                None => f.write_str("invalid API endpoint"),
            },
            ApiErrorKind::MissingCredential => f.write_str("API credential is not configured"),
            ApiErrorKind::Cancelled => f.write_str("analysis cancelled"),
            ApiErrorKind::Timeout => f.write_str("analysis deadline exceeded"),
            ApiErrorKind::Transport => match self.transport {
                Some(kind) => write!(f, "HTTP transport error: {}", kind.as_str()),
                None => f.write_str("HTTP transport error"),
            },
            ApiErrorKind::HttpStatus => match self.status {
                Some(status) => write!(f, "provider returned HTTP {status}"),
                None => f.write_str("provider returned an HTTP error"),
            },
            ApiErrorKind::ResponseTooLarge => {
                f.write_str("provider response exceeded the size limit")
            }
            ApiErrorKind::InputLimit => f.write_str("initial input exceeded the character budget"),
            ApiErrorKind::EmptyResponse => f.write_str("provider returned no final content"),
            ApiErrorKind::Refusal => f.write_str("provider refused or filtered the response"),
            ApiErrorKind::Truncated => f.write_str("provider output was truncated"),
            ApiErrorKind::MalformedResponse => f.write_str("provider response was malformed"),
            ApiErrorKind::InvalidJson => f.write_str("provider final content was not valid JSON"),
            ApiErrorKind::SchemaInvalid => match &self.schema {
                Some(error) => write!(f, "provider final JSON did not match the schema: {error}"),
                None => f.write_str("provider final JSON did not match the schema"),
            },
            ApiErrorKind::UnsupportedResponseFormat => {
                f.write_str("provider does not support response_format")
            }
            ApiErrorKind::ToolDefinitionInvalid => match &self.schema {
                Some(error) => write!(f, "invalid tool definition schema: {error}"),
                None => f.write_str("invalid tool definition"),
            },
            ApiErrorKind::UnknownTool => {
                f.write_str("provider requested a tool that was not advertised")
            }
            ApiErrorKind::InvalidToolCall => f.write_str("provider returned an invalid tool call"),
            ApiErrorKind::InvalidToolArguments => match &self.schema {
                Some(error) => write!(
                    f,
                    "provider tool arguments did not match the tool schema: {error}"
                ),
                None => f.write_str("provider tool arguments were invalid"),
            },
            ApiErrorKind::ToolFailed => f.write_str("tool execution failed"),
            ApiErrorKind::NoProgress => {
                f.write_str("repeated tool query made no progress; stopped the tool loop")
            }
            ApiErrorKind::EmptyModels => f.write_str("provider returned an empty model list"),
            ApiErrorKind::ModelsTooMany => f.write_str("provider returned too many models"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageAggregate {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// Successful provider responses in this analysis run.
    pub responses: usize,
    /// Responses that carried an object `usage` member.
    pub reported_responses: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Usage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cached_tokens: Option<u64>,
}

crate::default_struct! {
    #[derive(Clone, Copy, Debug)]
    struct FieldSum {
        seen: bool,
        complete: bool = true,
        sum: u64,
    }
}

impl FieldSum {
    fn add(&mut self, value: Option<u64>) {
        self.seen = true;
        match value {
            Some(value) if self.complete => {
                self.sum = self.sum.saturating_add(value);
            }
            Some(_) => {}
            None => self.complete = false,
        }
    }

    fn finish(self) -> Option<u64> {
        if self.seen && self.complete {
            Some(self.sum)
        } else {
            None
        }
    }
}

#[derive(Clone, Default)]
struct UsageAggregator {
    prompt: FieldSum,
    completion: FieldSum,
    total: FieldSum,
    cached: FieldSum,
    responses: usize,
    reported: usize,
}

impl UsageAggregator {
    /// A missing or malformed usage object marks the corresponding aggregate
    /// fields unknown; it never contributes zero.
    fn record(&mut self, usage: Option<&Value>) {
        self.responses += 1;
        let parsed = usage.and_then(parse_usage);
        if let Some(usage) = parsed {
            self.reported += 1;
            self.prompt.add(usage.prompt_tokens);
            self.completion.add(usage.completion_tokens);
            self.total.add(usage.total_tokens);
            self.cached.add(usage.cached_tokens);
        } else {
            self.prompt.add(None);
            self.completion.add(None);
            self.total.add(None);
            self.cached.add(None);
        }
    }

    fn finish(self) -> UsageAggregate {
        let prompt = self.prompt.finish();
        let cached = if prompt.is_some() {
            self.cached.finish().map(|value| {
                let input = prompt.unwrap_or(0);
                value.min(input)
            })
        } else {
            None
        };
        UsageAggregate {
            prompt_tokens: prompt,
            completion_tokens: self.completion.finish(),
            total_tokens: self.total.finish(),
            cached_tokens: cached,
            responses: self.responses,
            reported_responses: self.reported,
        }
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let prompt_tokens = value.get("prompt_tokens").and_then(Value::as_u64);
    let completion_tokens = value.get("completion_tokens").and_then(Value::as_u64);
    let total_tokens = value.get("total_tokens").and_then(Value::as_u64);
    let cached = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens_details"]["cached_tokens"].as_u64())
        .or_else(|| value.get("cached_input_tokens").and_then(Value::as_u64));
    let cached_tokens = match (cached, prompt_tokens) {
        (Some(cached), Some(prompt)) => Some(cached.min(prompt)),
        // Without a known input count the cached <= input invariant cannot be
        // claimed, so keep the field unknown rather than inventing a bound.
        (Some(_), None) => None,
        (None, _) => None,
    };
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Map one MCP-style tool (`{name, description, inputSchema}`) to a Chat
    /// Completions function tool. The caller still decides which tools, if any,
    /// to advertise.
    pub fn from_mcp_tool(value: &Value) -> Result<Self, ApiError> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ApiError::simple(ApiErrorKind::ToolDefinitionInvalid))?;
        let description = match value.get("description") {
            None => "",
            Some(Value::String(text)) => text,
            Some(_) => return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid)),
        };
        let parameters = value
            .get("inputSchema")
            .or_else(|| value.get("parameters"))
            .cloned()
            .ok_or_else(|| ApiError::simple(ApiErrorKind::ToolDefinitionInvalid))?;
        if !parameters.is_object() {
            return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
        }
        Ok(Self::new(name, description, parameters))
    }
}

/// Map `analysis_context::tools()` output (`{"tools":[...]}`) or a bare tool
/// array into validated Chat Completions tool definitions.
pub fn tool_definitions_from_mcp(value: &Value) -> Result<Vec<ToolDefinition>, ApiError> {
    let tools = if let Some(array) = value.as_array() {
        array
    } else {
        value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::ToolDefinitionInvalid))?
    };
    if tools.len() > MAX_TOOLS {
        return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
    }
    let definitions = tools
        .iter()
        .map(ToolDefinition::from_mcp_tool)
        .collect::<Result<Vec<_>, _>>()?;
    validate_tool_definitions(&definitions)?;
    Ok(definitions)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolFailureKind {
    Unavailable,
    Denied,
    InvalidArguments,
    BudgetExhausted,
    UnsafeReplay,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ToolFailure {
    kind: ToolFailureKind,
    message: Option<String>,
}

impl ToolFailure {
    pub fn new(kind: ToolFailureKind) -> Self {
        Self {
            kind,
            message: None,
        }
    }

    /// A host-authored diagnostic for the model, never included in safe errors.
    pub fn with_message(kind: ToolFailureKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: Some(message.as_ref().chars().take(512).collect()),
        }
    }

    pub fn kind(&self) -> ToolFailureKind {
        self.kind
    }

    fn feedback(&self) -> &str {
        self.message.as_deref().unwrap_or(match self.kind {
            ToolFailureKind::Unavailable => "This tool is currently unavailable. The operation was not executed.",
            ToolFailureKind::Denied => "This tool request was denied. The operation was not executed.",
            ToolFailureKind::InvalidArguments => "The arguments could not be used safely. The operation was not executed. Correct them against the advertised tool schema.",
            ToolFailureKind::BudgetExhausted => "The shared continuation budget is exhausted. The operation was not executed.",
            ToolFailureKind::UnsafeReplay => "A prior execution of this operation has an uncertain outcome. Do not repeat it; finish using the evidence already received.",
            ToolFailureKind::Failed => "The tool failed and its result cannot be safely accepted.",
        })
    }
}

impl fmt::Debug for ToolFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolFailure")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub struct ToolInvocation<'a> {
    call_id: &'a str,
    name: &'a str,
    arguments: &'a Value,
}

impl<'a> ToolInvocation<'a> {
    pub fn call_id(&self) -> &'a str {
        self.call_id
    }

    pub fn name(&self) -> &'a str {
        self.name
    }

    pub fn arguments(&self) -> &'a Value {
        self.arguments
    }
}

impl fmt::Debug for ToolInvocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Arguments can contain caller context; only the safe correlation fields
        // and the advertised tool name are rendered.
        f.debug_struct("ToolInvocation")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("arguments", &"<redacted>")
            .finish()
    }
}

/// The only execution path available to provider tool calls. The client passes
/// the advertised name, parsed arguments, and tool-call id; the handler owns the
/// frozen caller context and must not consult live files or the network based on
/// provider-supplied paths.
pub trait ToolHandler {
    fn invoke(&mut self, invocation: &ToolInvocation<'_>) -> Result<ToolResult, ToolFailure>;
    /// Cross-field business constraints share the existing single correction.
    /// May normalize an accepted value, but must not commit state or execute tools.
    fn validate_output(
        &mut self,
        _value: &mut Value,
        _corrected: bool,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    /// A handler may opt individual actions into repeated arguments under
    /// distinct IDs; IDs never replay. Metered queries use query_progress.
    fn allows_repeated_arguments(&self, _name: &str) -> bool {
        false
    }
    fn input_char_limit(&self) -> Option<usize> {
        None
    }
    /// Shared allowance for model continuations and all delivered tool results.
    /// Host-authored bounded refusals and correction prompts are not charged.
    fn continuation_char_limit(&self) -> Option<usize> {
        None
    }
    /// The revision changes only when a query delivers new evidence. It permits
    /// repeated arguments for pagination without permitting a no-progress loop.
    fn query_progress(&self, _name: &str) -> Option<usize> {
        None
    }
    fn input_chars(&self, body: &Value, serialized_chars: usize) -> usize {
        serialized_chars.saturating_sub(
            body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .map(image_data_chars)
                .sum(),
        )
    }
    fn validate_batch(&mut self, _calls: &[ToolInvocation<'_>]) -> Result<(), ToolFailure> {
        Ok(())
    }
    fn invoke_guarded(
        &mut self,
        invocation: &ToolInvocation<'_>,
        _cancel: &AtomicBool,
        _deadline: Instant,
        _result_budget: Option<usize>,
    ) -> Result<ToolResult, ToolFailure> {
        self.invoke(invocation)
    }
}

fn image_data_chars(message: &Value) -> usize {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .filter_map(|part| part["image_url"]["url"].as_str())
        .filter_map(|url| url.strip_prefix("data:")?.split_once(";base64,"))
        .map(|(_, data)| data.len())
        .sum()
}

/// JSON message framing counts; native image pixels have their own limit.
pub fn message_input_chars(message: &Value) -> usize {
    json_chars(message).saturating_sub(image_data_chars(message))
}

fn empty_tool_message(call_id: &str) -> Value {
    json!({"role":"tool","tool_call_id":call_id,"content":""})
}

fn retain_continuation(
    messages: &mut Vec<Value>,
    message: &Value,
    used: &mut usize,
    limit: Option<usize>,
) -> bool {
    let chars = message_input_chars(message) + 1;
    if limit.is_some_and(|limit| chars > limit.saturating_sub(*used)) {
        return false;
    }
    *used = used.saturating_add(chars);
    messages.push(message.clone());
    true
}

fn tool_feedback(call_id: &str, reason: &str) -> Value {
    json!({"role":"tool","tool_call_id":call_id,"content":reason})
}

fn finish_without_tools(messages: &mut Vec<Value>, reason: &str) {
    messages.push(json!({"role":"user","content":format!("{reason} No further tools are available for this analysis. Return the final JSON required by the schema using the information already provided. State any missing evidence honestly; do not invent results or repeat operations.")}));
}

/// Images are explicit local tool output, never provider-supplied URLs. Keep
/// their bytes out of Debug and ordinary text messages.
#[derive(Clone)]
pub enum ToolResult {
    Text(String),
    Content(Vec<ToolContent>),
}

#[derive(Clone)]
pub enum ToolContent {
    Text(String),
    Image { mime_type: String, data: String },
}

impl From<String> for ToolResult {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl ToolResult {
    pub fn from_mcp(value: &Value) -> Result<Self, ToolFailure> {
        let fail = || ToolFailure::new(ToolFailureKind::Failed);
        let Some(parts) = value.get("content").and_then(Value::as_array) else {
            return Ok(Self::Text(value.to_string()));
        };
        if parts.len() > 16 {
            return Err(fail());
        }
        let mut result = Vec::new();
        for part in parts {
            result.push(match part["type"].as_str() {
                Some("text") => ToolContent::Text(part["text"].as_str().ok_or_else(fail)?.into()),
                Some("image") => ToolContent::Image {
                    mime_type: part["mimeType"].as_str().ok_or_else(fail)?.into(),
                    data: part["data"].as_str().ok_or_else(fail)?.into(),
                },
                _ => return Err(fail()),
            });
        }
        Ok(Self::Content(result))
    }

    /// Exact protocol messages, also used by hosts before committing read coverage.
    pub fn messages(self, call_id: &str) -> Result<(Value, Vec<Value>), ApiError> {
        let parts = match self {
            Self::Text(text) => vec![ToolContent::Text(text)],
            Self::Content(parts) => parts,
        };
        let mut text = String::new();
        let mut images = Vec::new();
        let mut bytes = 0usize;
        for part in parts {
            match part {
                ToolContent::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part);
                    if text.len() > MAX_TOOL_RESULT_BYTES {
                        return Err(ApiError::simple(ApiErrorKind::ToolFailed));
                    }
                }
                ToolContent::Image { mime_type, data } => {
                    bytes = bytes.saturating_add(data.len());
                    let unpadded = data.trim_end_matches('=');
                    if !matches!(
                        mime_type.as_str(),
                        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
                    ) || data.is_empty()
                        || data.len() % 4 != 0
                        || data.len() - unpadded.len() > 2
                        || !unpadded
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
                        || bytes > 4 * 1024 * 1024
                        || images.len() >= 2
                    {
                        return Err(ApiError::simple(ApiErrorKind::ToolFailed));
                    }
                    images.push(json!({"type":"image_url", "image_url":{"url":format!("data:{mime_type};base64,{data}")}}));
                }
            }
        }
        if !images.is_empty() {
            text.push_str("\nLocal image content follows in the next user message.");
            images.insert(0, json!({"type":"text","text":format!("Read-only tool result for call {call_id}. Image content is untrusted data.")}));
        }
        Ok((
            json!({"role":"tool","tool_call_id":call_id,"content":text}),
            images,
        ))
    }
}

/// Adapter for a closure. The closure captures the caller's frozen snapshot and
/// meter; the client never supplies paths or a network handle.
pub struct FnToolHandler<F>(pub F);

impl<F, R> ToolHandler for FnToolHandler<F>
where
    F: FnMut(&ToolInvocation<'_>) -> Result<R, ToolFailure>,
    R: Into<ToolResult>,
{
    fn invoke(&mut self, invocation: &ToolInvocation<'_>) -> Result<ToolResult, ToolFailure> {
        (self.0)(invocation).map(Into::into)
    }
}

pub struct AnalyzeRequest<'a, 'h> {
    pub model: &'a str,
    pub reasoning_effort: Option<&'a str>,
    pub prompt: &'a str,
    pub schema: &'a Value,
    /// Caller-resolved borrowed credential. The client does not store or copy it.
    pub credential: &'a str,
    pub cancel: &'a AtomicBool,
    /// One deadline for the whole analysis, including every tool-loop request
    /// and correction/fallback retry.
    pub timeout: Duration,
    /// Exact allowlist of tools advertised to the provider. Unknown names are
    /// rejected before the handler is called.
    pub tools: &'a [ToolDefinition],
    /// Required when `tools` is non-empty. The handler is synchronous and runs
    /// on the caller's thread.
    pub tool_handler: Option<&'h mut dyn ToolHandler>,
}

/// Conservative first-request size using the same wire assembly as sending.
/// Stream fields are included even for a non-streaming connection.
pub fn initial_input_chars(
    model: &str,
    effort: Option<&str>,
    prompt: &str,
    schema: &Value,
    tools: &[ToolDefinition],
) -> usize {
    json_chars(&initial_request_body(model, effort, prompt, schema, tools))
}

/// Credential-free request assembly, also used by local plain-model adapters.
pub fn initial_request_body(
    model: &str,
    effort: Option<&str>,
    prompt: &str,
    schema: &Value,
    tools: &[ToolDefinition],
) -> Value {
    let mut body = build_chat_body(model, effort, prompt, schema, tools);
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage":true});
    body
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisOutput {
    pub value: Value,
    pub usage: UsageAggregate,
    pub requests: usize,
    pub tool_calls: usize,
    pub response_format_fallback: bool,
    pub correction_used: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub supported_reasoning_efforts: Option<Vec<String>>,
}

pub struct ChatCompletionsClient<'a> {
    endpoint: Endpoint,
    transport: &'a dyn Transport,
    max_response_bytes: usize,
}

static SYSTEM_TRANSPORT: SystemTransport = SystemTransport;

impl<'a> ChatCompletionsClient<'a> {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            transport: &SYSTEM_TRANSPORT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }

    pub fn from_base_url(base_url: &str) -> Result<Self, ApiError> {
        let endpoint = Endpoint::parse(base_url).map_err(ApiError::from_endpoint)?;
        Ok(Self::new(endpoint))
    }

    /// Test/integration seam. Production code should use [`Self::new`], which
    /// always goes through the HTTPS-only system transport.
    #[doc(hidden)]
    pub fn with_transport(endpoint: Endpoint, transport: &'a dyn Transport) -> Self {
        Self {
            endpoint,
            transport,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn analyze(&self, request: AnalyzeRequest<'_, '_>) -> Result<AnalysisOutput, ApiError> {
        self.analyze_with_events(request, false, &mut |_| {})
    }
    pub fn analyze_with_events(
        &self,
        request: AnalyzeRequest<'_, '_>,
        stream: bool,
        event: &mut dyn FnMut(ApiEvent),
    ) -> Result<AnalysisOutput, ApiError> {
        validate_credential(request.credential)?;
        let mut model = HttpModel {
            client: self,
            credential: request.credential,
            stream,
        };
        analyze_with_model(request, &mut model, event)
    }

    /// Discovery-only `/models` GET. It never issues a completion or health
    /// model call. An empty or malformed list is an error; callers keep their
    /// manual model choice on failure.
    pub fn discover_models(
        &self,
        credential: &str,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<Vec<ModelInfo>, ApiError> {
        validate_timeout(timeout)?;
        validate_credential(credential)?;
        check_cancel(cancel)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::Timeout))?;
        let remaining = remaining_time(deadline)?;
        let url = self.endpoint.models_url();
        let response = self
            .transport
            .send(
                &TransportRequest::new(
                    HttpMethod::Get,
                    &url,
                    credential,
                    None,
                    MODELS_MAX_RESPONSE_BYTES,
                ),
                remaining,
                cancel,
            )
            .map_err(ApiError::from_transport)?;
        if !(200..300).contains(&response.status()) {
            return Err(ApiError::http_status(response.status()));
        }
        if response.body().len() > MODELS_MAX_RESPONSE_BYTES {
            return Err(ApiError::simple(ApiErrorKind::ResponseTooLarge));
        }
        parse_models(response.body())
    }
}

fn validate_timeout(timeout: Duration) -> Result<(), ApiError> {
    if timeout.as_millis() == 0 || timeout > MAX_TIMEOUT {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    Ok(())
}

fn validate_credential(credential: &str) -> Result<(), ApiError> {
    if credential.trim().is_empty() {
        return Err(ApiError::simple(ApiErrorKind::MissingCredential));
    }
    if credential.len() > MAX_CREDENTIAL_BYTES
        || !credential.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    Ok(())
}

fn validate_model(model: &str) -> Result<(), ApiError> {
    if model.trim().is_empty() || model.chars().count() > MAX_MODEL_CHARS {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    Ok(())
}

fn validate_tool_definitions(
    tools: &[ToolDefinition],
) -> Result<BTreeMap<&str, CheckedSchema<'_>>, ApiError> {
    if tools.len() > MAX_TOOLS {
        return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
    }
    let mut schemas = BTreeMap::new();
    for tool in tools {
        if !valid_tool_name(&tool.name)
            || tool.description.chars().count() > MAX_TOOL_DESCRIPTION_CHARS
            || schemas.contains_key(tool.name.as_str())
        {
            return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
        }
        if !tool.parameters.is_object()
            || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
        }
        let schema = CheckedSchema::new(&tool.parameters)
            .map_err(|error| ApiError::with_schema(ApiErrorKind::ToolDefinitionInvalid, error))?;
        schemas.insert(tool.name.as_str(), schema);
    }
    Ok(schemas)
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_TOOL_NAME_CHARS
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), ApiError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ApiError::simple(ApiErrorKind::Cancelled))
    } else {
        Ok(())
    }
}

/// A single model round; conversation, tool validation and budgets belong to
/// the shared loop. Implementations preserve the caller's absolute deadline.
pub trait ModelRound {
    fn configure(&self, _body: &mut Value) {}
    fn complete(
        &mut self,
        body: &Value,
        serialized: &str,
        deadline: Instant,
        cancel: &AtomicBool,
        output_base: Option<u64>,
        event: &mut dyn FnMut(ApiEvent),
    ) -> Result<ModelReply, ApiError>;
}

pub fn analyze_with_model(
    request: AnalyzeRequest<'_, '_>,
    model: &mut dyn ModelRound,
    event: &mut dyn FnMut(ApiEvent),
) -> Result<AnalysisOutput, ApiError> {
    let mut usage = UsageAggregator::default();
    analyze_inner(model, request, &mut usage, event).map_err(|mut error| {
        error.usage = Some(usage.finish());
        error
    })
}

struct HttpModel<'a, 't> {
    client: &'a ChatCompletionsClient<'t>,
    credential: &'a str,
    stream: bool,
}
impl ModelRound for HttpModel<'_, '_> {
    fn configure(&self, body: &mut Value) {
        body["stream"] = json!(self.stream);
        if self.stream {
            body["stream_options"] = json!({"include_usage":true});
        }
    }
    fn complete(
        &mut self,
        body: &Value,
        serialized: &str,
        deadline: Instant,
        cancel: &AtomicBool,
        output_base: Option<u64>,
        event: &mut dyn FnMut(ApiEvent),
    ) -> Result<ModelReply, ApiError> {
        let url = self.client.endpoint.chat_completions_url();
        let wire_request = TransportRequest::new(
            HttpMethod::Post,
            &url,
            self.credential,
            Some(serialized.as_bytes()),
            self.client.max_response_bytes,
        );
        let mut collector = StreamCollector::with_output_base(output_base);
        let mut stream_invalid = false;
        let remaining = remaining_time(deadline)?;
        let response = if self.stream {
            self.client.transport.send_stream(
                &wire_request,
                remaining,
                cancel,
                &mut |status, bytes| {
                    if (200..300).contains(&status)
                        && !stream_invalid
                        && collector.push(bytes, event).is_err()
                    {
                        stream_invalid = true;
                    }
                    !stream_invalid
                },
            )
        } else {
            self.client.transport.send(&wire_request, remaining, cancel)
        }
        .map_err(|error| {
            let mut error = if cancel.load(Ordering::Relaxed) {
                ApiError::simple(ApiErrorKind::Cancelled)
            } else if stream_invalid {
                ApiError::simple(ApiErrorKind::MalformedResponse)
            } else {
                ApiError::from_transport(error)
            };
            error.partial_usage = collector.usage().cloned();
            error
        })?;
        check_cancel(cancel)?;
        remaining_time(deadline)?;
        let status = response.status();
        if !(200..300).contains(&status) {
            event(ApiEvent::Response(
                json!({"http_status":status,"body":String::from_utf8_lossy(response.body())}),
            ));
            if body.get("response_format").is_some()
                && matches!(status, 400 | 422)
                && explicit_unsupported_response_format(response.body())
            {
                return Err(ApiError::simple(ApiErrorKind::UnsupportedResponseFormat));
            }
            let mut error = ApiError::http_status(status);
            if let Ok(value) = serde_json::from_slice::<Value>(response.body()) {
                let param = value["error"]["param"].as_str().unwrap_or("");
                error.parameter = [
                    "model",
                    "reasoning_effort",
                    "response_format",
                    "tools",
                    "messages",
                    "max_tokens",
                ]
                .into_iter()
                .find(|name| *name == param);
            }
            return Err(error);
        }
        if !self.stream && response.body().len() > self.client.max_response_bytes {
            return Err(ApiError::simple(ApiErrorKind::ResponseTooLarge));
        }
        let wire = if self.stream {
            if stream_invalid {
                let mut error = ApiError::simple(ApiErrorKind::MalformedResponse);
                error.partial_usage = collector.usage().cloned();
                return Err(error);
            }
            let partial = collector.usage().cloned();
            collector.finish(event).map_err(|_| {
                let mut error = ApiError::simple(ApiErrorKind::Truncated);
                error.partial_usage = partial;
                error
            })?
        } else {
            let value: Value = serde_json::from_slice(response.body())
                .map_err(|_| ApiError::simple(ApiErrorKind::MalformedResponse))?;
            event(ApiEvent::Response(value.clone()));
            value
        };
        if self.stream && !crate::json::fits_bytes(&wire, self.client.max_response_bytes) {
            let mut error = ApiError::simple(ApiErrorKind::ResponseTooLarge);
            error.partial_usage = wire.get("usage").filter(|v| v.is_object()).cloned();
            return Err(error);
        }
        parse_chat_response(wire)
    }
}

fn analyze_inner(
    model: &mut dyn ModelRound,
    request: AnalyzeRequest<'_, '_>,
    usage: &mut UsageAggregator,
    event: &mut dyn FnMut(ApiEvent),
) -> Result<AnalysisOutput, ApiError> {
    validate_timeout(request.timeout)?;
    validate_model(request.model)?;
    if request.reasoning_effort.is_some_and(|v| {
        v.is_empty()
            || v.len() > 64
            || !v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }) {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    if request.prompt.len() > MAX_PROMPT_BYTES {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    let output_schema = CheckedSchema::new(request.schema)
        .map_err(|error| ApiError::with_schema(ApiErrorKind::InvalidConfig, error))?;
    let tool_schemas = validate_tool_definitions(request.tools)?;
    if !request.tools.is_empty() && request.tool_handler.is_none() {
        return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
    }
    check_cancel(request.cancel)?;

    let deadline = Instant::now()
        .checked_add(request.timeout)
        .ok_or_else(|| ApiError::simple(ApiErrorKind::Timeout))?;
    let mut body = build_chat_body(
        request.model,
        request.reasoning_effort,
        request.prompt,
        request.schema,
        request.tools,
    );
    let mut fallback_used = false;
    let mut correction_used = false;
    let mut tool_correction_used = false;
    let mut requests = 0usize;
    let mut tool_calls = 0usize;
    let mut seen_calls = BTreeSet::<String>::new();
    let mut seen_ids = BTreeSet::<String>::new();
    let mut tool_handler = request.tool_handler;
    let continuation_limit = tool_handler
        .as_ref()
        .and_then(|h| h.continuation_char_limit());
    let mut continuation_chars = 0usize;
    let mut finish_only = false;

    loop {
        check_cancel(request.cancel)?;
        remaining_time(deadline)?;
        if finish_only {
            let fields = body.as_object_mut().unwrap();
            fields.remove("tools");
            fields.remove("tool_choice");
        }
        model.configure(&mut body);
        let body_text = serde_json::to_string(&body)
            .map_err(|_| ApiError::simple(ApiErrorKind::InvalidConfig))?;
        if body_text.len() > MAX_REQUEST_BYTES {
            return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
        }
        if requests == 0 {
            if let Some(handler) = tool_handler.as_ref() {
                if handler.input_char_limit().is_some_and(|limit| {
                    handler.input_chars(&body, body_text.chars().count()) > limit
                }) {
                    return Err(ApiError::simple(ApiErrorKind::InputLimit));
                }
            }
        }
        requests += 1;
        event(ApiEvent::Phase("等待首包"));
        let output_base = if usage.responses == 0 {
            Some(0)
        } else {
            usage.clone().finish().completion_tokens
        };
        let parsed = match model.complete(
            &body,
            &body_text,
            deadline,
            request.cancel,
            output_base,
            event,
        ) {
            Ok(reply) => reply,
            Err(error)
                if body.get("response_format").is_some()
                    && !fallback_used
                    && error.kind() == ApiErrorKind::UnsupportedResponseFormat =>
            {
                fallback_used = true;
                body.as_object_mut().unwrap().remove("response_format");
                continue;
            }
            Err(mut error) => {
                if let Some(partial) = error.partial_usage.take() {
                    usage.record(Some(&partial));
                }
                return Err(error);
            }
        };
        check_cancel(request.cancel)?;
        remaining_time(deadline)?;
        usage.record(parsed.usage.as_ref());
        let messages = body["messages"].as_array_mut().unwrap();

        if parsed.refusal {
            return Err(ApiError::simple(ApiErrorKind::Refusal));
        }
        if matches!(
            parsed.finish_reason.as_deref(),
            Some("content_filter" | "refusal")
        ) {
            return Err(ApiError::simple(ApiErrorKind::Refusal));
        }
        if parsed.finish_reason.as_deref() == Some("length") {
            return Err(ApiError::simple(ApiErrorKind::Truncated));
        }

        // Only a trusted local model adapter can attach this metadata.
        // Provider JSON fields never authorize a repair or a tool call.
        if let Some(error) = &parsed.model_output_error {
            if finish_only {
                return Err(ApiError::simple(ApiErrorKind::SchemaInvalid));
            }
            finish_only = !retain_continuation(
                messages,
                &parsed.message,
                &mut continuation_chars,
                continuation_limit,
            ) || tool_correction_used;
            tool_correction_used = true;
            let reason = format!("The model response envelope was invalid: {error}. No tools in that reply were executed.");
            if finish_only {
                finish_without_tools(messages, &reason);
            } else {
                messages.push(json!({"role":"user","content":format!("{reason} Correct the response against the required envelope; provide exactly one tool request or final result.")}));
            }
            event(ApiEvent::Phase("模型输出格式错误，等待调整"));
            continue;
        }

        if !parsed.tool_calls.is_empty() {
            tool_calls = tool_calls.saturating_add(parsed.tool_calls.len());
            // A final-only turn cannot reopen tools. Reused ids are never
            // replayed, including ids from a previously rejected proposal.
            if finish_only
                || parsed
                    .tool_calls
                    .iter()
                    .any(|call| seen_ids.contains(&call.id))
            {
                return Err(ApiError::simple(ApiErrorKind::NoProgress));
            }
            seen_ids.extend(parsed.tool_calls.iter().map(|call| call.id.clone()));
            if !retain_continuation(
                messages,
                &parsed.message,
                &mut continuation_chars,
                continuation_limit,
            ) {
                let reason = "The latest tool proposal exceeded the shared continuation budget. No tools in that reply were executed, and that oversized reply was not added to the conversation.";
                event(ApiEvent::ToolRejected(
                    json!({"response":parsed.message,"error":reason,"final_only":true}),
                ));
                finish_only = true;
                finish_without_tools(messages, reason);
                continue;
            }
            let validated = (|| -> Result<Vec<ValidatedToolCall<'_>>, ToolFailure> {
                if parsed.tool_calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
                    return Err(ToolFailure::with_message(
                        ToolFailureKind::BudgetExhausted,
                        format!("This reply requested more than {MAX_TOOL_CALLS_PER_RESPONSE} tool calls."),
                    ));
                }
                let calls =
                    validate_tool_calls(&parsed.tool_calls, &tool_schemas).map_err(|error| {
                        ToolFailure::with_message(
                            if error.kind() == ApiErrorKind::UnknownTool {
                                ToolFailureKind::Denied
                            } else {
                                ToolFailureKind::InvalidArguments
                            },
                            error.to_string(),
                        )
                    })?;
                let mut signatures = BTreeSet::new();
                for call in &calls {
                    let allow_repeated = tool_handler.as_ref().is_some_and(|handler| {
                        handler.allows_repeated_arguments(&call.name)
                            || handler.query_progress(&call.name).is_some()
                    });
                    if !allow_repeated
                        && (!signatures.insert(&call.signature)
                            || seen_calls.contains(&call.signature))
                    {
                        return Err(ToolFailure::with_message(
                            ToolFailureKind::BudgetExhausted,
                            "The same query has already been answered; repeating it would make no progress.",
                        ));
                    }
                }
                let invocations = calls
                    .iter()
                    .map(|call| ToolInvocation {
                        call_id: &call.id,
                        name: &call.name,
                        arguments: &call.arguments,
                    })
                    .collect::<Vec<_>>();
                tool_handler
                    .as_mut()
                    .ok_or_else(|| ToolFailure::new(ToolFailureKind::Failed))?
                    .validate_batch(&invocations)?;
                Ok(calls)
            })();
            let validated = match validated {
                Ok(calls) => calls,
                Err(error) if error.kind() == ToolFailureKind::Failed => {
                    return Err(ApiError::simple(ApiErrorKind::ToolFailed));
                }
                Err(error) => {
                    check_cancel(request.cancel)?;
                    remaining_time(deadline)?;
                    finish_only = matches!(
                        error.kind(),
                        ToolFailureKind::BudgetExhausted | ToolFailureKind::UnsafeReplay
                    ) || tool_correction_used;
                    let reason =
                        format!("No tools in this reply were executed. {}", error.feedback());
                    event(ApiEvent::ToolRejected(
                        json!({"response":parsed.message,"error":reason,"correction_used":tool_correction_used,"final_only":finish_only}),
                    ));
                    for call in &parsed.tool_calls {
                        messages.push(tool_feedback(&call.id, &reason));
                    }
                    if finish_only {
                        finish_without_tools(messages, &reason);
                    } else {
                        tool_correction_used = true;
                        event(ApiEvent::Phase("工具请求已拒绝，等待模型调整"));
                    }
                    continue;
                }
            };
            let mut image_messages = Vec::new();
            let mut stopped_reason: Option<String> = None;
            for (position, call) in validated.iter().enumerate() {
                check_cancel(request.cancel)?;
                remaining_time(deadline)?;
                if let Some(reason) = &stopped_reason {
                    messages.push(tool_feedback(&call.id, &format!("This call was not executed because the preceding tool stopped this batch: {reason}")));
                    continue;
                }
                let repeated = !seen_calls.insert(call.signature.clone());
                let handler = tool_handler
                    .as_mut()
                    .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidConfig))?;
                event(ApiEvent::Phase(if call.name == "read_context" {
                    "读取上下文"
                } else if call.name == "read_image" {
                    "读取图片"
                } else {
                    "执行工具"
                }));
                let progress = handler.query_progress(&call.name);
                // Reserve later calls' minimum wrappers. The host receives
                // this call's full allowance, including its own framing.
                let result_budget = continuation_limit.map(|limit| {
                    let wrappers = validated[position + 1..]
                        .iter()
                        .map(|call| message_input_chars(&empty_tool_message(&call.id)) + 1)
                        .sum::<usize>();
                    limit.saturating_sub(continuation_chars.saturating_add(wrappers))
                });
                let result = if result_budget == Some(0) {
                    Err(ToolFailure::new(ToolFailureKind::BudgetExhausted))
                } else {
                    handler.invoke_guarded(
                        &ToolInvocation {
                            call_id: &call.id,
                            name: &call.name,
                            arguments: &call.arguments,
                        },
                        request.cancel,
                        deadline,
                        result_budget,
                    )
                };
                check_cancel(request.cancel)?;
                remaining_time(deadline)?;
                let result = match result {
                    Ok(result) => result,
                    Err(error) if error.kind() == ToolFailureKind::Failed => {
                        return Err(ApiError::simple(ApiErrorKind::ToolFailed));
                    }
                    Err(error) => {
                        let reason = error.feedback().to_owned();
                        messages.push(tool_feedback(&call.id, &reason));
                        finish_only = matches!(
                            error.kind(),
                            ToolFailureKind::BudgetExhausted | ToolFailureKind::UnsafeReplay
                        ) || tool_correction_used;
                        tool_correction_used = true;
                        stopped_reason = Some(reason);
                        continue;
                    }
                };
                let (tool_message, images) = match result.messages(&call.id) {
                    Ok(messages) => messages,
                    Err(_) => {
                        let reason = "The tool result could not be delivered within the allowed output format or size. Do not repeat the operation; its result is unavailable.";
                        messages.push(tool_feedback(&call.id, reason));
                        finish_only = true;
                        stopped_reason = Some(reason.into());
                        continue;
                    }
                };
                let image_message =
                    (!images.is_empty()).then(|| json!({"role":"user", "content":images}));
                let chars = message_input_chars(&tool_message)
                    + 1
                    + image_message
                        .as_ref()
                        .map_or(0, |m| message_input_chars(m) + 1);
                if result_budget.is_some_and(|limit| chars > limit) {
                    let reason = "The tool result exceeded the shared continuation budget and was not delivered. Do not repeat the operation; use the previously delivered evidence.";
                    messages.push(tool_feedback(&call.id, reason));
                    finish_only = true;
                    stopped_reason = Some(reason.into());
                    continue;
                }
                continuation_chars = continuation_chars.saturating_add(chars);
                messages.push(tool_message);
                image_messages.extend(image_message);
                if repeated && progress.is_some() && progress == handler.query_progress(&call.name)
                {
                    finish_only = true;
                    stopped_reason =
                        Some("The repeated query returned no additional evidence.".into());
                }
            }
            // Correlate every tool result before presenting attached images.
            messages.extend(image_messages);
            if continuation_limit.is_some_and(|limit| continuation_chars >= limit) {
                finish_only = true;
                stopped_reason
                    .get_or_insert_with(|| "The shared continuation budget is exhausted.".into());
            }
            if finish_only {
                finish_without_tools(
                    messages,
                    stopped_reason
                        .as_deref()
                        .unwrap_or("Tool retrieval has ended."),
                );
            }
            continue;
        }

        let Some(content) = parsed.message["content"].as_str() else {
            return Err(ApiError::simple(ApiErrorKind::EmptyResponse));
        };
        event(ApiEvent::Phase("校验结果"));
        let validated = serde_json::from_str::<Value>(content.trim())
            .map_err(|_| ApiError::simple(ApiErrorKind::InvalidJson))
            .and_then(|value| {
                output_schema
                    .output(value)
                    .map_err(|error| ApiError::with_schema(ApiErrorKind::SchemaInvalid, error))
            })
            .and_then(|mut value| {
                if let Some(handler) = tool_handler.as_deref_mut() {
                    handler
                        .validate_output(&mut value, correction_used)
                        .map_err(|error| {
                            ApiError::with_schema(ApiErrorKind::SchemaInvalid, error)
                        })?;
                }
                Ok(value)
            });
        let error = match validated {
            Ok(value) => {
                check_cancel(request.cancel)?;
                remaining_time(deadline)?;
                return Ok(AnalysisOutput {
                    value,
                    usage: usage.clone().finish(),
                    requests,
                    tool_calls,
                    response_format_fallback: fallback_used,
                    correction_used,
                });
            }
            Err(error) => error,
        };
        check_cancel(request.cancel)?;
        if correction_used {
            return Err(error);
        }
        remaining_time(deadline)?;
        correction_used = true;
        if !retain_continuation(
            messages,
            &parsed.message,
            &mut continuation_chars,
            continuation_limit,
        ) {
            finish_only = true;
            finish_without_tools(messages, "The invalid reply exceeded the remaining continuation budget and was not retained.");
        }
        let correction = error.schema.as_ref().map(correction_schema_text).unwrap_or_else(||
            "The previous reply was not a single valid JSON value. Return only the JSON value required by the system message. Do not include prose, Markdown fences, or comments.".into());
        messages.push(json!({"role":"user", "content":correction}));
    }
}

fn remaining_time(deadline: Instant) -> Result<Duration, ApiError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(ApiError::simple(ApiErrorKind::Timeout));
    }
    let remaining = deadline - now;
    if remaining.as_millis() == 0 {
        // The transport rejects a zero duration; the deadline is nearly gone.
        Ok(Duration::from_millis(1))
    } else {
        Ok(remaining)
    }
}

fn build_chat_body(
    model: &str,
    effort: Option<&str>,
    prompt: &str,
    schema: &Value,
    tools: &[ToolDefinition],
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            {"role":"system", "content":prompt},
            {"role":"user", "content":format!("{USER_INSTRUCTION}\n\nRequired JSON Schema (authoritative):\n{schema}")},
        ],
        "stream": false,
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": RESPONSE_FORMAT_NAME, "strict": true, "schema": schema},
        },
    });
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    if !tools.is_empty() {
        body["tools"] = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body["tool_choice"] = json!("auto");
    }
    body
}

#[derive(Clone, Debug)]
pub struct ModelReply {
    message: Value,
    tool_calls: Vec<RawToolCall>,
    refusal: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    model_output_error: Option<String>,
}

#[derive(Clone, Debug)]
struct RawToolCall {
    id: String,
    name: String,
    raw_arguments: String,
}

#[derive(Clone, Debug)]
struct ValidatedToolCall<'a> {
    id: &'a str,
    name: &'a str,
    arguments: Value,
    signature: String,
}

fn parse_chat_response(mut value: Value) -> Result<ModelReply, ApiError> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(ApiError::simple(ApiErrorKind::MalformedResponse));
    }
    let usage = value
        .get_mut("usage")
        .filter(|v| v.is_object())
        .map(Value::take);
    let choices = value
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::simple(ApiErrorKind::EmptyResponse))?;
    let choice = choices
        .first_mut()
        .ok_or_else(|| ApiError::simple(ApiErrorKind::EmptyResponse))?;
    let choice = choice
        .as_object_mut()
        .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    ModelReply::parse_message(
        choice
            .remove("message")
            .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?,
        match choice.remove("finish_reason") {
            Some(Value::String(reason)) => Some(reason),
            _ => None,
        },
        usage,
    )
}

impl ModelReply {
    pub fn from_message(
        message: Value,
        finish_reason: Option<String>,
        usage: Option<Value>,
    ) -> Result<Self, ApiError> {
        if !crate::json::fits_bytes(&message, DEFAULT_MAX_RESPONSE_BYTES) {
            return Err(ApiError::simple(ApiErrorKind::ResponseTooLarge));
        }
        Self::parse_message(message, finish_reason, usage)
    }

    fn parse_message(
        message: Value,
        finish_reason: Option<String>,
        usage: Option<Value>,
    ) -> Result<Self, ApiError> {
        let Value::Object(mut message) = message else {
            return Err(ApiError::simple(ApiErrorKind::MalformedResponse));
        };
        let refusal = message.get("refusal").is_some_and(|refusal| match refusal {
            Value::String(text) => !text.is_empty(),
            Value::Bool(flag) => *flag,
            _ => false,
        });
        let refused =
            refusal || matches!(finish_reason.as_deref(), Some("refusal" | "content_filter"));
        let content = if refused {
            None
        } else {
            extract_content(message.remove("content"))?
        };
        let tool_calls = if refused {
            Vec::new()
        } else {
            extract_tool_calls(&message)?
        };
        let mut replay = json!({"role":"assistant","content":null});
        replay["content"] = content.map(Value::String).unwrap_or(Value::Null);
        if let Some(calls) = message.remove("tool_calls") {
            replay["tool_calls"] = calls;
        }
        if let Some(reasoning) = message.remove("reasoning_content").filter(Value::is_string) {
            replay["reasoning_content"] = reasoning;
        }
        Ok(Self {
            message: replay,
            tool_calls,
            refusal,
            finish_reason,
            usage,
            model_output_error: None,
        })
    }
    /// Only trusted local adapters set envelope failures; wire JSON cannot.
    pub fn with_output_error(mut self, error: impl AsRef<str>) -> Self {
        self.model_output_error = Some(error.as_ref().chars().take(512).collect());
        self
    }
}

fn extract_content(content: Option<Value>) -> Result<Option<String>, ApiError> {
    match content {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(Value::Array(parts)) => {
            if parts.is_empty() {
                return Ok(None);
            }
            let mut text = String::new();
            for part in parts {
                let part = part
                    .as_object()
                    .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(ApiError::simple(ApiErrorKind::MalformedResponse));
                }
                let value = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
                text.push_str(value);
            }
            Ok(Some(text))
        }
        Some(_) => Err(ApiError::simple(ApiErrorKind::MalformedResponse)),
    }
}

fn extract_tool_calls(message: &Map<String, Value>) -> Result<Vec<RawToolCall>, ApiError> {
    let calls = match message.get("tool_calls") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(calls)) => calls,
        Some(_) => return Err(ApiError::simple(ApiErrorKind::InvalidToolCall)),
    };
    if calls.len() > MAX_TOOL_CALLS_IN_RESPONSE {
        return Err(ApiError::simple(ApiErrorKind::InvalidToolCall));
    }
    let mut ids = BTreeSet::new();
    let mut result = Vec::with_capacity(calls.len());
    for call in calls {
        let call = call
            .as_object()
            .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidToolCall))?;
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty() && id.chars().count() <= 128 && !id.chars().any(|ch| ch.is_control())
            })
            .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidToolCall))?;
        if !ids.insert(id) {
            return Err(ApiError::simple(ApiErrorKind::InvalidToolCall));
        }
        if call.get("type").and_then(Value::as_str) != Some("function") {
            return Err(ApiError::simple(ApiErrorKind::InvalidToolCall));
        }
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidToolCall))?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidToolCall))?;
        let raw_arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidToolCall))?;
        if raw_arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ApiError::simple(ApiErrorKind::InvalidToolCall));
        }
        result.push(RawToolCall {
            id: id.to_string(),
            name: name.to_string(),
            raw_arguments: raw_arguments.to_string(),
        });
    }
    Ok(result)
}

fn validate_tool_calls<'a>(
    calls: &'a [RawToolCall],
    schemas: &BTreeMap<&str, CheckedSchema<'_>>,
) -> Result<Vec<ValidatedToolCall<'a>>, ApiError> {
    let mut result = Vec::with_capacity(calls.len());
    for call in calls {
        let schema = schemas
            .get(call.name.as_str())
            .ok_or_else(|| ApiError::simple(ApiErrorKind::UnknownTool))?;
        let arguments: Value = serde_json::from_str(&call.raw_arguments)
            .map_err(|_| ApiError::simple(ApiErrorKind::InvalidToolArguments))?;
        schema
            .validate(&arguments)
            .map_err(|error| ApiError::with_schema(ApiErrorKind::InvalidToolArguments, error))?;
        let signature = format!("{}:{}", call.name, canonical_json(&arguments));
        result.push(ValidatedToolCall {
            id: &call.id,
            name: &call.name,
            arguments,
            signature,
        });
    }
    Ok(result)
}

/// Stable prompt and tool identity: sort object keys, preserve array order.
/// Borrow values and serialize into one buffer instead of joining child strings.
fn correction_schema_text(error: &ValidationError) -> String {
    let path = error
        .path()
        .chars()
        .take(MAX_CORRECTION_PATH_CHARS)
        .collect::<String>();
    let constraint = error
        .constraint()
        .chars()
        .take(MAX_CORRECTION_PATH_CHARS)
        .collect::<String>();
    format!(
        "The previous reply did not match the required JSON schema at {path}: {constraint}. Return only a corrected JSON value that satisfies the schema. Do not include prose or Markdown."
    )
}

fn explicit_unsupported_response_format(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let error = value.get("error").unwrap_or(&value);
    let field = |key| {
        error
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let code = field("code");
    let param = field("param");
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| field("message"));
    if !param.is_empty() && param != "response_format" {
        return false;
    }
    let names_parameter = param == "response_format" || message.contains("response_format");
    if !names_parameter {
        return false;
    }
    let explicit = matches!(
        code.as_str(),
        "unsupported_parameter"
            | "unsupported_value"
            | "unknown_parameter"
            | "unrecognized_parameter"
            | "not_supported"
    ) || message.contains("unsupported parameter")
        || message.contains("response_format type is unavailable")
        || message.contains("not supported")
        || message.contains("does not support")
        || message.contains("unknown parameter")
        || message.contains("unrecognized parameter")
        || message.contains("unexpected parameter");
    explicit
}

fn parse_models(body: &[u8]) -> Result<Vec<ModelInfo>, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    if data.is_empty() {
        return Err(ApiError::simple(ApiErrorKind::EmptyModels));
    }
    if data.len() > MAX_MODELS {
        return Err(ApiError::simple(ApiErrorKind::ModelsTooMany));
    }
    let mut models = Vec::with_capacity(data.len());
    for item in data {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.chars().count() <= 512)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
        let display_name = item
            .get("display_name")
            .and_then(Value::as_str)
            .or_else(|| item.get("name").and_then(Value::as_str))
            .map(|name| name.chars().take(200).collect::<String>());
        models.push(ModelInfo {
            id: id.to_string(),
            display_name,
            supported_reasoning_efforts: item
                .get("supported_reasoning_efforts")
                .or_else(|| item.get("supportedReasoningEfforts"))
                .or_else(|| item.get("reasoning").and_then(|v| v.get("efforts")))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| {
                            v.as_str()
                                .or_else(|| v.get("reasoningEffort").and_then(Value::as_str))
                                .or_else(|| v.get("id").and_then(Value::as_str))
                        })
                        .filter(|v| !v.is_empty() && v.len() <= 64)
                        .map(str::to_string)
                        .collect()
                }),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestResponse, TestServer};
    use serde_json::json;

    #[test]
    fn usage_unknown_is_not_zero_and_cached_is_bounded() {
        let mut aggregator = UsageAggregator::default();
        aggregator.record(Some(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14,
            "prompt_tokens_details": {"cached_tokens": 20}
        })));
        let usage = aggregator.finish();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(4));
        assert_eq!(usage.total_tokens, Some(14));
        assert_eq!(usage.cached_tokens, Some(10));

        let mut unknown = UsageAggregator::default();
        unknown.record(None);
        let usage = unknown.finish();
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, None);
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.responses, 1);
        assert_eq!(usage.reported_responses, 0);
    }

    #[test]
    fn usage_aggregation_marks_partially_reported_fields_unknown() {
        let mut aggregator = UsageAggregator::default();
        aggregator.record(Some(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14,
            "prompt_tokens_details": {"cached_tokens": 3}
        })));
        aggregator.record(Some(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 2
        })));
        let usage = aggregator.finish();
        assert_eq!(usage.prompt_tokens, Some(15));
        assert_eq!(usage.completion_tokens, Some(6));
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.responses, 2);
        assert_eq!(usage.reported_responses, 2);
    }

    #[test]
    fn explicit_response_format_detection_is_narrow() {
        assert!(explicit_unsupported_response_format(br#"{"error":{"message":"InternalError.Algo.InvalidParameter: This response_format type is unavailable now","code":"invalid_parameter_error"}}"#));
        assert!(!explicit_unsupported_response_format(
            br#"{"error":{"message":"model is unavailable now","code":"invalid_parameter_error"}}"#
        ));
        assert!(explicit_unsupported_response_format(
            br#"{"error":{"message":"Unsupported parameter: 'response_format' is not supported with this model.","param":"response_format","code":"unsupported_parameter"}}"#
        ));
        assert!(explicit_unsupported_response_format(
            br#"{"error":"Unsupported parameter: response_format"}"#
        ));
        assert!(!explicit_unsupported_response_format(
            br#"{"error":{"message":"Bad request","param":null,"code":"invalid_request"}}"#
        ));
        assert!(!explicit_unsupported_response_format(
            br#"{"error":{"message":"response_format is invalid","param":"response_format","code":"invalid_parameter"}}"#
        ));
        assert!(!explicit_unsupported_response_format(
            br#"{"error":{"message":"Unsupported parameter: temperature","param":"temperature","code":"unsupported_parameter"}}"#
        ));
        assert!(!explicit_unsupported_response_format(b"not json"));
    }

    #[test]
    fn tool_names_are_bounded_and_advertised_only() {
        assert!(valid_tool_name("read_context"));
        assert!(valid_tool_name("read_image"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("shell;rm"));
        assert!(!valid_tool_name("1tool"));
        assert!(ToolDefinition::new(
            "read_context",
            "read",
            json!({"type":"object","properties":{},"additionalProperties":false})
        )
        .name
        .eq("read_context"));
    }

    #[test]
    fn canonical_json_preserves_values_and_json_accounting_matches_wire_characters() {
        let first = json!({"z":{"b":2,"a":1},"a":[{"y":2,"x":1},0]});
        let second = json!({"a":[{"x":1,"y":2},0],"z":{"a":1,"b":2}});
        assert_eq!(canonical_json(&first), canonical_json(&second));
        assert_eq!(
            canonical_json(&first),
            r#"{"a":[{"x":1,"y":2},0],"z":{"a":1,"b":2}}"#
        );
        let mut reversed = second.clone();
        reversed["a"].as_array_mut().unwrap().reverse();
        assert_ne!(canonical_json(&first), canonical_json(&reversed));
        let scalars = json!([
            "中文🙂\n\t\"\\",
            -0.0,
            -4,
            u64::MAX,
            1.25e30,
            true,
            false,
            null
        ]);
        assert_eq!(canonical_json(&scalars), scalars.to_string());
        let controls = (0u8..32).map(char::from).collect::<String>();
        for value in [
            first,
            reversed,
            scalars,
            json!({"controls":controls,"numbers":[f64::MIN_POSITIVE, f64::MAX]}),
        ] {
            assert_eq!(json_chars(&value), value.to_string().chars().count());
        }
    }

    #[test]
    fn error_redaction_never_contains_provider_body_or_key() {
        let key = "synthetic-key";
        let error = ApiError::http_status(400);
        let rendered = format!("{error} {error:?} {error}");
        assert!(!rendered.contains(key));
        assert!(!rendered.contains("response_format"));
        let error = ApiError::from_transport(TransportError::new(TransportErrorKind::Network));
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(key));
        assert!(!rendered.contains("http://"));
    }

    #[test]
    fn localhost_system_transport_completes_a_tool_loop() {
        let server = TestServer::start(2, |_, index| {
            let value = if index == 0 {
                json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_local",
                                "type": "function",
                                "function": {
                                    "name": "read_context",
                                    "arguments": "{\"ids\":[\"m1\"]}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                })
            } else {
                json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "{\"ok\":true}"},
                        "finish_reason": "stop"
                    }]
                })
            };
            TestResponse::json(&serde_json::to_vec(&value).expect("local response JSON"))
        });
        let addr = server.addr;
        let endpoint = Endpoint::parse_localhost_for_tests(&format!("http://{addr}/openai/v1"))
            .expect("local endpoint");
        let client = ChatCompletionsClient::new(endpoint);
        let schema = json!({
            "type":"object",
            "additionalProperties":false,
            "properties":{"ok":{"type":"boolean"}},
            "required":["ok"]
        });
        let tools = [ToolDefinition::new(
            "read_context",
            "local synthetic read",
            json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"ids":{"type":"array","items":{"type":"string"}}},
                "required":["ids"]
            }),
        )];
        let cancel = AtomicBool::new(false);
        let mut handler = FnToolHandler(|invocation: &ToolInvocation<'_>| {
            assert_eq!(invocation.name(), "read_context");
            assert_eq!(invocation.call_id(), "call_local");
            Ok(r#"{"records":[{"id":"m1","text":"local synthetic"}]}"#.to_string())
        });
        let output = client
            .analyze(AnalyzeRequest {
                model: "local-synthetic-model",
                reasoning_effort: None,
                prompt: "LOCAL-SENTINEL-PROMPT\n## 运行上下文（最后）\n当前上海时间：2026-10-18 09:00:00 +08:00",
                schema: &schema,
                credential: "sk-synthetic-local-key",
                cancel: &cancel,
                timeout: Duration::from_secs(5),
                tools: &tools,
                tool_handler: Some(&mut handler),
            })
            .expect("localhost tool loop");
        assert_eq!(output.value, json!({"ok":true}));
        assert_eq!(output.tool_calls, 1);
        assert_eq!(output.requests, 2);

        let requests = server.join();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/openai/v1/chat/completions");
        assert_eq!(
            requests[0].header("authorization"),
            Some("Bearer sk-synthetic-local-key")
        );
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(first["messages"][0]["content"], "LOCAL-SENTINEL-PROMPT\n## 运行上下文（最后）\n当前上海时间：2026-10-18 09:00:00 +08:00");
        assert!(first["messages"][1]["content"]
            .as_str()
            .unwrap()
            .starts_with(USER_INSTRUCTION));
        assert!(first["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Required JSON Schema"));
        assert_eq!(first["tools"][0]["function"]["name"], "read_context");
        let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert!(second["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("local synthetic"));
    }

    #[test]
    fn localhost_models_discovery_is_a_get_without_a_model_call() {
        let server = TestServer::start(1, |_, _| {
            TestResponse::json(br#"{"data":[{"id":"local-model-a"},{"id":"local-model-b"}]}"#)
        });
        let addr = server.addr;
        let endpoint = Endpoint::parse_localhost_for_tests(&format!("http://{addr}/openai/v1"))
            .expect("local endpoint");
        let client = ChatCompletionsClient::new(endpoint);
        let cancel = AtomicBool::new(false);
        let models = client
            .discover_models("sk-synthetic-local-key", &cancel, Duration::from_secs(5))
            .expect("localhost models");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "local-model-a");
        assert_eq!(models[1].id, "local-model-b");

        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/openai/v1/models");
        assert_eq!(
            requests[0].header("authorization"),
            Some("Bearer sk-synthetic-local-key")
        );
        assert!(requests[0].body.is_empty());
    }
}
