use std::{
    collections::BTreeMap,
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{RequestBuilder, Response, Url};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_AZURE_API_VERSION: &str = "v1";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 512;

#[derive(Clone)]
enum Authentication {
    Bearer(SecretString),
    ApiKey(SecretString),
    CloudflareGateway(SecretString),
}

impl Authentication {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::Bearer(secret) => request.bearer_auth(secret.expose_secret()),
            Self::ApiKey(secret) => request.header("api-key", secret.expose_secret()),
            Self::CloudflareGateway(secret) => request.header(
                "cf-aig-authorization",
                format!("Bearer {}", secret.expose_secret()),
            ),
        }
    }

    fn expose_secret(&self) -> &str {
        match self {
            Self::Bearer(secret) | Self::ApiKey(secret) | Self::CloudflareGateway(secret) => {
                secret.expose_secret()
            }
        }
    }
}

/// Native transport for OpenAI-compatible Responses API endpoints.
pub struct ResponsesProvider {
    endpoint: Url,
    authentication: Authentication,
    client: reqwest::Client,
    priority_service_tier: AtomicBool,
}

impl ResponsesProvider {
    /// Creates a bearer-authenticated Responses API transport.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for a blank key, or a protocol error for
    /// an invalid base URL or HTTP client configuration failure.
    pub fn new(base_url: Option<&str>, api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::build(
            base_url.unwrap_or(DEFAULT_BASE_URL),
            api_key.into(),
            false,
            None,
        )
    }

    /// Creates a Cloudflare AI Gateway Responses transport. The gateway token
    /// is sent only through `cf-aig-authorization`, not as an upstream `OpenAI`
    /// Bearer credential.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for a blank key, or a protocol error for
    /// an invalid base URL or HTTP client configuration failure.
    pub fn new_cloudflare_gateway(
        base_url: Option<&str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::build_with_authentication(
            base_url.unwrap_or(DEFAULT_BASE_URL),
            api_key.into(),
            false,
            None,
            true,
        )
    }

    /// Creates an Azure Responses API transport using Azure's `api-key` header.
    ///
    /// The API version defaults to `v1`. An existing `api-version` query
    /// parameter on the supplied base URL takes precedence.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for a blank key, or a protocol error for
    /// an invalid base URL, API version, or HTTP client configuration failure.
    pub fn new_azure(
        base_url: Option<&str>,
        api_key: impl Into<String>,
        api_version: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.ok_or_else(|| ProviderError::Protocol {
            message: "Azure OpenAI base URL is required".into(),
        })?;
        Self::build(base_url, api_key.into(), true, api_version)
    }

    fn build(
        base_url: &str,
        api_key: String,
        azure: bool,
        api_version: Option<&str>,
    ) -> Result<Self, ProviderError> {
        Self::build_with_authentication(base_url, api_key, azure, api_version, false)
    }

    fn build_with_authentication(
        base_url: &str,
        api_key: String,
        azure: bool,
        api_version: Option<&str>,
        cloudflare_gateway: bool,
    ) -> Result<Self, ProviderError> {
        if api_key.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let endpoint = responses_endpoint(base_url, azure, api_version)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ProviderError::Protocol {
                message: "cannot build Responses HTTP client".into(),
            })?;
        Ok(Self {
            endpoint,
            authentication: if cloudflare_gateway {
                Authentication::CloudflareGateway(SecretString::from(api_key))
            } else if azure {
                Authentication::ApiKey(SecretString::from(api_key))
            } else {
                Authentication::Bearer(SecretString::from(api_key))
            },
            client,
            priority_service_tier: AtomicBool::new(false),
        })
    }

    /// Streams a response while observing an explicit cancellation token.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Aborted`] when cancellation wins, otherwise the
    /// same provider errors as [`Provider::stream`].
    pub async fn stream_with_cancellation(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, sink, Some(&cancellation)).await
    }

    async fn stream_inner(
        &self,
        request: &ModelRequest,
        sink: &dyn ProviderEventSink,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ModelResponse, ProviderError> {
        let mut body = build_request_body(request);
        if self.priority_service_tier.load(Ordering::Acquire) {
            body["service_tier"] = json!("priority");
        }
        let request = self.authentication.apply(
            self.client
                .post(self.endpoint.clone())
                .header("accept", "text/event-stream")
                .json(&body),
        );
        let response = send_cancellable(request, cancellation).await?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::Authentication);
        }
        if !status.is_success() {
            let body = read_bounded_body(response, cancellation).await?;
            let message = safe_error_excerpt(&body, self.authentication.expose_secret());
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited { message });
            }
            if status.is_server_error() {
                return Err(ProviderError::Unavailable {
                    message: format!("HTTP {status}: {message}"),
                });
            }
            return Err(ProviderError::Protocol {
                message: format!("HTTP {status}: {message}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large());
        }

        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut accumulator = ResponsesAccumulator::default();
        let mut received = 0_usize;
        loop {
            let next = next_cancellable(&mut stream, cancellation).await?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| classify_transport(&error))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(response_too_large());
            }
            for payload in decoder.push(&chunk)? {
                accumulator
                    .apply_payload(&payload, sink, self.authentication.expose_secret())
                    .await?;
            }
        }
        for payload in decoder.finish()? {
            accumulator
                .apply_payload(&payload, sink, self.authentication.expose_secret())
                .await?;
        }
        accumulator.finish()
    }
}

impl fmt::Debug for ResponsesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesProvider")
            .field("endpoint", &redacted_url(&self.endpoint))
            .field("credential", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn set_service_tier(&self, tier: Option<&str>) -> Result<(), ProviderError> {
        let priority = match tier {
            None | Some("default") => false,
            Some("priority") => true,
            Some(value) => {
                return Err(ProviderError::Protocol {
                    message: format!("unsupported OpenAI service tier: {value}"),
                });
            }
        };
        self.priority_service_tier
            .store(priority, Ordering::Release);
        Ok(())
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, &NoopSink, None).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, sink, None).await
    }
}

struct NoopSink;

#[async_trait]
impl ProviderEventSink for NoopSink {
    async fn emit(&self, _event: ProviderEvent) {}
}

fn responses_endpoint(
    base_url: &str,
    azure: bool,
    api_version: Option<&str>,
) -> Result<Url, ProviderError> {
    let mut endpoint = Url::parse(base_url.trim()).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Responses base URL: {error}"),
    })?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(ProviderError::Protocol {
            message: "Responses base URL must use HTTP or HTTPS".into(),
        });
    }
    if azure && is_azure_host(&endpoint) {
        let path = endpoint.path().trim_end_matches('/');
        if path.is_empty() || path == "/openai" {
            endpoint.set_path("/openai/v1");
        }
    }
    let path = endpoint.path().trim_end_matches('/');
    if !path.ends_with("/responses") && path != "responses" {
        endpoint.set_path(&format!("{path}/responses"));
    }
    if azure && !endpoint.query_pairs().any(|(key, _)| key == "api-version") {
        let version = api_version.unwrap_or(DEFAULT_AZURE_API_VERSION).trim();
        if version.is_empty() {
            return Err(ProviderError::Protocol {
                message: "Azure OpenAI API version must not be blank".into(),
            });
        }
        endpoint
            .query_pairs_mut()
            .append_pair("api-version", version);
    }
    Ok(endpoint)
}

fn is_azure_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.ends_with(".openai.azure.com") || host.ends_with(".cognitiveservices.azure.com")
    })
}

fn build_request_body(request: &ModelRequest) -> Value {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::User => push_input_message(&mut input, message),
            Role::Assistant => push_assistant_message(&mut input, message),
            Role::Tool => push_tool_results(&mut input, message),
        }
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "store": false,
        "stream": true,
        "input": input,
        "max_output_tokens": request.max_output_tokens
    });
    if !request.system_prompt.trim().is_empty() {
        body["instructions"] = json!(request.system_prompt);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(effort) = request.thinking_effort.as_deref().or_else(|| {
        (request.thinking_level != crate::model::ThinkingLevel::Off)
            .then(|| request.thinking_level.as_str())
    }) {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

fn push_input_message(input: &mut Vec<Value>, message: &Message) {
    let content = message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(json!({"type": "input_text", "text": text})),
            Content::Image { data, mime_type } if message.role == Role::User => Some(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}")
            })),
            Content::Image { .. }
            | Content::Thinking { .. }
            | Content::ToolCall(_)
            | Content::ToolResult(_) => None,
        })
        .collect::<Vec<_>>();
    if !content.is_empty() {
        input.push(json!({
            "role": if message.role == Role::System { "system" } else { "user" },
            "content": content
        }));
    }
}

fn push_assistant_message(input: &mut Vec<Value>, message: &Message) {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(json!({
                "type": "output_text",
                "text": text,
                "annotations": []
            })),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !text.is_empty() {
        input.push(json!({"role": "assistant", "content": text}));
    }
    for block in &message.content {
        match block {
            Content::Thinking {
                signature: Some(signature),
                ..
            } => {
                if let Ok(item) = serde_json::from_str::<Value>(signature)
                    && item.get("type").and_then(Value::as_str) == Some("reasoning")
                {
                    input.push(item);
                }
            }
            Content::ToolCall(call) => input.push(json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments.to_string()
            })),
            Content::Text { .. }
            | Content::Image { .. }
            | Content::Thinking {
                signature: None, ..
            }
            | Content::ToolResult(_) => {}
        }
    }
}

fn push_tool_results(input: &mut Vec<Value>, message: &Message) {
    input.extend(message.content.iter().filter_map(|block| match block {
        Content::ToolResult(result) => Some(json!({
            "type": "function_call_output",
            "call_id": result.tool_call_id,
            "output": result.content
        })),
        _ => None,
    }));
}

#[derive(Default)]
struct ResponsesAccumulator {
    response_id: Option<String>,
    text: String,
    thinking: String,
    reasoning_signature: Option<String>,
    tools: BTreeMap<String, ToolBuilder>,
    output_indexes: BTreeMap<u64, String>,
    usage: Usage,
    terminal: Option<StopReason>,
}

#[derive(Default)]
struct ToolBuilder {
    call_id: String,
    name: String,
    arguments: String,
}

impl ResponsesAccumulator {
    async fn apply_payload(
        &mut self,
        payload: &[u8],
        sink: &dyn ProviderEventSink,
        secret: &str,
    ) -> Result<(), ProviderError> {
        if payload == b"[DONE]" || payload.is_empty() {
            return Ok(());
        }
        let value: Value =
            serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Responses stream event: {error}"),
            })?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.created" => self.set_response_id(&value["response"]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.thinking.push_str(delta);
                    sink.emit(ProviderEvent::ThinkingDelta(delta.into())).await;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    sink.emit(ProviderEvent::TextDelta(delta.into())).await;
                }
            }
            "response.output_item.added" => self.apply_output_item(&value, false),
            "response.function_call_arguments.delta" => self.apply_arguments_delta(&value),
            "response.function_call_arguments.done" => self.apply_arguments_done(&value),
            "response.output_item.done" => self.apply_output_item(&value, true),
            "response.completed" => self.apply_terminal(&value["response"], StopReason::Stop),
            "response.incomplete" => self.apply_terminal(&value["response"], StopReason::Length),
            "response.cancelled" => return Err(ProviderError::Aborted),
            "response.failed" => {
                return Err(ProviderError::Protocol {
                    message: safe_stream_error(&value, secret, "Responses request failed"),
                });
            }
            "error" => {
                let message = safe_stream_error(&value, secret, "Responses stream failed");
                if value
                    .get("code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| matches!(code, "rate_limit_exceeded" | "rate_limit_error"))
                {
                    return Err(ProviderError::RateLimited { message });
                }
                return Err(ProviderError::Protocol { message });
            }
            _ => {}
        }
        Ok(())
    }

    fn set_response_id(&mut self, response: &Value) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.into());
        }
    }

    fn apply_terminal(&mut self, response: &Value, default: StopReason) {
        self.set_response_id(response);
        self.usage = parse_usage(response.get("usage"));
        self.terminal = Some(match response.get("status").and_then(Value::as_str) {
            Some("incomplete") => StopReason::Length,
            Some("cancelled") => StopReason::Aborted,
            Some("completed" | "queued" | "in_progress") | None => default,
            Some(_) => StopReason::Error,
        });
    }

    fn apply_output_item(&mut self, event: &Value, done: bool) {
        let item = &event["item"];
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => self.apply_tool_item(event, item, done),
            Some("reasoning") if done => {
                self.reasoning_signature = serde_json::to_string(item).ok();
                if self.thinking.is_empty() {
                    self.thinking = item_text(item, "summary")
                        .or_else(|| item_text(item, "content"))
                        .unwrap_or_default();
                }
            }
            Some("message") if done && self.text.is_empty() => {
                self.text = item.get("content").and_then(Value::as_array).map_or_else(
                    String::new,
                    |content| {
                        content
                            .iter()
                            .filter_map(|part| {
                                part.get("text")
                                    .or_else(|| part.get("refusal"))
                                    .and_then(Value::as_str)
                            })
                            .collect::<Vec<_>>()
                            .join("")
                    },
                );
            }
            _ => {}
        }
    }

    fn apply_tool_item(&mut self, event: &Value, item: &Value, done: bool) {
        let key = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("call_id").and_then(Value::as_str))
            .unwrap_or("0")
            .to_owned();
        if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
            self.output_indexes.insert(index, key.clone());
        }
        let tool = self.tools.entry(key).or_default();
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            tool.call_id = call_id.into();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.into();
        }
        if done && let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            tool.arguments = arguments.into();
        }
    }

    fn apply_arguments_delta(&mut self, event: &Value) {
        let Some(delta) = event.get("delta").and_then(Value::as_str) else {
            return;
        };
        let key = self.event_tool_key(event);
        self.tools.entry(key).or_default().arguments.push_str(delta);
    }

    fn apply_arguments_done(&mut self, event: &Value) {
        let Some(arguments) = event.get("arguments").and_then(Value::as_str) else {
            return;
        };
        let key = self.event_tool_key(event);
        self.tools.entry(key).or_default().arguments = arguments.into();
    }

    fn event_tool_key(&self, event: &Value) -> String {
        event
            .get("item_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| self.output_indexes.get(&index).cloned())
            })
            .unwrap_or_else(|| "0".into())
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let Some(mut stop_reason) = self.terminal else {
            return Err(ProviderError::Protocol {
                message: "Responses stream ended without a terminal event".into(),
            });
        };
        if stop_reason == StopReason::Aborted {
            return Err(ProviderError::Aborted);
        }
        if stop_reason == StopReason::Error {
            return Err(ProviderError::Protocol {
                message: "Responses request failed".into(),
            });
        }
        let mut content = Vec::new();
        if !self.thinking.is_empty() || self.reasoning_signature.is_some() {
            content.push(Content::Thinking {
                text: self.thinking,
                signature: self.reasoning_signature,
                redacted: false,
            });
        }
        if !self.text.is_empty() {
            content.push(Content::Text { text: self.text });
        }
        for (_, tool) in self.tools {
            if tool.call_id.is_empty() || tool.name.is_empty() {
                return Err(ProviderError::Protocol {
                    message: "Responses stream ended with an incomplete tool call".into(),
                });
            }
            let arguments =
                serde_json::from_str(&tool.arguments).map_err(|error| ProviderError::Protocol {
                    message: format!("Responses tool arguments are invalid JSON: {error}"),
                })?;
            content.push(Content::ToolCall(ToolCall {
                id: tool.call_id,
                name: tool.name,
                arguments,
            }));
        }
        if stop_reason == StopReason::Stop
            && content
                .iter()
                .any(|block| matches!(block, Content::ToolCall(_)))
        {
            stop_reason = StopReason::ToolUse;
        }
        let mut message = Message::assistant(content, stop_reason);
        message.usage = self.usage;
        Ok(ModelResponse {
            message,
            response_id: self.response_id,
        })
    }
}

fn item_text(item: &Value, field: &str) -> Option<String> {
    item.get(field)
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|text| !text.is_empty())
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let value = value.unwrap_or(&Value::Null);
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
    data: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, ProviderError> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.apply_line(&line, &mut events)?;
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<Vec<u8>>, ProviderError> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.apply_line(&line, &mut events)?;
        }
        if !self.data.is_empty() {
            events.push(std::mem::take(&mut self.data));
        }
        Ok(events)
    }

    fn apply_line(&mut self, line: &[u8], events: &mut Vec<Vec<u8>>) -> Result<(), ProviderError> {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(std::mem::take(&mut self.data));
            }
            return Ok(());
        }
        if let Some(mut value) = line.strip_prefix(b"data:") {
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            if self.data.len().saturating_add(value.len()) > MAX_RESPONSE_BYTES {
                return Err(response_too_large());
            }
            self.data.extend_from_slice(value);
        }
        Ok(())
    }
}

async fn send_cancellable(
    request: RequestBuilder,
    cancellation: Option<&CancellationToken>,
) -> Result<Response, ProviderError> {
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ProviderError::Aborted),
            result = request.send() => result.map_err(|error| classify_transport(&error)),
        }
    } else {
        request
            .send()
            .await
            .map_err(|error| classify_transport(&error))
    }
}

async fn next_cancellable<S>(
    stream: &mut S,
    cancellation: Option<&CancellationToken>,
) -> Result<Option<S::Item>, ProviderError>
where
    S: futures::Stream + Unpin,
{
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ProviderError::Aborted),
            item = stream.next() => Ok(item),
        }
    } else {
        Ok(stream.next().await)
    }
}

async fn read_bounded_body(
    response: Response,
    cancellation: Option<&CancellationToken>,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(response_too_large());
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let Some(chunk) = next_cancellable(&mut stream, cancellation).await? else {
            break;
        };
        let chunk = chunk.map_err(|error| classify_transport(&error))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_too_large() -> ProviderError {
    ProviderError::Protocol {
        message: "Responses body exceeds the 8 MiB limit".into(),
    }
}

fn safe_error_excerpt(body: &[u8], secret: &str) -> String {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("provider returned an error");
    redact_and_limit(message, secret)
}

fn safe_stream_error(value: &Value, secret: &str, fallback: &str) -> String {
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    redact_and_limit(message, secret)
}

fn redact_and_limit(message: &str, secret: &str) -> String {
    let redacted = if secret.is_empty() {
        message.into()
    } else {
        message.replace(secret, "[REDACTED]")
    };
    redacted.chars().take(MAX_ERROR_CHARS).collect()
}

fn classify_transport(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Unavailable {
            message: "Responses request timed out".into(),
        }
    } else if error.is_connect() {
        ProviderError::Unavailable {
            message: "Responses endpoint is unavailable".into(),
        }
    } else {
        ProviderError::Protocol {
            message: "Responses HTTP transport failed".into(),
        }
    }
}

fn redacted_url(url: &Url) -> String {
    let sensitive = |key: &str| {
        let key = key.to_ascii_lowercase();
        key.contains("key")
            || key.contains("token")
            || key.contains("authorization")
            || key.contains("signature")
    };
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let value = if sensitive(&key) {
                "[REDACTED]".into()
            } else {
                value
            };
            (key.into_owned(), value.into_owned())
        })
        .collect::<Vec<_>>();
    let mut redacted = url.clone();
    redacted.set_query(None);
    if !pairs.is_empty() {
        redacted.query_pairs_mut().extend_pairs(pairs);
    }
    redacted.to_string()
}
