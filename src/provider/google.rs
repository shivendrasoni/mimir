use std::{fmt, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ThinkingLevel, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Native adapter for Google's Gemini Generate Content REST API.
pub struct GoogleProvider {
    base_url: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
}

impl GoogleProvider {
    /// Creates a native Gemini Generate Content transport.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank key, invalid URL,
    /// or HTTP client construction failure.
    pub fn new(base_url: Option<&str>, api_key: impl Into<String>) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
        let parsed = reqwest::Url::parse(base_url).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Google Gemini base URL: {error}"),
        })?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ProviderError::Protocol {
                message: "Google Gemini base URL must not contain credentials, query, or fragment"
                    .into(),
            });
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build Google Gemini HTTP client: {error}"),
            })?;
        Ok(Self {
            base_url: base_url.into(),
            api_key: SecretString::from(api_key),
            client,
            max_retries: 2,
        })
    }

    #[must_use]
    pub fn request_preview(&self, request: &ModelRequest) -> Value {
        build_request_body(request)
    }

    fn endpoint(&self, model: &str, streaming: bool) -> Result<String, ProviderError> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ProviderError::Protocol {
                message: "invalid Google Gemini model id".into(),
            });
        }
        let method = if streaming {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let endpoint = if self.base_url.ends_with("/v1beta/models") {
            format!("{}/{model}:{method}", self.base_url)
        } else if self.base_url.ends_with("/v1beta") {
            format!("{}/models/{model}:{method}", self.base_url)
        } else {
            format!("{}/v1beta/models/{model}:{method}", self.base_url)
        };
        Ok(endpoint)
    }

    fn request(
        &self,
        request: &ModelRequest,
        streaming: bool,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let mut builder = self
            .client
            .post(self.endpoint(&request.model, streaming)?)
            .header("x-goog-api-key", self.api_key.expose_secret())
            .json(&build_request_body(request));
        if streaming {
            builder = builder.header("accept", "text/event-stream");
        }
        Ok(builder)
    }

    async fn send_once(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self
            .request(request, false)?
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        let body = read_bounded_body(response).await?;
        classify_status(status, &body, self.api_key.expose_secret())?;
        parse_response(&body)
    }

    async fn stream_once(
        &self,
        request: &ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self
            .request(request, true)?
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_bounded_body(response).await?;
            classify_status(status, &body, self.api_key.expose_secret())?;
            return Err(ProviderError::Protocol {
                message: format!("Google Gemini returned HTTP {status}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large());
        }

        let mut chunks = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut accumulator = StreamAccumulator::default();
        let mut received = 0_usize;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| classify_transport(&error))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(response_too_large());
            }
            for payload in parser.push(&chunk)? {
                accumulator
                    .apply_payload(&payload, sink, self.api_key.expose_secret())
                    .await?;
            }
        }
        if let Some(payload) = parser.finish()? {
            accumulator
                .apply_payload(&payload, sink, self.api_key.expose_secret())
                .await?;
        }
        accumulator.finish()
    }
}

impl fmt::Debug for GoogleProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for GoogleProvider {
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

fn build_request_body(request: &ModelRequest) -> Value {
    let mut system = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        system.push(request.system_prompt.trim().to_owned());
    }
    let mut contents = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            let text = message.text();
            if !text.trim().is_empty() {
                system.push(text);
            }
        } else if let Some(content) = translate_message(message) {
            contents.push(content);
        }
    }

    let declarations = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters
            })
        })
        .collect::<Vec<_>>();
    let mut generation_config = json!({"maxOutputTokens": request.max_output_tokens});
    apply_thinking(
        &mut generation_config,
        &request.model,
        request.thinking_level,
    );
    let mut body = json!({
        "contents": contents,
        "generationConfig": generation_config
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{"text": system.join("\n\n")}]
        });
    }
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    body
}

fn apply_thinking(generation_config: &mut Value, model: &str, level: ThinkingLevel) {
    if level == ThinkingLevel::Off {
        return;
    }
    if model.to_ascii_lowercase().contains("gemini-3") {
        let thinking_level = match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal => "MINIMAL",
            ThinkingLevel::Low => "LOW",
            ThinkingLevel::Medium => "MEDIUM",
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "HIGH",
        };
        generation_config["thinkingConfig"] = json!({
            "includeThoughts": true,
            "thinkingLevel": thinking_level
        });
        return;
    }
    let budget = match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 4_096,
        ThinkingLevel::High => 8_192,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => 16_384,
    };
    generation_config["thinkingConfig"] = json!({
        "includeThoughts": true,
        "thinkingBudget": budget
    });
}

fn translate_message(message: &Message) -> Option<Value> {
    let mut parts = Vec::new();
    let mut index = 0_usize;
    while index < message.content.len() {
        match &message.content[index] {
            Content::Text { text } if !text.is_empty() => {
                let mut part = json!({"text": text});
                attach_following_signature(&mut part, &message.content, &mut index);
                parts.push(part);
            }
            Content::Image { data, mime_type } => parts.push(json!({
                "inlineData": {"mimeType": mime_type, "data": data}
            })),
            Content::Thinking {
                text,
                signature,
                redacted,
            } if !text.is_empty() || signature.is_some() => {
                let mut part = json!({"text": if *redacted { "" } else { text }, "thought": true});
                if let Some(signature) = signature {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            Content::ToolCall(call) => {
                let mut part = json!({
                    "functionCall": {
                        "id": call.id,
                        "name": call.name,
                        "args": call.arguments
                    }
                });
                attach_following_signature(&mut part, &message.content, &mut index);
                parts.push(part);
            }
            Content::ToolResult(result) => {
                let response = function_response_value(&result.content, result.is_error);
                parts.push(json!({
                    "functionResponse": {
                        "id": result.tool_call_id,
                        "name": result.tool_name,
                        "response": response
                    }
                }));
            }
            Content::Text { .. } | Content::Thinking { .. } => {}
        }
        index = index.saturating_add(1);
    }
    (!parts.is_empty()).then(|| {
        json!({
            "role": if message.role == Role::Assistant { "model" } else { "user" },
            "parts": parts
        })
    })
}

fn attach_following_signature(part: &mut Value, content: &[Content], index: &mut usize) {
    let Some(Content::Thinking {
        text,
        signature: Some(signature),
        ..
    }) = content.get(index.saturating_add(1))
    else {
        return;
    };
    if text.is_empty() {
        part["thoughtSignature"] = json!(signature);
        *index = index.saturating_add(1);
    }
}

fn function_response_value(content: &str, is_error: bool) -> Value {
    let parsed = serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.into()));
    if is_error {
        json!({"error": parsed})
    } else if parsed.is_object() {
        parsed
    } else {
        json!({"result": parsed})
    }
}

fn parse_response(body: &[u8]) -> Result<ModelResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Google Gemini JSON response: {error}"),
    })?;
    if value.get("error").is_some() {
        return Err(classify_api_error(&value, ""));
    }
    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| ProviderError::Protocol {
            message: "Google Gemini response has no candidates".into(),
        })?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol {
            message: "Google Gemini candidate has no content parts".into(),
        })?;
    let content = parse_parts(parts)?;
    let contains_call = content
        .iter()
        .any(|block| matches!(block, Content::ToolCall(_)));
    let reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map_or(StopReason::Stop, parse_stop_reason);
    let mut message = Message::assistant(
        content,
        if contains_call {
            StopReason::ToolUse
        } else {
            reason
        },
    );
    message.usage = parse_usage(value.get("usageMetadata"));
    Ok(ModelResponse {
        message,
        response_id: value
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn parse_parts(parts: &[Value]) -> Result<Vec<Content>, ProviderError> {
    let mut content = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let text = part.get("text").and_then(Value::as_str);
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            if text.is_some_and(|text| !text.is_empty()) || signature.is_some() {
                content.push(Content::Thinking {
                    text: text.unwrap_or_default().into(),
                    signature,
                    redacted: false,
                });
            }
            continue;
        }
        if let Some(text) = text
            && !text.is_empty()
        {
            content.push(Content::Text { text: text.into() });
        }
        if let Some(call) = part.get("functionCall") {
            let name = required_string(call, "name", "function call name")?;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(|| format!("gemini-call-{index}"), str::to_owned);
            content.push(Content::ToolCall(ToolCall {
                id,
                name,
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            }));
        }
        if let Some(signature) = signature {
            content.push(Content::Thinking {
                text: String::new(),
                signature: Some(signature),
                redacted: false,
            });
        }
    }
    Ok(content)
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Protocol {
            message: format!("Google Gemini response is missing {label}"),
        })
}

fn parse_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" | "FINISH_REASON_UNSPECIFIED" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let value = value.unwrap_or(&Value::Null);
    Usage {
        input_tokens: value
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens: value
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
    }
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    content: Vec<Content>,
    usage: Usage,
    stop_reason: StopReason,
    saw_tool_call: bool,
}

impl StreamAccumulator {
    async fn apply_payload(
        &mut self,
        payload: &[u8],
        sink: &dyn ProviderEventSink,
        api_key: &str,
    ) -> Result<(), ProviderError> {
        let value: Value =
            serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Google Gemini stream event: {error}"),
            })?;
        if value.get("error").is_some() {
            return Err(classify_api_error(&value, api_key));
        }
        if let Some(response_id) = value.get("responseId").and_then(Value::as_str) {
            self.response_id = Some(response_id.into());
        }
        if let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                let parsed = parse_parts(parts)?;
                for block in parsed {
                    match &block {
                        Content::Text { text } if !text.is_empty() => {
                            sink.emit(ProviderEvent::TextDelta(text.clone())).await;
                        }
                        Content::Thinking { text, .. } if !text.is_empty() => {
                            sink.emit(ProviderEvent::ThinkingDelta(text.clone())).await;
                        }
                        Content::ToolCall(_) => self.saw_tool_call = true,
                        Content::Text { .. }
                        | Content::Image { .. }
                        | Content::Thinking { .. }
                        | Content::ToolResult(_) => {}
                    }
                    self.content.push(block);
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.stop_reason = parse_stop_reason(reason);
            }
        }
        let usage = parse_usage(value.get("usageMetadata"));
        if usage.input_tokens != 0 {
            self.usage.input_tokens = usage.input_tokens;
        }
        if usage.output_tokens != 0 {
            self.usage.output_tokens = usage.output_tokens;
        }
        if usage.cached_tokens != 0 {
            self.usage.cached_tokens = usage.cached_tokens;
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        if self.content.is_empty() {
            return Err(ProviderError::Protocol {
                message: "Google Gemini stream returned no content".into(),
            });
        }
        let mut message = Message::assistant(
            self.content,
            if self.saw_tool_call {
                StopReason::ToolUse
            } else {
                self.stop_reason
            },
        );
        message.usage = self.usage;
        Ok(ModelResponse {
            message,
            response_id: self.response_id,
        })
    }
}

#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
    data: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, ProviderError> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<_> = self.pending.drain(..=position).collect();
            trim_newline(&mut line);
            if let Some(event) = self.apply_line(&line)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Option<Vec<u8>>, ProviderError> {
        let mut completed = None;
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            completed = self.apply_line(&line)?;
        }
        Ok(completed.or_else(|| self.take_event()))
    }

    fn apply_line(&mut self, line: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        if line.is_empty() {
            return Ok(self.take_event());
        }
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Ok(None);
        };
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);
        if !self.data.is_empty() {
            self.data.push(b'\n');
        }
        if self.data.len().saturating_add(payload.len()) > MAX_RESPONSE_BYTES {
            return Err(response_too_large());
        }
        self.data.extend_from_slice(payload);
        Ok(None)
    }

    fn take_event(&mut self) -> Option<Vec<u8>> {
        if self.data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.data))
        }
    }
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(response_too_large());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
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
        message: "Google Gemini response exceeds the 8 MiB limit".into(),
    }
}

fn classify_status(
    status: reqwest::StatusCode,
    body: &[u8],
    api_key: &str,
) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(ProviderError::Authentication);
    }
    let message = safe_error_excerpt(body, api_key);
    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited { message });
    }
    if status.is_server_error() || status.as_u16() == 408 {
        return Err(ProviderError::Unavailable {
            message: format!("HTTP {status}: {message}"),
        });
    }
    Err(ProviderError::Protocol {
        message: format!("HTTP {status}: {message}"),
    })
}

fn classify_api_error(value: &Value, api_key: &str) -> ProviderError {
    let status = value.pointer("/error/status").and_then(Value::as_str);
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Google Gemini returned an error");
    let message = redact_and_cap(message, api_key);
    match status {
        Some("UNAUTHENTICATED" | "PERMISSION_DENIED") => ProviderError::Authentication,
        Some("RESOURCE_EXHAUSTED") => ProviderError::RateLimited { message },
        Some("UNAVAILABLE" | "DEADLINE_EXCEEDED" | "INTERNAL") => {
            ProviderError::Unavailable { message }
        }
        _ => ProviderError::Protocol { message },
    }
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

fn safe_error_excerpt(body: &[u8], api_key: &str) -> String {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Google Gemini returned an error".into());
    redact_and_cap(&message, api_key)
}

fn redact_and_cap(message: &str, api_key: &str) -> String {
    let redacted = if api_key.is_empty() {
        message.to_owned()
    } else {
        message.replace(api_key, "[REDACTED]")
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
