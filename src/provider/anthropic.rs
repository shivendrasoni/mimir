use std::{collections::BTreeMap, fmt, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ThinkingLevel, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.75";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicCredentialKind {
    ApiKey,
    OAuthToken,
    CloudflareGateway,
}

pub struct AnthropicProvider {
    base_url: String,
    credential: SecretString,
    credential_kind: AnthropicCredentialKind,
    static_headers: HeaderMap,
    client: reqwest::Client,
    max_retries: u32,
}

impl AnthropicProvider {
    /// Creates a native Anthropic Messages API transport.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank key, invalid URL,
    /// or HTTP client construction failure.
    pub fn new(base_url: Option<&str>, api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_credential_kind(base_url, api_key, AnthropicCredentialKind::ApiKey)
    }

    /// Creates an Anthropic-compatible transport with an explicit credential
    /// protocol. The credential's source, not its string shape, selects the
    /// header semantics so API keys and OAuth access tokens are never treated
    /// interchangeably.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank credential,
    /// invalid URL, or HTTP client construction failure.
    pub fn with_credential_kind(
        base_url: Option<&str>,
        credential: impl Into<String>,
        credential_kind: AnthropicCredentialKind,
    ) -> Result<Self, ProviderError> {
        Self::with_credential_kind_and_headers(
            base_url,
            credential,
            credential_kind,
            &BTreeMap::new(),
        )
    }

    /// Creates an explicitly authenticated Anthropic-compatible transport and
    /// applies cataloged non-authentication headers such as Kimi's user agent.
    /// Authentication and protocol headers cannot be overridden.
    ///
    /// # Errors
    ///
    /// Returns a sanitized protocol error for invalid or protected headers, in
    /// addition to the constructor errors from [`Self::with_credential_kind`].
    pub fn with_credential_kind_and_headers(
        base_url: Option<&str>,
        credential: impl Into<String>,
        credential_kind: AnthropicCredentialKind,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        let credential = credential.into();
        if credential.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
        reqwest::Url::parse(base_url).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Anthropic base URL: {error}"),
        })?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build Anthropic HTTP client: {error}"),
            })?;
        let static_headers = validated_static_headers(headers)?;
        Ok(Self {
            base_url: base_url.into(),
            credential: SecretString::from(credential),
            credential_kind,
            static_headers,
            client,
            max_retries: 2,
        })
    }

    #[must_use]
    pub fn request_preview(&self, request: &ModelRequest) -> Value {
        build_request_body(request, self.credential_kind)
    }

    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/v1/messages") {
            self.base_url.clone()
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }

    fn request(&self, request: &ModelRequest, streaming: bool) -> reqwest::RequestBuilder {
        let mut body = build_request_body(request, self.credential_kind);
        if streaming {
            body["stream"] = Value::Bool(true);
        }
        let builder = self
            .client
            .post(self.endpoint())
            .header("anthropic-version", API_VERSION)
            .headers(self.static_headers.clone())
            .json(&body);
        let mut builder = match self.credential_kind {
            AnthropicCredentialKind::ApiKey => {
                builder.header("x-api-key", self.credential.expose_secret())
            }
            AnthropicCredentialKind::OAuthToken => builder
                .bearer_auth(self.credential.expose_secret())
                .header("anthropic-beta", OAUTH_BETA)
                .header("user-agent", CLAUDE_CODE_USER_AGENT)
                .header("x-app", "cli"),
            AnthropicCredentialKind::CloudflareGateway => builder.header(
                "cf-aig-authorization",
                format!("Bearer {}", self.credential.expose_secret()),
            ),
        };
        if streaming {
            builder = builder.header("accept", "text/event-stream");
        }
        builder
    }

    async fn send_once(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .request(request, false)
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        let body = read_bounded_body(response).await?;
        classify_status(status, &body, self.credential.expose_secret())?;
        parse_response(&body)
    }

    async fn stream_once(
        &self,
        request: &ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self
            .request(request, true)
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_bounded_body(response).await?;
            classify_status(status, &body, self.credential.expose_secret())?;
            return Err(ProviderError::Protocol {
                message: format!("Anthropic returned HTTP {status}"),
            });
        }

        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut received = 0_usize;
        let mut accumulator = StreamAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| classify_transport(&error))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(ProviderError::Protocol {
                    message: "Anthropic stream exceeds the 8 MiB limit".into(),
                });
            }
            pending.extend_from_slice(&chunk);
            while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<_> = pending.drain(..=position).collect();
                trim_newline(&mut line);
                apply_sse_line(&mut accumulator, &line, sink).await?;
            }
        }
        if !pending.is_empty() {
            trim_newline(&mut pending);
            apply_sse_line(&mut accumulator, &pending, sink).await?;
        }
        accumulator.finish()
    }
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("credential", &"[REDACTED]")
            .field("credential_kind", &self.credential_kind)
            .field(
                "static_headers",
                &self.static_headers.keys().collect::<Vec<_>>(),
            )
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

fn validated_static_headers(
    headers: &BTreeMap<String, String>,
) -> Result<HeaderMap, ProviderError> {
    const PROTECTED: &[&str] = &[
        "authorization",
        "x-api-key",
        "cf-aig-authorization",
        "anthropic-version",
        "anthropic-beta",
        "x-app",
    ];
    let mut result = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let normalized = name.to_ascii_lowercase();
        if PROTECTED.contains(&normalized.as_str()) {
            return Err(ProviderError::Protocol {
                message: format!(
                    "catalog header {normalized} cannot override provider authentication"
                ),
            });
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| ProviderError::Protocol {
                message: "catalog contains an invalid HTTP header name".into(),
            })?;
        let value = HeaderValue::from_str(value).map_err(|_| ProviderError::Protocol {
            message: "catalog contains an invalid HTTP header value".into(),
        })?;
        result.insert(name, value);
    }
    Ok(result)
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let mut attempt = 0_u32;
        loop {
            match self.send_once(&request).await {
                Ok(response) => return Ok(response),
                Err(error) if error.is_retryable() && attempt < self.max_retries => {
                    let delay = 200_u64.saturating_mul(1_u64 << attempt.min(8));
                    tokio::time::sleep(Duration::from_millis(delay.min(5_000))).await;
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_once(&request, sink).await
    }
}

fn build_request_body(request: &ModelRequest, credential_kind: AnthropicCredentialKind) -> Value {
    let mut system = Vec::<String>::new();
    if credential_kind == AnthropicCredentialKind::OAuthToken {
        system.push(CLAUDE_CODE_IDENTITY.into());
    }
    if !request.system_prompt.trim().is_empty() {
        system.push(request.system_prompt.trim().into());
    }
    let mut messages = Vec::new();
    for message in anthropic_message_sequence(&request.messages) {
        if message.role == Role::System {
            let text = message.text();
            if !text.trim().is_empty() {
                system.push(text);
            }
            continue;
        }
        if let Some(message) = translate_message(&message) {
            messages.push(message);
        }
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "messages": messages
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    apply_thinking(&mut body, request);
    body
}

/// Rebuilds tool exchanges into the adjacency required by the Messages API.
///
/// A session can be resumed after an assistant tool-call turn was persisted but
/// before the corresponding tool result was written. Compaction can also leave
/// the two sides on opposite sides of its retained suffix. Anthropic rejects
/// either history shape, so preserve completed exchanges as one user result
/// turn and make an interrupted exchange explicit to the model.
fn anthropic_message_sequence(messages: &[Message]) -> Vec<Message> {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut index = 0;

    while let Some(message) = messages.get(index) {
        normalized.push(message.clone());
        index += 1;

        if message.role != Role::Assistant {
            continue;
        }
        let calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            continue;
        }

        let mut results = Vec::new();
        while let Some(result_message) = messages.get(index) {
            if result_message.role != Role::Tool {
                break;
            }
            results.extend(
                result_message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolResult(result)
                            if calls.iter().any(|call| call.id == result.tool_call_id) =>
                        {
                            Some(Content::ToolResult(result.clone()))
                        }
                        _ => None,
                    }),
            );
            index += 1;
        }

        for call in calls {
            if !results.iter().any(|content| {
                matches!(content, Content::ToolResult(result) if result.tool_call_id == call.id)
            }) {
                results.push(Content::ToolResult(crate::model::ToolResult {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: "Tool execution did not complete before this session was resumed. Retry the work if it is still needed.".into(),
                    is_error: true,
                }));
            }
        }
        normalized.push(Message::user_content(results));
    }

    normalized
}

fn translate_message(message: &Message) -> Option<Value> {
    let mut blocks = Vec::new();
    match message.role {
        Role::User => {
            for content in &message.content {
                match content {
                    Content::Text { text }
                    | Content::Thinking {
                        text,
                        signature: None,
                        ..
                    } if !text.is_empty() => {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    Content::Image { data, mime_type } => blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": data
                        }
                    })),
                    Content::ToolResult(result) => blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": normalize_tool_call_id(&result.tool_call_id),
                        "content": result.content,
                        "is_error": result.is_error
                    })),
                    Content::Text { .. } | Content::Thinking { .. } | Content::ToolCall(_) => {}
                }
            }
        }
        Role::Assistant => {
            for content in &message.content {
                match content {
                    Content::Text { text } if !text.is_empty() => {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    Content::Thinking {
                        signature: Some(signature),
                        redacted: true,
                        ..
                    } => blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": signature
                    })),
                    Content::Thinking {
                        text,
                        signature: Some(signature),
                        redacted: false,
                    } => blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature
                    })),
                    Content::ToolCall(call) => blocks.push(json!({
                        "type": "tool_use",
                        "id": normalize_tool_call_id(&call.id),
                        "name": call.name,
                        "input": call.arguments
                    })),
                    Content::Text { .. }
                    | Content::Image { .. }
                    | Content::Thinking { .. }
                    | Content::ToolResult(_) => {}
                }
            }
        }
        Role::Tool => {
            for content in &message.content {
                if let Content::ToolResult(result) = content {
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": normalize_tool_call_id(&result.tool_call_id),
                        "content": result.content,
                        "is_error": result.is_error
                    }));
                }
            }
        }
        Role::System => return None,
    }
    if blocks.is_empty() {
        None
    } else {
        Some(json!({
            "role": if message.role == Role::Assistant { "assistant" } else { "user" },
            "content": blocks
        }))
    }
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn apply_thinking(body: &mut Value, request: &ModelRequest) {
    if request.thinking_level == ThinkingLevel::Off {
        if supports_adaptive_thinking(&request.model)
            && !is_always_on_thinking_model(&request.model)
        {
            body["thinking"] = json!({"type": "disabled"});
        }
        return;
    }
    if supports_adaptive_thinking(&request.model) {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        body["output_config"] = json!({"effort": thinking_effort(request)});
        return;
    }
    if request.max_output_tokens <= 1_024 {
        return;
    }
    let requested = match request.thinking_level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 4_096,
        ThinkingLevel::High => 8_192,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => 16_384,
    };
    let budget = requested.min(request.max_output_tokens.saturating_sub(1));
    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget, "display": "summarized"});
}

fn is_always_on_thinking_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("fable-5") || model.contains("mythos-5") || model.contains("mythos-preview")
}

fn supports_adaptive_thinking(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "opus-4-6",
        "opus-4.6",
        "opus-4-7",
        "opus-4.7",
        "opus-4-8",
        "opus-4.8",
        "opus-5",
        "sonnet-4-6",
        "sonnet-4.6",
        "sonnet-5",
        "fable-5",
        "mythos-5",
        "mythos-preview",
    ]
    .iter()
    .any(|fragment| model.contains(fragment))
}

fn thinking_effort(request: &ModelRequest) -> &'static str {
    match request.thinking_effort.as_deref() {
        Some("low" | "minimal") => "low",
        Some("medium") => "medium",
        Some("high") => "high",
        Some("xhigh") => "xhigh",
        Some("max") => "max",
        _ => match request.thinking_level {
            ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        },
    }
}

fn parse_response(body: &[u8]) -> Result<ModelResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Anthropic JSON response: {error}"),
    })?;
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol {
            message: "Anthropic response has no content array".into(),
        })?;
    let mut content = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    content.push(Content::Text { text: text.into() });
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    let signature = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if !text.is_empty() || signature.is_some() {
                        content.push(Content::Thinking {
                            text: text.into(),
                            signature,
                            redacted: false,
                        });
                    }
                }
            }
            Some("redacted_thinking") => {
                if let Some(data) = block.get("data").and_then(Value::as_str) {
                    content.push(Content::Thinking {
                        text: "[Reasoning redacted]".into(),
                        signature: Some(data.into()),
                        redacted: true,
                    });
                }
            }
            Some("tool_use") => content.push(Content::ToolCall(ToolCall {
                id: required_string(block, "/id", "tool use id")?,
                name: required_string(block, "/name", "tool use name")?,
                arguments: block
                    .get("input")
                    .cloned()
                    .ok_or_else(|| ProviderError::Protocol {
                        message: "Anthropic tool use has no input".into(),
                    })?,
            })),
            _ => {}
        }
    }
    let mut message = Message::assistant(
        content,
        parse_stop_reason(
            value
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn"),
        ),
    );
    message.usage = parse_usage(value.get("usage"));
    Ok(ModelResponse {
        message,
        response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn required_string(value: &Value, pointer: &str, label: &str) -> Result<String, ProviderError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Protocol {
            message: format!("Anthropic response is missing {label}"),
        })
}

fn parse_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let value = value.unwrap_or(&Value::Null);
    let uncached_input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = value
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            value
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    Usage::from_separate_cached_input(uncached_input_tokens, output_tokens, cached_tokens)
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    blocks: BTreeMap<u64, StreamBlock>,
    usage: Usage,
    stop_reason: StopReason,
}

#[derive(Default)]
enum StreamBlock {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
        redacted: bool,
    },
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
    #[default]
    Ignored,
}

impl StreamAccumulator {
    async fn apply(
        &mut self,
        value: &Value,
        sink: &dyn ProviderEventSink,
    ) -> Result<(), ProviderError> {
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = &value["message"];
                self.response_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
                self.usage = parse_usage(message.get("usage"));
            }
            Some("content_block_start") => self.start_block(value)?,
            Some("content_block_delta") => self.apply_block_delta(value, sink).await,
            Some("message_delta") => self.apply_message_delta(value),
            Some("error") => return Err(classify_stream_error(value)),
            _ => {}
        }
        Ok(())
    }

    fn start_block(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block = &value["content_block"];
        let stream_block = match block.get("type").and_then(Value::as_str) {
            Some("text") => StreamBlock::Text(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            ),
            Some("thinking") => StreamBlock::Thinking {
                text: block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                signature: block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                redacted: false,
            },
            Some("redacted_thinking") => StreamBlock::Thinking {
                text: "[Reasoning redacted]".into(),
                signature: Some(required_string(block, "/data", "redacted thinking data")?),
                redacted: true,
            },
            Some("tool_use") => StreamBlock::Tool {
                id: required_string(block, "/id", "streamed tool use id")?,
                name: required_string(block, "/name", "streamed tool use name")?,
                arguments: String::new(),
            },
            _ => StreamBlock::Ignored,
        };
        self.blocks.insert(index, stream_block);
        Ok(())
    }

    async fn apply_block_delta(&mut self, value: &Value, sink: &dyn ProviderEventSink) {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        let delta = &value["delta"];
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.blocks.entry(index).or_default() {
                    StreamBlock::Text(accumulated) => accumulated.push_str(text),
                    block => *block = StreamBlock::Text(text.into()),
                }
                if !text.is_empty() {
                    sink.emit(ProviderEvent::TextDelta(text.into())).await;
                }
            }
            Some("thinking_delta") => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.blocks.entry(index).or_default() {
                    StreamBlock::Thinking {
                        text: accumulated, ..
                    } => accumulated.push_str(text),
                    block => {
                        *block = StreamBlock::Thinking {
                            text: text.into(),
                            signature: None,
                            redacted: false,
                        };
                    }
                }
                if !text.is_empty() {
                    sink.emit(ProviderEvent::ThinkingDelta(text.into())).await;
                }
            }
            Some("input_json_delta") => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(StreamBlock::Tool { arguments, .. }) = self.blocks.get_mut(&index) {
                    arguments.push_str(partial);
                }
            }
            Some("signature_delta") => {
                let signature = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(StreamBlock::Thinking {
                    signature: stored, ..
                }) = self.blocks.get_mut(&index)
                {
                    *stored = signature;
                }
            }
            _ => {}
        }
    }

    fn apply_message_delta(&mut self, value: &Value) {
        if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = parse_stop_reason(reason);
        }
        let usage = parse_usage(value.get("usage"));
        self.usage.output_tokens = usage.output_tokens;
        if usage.input_tokens != 0 {
            self.usage.input_tokens = usage.input_tokens;
        }
        if usage.cached_tokens != 0 {
            self.usage.cached_tokens = usage.cached_tokens;
        }
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut content = Vec::new();
        for (_, block) in self.blocks {
            match block {
                StreamBlock::Text(text) if !text.is_empty() => {
                    content.push(Content::Text { text });
                }
                StreamBlock::Thinking {
                    text,
                    signature,
                    redacted,
                } if !text.is_empty() || signature.is_some() => {
                    content.push(Content::Thinking {
                        text,
                        signature,
                        redacted,
                    });
                }
                StreamBlock::Tool {
                    id,
                    name,
                    arguments,
                } => {
                    let arguments = if arguments.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&arguments).map_err(|error| {
                            ProviderError::Protocol {
                                message: format!(
                                    "Anthropic streamed tool input is invalid JSON: {error}"
                                ),
                            }
                        })?
                    };
                    content.push(Content::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
                StreamBlock::Text(_) | StreamBlock::Thinking { .. } | StreamBlock::Ignored => {}
            }
        }
        let mut message = Message::assistant(content, self.stop_reason);
        message.usage = self.usage;
        Ok(ModelResponse {
            message,
            response_id: self.response_id,
        })
    }
}

async fn apply_sse_line(
    accumulator: &mut StreamAccumulator,
    line: &[u8],
    sink: &dyn ProviderEventSink,
) -> Result<(), ProviderError> {
    let Some(payload) = line.strip_prefix(b"data:") else {
        return Ok(());
    };
    let payload = payload.strip_prefix(b" ").unwrap_or(payload);
    if payload.is_empty() {
        return Ok(());
    }
    let value: Value =
        serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Anthropic stream event: {error}"),
        })?;
    accumulator.apply(&value, sink).await
}

fn classify_stream_error(value: &Value) -> ProviderError {
    let kind = value.pointer("/error/type").and_then(Value::as_str);
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Anthropic stream error")
        .chars()
        .take(300)
        .collect();
    match kind {
        Some("authentication_error" | "permission_error") => ProviderError::Authentication,
        Some("rate_limit_error") => ProviderError::RateLimited { message },
        Some("overloaded_error" | "api_error") => ProviderError::Unavailable { message },
        _ => ProviderError::Protocol { message },
    }
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::Protocol {
            message: "Anthropic response exceeds the 8 MiB limit".into(),
        });
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| classify_transport(&error))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::Protocol {
                message: "Anthropic response exceeds the 8 MiB limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_status(
    status: reqwest::StatusCode,
    body: &[u8],
    credential: &str,
) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(ProviderError::Authentication);
    }
    let message = safe_error_excerpt(body, credential);
    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited { message });
    }
    if status.is_server_error() || matches!(status.as_u16(), 408 | 529) {
        return Err(ProviderError::Unavailable {
            message: format!("HTTP {status}: {message}"),
        });
    }
    Err(ProviderError::Protocol {
        message: format!("HTTP {status}: {message}"),
    })
}

fn classify_transport(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() || error.is_connect() {
        ProviderError::Unavailable {
            message: error.to_string(),
        }
    } else {
        ProviderError::Protocol {
            message: error.to_string(),
        }
    }
}

fn safe_error_excerpt(body: &[u8], credential: &str) -> String {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Anthropic returned an error".into());
    let redacted = if credential.is_empty() {
        message
    } else {
        message.replace(credential, "[REDACTED]")
    };
    redacted.chars().take(300).collect()
}

fn trim_newline(line: &mut Vec<u8>) {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool_call(id: &str) -> Content {
        Content::ToolCall(ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
        })
    }

    #[test]
    fn repairs_an_interrupted_tool_exchange_before_replay() {
        let messages = vec![
            Message::user("inspect the README"),
            Message::assistant(vec![tool_call("toolu_missing")], StopReason::ToolUse),
            Message::user("continue"),
        ];

        let normalized = anthropic_message_sequence(&messages);

        assert_eq!(normalized.len(), 4);
        assert!(matches!(
            &normalized[2].content[..],
            [Content::ToolResult(result)]
                if result.tool_call_id == "toolu_missing" && result.is_error
        ));
        assert_eq!(normalized[3].text(), "continue");
    }

    #[test]
    fn combines_multiple_persisted_tool_results_after_one_tool_turn() {
        let messages = vec![
            Message::assistant(
                vec![tool_call("toolu_first"), tool_call("toolu_second")],
                StopReason::ToolUse,
            ),
            Message::tool_result("toolu_first", "read_file", "first", false),
            Message::tool_result("toolu_second", "read_file", "second", false),
        ];

        let normalized = anthropic_message_sequence(&messages);

        assert_eq!(normalized.len(), 2);
        assert!(matches!(
            &normalized[1].content[..],
            [Content::ToolResult(first), Content::ToolResult(second)]
                if first.tool_call_id == "toolu_first" && second.tool_call_id == "toolu_second"
        ));
    }
}
