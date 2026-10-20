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
//! schema validation gets one bounded correction. Known read-only tool argument
//! errors get one separate correction without executing the invalid batch.
//! Other failures are never retried.
//!
//! The tool loop supports any caller-advertised tools (including
//! `read_context`/`read_image`), validates names and arguments against those
//! definitions, and calls only the caller's handler. It stops on the overall
//! deadline, cancellation, or a repeated no-progress tool query. There is no
//! cumulative RAG round or token budget.

use crate::endpoint::{Endpoint, EndpointError};
use crate::schema_validate::{self, ValidationError};
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
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CREDENTIAL_BYTES: usize = 4096;
pub const MAX_MODEL_CHARS: usize = 256;
pub const MAX_TOOL_NAME_CHARS: usize = 64;
pub const MAX_TOOL_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_FALLBACK_SCHEMA_BYTES: usize = 1024 * 1024;

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
    parameter: Option<&'static str>,
    kind: ApiErrorKind,
    status: Option<u16>,
    schema: Option<ValidationError>,
    endpoint: Option<EndpointError>,
    transport: Option<TransportErrorKind>,
}

impl ApiError {
    fn simple(kind: ApiErrorKind) -> Self {
        Self {
            usage: None,
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

    pub fn schema_error(&self) -> Option<&ValidationError> {
        self.schema.as_ref()
    }

    pub fn endpoint_error(&self) -> Option<EndpointError> {
        self.endpoint
    }

    pub fn transport_error(&self) -> Option<TransportErrorKind> {
        self.transport
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

#[derive(Clone, Copy, Debug)]
struct FieldSum {
    seen: bool,
    complete: bool,
    sum: u64,
}

impl FieldSum {
    fn new() -> Self {
        Self {
            seen: false,
            complete: true,
            sum: 0,
        }
    }

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

#[derive(Clone)]
struct UsageAggregator {
    prompt: FieldSum,
    completion: FieldSum,
    total: FieldSum,
    cached: FieldSum,
    responses: usize,
    reported: usize,
}

impl Default for UsageAggregator {
    fn default() -> Self {
        Self {
            prompt: FieldSum::new(),
            completion: FieldSum::new(),
            total: FieldSum::new(),
            cached: FieldSum::new(),
            responses: 0,
            reported: 0,
        }
    }
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
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolFailure {
    kind: ToolFailureKind,
}

impl ToolFailure {
    pub fn new(kind: ToolFailureKind) -> Self {
        Self { kind }
    }

    pub fn kind(self) -> ToolFailureKind {
        self.kind
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
}

/// Images are explicit local tool output, never provider-supplied URLs. Keep
/// their bytes out of Debug and ordinary text messages.
pub enum ToolResult {
    Text(String),
    Content(Vec<ToolContent>),
}

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

    fn messages(self, call_id: &str) -> Result<(Value, Vec<Value>), ApiError> {
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
        let mut usage = UsageAggregator::default();
        self.analyze_inner(request, &mut usage, stream, event)
            .map_err(|mut error| {
                error.usage = Some(usage.finish());
                error
            })
    }
    fn analyze_inner(
        &self,
        request: AnalyzeRequest<'_, '_>,
        usage: &mut UsageAggregator,
        stream: bool,
        event: &mut dyn FnMut(ApiEvent),
    ) -> Result<AnalysisOutput, ApiError> {
        validate_timeout(request.timeout)?;
        validate_credential(request.credential)?;
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
        schema_validate::check_schema(request.schema)
            .map_err(|error| ApiError::with_schema(ApiErrorKind::InvalidConfig, error))?;
        validate_tool_definitions(request.tools)?;
        if !request.tools.is_empty() && request.tool_handler.is_none() {
            return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
        }
        check_cancel(request.cancel)?;

        let deadline = Instant::now()
            .checked_add(request.timeout)
            .ok_or_else(|| ApiError::simple(ApiErrorKind::Timeout))?;
        let mut messages = vec![
            json!({"role":"system","content":request.prompt}),
            json!({"role":"user","content":format!("{USER_INSTRUCTION}\n\nRequired JSON Schema (authoritative):\n{}",request.schema)}),
        ];
        let mut use_response_format = true;
        let mut fallback_used = false;
        let mut schema_in_user = true;
        let mut correction_used = false;
        let mut tool_correction_used = false;
        let mut requests = 0usize;
        let mut tool_calls = 0usize;
        let mut seen_calls = BTreeSet::<String>::new();
        let mut tool_handler = request.tool_handler;

        loop {
            check_cancel(request.cancel)?;
            let remaining = remaining_time(deadline)?;
            let mut body = build_chat_body(
                request.model,
                &messages,
                request.schema,
                request.tools,
                use_response_format,
            );
            if let Some(effort) = request.reasoning_effort {
                body["reasoning_effort"] = json!(effort);
            }
            body["stream"] = json!(stream);
            if stream {
                body["stream_options"] = json!({"include_usage":true});
            }
            let body_bytes = serde_json::to_vec(&body)
                .map_err(|_| ApiError::simple(ApiErrorKind::InvalidConfig))?;
            if body_bytes.len() > MAX_REQUEST_BYTES {
                return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
            }
            let url = self.endpoint.chat_completions_url();
            requests += 1;
            event(ApiEvent::Phase("等待首包"));
            let wire_request = TransportRequest::new(
                HttpMethod::Post,
                &url,
                request.credential,
                Some(&body_bytes),
                self.max_response_bytes,
            );
            let mut collector = StreamCollector::default();
            let mut stream_invalid = false;
            let response = if stream {
                self.transport.send_stream(
                    &wire_request,
                    remaining,
                    request.cancel,
                    &mut |status, bytes| {
                        if (200..300).contains(&status)
                            && !stream_invalid
                            && collector.push(bytes, event).is_err()
                        {
                            stream_invalid = true;
                        }
                    },
                )
            } else {
                self.transport
                    .send(&wire_request, remaining, request.cancel)
            }
            .map_err(|error| {
                if let Some(u) = collector.usage() {
                    usage.record(Some(u));
                }
                ApiError::from_transport(error)
            })?;

            check_cancel(request.cancel)?;
            remaining_time(deadline)?;

            let status = response.status();
            if !(200..300).contains(&status) {
                event(ApiEvent::Response(
                    json!({"http_status":status,"body":String::from_utf8_lossy(response.body())}),
                ));
                if use_response_format
                    && !fallback_used
                    && matches!(status, 400 | 422)
                    && explicit_unsupported_response_format(response.body())
                {
                    fallback_used = true;
                    use_response_format = false;
                    if !schema_in_user {
                        let schema_text = request.schema.to_string();
                        if schema_text.len() > MAX_FALLBACK_SCHEMA_BYTES {
                            return Err(ApiError::simple(ApiErrorKind::InvalidConfig));
                        }
                        let content = format!(
                            "{USER_INSTRUCTION}\n\nRequired JSON Schema (authoritative):\n{schema_text}"
                        );
                        messages[1] = json!({"role":"user","content":content});
                        schema_in_user = true;
                    }
                    continue;
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

            if response.body().len() > self.max_response_bytes {
                return Err(ApiError::simple(ApiErrorKind::ResponseTooLarge));
            }
            let wire = if stream {
                if stream_invalid {
                    if let Some(u) = collector.usage() {
                        usage.record(Some(u));
                    }
                    return Err(ApiError::simple(ApiErrorKind::MalformedResponse));
                }
                let partial_usage = collector.usage().cloned();
                collector.finish(event).map_err(|_| {
                    if let Some(u) = partial_usage.as_ref() {
                        usage.record(Some(u));
                    }
                    ApiError::simple(ApiErrorKind::Truncated)
                })?
            } else {
                let v: Value = serde_json::from_slice(response.body())
                    .map_err(|_| ApiError::simple(ApiErrorKind::MalformedResponse))?;
                event(ApiEvent::Response(v.clone()));
                v
            };
            let parsed = parse_chat_response(&serde_json::to_vec(&wire).unwrap())?;
            usage.record(parsed.usage.as_ref());

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

            if !parsed.tool_calls.is_empty() {
                tool_calls = tool_calls.saturating_add(parsed.tool_calls.len());
                let validated = match validate_tool_calls(&parsed.tool_calls, request.tools) {
                    Ok(calls) => calls,
                    Err(error)
                        if error.kind() == ApiErrorKind::InvalidToolArguments
                            && !tool_correction_used
                            && parsed.tool_calls.iter().all(|call| {
                                matches!(call.name.as_str(), "read_context" | "read_image")
                                    && request.tools.iter().any(|t| t.name == call.name)
                            }) =>
                    {
                        event(ApiEvent::ToolRejected(
                            json!({"response":parsed.message,"error":error.to_string(),"correction":1}),
                        ));
                        tool_correction_used = true;
                        event(ApiEvent::Phase("工具参数格式错误，正在纠正（1/1）"));
                        messages.push(parsed.message.clone());
                        for call in &parsed.tool_calls {
                            messages.push(json!({"role":"tool","tool_call_id":call.id,"content":format!("No tools in this reply were executed. Tool argument validation failed: {error}. Correct the arguments against the advertised schema. Extra properties are forbidden. This is the only tool-argument correction opportunity.")}));
                        }
                        continue;
                    }
                    Err(error) => {
                        event(ApiEvent::ToolRejected(
                            json!({"response":parsed.message,"error":error.to_string(),"correction_used":tool_correction_used}),
                        ));
                        return Err(error);
                    }
                };
                // Reject a duplicate query before invoking any handler in this
                // response, so two identical calls cannot both run. The same
                // signature repeated in a later response is also no progress.
                let mut response_signatures = BTreeSet::new();
                for call in &validated {
                    if !response_signatures.insert(call.signature.clone())
                        || seen_calls.contains(&call.signature)
                    {
                        return Err(ApiError::simple(ApiErrorKind::NoProgress));
                    }
                }
                // Preserve provider-published reasoning and replay metadata on tool rounds.
                messages.push(parsed.message.clone());
                let mut image_messages = Vec::new();
                for call in validated {
                    check_cancel(request.cancel)?;
                    remaining_time(deadline)?;
                    seen_calls.insert(call.signature.clone());
                    let handler = tool_handler
                        .as_mut()
                        .ok_or_else(|| ApiError::simple(ApiErrorKind::InvalidConfig))?;
                    event(ApiEvent::Phase(if call.name == "read_context" {
                        "读取上下文"
                    } else {
                        "读取图片"
                    }));
                    let result = handler
                        .invoke(&ToolInvocation {
                            call_id: &call.id,
                            name: &call.name,
                            arguments: &call.arguments,
                        })
                        .map_err(|_| ApiError::simple(ApiErrorKind::ToolFailed))?;
                    check_cancel(request.cancel)?;
                    remaining_time(deadline)?;
                    let (tool_message, images) = result.messages(&call.id)?;
                    messages.push(tool_message);
                    if !images.is_empty() {
                        image_messages.push(json!({"role":"user", "content":images}));
                    }
                }
                // Complete every tool_call_id before adding multimodal input.
                messages.extend(image_messages);
                continue;
            }

            let Some(content) = parsed.content else {
                return Err(ApiError::simple(ApiErrorKind::EmptyResponse));
            };
            event(ApiEvent::Phase("校验结果"));
            match serde_json::from_str::<Value>(content.trim()) {
                Ok(value) => match schema_validate::validate(request.schema, &value) {
                    Ok(()) => {
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
                    Err(error) => {
                        check_cancel(request.cancel)?;
                        if !correction_used {
                            if Instant::now() >= deadline {
                                return Err(ApiError::simple(ApiErrorKind::Timeout));
                            }
                            correction_used = true;
                            messages.push(parsed.message.clone());
                            messages.push(json!({
                                "role":"user",
                                "content": correction_schema_text(&error),
                            }));
                            continue;
                        }
                        return Err(ApiError::with_schema(ApiErrorKind::SchemaInvalid, error));
                    }
                },
                Err(_) => {
                    check_cancel(request.cancel)?;
                    if !correction_used {
                        if Instant::now() >= deadline {
                            return Err(ApiError::simple(ApiErrorKind::Timeout));
                        }
                        correction_used = true;
                        messages.push(parsed.message.clone());
                        messages.push(json!({
                            "role":"user",
                            "content": "The previous reply was not a single valid JSON value. Return only the JSON value required by the system message. Do not include prose, Markdown fences, or comments.",
                        }));
                        continue;
                    }
                    return Err(ApiError::simple(ApiErrorKind::InvalidJson));
                }
            }
        }
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

fn validate_tool_definitions(tools: &[ToolDefinition]) -> Result<(), ApiError> {
    if tools.len() > MAX_TOOLS {
        return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
    }
    let mut names = BTreeSet::new();
    for tool in tools {
        if !valid_tool_name(&tool.name)
            || tool.description.chars().count() > MAX_TOOL_DESCRIPTION_CHARS
            || !names.insert(tool.name.as_str())
        {
            return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
        }
        if !tool.parameters.is_object()
            || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(ApiError::simple(ApiErrorKind::ToolDefinitionInvalid));
        }
        schema_validate::check_schema(&tool.parameters)
            .map_err(|error| ApiError::with_schema(ApiErrorKind::ToolDefinitionInvalid, error))?;
    }
    Ok(())
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
    messages: &[Value],
    schema: &Value,
    tools: &[ToolDefinition],
    use_response_format: bool,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.to_string()));
    body.insert("messages".into(), Value::Array(messages.to_vec()));
    body.insert("stream".into(), Value::Bool(false));
    if use_response_format {
        body.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": RESPONSE_FORMAT_NAME,
                    "strict": true,
                    "schema": schema,
                }
            }),
        );
    }
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            }
                        })
                    })
                    .collect(),
            ),
        );
        body.insert("tool_choice".into(), json!("auto"));
    }
    Value::Object(body)
}

#[derive(Clone, Debug)]
struct ParsedResponse {
    message: Value,
    content: Option<String>,
    tool_calls: Vec<RawToolCall>,
    refusal: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Clone, Debug)]
struct RawToolCall {
    id: String,
    name: String,
    raw_arguments: String,
}

#[derive(Clone, Debug)]
struct ValidatedToolCall {
    id: String,
    name: String,
    arguments: Value,
    signature: String,
}

fn parse_chat_response(body: &[u8]) -> Result<ParsedResponse, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(ApiError::simple(ApiErrorKind::MalformedResponse));
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::simple(ApiErrorKind::EmptyResponse))?;
    let choice = choices
        .first()
        .ok_or_else(|| ApiError::simple(ApiErrorKind::EmptyResponse))?;
    let choice = choice
        .as_object()
        .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::simple(ApiErrorKind::MalformedResponse))?;
    let refusal = message.get("refusal").is_some_and(|refusal| match refusal {
        Value::String(text) => !text.is_empty(),
        Value::Bool(flag) => *flag,
        _ => false,
    });
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let refused = refusal || matches!(finish_reason.as_deref(), Some("refusal" | "content_filter"));
    let content = if refused {
        None
    } else {
        extract_content(message)?
    };
    let tool_calls = if refused {
        Vec::new()
    } else {
        extract_tool_calls(message)?
    };
    let usage = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .cloned();
    let mut replay = json!({"role":"assistant","content":content});
    if let Some(calls) = message.get("tool_calls") {
        replay["tool_calls"] = calls.clone();
    }
    if let Some(reasoning) = message.get("reasoning_content").filter(|v| v.is_string()) {
        replay["reasoning_content"] = reasoning.clone();
    }
    Ok(ParsedResponse {
        message: replay,
        content,
        tool_calls,
        refusal,
        finish_reason,
        usage,
    })
}

fn extract_content(message: &Map<String, Value>) -> Result<Option<String>, ApiError> {
    match message.get("content") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
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
    if calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
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
        if !ids.insert(id.to_string()) {
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

fn validate_tool_calls(
    calls: &[RawToolCall],
    tools: &[ToolDefinition],
) -> Result<Vec<ValidatedToolCall>, ApiError> {
    let by_name = tools
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect::<BTreeMap<_, _>>();
    let mut result = Vec::with_capacity(calls.len());
    for call in calls {
        let tool = by_name
            .get(call.name.as_str())
            .ok_or_else(|| ApiError::simple(ApiErrorKind::UnknownTool))?;
        let arguments: Value = serde_json::from_str(&call.raw_arguments)
            .map_err(|_| ApiError::simple(ApiErrorKind::InvalidToolArguments))?;
        schema_validate::validate(&tool.parameters, &arguments)
            .map_err(|error| ApiError::with_schema(ApiErrorKind::InvalidToolArguments, error))?;
        let signature = format!("{}:{}", call.name, canonical_json(&arguments));
        result.push(ValidatedToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments,
            signature,
        });
    }
    Ok(result)
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let fields = keys
                .into_iter()
                .map(|key| {
                    let encoded = serde_json::to_string(key).unwrap_or_else(|_| "\"?\"".into());
                    format!("{encoded}:{}", canonical_json(&object[key]))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

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
    let (code, param, message) = if let Some(error) = value.get("error") {
        if let Some(text) = error.as_str() {
            (String::new(), String::new(), text.to_ascii_lowercase())
        } else {
            (
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
                error
                    .get("param")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
            )
        }
    } else {
        (
            value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase(),
            value
                .get("param")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase(),
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase(),
        )
    };
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
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread;

    #[derive(Clone)]
    struct LocalRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl LocalRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    struct LocalServer {
        addr: SocketAddr,
        requests: Arc<std::sync::Mutex<Vec<LocalRequest>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl LocalServer {
        fn start<F>(expected: usize, handler: F) -> Self
        where
            F: Fn(&LocalRequest, usize) -> (u16, Vec<u8>) + Send + 'static,
        {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test server");
            let addr = listener.local_addr().expect("local server addr");
            listener
                .set_nonblocking(true)
                .expect("nonblocking local listener");
            let requests = Arc::new(std::sync::Mutex::new(Vec::<LocalRequest>::new()));
            let captured = Arc::clone(&requests);
            let handle = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut served = 0usize;
                while served < expected && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("blocking local stream");
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            let request = read_local_request(&mut stream);
                            let (status, body) = handler(&request, served);
                            write_local_response(&mut stream, status, &body);
                            captured.lock().expect("local requests").push(request);
                            served += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                requests,
                handle: Some(handle),
            }
        }

        fn join(mut self) -> Vec<LocalRequest> {
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join local test server");
            }
            let requests = match Arc::try_unwrap(self.requests) {
                Ok(requests) => requests.into_inner().expect("local requests mutex"),
                Err(_) => panic!("local requests are still shared"),
            };
            requests
        }
    }

    fn read_local_request(stream: &mut TcpStream) -> LocalRequest {
        let mut data = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read local request");
            if read == 0 {
                panic!("local connection closed before headers");
            }
            data.extend_from_slice(&chunk[..read]);
            if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if data.len() > 64 * 1024 {
                panic!("local request headers too large");
            }
        };
        let head = std::str::from_utf8(&data[..header_end - 4]).expect("ASCII local headers");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("local request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("local method").to_string();
        let path = parts.next().expect("local path").to_string();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let content_length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = data[header_end..].to_vec();
        while body.len() < content_length {
            let read = stream.read(&mut chunk).expect("read local request body");
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);
        LocalRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn write_local_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let head = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }

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
    fn canonical_json_ignores_object_key_order() {
        let first = json!({"a":1,"b":{"z":2,"y":3}});
        let second = json!({"b":{"y":3,"z":2},"a":1});
        assert_eq!(canonical_json(&first), canonical_json(&second));
    }

    #[test]
    fn error_redaction_never_contains_provider_body_or_key() {
        let key = "sk-synthetic-key-echo-9f4a";
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
        let server = LocalServer::start(2, |_, index| {
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
            (
                200,
                serde_json::to_vec(&value).expect("local response JSON"),
            )
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
        let server = LocalServer::start(1, |_, _| {
            (
                200,
                br#"{"data":[{"id":"local-model-a"},{"id":"local-model-b"}]}"#.to_vec(),
            )
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
