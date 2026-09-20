use std::{collections::BTreeMap, fmt, net::IpAddr, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ThinkingLevel, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 4_000;

/// Native transport for Mistral's Conversations (`chat/completions`) API.
pub struct MistralProvider {
    base_url: String,
    credential: SecretString,
    static_headers: HeaderMap,
    client: reqwest::Client,
}

impl MistralProvider {
    /// Creates a Mistral transport using Bearer authentication.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank key, invalid
    /// base URL, or HTTP client construction failure.
    pub fn new(base_url: Option<&str>, api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_headers(base_url, api_key, &BTreeMap::new())
    }

    /// Creates a Mistral transport with catalog-provided, non-authentication
    /// headers. Authentication and content negotiation headers cannot be
    /// overridden.
    ///
    /// # Errors
    ///
    /// Returns a sanitized protocol error for invalid or protected headers, in
    /// addition to the errors returned by [`Self::new`].
    pub fn with_headers(
        base_url: Option<&str>,
        api_key: impl Into<String>,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
        let parsed = reqwest::Url::parse(base_url).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Mistral base URL: {error}"),
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ProviderError::Protocol {
                message: "Mistral base URL must use HTTP or HTTPS".into(),
            });
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(ProviderError::Protocol {
                message: "Mistral base URL cannot contain user information".into(),
            });
        }
        if parsed.scheme() == "http" && !is_loopback_url(&parsed) {
            return Err(ProviderError::Protocol {
                message: "Mistral base URL must use HTTPS outside loopback tests".into(),
            });
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build Mistral HTTP client: {error}"),
            })?;
        Ok(Self {
            base_url: base_url.into(),
            credential: SecretString::from(api_key),
            static_headers: validated_static_headers(headers)?,
            client,
        })
    }

    #[must_use]
    pub fn request_preview(&self, request: &ModelRequest) -> Value {
        build_request_body(request, false)
    }

    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/v1/chat/completions")
            || self.base_url.ends_with("/chat/completions")
        {
            self.base_url.clone()
        } else if self.base_url.ends_with("/v1") {
            format!("{}/chat/completions", self.base_url)
        } else {
            format!("{}/v1/chat/completions", self.base_url)
        }
    }

    fn request(&self, request: &ModelRequest, streaming: bool) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(self.endpoint())
            .bearer_auth(self.credential.expose_secret())
            .headers(self.static_headers.clone())
            .json(&build_request_body(request, streaming));
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
                message: format!("Mistral returned HTTP {status}"),
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
                    message: "Mistral stream exceeds the 8 MiB limit".into(),
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

impl fmt::Debug for MistralProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MistralProvider")
            .field("base_url", &self.base_url)
            .field("credential", &"[REDACTED]")
            .field(
                "static_headers",
                &self.static_headers.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for MistralProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        // The reference transport explicitly disables SDK retries. The agent
        // runtime owns retry policy and can account for budget/cancellation.
        self.send_once(&request).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        // A streamed request is never retried after bytes may have reached the
        // caller; doing so could duplicate text or tool calls.
        self.stream_once(&request, sink).await
    }
}

fn validated_static_headers(
    headers: &BTreeMap<String, String>,
) -> Result<HeaderMap, ProviderError> {
    const PROTECTED: &[&str] = &["authorization", "content-type", "accept"];
    let mut result = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let normalized = name.to_ascii_lowercase();
        if PROTECTED.contains(&normalized.as_str()) {
            return Err(ProviderError::Protocol {
                message: format!(
                    "catalog header {normalized} cannot override Mistral transport headers"
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

fn build_request_body(request: &ModelRequest, streaming: bool) -> Value {
    let mut normalizer = ToolIdNormalizer::default();
    let supports_images = model_supports_images(&request.model);
    let mut messages = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        messages.push(json!({"role": "system", "content": request.system_prompt}));
    }
    for message in &request.messages {
        append_message(&mut messages, message, supports_images, &mut normalizer);
    }

    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": false
                }
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "stream": streaming,
        "messages": messages,
        "max_tokens": request.max_output_tokens
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if request.thinking_level != ThinkingLevel::Off {
        if uses_reasoning_effort(&request.model) {
            body["reasoning_effort"] = Value::String(
                request
                    .thinking_effort
                    .as_deref()
                    .filter(|effort| matches!(*effort, "none" | "high"))
                    .unwrap_or("high")
                    .into(),
            );
        } else {
            body["prompt_mode"] = Value::String("reasoning".into());
        }
    }
    body
}

fn append_message(
    messages: &mut Vec<Value>,
    message: &Message,
    supports_images: bool,
    normalizer: &mut ToolIdNormalizer,
) {
    match message.role {
        Role::System => messages.push(json!({"role": "system", "content": message.text()})),
        Role::User => messages.push(json!({
            "role": "user",
            "content": user_content(message, supports_images)
        })),
        Role::Assistant => append_assistant_message(messages, message, normalizer),
        Role::Tool => {
            for block in &message.content {
                if let Content::ToolResult(result) = block {
                    let content = if result.content.trim().is_empty() {
                        if result.is_error {
                            "[tool error] (no tool output)".into()
                        } else {
                            "(no tool output)".into()
                        }
                    } else if result.is_error {
                        format!("[tool error] {}", result.content)
                    } else {
                        result.content.clone()
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": normalizer.normalize(&result.tool_call_id),
                        "name": result.tool_name,
                        "content": [{"type": "text", "text": content}]
                    }));
                }
            }
        }
    }
}

fn user_content(message: &Message, supports_images: bool) -> Value {
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, Content::Image { .. }));
    if !has_images {
        return Value::String(message.text());
    }
    let parts: Vec<_> = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(json!({"type": "text", "text": text})),
            Content::Image { data, mime_type } if supports_images => Some(json!({
                "type": "image_url",
                "image_url": format!("data:{mime_type};base64,{data}")
            })),
            Content::Image { .. }
            | Content::Thinking { .. }
            | Content::ToolCall(_)
            | Content::ToolResult(_) => None,
        })
        .collect();
    if parts.is_empty() {
        Value::String("(image omitted: model does not support images)".into())
    } else {
        Value::Array(parts)
    }
}

fn append_assistant_message(
    messages: &mut Vec<Value>,
    message: &Message,
    normalizer: &mut ToolIdNormalizer,
) {
    let content: Vec<_> = message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } if !text.trim().is_empty() => {
                Some(json!({"type": "text", "text": text}))
            }
            Content::Thinking {
                text,
                redacted: false,
                ..
            } if !text.trim().is_empty() => Some(json!({
                "type": "thinking",
                "thinking": [{"type": "text", "text": text}]
            })),
            Content::Text { .. }
            | Content::Image { .. }
            | Content::Thinking { .. }
            | Content::ToolCall(_)
            | Content::ToolResult(_) => None,
        })
        .collect();
    let tool_calls: Vec<_> = message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::ToolCall(call) => Some(json!({
                "id": normalizer.normalize(&call.id),
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": call.arguments.to_string()
                }
            })),
            _ => None,
        })
        .collect();
    if content.is_empty() && tool_calls.is_empty() {
        return;
    }
    let mut value = json!({"role": "assistant"});
    if !content.is_empty() {
        value["content"] = Value::Array(content);
    }
    if !tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(tool_calls);
    }
    messages.push(value);
}

fn model_supports_images(model: &str) -> bool {
    super::registry::model_catalog()
        .iter()
        .find(|entry| entry.provider == "mistral" && entry.id == model)
        .is_none_or(|entry| entry.input.iter().any(|kind| kind == "image"))
}

fn uses_reasoning_effort(model: &str) -> bool {
    matches!(
        model,
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[derive(Default)]
struct ToolIdNormalizer {
    original_to_normalized: BTreeMap<String, String>,
    normalized_to_original: BTreeMap<String, String>,
}

impl ToolIdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.original_to_normalized.get(id) {
            return existing.clone();
        }
        let alphanumeric: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
        let mut attempt = 0_u32;
        loop {
            let candidate = if attempt == 0 && alphanumeric.len() == 9 {
                alphanumeric.clone()
            } else {
                let base = if alphanumeric.is_empty() {
                    id
                } else {
                    &alphanumeric
                };
                let seed = if attempt == 0 {
                    base.to_owned()
                } else {
                    format!("{base}:{attempt}")
                };
                short_hash(&seed)
            };
            if self
                .normalized_to_original
                .get(&candidate)
                .is_none_or(|owner| owner == id)
            {
                self.original_to_normalized
                    .insert(id.into(), candidate.clone());
                self.normalized_to_original
                    .insert(candidate.clone(), id.into());
                return candidate;
            }
            attempt = attempt.saturating_add(1);
        }
    }
}

fn short_hash(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(9);
    for byte in digest.iter().take(5) {
        output.push(HEX[usize::from(byte >> 4)] as char);
        if output.len() == 9 {
            break;
        }
        output.push(HEX[usize::from(byte & 0x0f)] as char);
        if output.len() == 9 {
            break;
        }
    }
    output
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::Protocol {
            message: "Mistral response exceeds the 8 MiB limit".into(),
        });
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| classify_transport(&error))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::Protocol {
                message: "Mistral response exceeds the 8 MiB limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_status(
    status: reqwest::StatusCode,
    body: &[u8],
    secret: &str,
) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(ProviderError::Authentication);
    }
    let message = safe_error_excerpt(body, secret);
    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited { message });
    }
    if status.is_server_error() {
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

fn safe_error_excerpt(body: &[u8], secret: &str) -> String {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("Mistral returned an error");
    let redacted = if secret.is_empty() {
        message.into()
    } else {
        message.replace(secret, "[REDACTED]")
    };
    redacted.chars().take(MAX_ERROR_CHARS).collect()
}

fn parse_response(body: &[u8]) -> Result<ModelResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Mistral JSON response: {error}"),
    })?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::Protocol {
            message: "Mistral response has no choices".into(),
        })?;
    let upstream = choice
        .get("message")
        .ok_or_else(|| ProviderError::Protocol {
            message: "Mistral response choice has no message".into(),
        })?;
    let mut content = parse_content(upstream.get("content"));
    if let Some(calls) = upstream.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            content.push(Content::ToolCall(parse_tool_call(call, index)?));
        }
    }
    let stop_reason = map_stop_reason(
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop"),
    );
    let usage = parse_usage(value.get("usage"), Usage::default());
    let mut message = Message::assistant(content, stop_reason);
    message.usage = usage;
    Ok(ModelResponse {
        message,
        response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn parse_content(value: Option<&Value>) -> Vec<Content> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(text) = value.as_str() {
        return (!text.is_empty())
            .then(|| Content::Text { text: text.into() })
            .into_iter()
            .collect();
    }
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            if let Some(text) = item.as_str() {
                return (!text.is_empty()).then(|| Content::Text { text: text.into() });
            }
            match item.get("type").and_then(Value::as_str) {
                Some("text") => item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| Content::Text { text: text.into() }),
                Some("thinking") => {
                    let text = thinking_text(item);
                    (!text.is_empty()).then_some(Content::Thinking {
                        text,
                        signature: None,
                        redacted: false,
                    })
                }
                _ => None,
            }
        })
        .collect()
}

fn thinking_text(item: &Value) -> String {
    let Some(thinking) = item.get("thinking") else {
        return String::new();
    };
    if let Some(text) = thinking.as_str() {
        return text.into();
    }
    thinking
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

fn parse_tool_call(call: &Value, index: usize) -> Result<ToolCall, ProviderError> {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && *id != "null")
        .map_or_else(|| short_hash(&format!("toolcall:{index}")), str::to_owned);
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ProviderError::Protocol {
            message: "Mistral tool call is missing a function name".into(),
        })?
        .into();
    let arguments = parse_arguments(call.pointer("/function/arguments"))?;
    Ok(ToolCall {
        id,
        name,
        arguments,
    })
}

fn parse_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    match value {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::String(arguments)) if arguments.trim().is_empty() => Ok(json!({})),
        Some(Value::String(arguments)) => {
            serde_json::from_str(arguments).map_err(|error| ProviderError::Protocol {
                message: format!("Mistral tool arguments are invalid JSON: {error}"),
            })
        }
        Some(Value::Object(arguments)) => Ok(Value::Object(arguments.clone())),
        Some(_) => Err(ProviderError::Protocol {
            message: "Mistral tool arguments must be a JSON object or encoded object".into(),
        }),
    }
}

fn parse_usage(value: Option<&Value>, previous: Usage) -> Usage {
    let Some(value) = value else {
        return previous;
    };
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(previous.input_tokens),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(previous.output_tokens),
        cached_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "length" | "model_length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        "error" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

#[derive(Default)]
struct ToolCallBuilder {
    index: u64,
    id: String,
    name: String,
    arguments: String,
}

enum StreamBlock {
    Text(String),
    Thinking(String),
    Tool(ToolCallBuilder),
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    blocks: Vec<StreamBlock>,
    tool_indexes: BTreeMap<u64, usize>,
    usage: Usage,
    stop_reason: StopReason,
}

impl StreamAccumulator {
    async fn apply(
        &mut self,
        envelope: &Value,
        sink: &dyn ProviderEventSink,
    ) -> Result<(), ProviderError> {
        let value = envelope.get("data").unwrap_or(envelope);
        if self.response_id.is_none() {
            self.response_id = value.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        self.usage = parse_usage(value.get("usage"), self.usage);
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = map_stop_reason(reason);
        }
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        self.apply_content(delta.get("content"), sink).await;
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.apply_tool_delta(call);
            }
        }
        Ok(())
    }

    async fn apply_content(&mut self, value: Option<&Value>, sink: &dyn ProviderEventSink) {
        let Some(value) = value else {
            return;
        };
        if let Some(text) = value.as_str() {
            self.push_text(text, sink).await;
            return;
        }
        let Some(items) = value.as_array() else {
            return;
        };
        for item in items {
            if let Some(text) = item.as_str() {
                self.push_text(text, sink).await;
                continue;
            }
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        self.push_text(text, sink).await;
                    }
                }
                Some("thinking") => {
                    let text = thinking_text(item);
                    self.push_thinking(&text, sink).await;
                }
                _ => {}
            }
        }
    }

    async fn push_text(&mut self, delta: &str, sink: &dyn ProviderEventSink) {
        if delta.is_empty() {
            return;
        }
        if let Some(StreamBlock::Text(text)) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(StreamBlock::Text(delta.into()));
        }
        sink.emit(ProviderEvent::TextDelta(delta.into())).await;
    }

    async fn push_thinking(&mut self, delta: &str, sink: &dyn ProviderEventSink) {
        if delta.is_empty() {
            return;
        }
        if let Some(StreamBlock::Thinking(text)) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(StreamBlock::Thinking(delta.into()));
        }
        sink.emit(ProviderEvent::ThinkingDelta(delta.into())).await;
    }

    fn apply_tool_delta(&mut self, call: &Value) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block_index = *self.tool_indexes.entry(index).or_insert_with(|| {
            self.blocks.push(StreamBlock::Tool(ToolCallBuilder {
                index,
                ..ToolCallBuilder::default()
            }));
            self.blocks.len() - 1
        });
        let StreamBlock::Tool(builder) = &mut self.blocks[block_index] else {
            return;
        };
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| *id != "null")
        {
            builder.id.push_str(id);
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            builder.name.push_str(name);
        }
        if let Some(arguments) = call.pointer("/function/arguments") {
            match arguments {
                Value::String(arguments) => builder.arguments.push_str(arguments),
                Value::Object(_) => builder.arguments.push_str(&arguments.to_string()),
                _ => {}
            }
        }
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut content = Vec::with_capacity(self.blocks.len());
        for block in self.blocks {
            match block {
                StreamBlock::Text(text) => content.push(Content::Text { text }),
                StreamBlock::Thinking(text) => content.push(Content::Thinking {
                    text,
                    signature: None,
                    redacted: false,
                }),
                StreamBlock::Tool(tool) => {
                    if tool.name.is_empty() {
                        return Err(ProviderError::Protocol {
                            message: "Mistral stream ended with an incomplete tool call".into(),
                        });
                    }
                    let id = if tool.id.is_empty() {
                        short_hash(&format!("toolcall:{index}", index = tool.index))
                    } else {
                        tool.id
                    };
                    let arguments = if tool.arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&tool.arguments).map_err(|error| {
                            ProviderError::Protocol {
                                message: format!(
                                    "Mistral streamed tool arguments are invalid JSON: {error}"
                                ),
                            }
                        })?
                    };
                    content.push(Content::ToolCall(ToolCall {
                        id,
                        name: tool.name,
                        arguments,
                    }));
                }
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

fn trim_newline(line: &mut Vec<u8>) {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        line.pop();
    }
}

async fn apply_sse_line(
    accumulator: &mut StreamAccumulator,
    line: &[u8],
    sink: &dyn ProviderEventSink,
) -> Result<(), ProviderError> {
    let Some(payload) = line
        .strip_prefix(b"data:")
        .map(|payload| payload.strip_prefix(b" ").unwrap_or(payload))
    else {
        return Ok(());
    };
    if payload == b"[DONE]" || payload.is_empty() {
        return Ok(());
    }
    let value: Value =
        serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Mistral stream event: {error}"),
        })?;
    if value.get("error").is_some() {
        return Err(ProviderError::Protocol {
            message: "Mistral stream reported an error".into(),
        });
    }
    accumulator.apply(&value, sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolDefinition;

    fn request(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.into(),
            thinking_level: ThinkingLevel::High,
            thinking_effort: None,
            system_prompt: "system".into(),
            messages: vec![Message::user("hello")],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type": "object"}),
            }],
            max_output_tokens: 512,
        }
    }

    #[test]
    fn reasoning_controls_match_model_family() {
        let provider = MistralProvider::new(None, "secret").expect("provider");
        let effort = provider.request_preview(&request("mistral-small-2603"));
        assert_eq!(effort["reasoning_effort"], "high");
        assert!(effort.get("prompt_mode").is_none());

        let prompt = provider.request_preview(&request("magistral-small"));
        assert_eq!(prompt["prompt_mode"], "reasoning");
        assert!(prompt.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_id_normalization_is_stable_and_nine_characters() {
        let mut normalizer = ToolIdNormalizer::default();
        let first = normalizer.normalize("call:long/identifier");
        assert_eq!(first.len(), 9);
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
        assert_eq!(normalizer.normalize("call:long/identifier"), first);
        assert_eq!(normalizer.normalize("123ABCxyz"), "123ABCxyz");
    }
}
