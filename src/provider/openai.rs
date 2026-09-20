use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::RequestBuilder;
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use crate::{
    config::ProviderConfig,
    model::{Content, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage},
};

use super::{Provider, ProviderError};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct OpenAiProvider {
    pub(super) config: ProviderConfig,
    pub(super) client: reqwest::Client,
    pub(super) authentication: OpenAiAuthentication,
    compatibility: OpenAiCompatibility,
    priority_service_tier: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaxTokensField {
    MaxCompletionTokens,
    MaxTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenAiCompatibility {
    max_tokens_field: MaxTokensField,
    supports_reasoning_effort: bool,
    supports_strict_mode: bool,
}

impl OpenAiCompatibility {
    fn detected(config: &ProviderConfig) -> Self {
        let provider = config.name.as_str();
        let base_url = config.base_url.as_str();
        let moonshot = matches!(provider, "kimi-coding" | "moonshotai");
        let cloudflare_gateway =
            provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
        let prime_inference = base_url.contains("inference-api.primeintellect.ai");
        let chutes = base_url.contains("chutes.ai");
        let zai = matches!(provider, "zai" | "zai-coding-plan");
        let grok = provider == "xai";
        Self {
            max_tokens_field: if moonshot || cloudflare_gateway || prime_inference || chutes {
                MaxTokensField::MaxTokens
            } else {
                MaxTokensField::MaxCompletionTokens
            },
            supports_reasoning_effort: !(grok || zai || moonshot || cloudflare_gateway),
            supports_strict_mode: !(moonshot || cloudflare_gateway || prime_inference),
        }
    }

    fn with_override(mut self, compat: Option<&Value>) -> Result<Self, ProviderError> {
        let Some(compat) = compat else {
            return Ok(self);
        };
        let object = compat.as_object().ok_or_else(|| ProviderError::Protocol {
            message: "model compatibility settings must be an object".into(),
        })?;
        if let Some(value) = object.get("maxTokensField") {
            self.max_tokens_field = match value.as_str() {
                Some("max_completion_tokens") => MaxTokensField::MaxCompletionTokens,
                Some("max_tokens") => MaxTokensField::MaxTokens,
                _ => {
                    return Err(ProviderError::Protocol {
                        message: "model compat maxTokensField must be max_tokens or max_completion_tokens"
                            .into(),
                    });
                }
            };
        }
        if let Some(value) = object.get("supportsReasoningEffort") {
            self.supports_reasoning_effort =
                value.as_bool().ok_or_else(|| ProviderError::Protocol {
                    message: "model compat supportsReasoningEffort must be boolean".into(),
                })?;
        }
        if let Some(value) = object.get("supportsStrictMode") {
            self.supports_strict_mode = value.as_bool().ok_or_else(|| ProviderError::Protocol {
                message: "model compat supportsStrictMode must be boolean".into(),
            })?;
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenAiAuthentication {
    Bearer,
    CloudflareGateway,
}

impl OpenAiAuthentication {
    pub(super) fn apply(self, request: RequestBuilder, secret: &str) -> RequestBuilder {
        match self {
            Self::Bearer => request.bearer_auth(secret),
            Self::CloudflareGateway => {
                request.header("cf-aig-authorization", format!("Bearer {secret}"))
            }
        }
    }
}

impl OpenAiProvider {
    /// Creates an OpenAI-compatible HTTP adapter.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the HTTP client cannot be configured.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        Self::with_authentication(config, OpenAiAuthentication::Bearer, None)
    }

    /// Creates an OpenAI-compatible adapter shaped by catalog compatibility
    /// metadata. Unknown compatibility keys remain forward-compatible while
    /// request-affecting keys are validated strictly.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the client or compatibility policy is invalid.
    pub fn new_with_compat(
        config: ProviderConfig,
        compat: Option<&Value>,
    ) -> Result<Self, ProviderError> {
        Self::with_authentication(config, OpenAiAuthentication::Bearer, compat)
    }

    /// Creates a Cloudflare AI Gateway chat-completions transport. Gateway
    /// tokens use `cf-aig-authorization` and are not sent as upstream Bearer
    /// credentials.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the HTTP client cannot be configured.
    pub fn new_cloudflare_gateway(config: ProviderConfig) -> Result<Self, ProviderError> {
        Self::with_authentication(config, OpenAiAuthentication::CloudflareGateway, None)
    }

    /// Creates a Cloudflare Gateway adapter with per-model compatibility overrides.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the client or compatibility policy is invalid.
    pub fn new_cloudflare_gateway_with_compat(
        config: ProviderConfig,
        compat: Option<&Value>,
    ) -> Result<Self, ProviderError> {
        Self::with_authentication(config, OpenAiAuthentication::CloudflareGateway, compat)
    }

    fn with_authentication(
        config: ProviderConfig,
        authentication: OpenAiAuthentication,
        compat: Option<&Value>,
    ) -> Result<Self, ProviderError> {
        let compatibility = OpenAiCompatibility::detected(&config).with_override(compat)?;
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build HTTP client: {error}"),
            })?;
        Ok(Self {
            config,
            client,
            authentication,
            compatibility,
            priority_service_tier: AtomicBool::new(false),
        })
    }

    pub fn request_preview(&self, request: &ModelRequest) -> Value {
        self.request_body(request)
    }

    pub(super) fn request_body(&self, request: &ModelRequest) -> Value {
        let mut body = build_request_body_with_compat(request, self.compatibility);
        if self.priority_service_tier.load(Ordering::Acquire) {
            body["service_tier"] = json!("priority");
        }
        body
    }

    async fn send_once(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .authentication
            .apply(
                self.client.post(endpoint),
                self.config.api_key_secret().expose_secret(),
            )
            .json(&self.request_body(request))
            .send()
            .await
            .map_err(|error| classify_transport_error(&error))?;
        let status = response.status();
        let body = read_bounded_body(response).await?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::Authentication);
        }
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                message: safe_error_excerpt(&body, self.config.api_key_secret().expose_secret()),
            });
        }
        if status.is_server_error() {
            return Err(ProviderError::Unavailable {
                message: format!(
                    "HTTP {status}: {}",
                    safe_error_excerpt(&body, self.config.api_key_secret().expose_secret())
                ),
            });
        }
        if !status.is_success() {
            return Err(ProviderError::Protocol {
                message: format!(
                    "HTTP {status}: {}",
                    safe_error_excerpt(&body, self.config.api_key_secret().expose_secret())
                ),
            });
        }
        parse_response(&body)
    }
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
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
        let mut attempt = 0_u32;
        loop {
            match self.send_once(&request).await {
                Ok(response) => return Ok(response),
                Err(error) if error.is_retryable() && attempt < self.config.max_retries => {
                    let delay_ms = 200_u64.saturating_mul(1_u64 << attempt.min(8));
                    tokio::time::sleep(Duration::from_millis(delay_ms.min(5_000))).await;
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn super::ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        super::openai_stream::stream_openai(self, request, sink).await
    }
}

#[cfg(test)]
fn build_request_body(request: &ModelRequest) -> Value {
    build_request_body_with_compat(
        request,
        OpenAiCompatibility {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            supports_reasoning_effort: true,
            supports_strict_mode: true,
        },
    )
}

fn build_request_body_with_compat(
    request: &ModelRequest,
    compatibility: OpenAiCompatibility,
) -> Value {
    let mut messages = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        messages.push(json!({"role": "system", "content": request.system_prompt}));
    }
    for message in &request.messages {
        match message.role {
            Role::System => messages.push(json!({
                "role": "system",
                "content": message.text()
            })),
            Role::User => messages.push(json!({
                "role": "user",
                "content": openai_user_content(message)
            })),
            Role::Assistant => {
                let tool_calls: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call) => Some(json!({
                            "id": call.id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments.to_string()}
                        })),
                        _ => None,
                    })
                    .collect();
                let mut value = json!({"role": "assistant", "content": message.text()});
                if !tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(value);
            }
            Role::Tool => {
                for content in &message.content {
                    if let Content::ToolResult(result) = content {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": result.tool_call_id,
                            "content": result.content
                        }));
                    }
                }
            }
        }
    }
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            let mut function = json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters
            });
            if compatibility.supports_strict_mode {
                function["strict"] = json!(true);
            }
            json!({
                "type": "function",
                "function": function
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages
    });
    let max_tokens_field = match compatibility.max_tokens_field {
        MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        MaxTokensField::MaxTokens => "max_tokens",
    };
    body[max_tokens_field] = json!(request.max_output_tokens);
    if compatibility.supports_reasoning_effort
        && let Some(effort) = request.thinking_effort.as_deref().or_else(|| {
            (request.thinking_level != crate::model::ThinkingLevel::Off)
                .then(|| request.thinking_level.as_str())
        })
    {
        body["reasoning_effort"] = json!(effort);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

fn openai_user_content(message: &Message) -> Value {
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, Content::Image { .. }));
    if !has_images {
        return Value::String(message.text());
    }
    Value::Array(
        message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(json!({"type": "text", "text": text})),
                Content::Image { data, mime_type } => Some(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime_type};base64,{data}")}
                })),
                Content::Thinking { .. } | Content::ToolCall(_) | Content::ToolResult(_) => None,
            })
            .collect(),
    )
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::Protocol {
            message: "provider response exceeds the 8 MiB limit".into(),
        });
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| classify_transport_error(&error))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::Protocol {
                message: "provider response exceeds the 8 MiB limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_response(body: &[u8]) -> Result<ModelResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ProviderError::Protocol {
        message: format!("invalid JSON response: {error}"),
    })?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::Protocol {
            message: "response has no choices".into(),
        })?;
    let upstream = choice
        .get("message")
        .ok_or_else(|| ProviderError::Protocol {
            message: "response choice has no message".into(),
        })?;
    let mut content = Vec::new();
    if let Some(text) = upstream.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(Content::Text { text: text.into() });
    }
    if let Some(calls) = upstream.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::Protocol {
                    message: "tool call has no argument string".into(),
                })?;
            content.push(Content::ToolCall(ToolCall {
                id: required_string(call, "/id", "tool call id")?,
                name: required_string(call, "/function/name", "tool call name")?,
                arguments: serde_json::from_str(arguments).map_err(|error| {
                    ProviderError::Protocol {
                        message: format!("tool call arguments are not valid JSON: {error}"),
                    }
                })?,
            }));
        }
    }
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let stop_reason = match finish {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
        "stop" => StopReason::Stop,
        _ => StopReason::Error,
    };
    let usage = Usage {
        input_tokens: value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens: value
            .pointer("/usage/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
    };
    let mut message = Message::assistant(content, stop_reason);
    message.usage = usage;
    Ok(ModelResponse {
        message,
        response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn required_string(value: &Value, pointer: &str, label: &str) -> Result<String, ProviderError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Protocol {
            message: format!("response is missing {label}"),
        })
}

fn classify_transport_error(error: &reqwest::Error) -> ProviderError {
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
        .and_then(Value::as_str)
        .unwrap_or("provider returned an error");
    let redacted = if secret.is_empty() {
        message.to_owned()
    } else {
        message.replace(secret, "[REDACTED]")
    };
    redacted.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolDefinition;

    #[test]
    fn request_translation_preserves_typed_tools() {
        let request = ModelRequest {
            model: "test".into(),
            thinking_level: crate::model::ThinkingLevel::High,
            thinking_effort: None,
            system_prompt: "system".into(),
            messages: vec![Message::user("hello")],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type": "object"}),
            }],
            max_output_tokens: 100,
        };
        let body = build_request_body(&request);
        assert_eq!(
            body.pointer("/tools/0/function/name"),
            Some(&json!("read_file"))
        );
        assert_eq!(body.pointer("/messages/1/content"), Some(&json!("hello")));
        assert_eq!(body.get("reasoning_effort"), Some(&json!("high")));
    }

    #[test]
    fn model_compat_shapes_openai_completion_request_fields() {
        let provider = OpenAiProvider::new_with_compat(
            ProviderConfig::openai("https://api.example.test/v1", "test", "secret")
                .expect("config"),
            Some(&json!({
                "maxTokensField": "max_tokens",
                "supportsReasoningEffort": false,
                "supportsStrictMode": false
            })),
        )
        .expect("provider");
        let request = ModelRequest {
            model: "test".into(),
            thinking_level: crate::model::ThinkingLevel::High,
            thinking_effort: None,
            system_prompt: String::new(),
            messages: vec![Message::user("hello")],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type": "object"}),
            }],
            max_output_tokens: 4_096,
        };

        let body = provider.request_preview(&request);
        assert_eq!(body.get("max_tokens"), Some(&json!(4_096)));
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.pointer("/tools/0/function/strict").is_none());
    }

    #[test]
    fn priority_service_tier_is_applied_and_can_be_disabled() {
        let provider = OpenAiProvider::new(
            ProviderConfig::openai("https://api.example.test/v1", "test", "secret")
                .expect("config"),
        )
        .expect("provider");
        let request = ModelRequest {
            model: "test".into(),
            thinking_level: crate::model::ThinkingLevel::Off,
            thinking_effort: None,
            system_prompt: String::new(),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_output_tokens: 100,
        };
        provider
            .set_service_tier(Some("priority"))
            .expect("enable priority");
        assert_eq!(
            provider.request_preview(&request)["service_tier"],
            json!("priority")
        );
        provider.set_service_tier(None).expect("disable priority");
        assert!(
            provider
                .request_preview(&request)
                .get("service_tier")
                .is_none()
        );
    }

    #[test]
    fn request_translation_preserves_inline_images() {
        let mut user = Message::user("inspect");
        user.content.push(Content::Image {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        });
        let body = build_request_body(&ModelRequest {
            model: "test".into(),
            thinking_level: crate::model::ThinkingLevel::Off,
            thinking_effort: None,
            system_prompt: String::new(),
            messages: vec![user],
            tools: Vec::new(),
            max_output_tokens: 100,
        });
        assert_eq!(body["messages"][0]["content"][0]["text"], "inspect");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
    }

    #[test]
    fn response_translation_rejects_malformed_tool_arguments() {
        let body = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"tool_calls": [{
                    "id": "call-1",
                    "function": {"name": "read_file", "arguments": "{"}
                }]}
            }]
        });
        assert!(parse_response(&serde_json::to_vec(&body).expect("encode")).is_err());
    }

    #[test]
    fn response_translation_preserves_text_tool_calls_and_usage() {
        let body = json!({
            "id": "resp-7",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "inspect this",
                    "tool_calls": [{
                        "id": "call-1",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/main.rs\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 3,
                "prompt_tokens_details": {
                    "cached_tokens": 2
                }
            }
        });

        let response = parse_response(&serde_json::to_vec(&body).expect("encode")).expect("parse");

        assert_eq!(response.response_id.as_deref(), Some("resp-7"));
        assert_eq!(response.message.text(), "inspect this");
        assert_eq!(response.message.usage.input_tokens, 9);
        assert_eq!(response.message.usage.output_tokens, 3);
        assert_eq!(response.message.usage.cached_tokens, 2);
        assert!(matches!(
            response.message.content.get(1),
            Some(Content::ToolCall(call))
                if call.id == "call-1"
                    && call.name == "read_file"
                    && call.arguments == json!({"path":"src/main.rs"})
        ));
    }

    #[test]
    fn safe_error_excerpt_caps_and_prefers_provider_message() {
        let body = json!({
            "error": {
                "message": "x".repeat(400)
            }
        });

        let excerpt = safe_error_excerpt(&serde_json::to_vec(&body).expect("encode"), "secret");

        assert_eq!(excerpt.len(), 300);
        assert!(excerpt.chars().all(|character| character == 'x'));
    }
}
