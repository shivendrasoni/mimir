use std::{fmt, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ThinkingLevel, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://aiplatform.googleapis.com";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;

enum VertexAuth {
    ApiKey(SecretString),
    Bearer {
        project: String,
        location: String,
        token: SecretString,
        quota_project: Option<String>,
    },
}

impl VertexAuth {
    fn secret(&self) -> &str {
        match self {
            Self::ApiKey(secret) | Self::Bearer { token: secret, .. } => secret.expose_secret(),
        }
    }
}

/// Native Vertex AI Gemini transport supporting API-key express mode and
/// caller-resolved Application Default Credential bearer tokens.
pub struct VertexProvider {
    base_url: String,
    auth: VertexAuth,
    client: reqwest::Client,
    max_retries: u32,
}

impl VertexProvider {
    /// Creates a Vertex AI express-mode transport authenticated by Google Cloud
    /// API key. Express mode uses the projectless publisher-model resource.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank key or unsafe
    /// base URL.
    pub fn with_api_key(
        base_url: Option<&str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into().trim().to_owned();
        validate_secret(&api_key)?;
        Self::build(base_url, VertexAuth::ApiKey(SecretString::from(api_key)))
    }

    /// Creates a Vertex AI transport from a bearer token resolved from ADC by
    /// the caller. ADC resource requests require an explicit project/location.
    ///
    /// # Errors
    ///
    /// Returns an authentication or protocol error for a blank token, invalid
    /// project/location, or unsafe base URL.
    pub fn with_bearer_token(
        base_url: Option<&str>,
        project: impl Into<String>,
        location: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::with_bearer_token_and_quota_project(
            base_url,
            project,
            location,
            access_token,
            None::<String>,
        )
    }

    /// Creates an ADC bearer transport and forwards the optional quota project
    /// through Google's standard `x-goog-user-project` header.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::with_bearer_token`], plus
    /// a protocol error for an unsafe quota-project identifier.
    pub fn with_bearer_token_and_quota_project(
        base_url: Option<&str>,
        project: impl Into<String>,
        location: impl Into<String>,
        access_token: impl Into<String>,
        quota_project: Option<impl Into<String>>,
    ) -> Result<Self, ProviderError> {
        let project = project.into().trim().to_owned();
        let location = location.into().trim().to_owned();
        let access_token = access_token.into().trim().to_owned();
        let quota_project = quota_project
            .map(Into::into)
            .map(|value: String| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        validate_identifier(&project, "project")?;
        validate_identifier(&location, "location")?;
        if let Some(quota_project) = quota_project.as_deref() {
            validate_identifier(quota_project, "quota project")?;
        }
        validate_secret(&access_token)?;
        Self::build(
            base_url,
            VertexAuth::Bearer {
                project,
                location,
                token: SecretString::from(access_token),
                quota_project,
            },
        )
    }

    fn build(base_url: Option<&str>, auth: VertexAuth) -> Result<Self, ProviderError> {
        let base_url = resolve_base_url(base_url, &auth)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build Google Vertex HTTP client: {error}"),
            })?;
        Ok(Self {
            base_url,
            auth,
            client,
            max_retries: 2,
        })
    }

    #[must_use]
    pub fn request_preview(&self, request: &ModelRequest) -> Value {
        build_request_body(request)
    }

    /// Returns the exact endpoint without including credentials.
    ///
    /// # Errors
    ///
    /// Returns a protocol error if `model` is not a safe Vertex model ID.
    pub fn endpoint_preview(&self, model: &str, streaming: bool) -> Result<String, ProviderError> {
        self.endpoint(model, streaming)
    }

    fn endpoint(&self, model: &str, streaming: bool) -> Result<String, ProviderError> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        validate_identifier(model, "model id")?;
        let method = if streaming {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let resource = match &self.auth {
            VertexAuth::ApiKey(_) => format!("publishers/google/models/{model}"),
            VertexAuth::Bearer {
                project, location, ..
            } => {
                format!("projects/{project}/locations/{location}/publishers/google/models/{model}")
            }
        };
        let versioned_base = if self.base_url.rsplit('/').next().is_some_and(is_api_version) {
            self.base_url.clone()
        } else {
            format!("{}/v1", self.base_url)
        };
        Ok(format!("{versioned_base}/{resource}:{method}"))
    }

    fn request(
        &self,
        request: &ModelRequest,
        streaming: bool,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        validate_tools(request)?;
        let mut builder = self
            .client
            .post(self.endpoint(&request.model, streaming)?)
            .json(&build_request_body(request));
        builder = match &self.auth {
            VertexAuth::ApiKey(api_key) => {
                builder.header("x-goog-api-key", api_key.expose_secret())
            }
            VertexAuth::Bearer {
                token,
                quota_project,
                ..
            } => {
                let builder = builder.bearer_auth(token.expose_secret());
                if let Some(quota_project) = quota_project {
                    builder.header("x-goog-user-project", quota_project)
                } else {
                    builder
                }
            }
        };
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
        classify_status(status, &body, self.auth.secret())?;
        parse_response(&body, self.auth.secret())
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
            classify_status(status, &body, self.auth.secret())?;
            return Err(ProviderError::Protocol {
                message: format!("Google Vertex returned HTTP {status}"),
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
                    .apply_payload(&payload, sink, self.auth.secret())
                    .await?;
            }
        }
        if let Some(payload) = parser.finish()? {
            accumulator
                .apply_payload(&payload, sink, self.auth.secret())
                .await?;
        }
        accumulator.finish()
    }
}

impl fmt::Debug for VertexProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth = match self.auth {
            VertexAuth::ApiKey(_) => "api_key",
            VertexAuth::Bearer { .. } => "bearer",
        };
        formatter
            .debug_struct("VertexProvider")
            .field("base_url", &self.base_url)
            .field("auth", &auth)
            .field("credential", &"[REDACTED]")
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for VertexProvider {
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

fn validate_secret(secret: &str) -> Result<(), ProviderError> {
    let trimmed = secret.trim();
    if trimmed.is_empty()
        || trimmed == "gcp-vertex-credentials"
        || (trimmed.starts_with('<') && trimmed.ends_with('>'))
        || trimmed
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ProviderError::Authentication);
    }
    Ok(())
}

fn validate_tools(request: &ModelRequest) -> Result<(), ProviderError> {
    if request.tools.len() > 512 {
        return Err(ProviderError::Protocol {
            message: "Google Vertex accepts at most 512 function declarations".into(),
        });
    }
    for tool in &request.tools {
        validate_tool_name(&tool.name)?;
    }
    for message in &request.messages {
        for content in &message.content {
            match content {
                Content::ToolCall(call) => validate_tool_name(&call.name)?,
                Content::ToolResult(result) => validate_tool_name(&result.tool_name)?,
                Content::Text { .. } | Content::Image { .. } | Content::Thinking { .. } => {}
            }
        }
    }
    Ok(())
}

fn validate_tool_name(name: &str) -> Result<(), ProviderError> {
    let mut bytes = name.bytes();
    let first = bytes.next();
    if name.len() > 64
        || !first.is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(ProviderError::Protocol {
            message: "invalid Google Vertex function name".into(),
        });
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ProviderError::Protocol {
            message: format!("invalid Google Vertex {label}"),
        });
    }
    Ok(())
}

fn is_api_version(segment: &str) -> bool {
    let Some(suffix) = segment.strip_prefix('v') else {
        return false;
    };
    let digit_count = suffix.bytes().take_while(u8::is_ascii_digit).count();
    digit_count > 0
        && (digit_count == suffix.len()
            || suffix[digit_count..]
                .strip_prefix("beta")
                .is_some_and(|beta| beta.bytes().all(|byte| byte.is_ascii_digit())))
}

fn resolve_base_url(base_url: Option<&str>, auth: &VertexAuth) -> Result<String, ProviderError> {
    let location = match auth {
        VertexAuth::ApiKey(_) => "global",
        VertexAuth::Bearer { location, .. } => location,
    };
    let default = if location == "global" {
        DEFAULT_BASE_URL.to_owned()
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    };
    let configured = base_url.unwrap_or(&default).trim().trim_end_matches('/');
    let configured = if configured.contains("{location}") {
        if location == "global" {
            configured.replace("{location}-", "")
        } else {
            configured.replace("{location}", location)
        }
    } else {
        configured.to_owned()
    };
    let parsed = reqwest::Url::parse(&configured).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Google Vertex base URL: {error}"),
    })?;
    let insecure_remote = parsed.scheme() == "http"
        && !parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if !matches!(parsed.scheme(), "http" | "https")
        || insecure_remote
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ProviderError::Protocol {
            message:
                "Google Vertex base URL must be HTTP(S) without credentials, query, or fragment"
                    .into(),
        });
    }
    Ok(configured)
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
        } else if let Some(content) = translate_message(message, &request.model) {
            append_content(&mut contents, content);
        }
    }

    let declarations = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": sanitize_schema(&tool.parameters)
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
    let lower = model.to_ascii_lowercase();
    let gemini_three = lower
        .strip_prefix("gemini-")
        .is_some_and(|suffix| suffix.starts_with('3'));
    if level == ThinkingLevel::Off {
        generation_config["thinkingConfig"] = if gemini_three {
            json!({
                "thinkingLevel": if lower.contains("-pro") { "LOW" } else { "MINIMAL" }
            })
        } else {
            json!({"thinkingBudget": 0})
        };
        return;
    }
    if gemini_three {
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
    let budget = thinking_budget(&lower, level);
    generation_config["thinkingConfig"] = json!({
        "includeThoughts": true,
        "thinkingBudget": budget
    });
}

fn thinking_budget(model: &str, level: ThinkingLevel) -> i32 {
    let index = match level {
        ThinkingLevel::Off => return 0,
        ThinkingLevel::Minimal => 0,
        ThinkingLevel::Low => 1,
        ThinkingLevel::Medium => 2,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 3,
    };
    if model.contains("2.5-pro") {
        return [128, 2_048, 8_192, 32_768][index];
    }
    if model.contains("2.5-flash-lite") {
        return [512, 2_048, 8_192, 24_576][index];
    }
    if model.contains("2.5-flash") {
        return [128, 2_048, 8_192, 24_576][index];
    }
    -1
}

fn append_content(contents: &mut Vec<Value>, content: Value) {
    if is_function_response_turn(&content)
        && let Some(previous) = contents.last_mut()
        && is_function_response_turn(previous)
    {
        let Some(parts) = content.get("parts").and_then(Value::as_array) else {
            return;
        };
        if let Some(previous_parts) = previous.get_mut("parts").and_then(Value::as_array_mut) {
            previous_parts.extend(parts.iter().cloned());
            return;
        }
    }
    contents.push(content);
}

fn is_function_response_turn(content: &Value) -> bool {
    content.get("role").and_then(Value::as_str) == Some("user")
        && content
            .get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                !parts.is_empty()
                    && parts
                        .iter()
                        .all(|part| part.get("functionResponse").is_some())
            })
}

fn translate_message(message: &Message, model: &str) -> Option<Value> {
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
                let signature = signature.as_deref().filter(|value| valid_signature(value));
                let visible_text = if *redacted { "" } else { text };
                if visible_text.is_empty() && signature.is_none() {
                    index = index.saturating_add(1);
                    continue;
                }
                let mut part = json!({"text": visible_text, "thought": true});
                if let Some(signature) = signature {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            Content::ToolCall(call) => {
                let mut function_call = json!({
                    "name": call.name,
                    "args": call.arguments
                });
                if requires_tool_call_id(model) {
                    function_call["id"] = json!(normalized_tool_call_id(&call.id));
                }
                let mut part = json!({"functionCall": function_call});
                attach_following_signature(&mut part, &message.content, &mut index);
                parts.push(part);
            }
            Content::ToolResult(result) => {
                let mut function_response = json!({
                    "name": result.tool_name,
                    "response": function_response_value(&result.content, result.is_error)
                });
                if requires_tool_call_id(model) {
                    function_response["id"] = json!(normalized_tool_call_id(&result.tool_call_id));
                }
                parts.push(json!({"functionResponse": function_response}));
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
    if text.is_empty() && valid_signature(signature) {
        part["thoughtSignature"] = json!(signature);
        *index = index.saturating_add(1);
    }
}

fn requires_tool_call_id(model: &str) -> bool {
    model.starts_with("claude-") || model.starts_with("gpt-oss-")
}

fn normalized_tool_call_id(id: &str) -> String {
    let normalized = id
        .chars()
        .filter_map(|character| {
            character
                .is_ascii_alphanumeric()
                .then_some(character)
                .or_else(|| matches!(character, '_' | '-').then_some(character))
        })
        .take(64)
        .collect::<String>();
    if normalized.is_empty() {
        "tool_call".into()
    } else {
        normalized
    }
}

fn valid_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.as_bytes().chunks_exact(4).remainder().is_empty() {
        return false;
    }
    let padding = signature
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    padding <= 2
        && signature[..signature.len() - padding]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && signature[signature.len() - padding..]
            .bytes()
            .all(|byte| byte == b'=')
}

fn function_response_value(content: &str, is_error: bool) -> Value {
    let parsed = serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.into()));
    if is_error {
        json!({"error": parsed})
    } else if parsed.is_object() {
        parsed
    } else {
        json!({"output": parsed})
    }
}

fn sanitize_schema(value: &Value) -> Value {
    const META: &[&str] = &[
        "$schema",
        "$id",
        "$anchor",
        "$dynamicAnchor",
        "$vocabulary",
        "$comment",
        "$defs",
        "definitions",
    ];
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sanitize_schema).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !META.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), sanitize_schema(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn parse_response(body: &[u8], secret: &str) -> Result<ModelResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| ProviderError::Protocol {
        message: format!("invalid Google Vertex JSON response: {error}"),
    })?;
    if value.get("error").is_some() {
        return Err(classify_api_error(&value, secret));
    }
    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| blocked_or_missing_candidate(&value))?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol {
            message: "Google Vertex candidate has no content parts".into(),
        })?;
    let response_id = value
        .get("responseId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut tool_index = 0_usize;
    let content = parse_parts(parts, response_id.as_deref(), &mut tool_index)?;
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
        response_id,
    })
}

fn blocked_or_missing_candidate(value: &Value) -> ProviderError {
    let reason = value
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .unwrap_or("no candidates");
    ProviderError::Protocol {
        message: format!("Google Vertex response blocked or empty: {reason}"),
    }
}

fn parse_parts(
    parts: &[Value],
    response_id: Option<&str>,
    tool_index: &mut usize,
) -> Result<Vec<Content>, ProviderError> {
    let mut content = Vec::new();
    for part in parts {
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
        if let Some(inline) = part.get("inlineData") {
            let mime_type = required_string(inline, "mimeType", "inline image MIME type")?;
            let data = required_string(inline, "data", "inline image data")?;
            content.push(Content::Image { data, mime_type });
        }
        if let Some(call) = part.get("functionCall") {
            let name = required_string(call, "name", "function call name")?;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(
                    || {
                        let prefix = response_id
                            .map(safe_id_component)
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| "response".into());
                        let id = format!("vertex-{prefix}-call-{tool_index}");
                        *tool_index = tool_index.saturating_add(1);
                        id
                    },
                    str::to_owned,
                );
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

fn safe_id_component(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect()
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Protocol {
            message: format!("Google Vertex response is missing {label}"),
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
    let cached = value
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let candidates = value
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thoughts = value
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: prompt,
        output_tokens: candidates.saturating_add(thoughts),
        cached_tokens: cached,
    }
}

#[derive(Default)]
struct StreamAccumulator {
    response_id: Option<String>,
    content: Vec<Content>,
    usage: Usage,
    stop_reason: StopReason,
    saw_tool_call: bool,
    tool_index: usize,
}

impl StreamAccumulator {
    async fn apply_payload(
        &mut self,
        payload: &[u8],
        sink: &dyn ProviderEventSink,
        secret: &str,
    ) -> Result<(), ProviderError> {
        if payload == b"[DONE]" {
            return Ok(());
        }
        let value: Value =
            serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Google Vertex stream event: {error}"),
            })?;
        if value.get("error").is_some() {
            return Err(classify_api_error(&value, secret));
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
                let parsed = parse_parts(parts, self.response_id.as_deref(), &mut self.tool_index)?;
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
                    append_stream_block(&mut self.content, block);
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.stop_reason = parse_stop_reason(reason);
            }
        }
        let usage = parse_usage(value.get("usageMetadata"));
        if usage.input_tokens != 0 || usage.cached_tokens != 0 {
            self.usage.input_tokens = usage.input_tokens;
            self.usage.cached_tokens = usage.cached_tokens;
        }
        if usage.output_tokens != 0 {
            self.usage.output_tokens = usage.output_tokens;
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        if self.content.is_empty() {
            return Err(ProviderError::Protocol {
                message: "Google Vertex stream returned no content".into(),
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

fn append_stream_block(content: &mut Vec<Content>, block: Content) {
    match block {
        Content::Text { text } => {
            if let Some(Content::Text { text: current }) = content.last_mut() {
                current.push_str(&text);
                return;
            }
            if matches!(
                content.last(),
                Some(Content::Thinking {
                    text,
                    signature: Some(_),
                    ..
                }) if text.is_empty()
            ) {
                let previous_index = content.len().saturating_sub(2);
                if let Some(Content::Text { text: current }) = content.get_mut(previous_index) {
                    current.push_str(&text);
                    return;
                }
            }
            content.push(Content::Text { text });
        }
        Content::Thinking {
            text,
            signature,
            redacted,
        } if !text.is_empty() => {
            if let Some(Content::Thinking {
                text: current,
                signature: current_signature,
                redacted: current_redacted,
            }) = content.last_mut()
                && !current.is_empty()
                && *current_redacted == redacted
            {
                current.push_str(&text);
                if signature.is_some() {
                    *current_signature = signature;
                }
                return;
            }
            content.push(Content::Thinking {
                text,
                signature,
                redacted,
            });
        }
        other => content.push(other),
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
        message: "Google Vertex response exceeds the 8 MiB limit".into(),
    }
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
    if status.is_server_error() || status.as_u16() == 408 {
        return Err(ProviderError::Unavailable {
            message: format!("HTTP {status}: {message}"),
        });
    }
    Err(ProviderError::Protocol {
        message: format!("HTTP {status}: {message}"),
    })
}

fn classify_api_error(value: &Value, secret: &str) -> ProviderError {
    let status = value.pointer("/error/status").and_then(Value::as_str);
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Google Vertex returned an error");
    let message = redact_and_cap(message, secret);
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

fn safe_error_excerpt(body: &[u8], secret: &str) -> String {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Google Vertex returned an error".into());
    redact_and_cap(&message, secret)
}

fn redact_and_cap(message: &str, secret: &str) -> String {
    let redacted = if secret.is_empty() {
        message.to_owned()
    } else {
        message.replace(secret, "[REDACTED]")
    };
    redacted.chars().take(MAX_ERROR_CHARS).collect()
}

fn trim_newline(line: &mut Vec<u8>) {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        line.pop();
    }
}
