use std::{collections::BTreeMap, fmt, io::Read, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::{RequestBuilder, Response, Url};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ThinkingLevel, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 512;
const MAX_EVENT_HEADERS: usize = 64 * 1024;
const SERVICE: &str = "bedrock";

type HmacSha256 = Hmac<Sha256>;

/// Short-lived AWS credentials used to sign a Bedrock request.
pub struct AwsCredentials {
    access_key_id: SecretString,
    secret_access_key: SecretString,
    session_token: Option<SecretString>,
}

impl AwsCredentials {
    /// Creates a validated AWS credential value.
    ///
    /// # Errors
    ///
    /// Returns an authentication error when either required credential is blank.
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<impl Into<String>>,
    ) -> Result<Self, ProviderError> {
        let access_key_id = access_key_id.into();
        let secret_access_key = secret_access_key.into();
        let session_token = session_token.map(Into::into);
        if access_key_id.trim().is_empty()
            || secret_access_key.trim().is_empty()
            || session_token
                .as_ref()
                .is_some_and(|token| token.trim().is_empty())
        {
            return Err(ProviderError::Authentication);
        }
        Ok(Self {
            access_key_id: SecretString::from(access_key_id),
            secret_access_key: SecretString::from(secret_access_key),
            session_token: session_token.map(SecretString::from),
        })
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Supplies refreshable credentials to the Bedrock transport.
///
/// ECS, IMDS, web-identity, SSO, or process-based chains can implement this
/// boundary without coupling the provider transport to the AWS SDK.
#[async_trait]
pub trait BedrockCredentialSource: Send + Sync {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError>;
}

/// Resolves standard environment credentials and static shared-profile files.
///
/// Dynamic AWS chains are deliberately not approximated. Callers using ECS,
/// IMDS, web identity, SSO, or `credential_process` must inject a credential
/// source that implements [`BedrockCredentialSource`].
#[derive(Debug, Clone, Default)]
pub struct EnvironmentCredentialSource {
    profile: Option<String>,
}

impl EnvironmentCredentialSource {
    #[must_use]
    pub fn new(profile: Option<impl Into<String>>) -> Self {
        Self {
            profile: profile.map(Into::into),
        }
    }
}

/// Resolves the Bedrock region using the AWS-compatible precedence chain.
///
/// Resolution order is an explicit value, `AWS_REGION`, `AWS_DEFAULT_REGION`,
/// the selected shared-config profile, a standard Bedrock endpoint, then
/// `us-east-1`.
///
/// # Errors
///
/// Returns a protocol error when the selected region is invalid or the
/// endpoint URL is malformed.
pub async fn resolve_bedrock_region(
    explicit: Option<&str>,
    endpoint: Option<&str>,
    profile: Option<&str>,
) -> Result<String, ProviderError> {
    if let Some(region) = explicit
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
    {
        validate_region(&region)?;
        return Ok(region);
    }

    let profile = profile
        .map(str::to_owned)
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .unwrap_or_else(|| "default".into());
    if let Some(path) = std::env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| aws_home_file("config"))
        && let Ok(contents) = tokio::fs::read_to_string(path).await
    {
        let section = if profile == "default" {
            "default".into()
        } else {
            format!("profile {profile}")
        };
        if let Some(region) = profile_value(&contents, &section, "region") {
            validate_region(&region)?;
            return Ok(region);
        }
    }

    if let Some(endpoint) = endpoint {
        let url = Url::parse(endpoint).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Bedrock endpoint: {error}"),
        })?;
        if let Some(region) = standard_endpoint_region(&url) {
            validate_region(&region)?;
            return Ok(region);
        }
    }
    Ok("us-east-1".into())
}

#[async_trait]
impl BedrockCredentialSource for EnvironmentCredentialSource {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        if let (Some(access_key), Some(secret_key)) = (access_key.as_ref(), secret_key.as_ref()) {
            return AwsCredentials::new(
                access_key,
                secret_key,
                std::env::var("AWS_SESSION_TOKEN").ok(),
            );
        }
        if access_key.is_some() || secret_key.is_some() {
            return Err(ProviderError::Protocol {
                message: "Bedrock ambient environment credentials are incomplete; AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set together".into(),
            });
        }

        let profile = self
            .profile
            .clone()
            .or_else(|| std::env::var("AWS_PROFILE").ok())
            .unwrap_or_else(|| "default".into());
        for (path, section) in profile_candidates(&profile) {
            let Ok(contents) = tokio::fs::read_to_string(path).await else {
                continue;
            };
            if let Some(credentials) = static_profile_credentials(&contents, &section)? {
                return Ok(credentials);
            }
        }
        Err(ProviderError::Protocol {
            message: "Bedrock ambient credentials were unavailable; set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, configure a static AWS profile, or inject an ECS/IMDS/SSO credential source".into(),
        })
    }
}

enum BedrockAuthentication {
    Bearer(SecretString),
    SigV4(Arc<dyn BedrockCredentialSource>),
}

/// Native Amazon Bedrock `ConverseStream` transport.
pub struct BedrockProvider {
    region: String,
    endpoint: Url,
    authentication: BedrockAuthentication,
    client: reqwest::Client,
    max_retries: u32,
}

impl BedrockProvider {
    /// Creates a Bedrock bearer-token transport.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for a blank token or a protocol error
    /// for an invalid region, endpoint, or HTTP client configuration.
    pub fn new_bearer(
        region: &str,
        base_url: Option<&str>,
        token: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        Self::build(
            region,
            base_url,
            BedrockAuthentication::Bearer(SecretString::from(token)),
        )
    }

    /// Creates a `SigV4` transport using environment or static profile credentials.
    ///
    /// Dynamic ambient chains fail closed; use [`Self::new_ambient_with_source`]
    /// for ECS, IMDS, web-identity, SSO, or process-based credentials.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an invalid region, endpoint, or client.
    pub fn new_ambient(
        region: &str,
        base_url: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self, ProviderError> {
        Self::new_ambient_with_source(region, base_url, EnvironmentCredentialSource::new(profile))
    }

    /// Creates a `SigV4` transport backed by a refreshable credential source.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an invalid region, endpoint, or client.
    pub fn new_ambient_with_source(
        region: &str,
        base_url: Option<&str>,
        source: impl BedrockCredentialSource + 'static,
    ) -> Result<Self, ProviderError> {
        Self::build(
            region,
            base_url,
            BedrockAuthentication::SigV4(Arc::new(source)),
        )
    }

    fn build(
        region: &str,
        base_url: Option<&str>,
        authentication: BedrockAuthentication,
    ) -> Result<Self, ProviderError> {
        validate_region(region)?;
        let default_endpoint = default_bedrock_endpoint(region);
        let mut endpoint = Url::parse(base_url.unwrap_or(&default_endpoint)).map_err(|error| {
            ProviderError::Protocol {
                message: format!("invalid Bedrock endpoint: {error}"),
            }
        })?;
        if standard_endpoint_region(&endpoint)
            .is_some_and(|endpoint_region| endpoint_region != region)
        {
            endpoint = Url::parse(&default_endpoint).expect("generated Bedrock endpoint is valid");
        }
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ProviderError::Protocol {
                message: "Bedrock endpoint must not contain credentials, query, or fragment".into(),
            });
        }
        if endpoint.host_str().is_none() {
            return Err(ProviderError::Protocol {
                message: "Bedrock endpoint must include a host".into(),
            });
        }
        if endpoint.scheme() != "https" && !is_loopback_endpoint(&endpoint) {
            return Err(ProviderError::Protocol {
                message: "Bedrock endpoint must use HTTPS unless it is a loopback test endpoint"
                    .into(),
            });
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProviderError::Protocol {
                message: "cannot build Bedrock HTTP client".into(),
            })?;
        Ok(Self {
            region: region.into(),
            endpoint,
            authentication,
            client,
            max_retries: 2,
        })
    }

    /// Returns the JSON body that will be sent to `ConverseStream`.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for invalid image data or content that Bedrock
    /// cannot represent.
    pub fn request_preview(&self, request: &ModelRequest) -> Result<Value, ProviderError> {
        build_request_body(request)
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
        let body = serde_json::to_vec(&build_request_body(request)?).map_err(|error| {
            ProviderError::Protocol {
                message: format!("cannot encode Bedrock request: {error}"),
            }
        })?;
        let url = converse_stream_url(&self.endpoint, &request.model)?;
        let mut attempt = 0_u32;
        let (response, redaction) = loop {
            let prepared = self
                .prepare_request(url.clone(), &body, cancellation)
                .await?;
            match send_cancellable(prepared.builder, cancellation).await {
                Ok(response) if response.status().is_success() => {
                    break (response, prepared.redaction);
                }
                Ok(response) => {
                    let error =
                        classify_error_response(response, cancellation, &prepared.redaction)
                            .await?;
                    if error.is_retryable() && attempt < self.max_retries {
                        retry_delay(attempt, cancellation).await?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    return Err(error);
                }
                Err(error) if error.is_retryable() && attempt < self.max_retries => {
                    retry_delay(attempt, cancellation).await?;
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        };

        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large());
        }
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut chunks = response.bytes_stream();
        let mut decoder = EventStreamDecoder::default();
        let mut accumulator = BedrockAccumulator::default();
        let mut received = 0_usize;
        loop {
            let Some(chunk) = next_cancellable(&mut chunks, cancellation).await? else {
                break;
            };
            let chunk = chunk.map_err(|error| classify_transport(&error))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(response_too_large());
            }
            for event in decoder.push(&chunk)? {
                accumulator.apply(event, sink, &redaction).await?;
            }
        }
        decoder.finish()?;
        accumulator.finish(request_id)
    }

    async fn prepare_request(
        &self,
        url: Url,
        body: &[u8],
        cancellation: Option<&CancellationToken>,
    ) -> Result<PreparedRequest, ProviderError> {
        let common = self
            .client
            .post(url.clone())
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("x-amzn-bedrock-accept", "application/json")
            .body(body.to_vec());
        match &self.authentication {
            BedrockAuthentication::Bearer(token) => Ok(PreparedRequest {
                builder: common.bearer_auth(token.expose_secret()),
                redaction: Redaction::new([token.expose_secret()]),
            }),
            BedrockAuthentication::SigV4(source) => {
                let credentials = credentials_cancellable(source, cancellation).await?;
                let headers = sign_request(&url, body, &self.region, &credentials, Utc::now())?;
                let mut builder = common;
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                let mut secrets = vec![
                    credentials.access_key_id.expose_secret(),
                    credentials.secret_access_key.expose_secret(),
                ];
                if let Some(token) = &credentials.session_token {
                    secrets.push(token.expose_secret());
                }
                Ok(PreparedRequest {
                    builder,
                    redaction: Redaction::new(secrets),
                })
            }
        }
    }
}

async fn credentials_cancellable(
    source: &Arc<dyn BedrockCredentialSource>,
    cancellation: Option<&CancellationToken>,
) -> Result<AwsCredentials, ProviderError> {
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ProviderError::Aborted),
            credentials = source.credentials() => credentials,
        }
    } else {
        source.credentials().await
    }
}

impl fmt::Debug for BedrockProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BedrockProvider")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("authentication", &"[REDACTED]")
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, &DiscardEvents, None).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, sink, None).await
    }
}

struct DiscardEvents;

#[async_trait]
impl ProviderEventSink for DiscardEvents {
    async fn emit(&self, _event: ProviderEvent) {}
}

struct PreparedRequest {
    builder: RequestBuilder,
    redaction: Redaction,
}

#[derive(Default)]
struct Redaction(Vec<SecretString>);

impl Redaction {
    fn new<'a>(secrets: impl IntoIterator<Item = &'a str>) -> Self {
        Self(
            secrets
                .into_iter()
                .filter(|secret| !secret.is_empty())
                .map(|secret| SecretString::from(secret.to_owned()))
                .collect(),
        )
    }

    fn sanitize(&self, message: &str) -> String {
        let mut value = message.to_owned();
        for secret in &self.0 {
            value = value.replace(secret.expose_secret(), "[REDACTED]");
        }
        value.chars().take(MAX_ERROR_CHARS).collect()
    }
}

fn build_request_body(request: &ModelRequest) -> Result<Value, ProviderError> {
    let mut system_parts = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        system_parts.push(request.system_prompt.trim().to_owned());
    }
    for message in &request.messages {
        if message.role == Role::System {
            let text = message.text();
            if !text.trim().is_empty() {
                system_parts.push(text);
            }
        }
    }

    let mut messages = Vec::<Value>::new();
    let mut index = 0_usize;
    while index < request.messages.len() {
        let message = &request.messages[index];
        if message.role == Role::System {
            index = index.saturating_add(1);
            continue;
        }
        if message.role == Role::Tool {
            let mut blocks = Vec::new();
            while index < request.messages.len() && request.messages[index].role == Role::Tool {
                append_tool_results(&request.messages[index], &mut blocks)?;
                index = index.saturating_add(1);
            }
            if !blocks.is_empty() {
                messages.push(json!({"role": "user", "content": blocks}));
            }
            continue;
        }
        if let Some(translated) = translate_message(message, &request.model)? {
            messages.push(translated);
        }
        index = index.saturating_add(1);
    }

    let mut body = json!({
        "messages": messages,
        "inferenceConfig": {"maxTokens": request.max_output_tokens}
    });
    if !system_parts.is_empty() {
        let mut blocks = system_parts
            .into_iter()
            .map(|text| json!({"text": text}))
            .collect::<Vec<_>>();
        if supports_prompt_caching(&request.model) {
            blocks.push(json!({"cachePoint": {"type": "default"}}));
        }
        body["system"] = Value::Array(blocks);
    }
    if !request.tools.is_empty() {
        body["toolConfig"] = json!({
            "tools": request.tools.iter().map(|tool| json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": {"json": tool.parameters}
                }
            })).collect::<Vec<_>>()
        });
    }
    if let Some(fields) = thinking_fields(request) {
        body["additionalModelRequestFields"] = fields;
    }
    if supports_prompt_caching(&request.model)
        && let Some(message) = body["messages"].as_array_mut().and_then(|messages| {
            messages
                .iter_mut()
                .rev()
                .find(|message| message["role"] == "user")
        })
        && let Some(content) = message["content"].as_array_mut()
    {
        content.push(json!({"cachePoint": {"type": "default"}}));
    }
    Ok(body)
}

fn translate_message(message: &Message, model: &str) -> Result<Option<Value>, ProviderError> {
    let mut blocks = Vec::new();
    match message.role {
        Role::User => {
            for block in &message.content {
                match block {
                    Content::Text { text } if !text.is_empty() => {
                        blocks.push(json!({"text": text}));
                    }
                    Content::Image { data, mime_type } => {
                        let format = image_format(mime_type)?;
                        validate_base64(data)?;
                        blocks.push(json!({
                            "image": {"format": format, "source": {"bytes": data}}
                        }));
                    }
                    Content::Text { .. }
                    | Content::Thinking { .. }
                    | Content::ToolCall(_)
                    | Content::ToolResult(_) => {}
                }
            }
        }
        Role::Assistant => {
            for block in &message.content {
                match block {
                    Content::Text { text } if !text.trim().is_empty() => {
                        blocks.push(json!({"text": text}));
                    }
                    Content::Thinking {
                        signature: Some(signature),
                        redacted: true,
                        ..
                    } if !signature.is_empty() && is_anthropic_model(model) => {
                        validate_base64(signature)?;
                        blocks.push(json!({
                            "reasoningContent": {"redactedContent": signature}
                        }));
                    }
                    Content::Thinking {
                        text,
                        signature,
                        redacted: false,
                    } if !text.trim().is_empty() => {
                        if is_anthropic_model(model) {
                            if let Some(signature) =
                                signature.as_ref().filter(|value| !value.is_empty())
                            {
                                blocks.push(json!({
                                    "reasoningContent": {"reasoningText": {
                                        "text": text,
                                        "signature": signature
                                    }}
                                }));
                            } else {
                                blocks.push(json!({"text": text}));
                            }
                        } else {
                            blocks.push(json!({
                                "reasoningContent": {"reasoningText": {"text": text}}
                            }));
                        }
                    }
                    Content::ToolCall(call) => {
                        blocks.push(json!({
                            "toolUse": {
                                "toolUseId": valid_tool_call_id(&call.id)?,
                                "name": call.name,
                                "input": call.arguments
                            }
                        }));
                    }
                    Content::Text { .. }
                    | Content::Image { .. }
                    | Content::Thinking { .. }
                    | Content::ToolResult(_) => {}
                }
            }
        }
        Role::System | Role::Tool => return Ok(None),
    }
    Ok((!blocks.is_empty()).then(|| {
        json!({
            "role": if message.role == Role::Assistant { "assistant" } else { "user" },
            "content": blocks
        })
    }))
}

fn append_tool_results(message: &Message, blocks: &mut Vec<Value>) -> Result<(), ProviderError> {
    for content in &message.content {
        if let Content::ToolResult(result) = content {
            blocks.push(json!({
                "toolResult": {
                    "toolUseId": valid_tool_call_id(&result.tool_call_id)?,
                    "content": [{"text": result.content}],
                    "status": if result.is_error { "error" } else { "success" }
                }
            }));
        }
    }
    Ok(())
}

fn validate_base64(data: &str) -> Result<(), ProviderError> {
    let mut decoder = base64::read::DecoderReader::new(data.as_bytes(), &BASE64);
    let mut buffer = [0_u8; 4096];
    loop {
        match decoder.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                return Err(ProviderError::Protocol {
                    message: "Bedrock image content is not valid base64".into(),
                });
            }
        }
    }
}

fn image_format(mime_type: &str) -> Result<&'static str, ProviderError> {
    match mime_type.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Ok("jpeg"),
        "image/png" => Ok("png"),
        "image/gif" => Ok("gif"),
        "image/webp" => Ok("webp"),
        _ => Err(ProviderError::Protocol {
            message: format!("unsupported Bedrock image type: {mime_type}"),
        }),
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

fn valid_tool_call_id(id: &str) -> Result<String, ProviderError> {
    let id = normalize_tool_call_id(id);
    if id.is_empty() {
        Err(ProviderError::Protocol {
            message: "Bedrock tool call id cannot be empty".into(),
        })
    } else {
        Ok(id)
    }
}

fn thinking_fields(request: &ModelRequest) -> Option<Value> {
    if request.thinking_level == ThinkingLevel::Off || !is_anthropic_model(&request.model) {
        return None;
    }
    if supports_adaptive_thinking(&request.model) {
        return Some(json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": thinking_effort(request)}
        }));
    }
    if request.max_output_tokens <= 1_024 {
        return None;
    }
    let requested = match request.thinking_level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16_384,
    };
    let budget = requested.min(request.max_output_tokens.saturating_sub(1));
    Some(json!({
        "thinking": {"type": "enabled", "budget_tokens": budget, "display": "summarized"},
        "anthropic_beta": ["interleaved-thinking-2025-05-14"]
    }))
}

fn thinking_effort(request: &ModelRequest) -> &'static str {
    match request.thinking_effort.as_deref() {
        Some("low") => "low",
        Some("medium") => "medium",
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

fn is_anthropic_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("anthropic.claude")
        || model.contains("anthropic/claude")
        || model.contains("claude")
}

fn supports_prompt_caching(model: &str) -> bool {
    let model = model.to_ascii_lowercase().replace(['.', '_', ':'], "-");
    model.contains("claude")
        && (model.contains("-4-")
            || model.contains("claude-3-7-sonnet")
            || model.contains("claude-3-5-haiku"))
}

fn supports_adaptive_thinking(model: &str) -> bool {
    let model = model.to_ascii_lowercase().replace(['.', '_', ':'], "-");
    [
        "opus-4-6",
        "opus-4-7",
        "opus-4-8",
        "opus-5",
        "sonnet-4-6",
        "sonnet-5",
        "fable-5",
        "mythos-5",
        "mythos-preview",
    ]
    .iter()
    .any(|fragment| model.contains(fragment))
}

fn validate_region(region: &str) -> Result<(), ProviderError> {
    if region.is_empty()
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ProviderError::Protocol {
            message: "invalid AWS region for Bedrock".into(),
        });
    }
    Ok(())
}

fn default_bedrock_endpoint(region: &str) -> String {
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://bedrock-runtime.{region}.{suffix}")
}

fn standard_endpoint_region(endpoint: &Url) -> Option<String> {
    let host = endpoint.host_str()?.to_ascii_lowercase();
    let labels = host.split('.').collect::<Vec<_>>();
    let valid_suffix = labels.get(2..) == Some(&["amazonaws", "com"])
        || labels.get(2..) == Some(&["amazonaws", "com", "cn"]);
    (valid_suffix
        && matches!(
            labels.first().copied(),
            Some("bedrock-runtime" | "bedrock-runtime-fips")
        ))
    .then(|| labels[1].to_owned())
}

fn is_loopback_endpoint(endpoint: &Url) -> bool {
    endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn converse_stream_url(endpoint: &Url, model: &str) -> Result<Url, ProviderError> {
    if model.trim().is_empty() || model.chars().any(char::is_control) {
        return Err(ProviderError::Protocol {
            message: "invalid Bedrock model id".into(),
        });
    }
    let mut url = endpoint.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| ProviderError::Protocol {
                message: "Bedrock endpoint cannot be used as a base URL".into(),
            })?;
        segments.pop_if_empty();
        segments.push("model");
        segments.push(model);
        segments.push("converse-stream");
    }
    Ok(url)
}

fn sign_request(
    url: &Url,
    body: &[u8],
    region: &str,
    credentials: &AwsCredentials,
    now: DateTime<Utc>,
) -> Result<BTreeMap<&'static str, String>, ProviderError> {
    let date = now.format("%Y%m%d").to_string();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let host = canonical_host(url)?;
    let payload_hash = hex_sha256(body);
    let mut canonical_headers = BTreeMap::new();
    canonical_headers.insert("content-type", "application/json".to_owned());
    canonical_headers.insert("host", host);
    canonical_headers.insert("x-amz-content-sha256", payload_hash.clone());
    canonical_headers.insert("x-amz-date", timestamp.clone());
    if let Some(token) = &credentials.session_token {
        canonical_headers.insert("x-amz-security-token", token.expose_secret().to_owned());
    }
    let signed_headers = canonical_headers
        .keys()
        .copied()
        .collect::<Vec<_>>()
        .join(";");
    let canonical_header_text = canonical_headers
        .iter()
        .map(|(name, value)| format!("{name}:{}", normalize_header(value)))
        .collect::<Vec<_>>()
        .join("\n");
    let canonical_request = format!(
        "POST\n{}\n{}\n{canonical_header_text}\n{signed_headers}\n{payload_hash}",
        canonical_uri(url),
        canonical_query(url)
    );
    let scope = format!("{date}/{region}/{SERVICE}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );
    let date_key = hmac(
        format!("AWS4{}", credentials.secret_access_key.expose_secret()).as_bytes(),
        date.as_bytes(),
    )?;
    let region_key = hmac(&date_key, region.as_bytes())?;
    let service_key = hmac(&region_key, SERVICE.as_bytes())?;
    let signing_key = hmac(&service_key, b"aws4_request")?;
    let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes())?);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id.expose_secret()
    );

    let mut headers = BTreeMap::new();
    headers.insert("authorization", authorization);
    headers.insert("x-amz-content-sha256", payload_hash);
    headers.insert("x-amz-date", timestamp);
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token", token.expose_secret().to_owned());
    }
    Ok(headers)
}

fn canonical_host(url: &Url) -> Result<String, ProviderError> {
    let host = url.host_str().ok_or_else(|| ProviderError::Protocol {
        message: "Bedrock endpoint is missing a host".into(),
    })?;
    Ok(url
        .port()
        .map_or_else(|| host.into(), |port| format!("{host}:{port}")))
}

fn canonical_uri(url: &Url) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let mut canonical = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte == b'/' || byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
        {
            canonical.push(char::from(byte));
        } else {
            canonical.push('%');
            canonical.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            canonical.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 0x0f)]));
        }
    }
    canonical
}

fn canonical_query(url: &Url) -> String {
    let mut pairs = url.query_pairs().collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn normalize_header(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hmac(key: &[u8], value: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| ProviderError::Protocol {
        message: "cannot initialize Bedrock SigV4 signer".into(),
    })?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_sha256(value: &[u8]) -> String {
    hex(&Sha256::digest(value))
}

fn hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Debug)]
struct EventStreamMessage {
    headers: BTreeMap<String, String>,
    payload: Vec<u8>,
}

#[derive(Default)]
struct EventStreamDecoder {
    pending: Vec<u8>,
}

impl EventStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventStreamMessage>, ProviderError> {
        if self.pending.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(response_too_large());
        }
        self.pending.extend_from_slice(chunk);
        let mut messages = Vec::new();
        loop {
            if self.pending.len() < 12 {
                break;
            }
            let total_len = read_u32(&self.pending[0..4]) as usize;
            let header_len = read_u32(&self.pending[4..8]) as usize;
            if !(16..=MAX_RESPONSE_BYTES).contains(&total_len)
                || header_len > MAX_EVENT_HEADERS
                || header_len > total_len.saturating_sub(16)
            {
                return Err(event_protocol_error("invalid frame lengths"));
            }
            if crc32(&self.pending[0..8]) != read_u32(&self.pending[8..12]) {
                return Err(event_protocol_error("prelude checksum mismatch"));
            }
            if self.pending.len() < total_len {
                break;
            }
            let frame = self.pending.drain(..total_len).collect::<Vec<_>>();
            if crc32(&frame[..total_len - 4]) != read_u32(&frame[total_len - 4..]) {
                return Err(event_protocol_error("message checksum mismatch"));
            }
            let headers = parse_event_headers(&frame[12..12 + header_len])?;
            let payload = frame[12 + header_len..total_len - 4].to_vec();
            messages.push(EventStreamMessage { headers, payload });
        }
        Ok(messages)
    }

    fn finish(&self) -> Result<(), ProviderError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(event_protocol_error("truncated final frame"))
        }
    }
}

fn parse_event_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, ProviderError> {
    let mut headers = BTreeMap::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let name_len = usize::from(take_byte(bytes, &mut cursor)?);
        let name = take(bytes, &mut cursor, name_len)?;
        let name =
            std::str::from_utf8(name).map_err(|_| event_protocol_error("invalid header name"))?;
        let kind = take_byte(bytes, &mut cursor)?;
        let value = match kind {
            0 => "true".into(),
            1 => "false".into(),
            2 => take_byte(bytes, &mut cursor)?.to_string(),
            3 => i16::from_be_bytes(take_array(bytes, &mut cursor)?).to_string(),
            4 => i32::from_be_bytes(take_array(bytes, &mut cursor)?).to_string(),
            5 | 8 => i64::from_be_bytes(take_array(bytes, &mut cursor)?).to_string(),
            6 => {
                let length = usize::from(u16::from_be_bytes(take_array(bytes, &mut cursor)?));
                BASE64.encode(take(bytes, &mut cursor, length)?)
            }
            7 => {
                let length = usize::from(u16::from_be_bytes(take_array(bytes, &mut cursor)?));
                let value = take(bytes, &mut cursor, length)?;
                std::str::from_utf8(value)
                    .map_err(|_| event_protocol_error("invalid string header"))?
                    .into()
            }
            9 => {
                let value = take(bytes, &mut cursor, 16)?;
                hex(value)
            }
            _ => return Err(event_protocol_error("unknown header value type")),
        };
        headers.insert(name.into(), value);
    }
    Ok(headers)
}

fn take_byte(bytes: &[u8], cursor: &mut usize) -> Result<u8, ProviderError> {
    let value = *bytes
        .get(*cursor)
        .ok_or_else(|| event_protocol_error("truncated header"))?;
    *cursor = cursor.saturating_add(1);
    Ok(value)
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], ProviderError> {
    let end = cursor.saturating_add(length);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| event_protocol_error("truncated header value"))?;
    *cursor = end;
    Ok(value)
}

fn take_array<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N], ProviderError> {
    take(bytes, cursor, N)?
        .try_into()
        .map_err(|_| event_protocol_error("truncated fixed header value"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("four-byte slice"))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn event_protocol_error(detail: &str) -> ProviderError {
    ProviderError::Protocol {
        message: format!("invalid Bedrock event stream: {detail}"),
    }
}

enum PartialBlock {
    Text(String),
    Thinking {
        text: String,
        signature: String,
        redacted_content: Vec<u8>,
        redacted: bool,
    },
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Default)]
struct BedrockAccumulator {
    started: bool,
    blocks: BTreeMap<u64, PartialBlock>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl BedrockAccumulator {
    async fn apply(
        &mut self,
        event: EventStreamMessage,
        sink: &dyn ProviderEventSink,
        redaction: &Redaction,
    ) -> Result<(), ProviderError> {
        let message_type = event
            .headers
            .get(":message-type")
            .map_or("event", String::as_str);
        let event_type = event
            .headers
            .get(":event-type")
            .or_else(|| event.headers.get(":exception-type"))
            .map_or("unknown", String::as_str);
        let value: Value = serde_json::from_slice(&event.payload)
            .map_err(|_| event_protocol_error("event payload is not valid JSON"))?;
        if message_type == "exception" || event_type.ends_with("Exception") {
            return Err(classify_stream_exception(event_type, &value, redaction));
        }
        match event_type {
            "messageStart" => {
                if value.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err(event_protocol_error("messageStart role is not assistant"));
                }
                self.started = true;
            }
            "contentBlockStart" => self.apply_block_start(&value)?,
            "contentBlockDelta" => self.apply_block_delta(&value, sink).await?,
            "contentBlockStop" => Self::apply_block_stop(&value)?,
            "metadata" => self.usage = parse_usage(value.get("usage")),
            "messageStop" => {
                self.stop_reason = Some(map_stop_reason(
                    value.get("stopReason").and_then(Value::as_str),
                )?);
            }
            _ => return Err(event_protocol_error("unknown event type")),
        }
        Ok(())
    }

    fn apply_block_start(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = event_index(value)?;
        if let Some(tool) = value.pointer("/start/toolUse") {
            let id = required_string(tool, "toolUseId", "tool use id")?;
            let name = required_string(tool, "name", "tool name")?;
            self.insert_block(
                index,
                PartialBlock::Tool {
                    id,
                    name,
                    arguments: String::new(),
                },
            )?;
        }
        Ok(())
    }

    async fn apply_block_delta(
        &mut self,
        value: &Value,
        sink: &dyn ProviderEventSink,
    ) -> Result<(), ProviderError> {
        let index = event_index(value)?;
        if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str) {
            let block = self
                .blocks
                .entry(index)
                .or_insert_with(|| PartialBlock::Text(String::new()));
            let PartialBlock::Text(output) = block else {
                return Err(event_protocol_error("text delta changed block type"));
            };
            output.push_str(text);
            sink.emit(ProviderEvent::TextDelta(text.into())).await;
            return Ok(());
        }
        if let Some(reasoning) = value.pointer("/delta/reasoningContent") {
            let block = self
                .blocks
                .entry(index)
                .or_insert_with(|| PartialBlock::Thinking {
                    text: String::new(),
                    signature: String::new(),
                    redacted_content: Vec::new(),
                    redacted: false,
                });
            let PartialBlock::Thinking {
                text,
                signature,
                redacted_content,
                redacted,
            } = block
            else {
                return Err(event_protocol_error("reasoning delta changed block type"));
            };
            if let Some(delta) = reasoning.get("text").and_then(Value::as_str) {
                text.push_str(delta);
                sink.emit(ProviderEvent::ThinkingDelta(delta.into())).await;
            }
            if let Some(delta) = reasoning.get("signature").and_then(Value::as_str) {
                signature.push_str(delta);
            }
            if let Some(data) = reasoning.get("redactedContent").and_then(Value::as_str) {
                let bytes = BASE64
                    .decode(data)
                    .map_err(|_| event_protocol_error("redacted reasoning is not valid base64"))?;
                redacted_content.extend_from_slice(&bytes);
                *redacted = true;
            }
            return Ok(());
        }
        if let Some(input) = value
            .pointer("/delta/toolUse/input")
            .and_then(Value::as_str)
        {
            let Some(PartialBlock::Tool { arguments, .. }) = self.blocks.get_mut(&index) else {
                return Err(event_protocol_error("tool delta has no matching start"));
            };
            arguments.push_str(input);
        }
        Ok(())
    }

    fn apply_block_stop(value: &Value) -> Result<(), ProviderError> {
        event_index(value)?;
        Ok(())
    }

    fn insert_block(&mut self, index: u64, block: PartialBlock) -> Result<(), ProviderError> {
        if self.blocks.insert(index, block).is_some() {
            return Err(event_protocol_error("duplicate content block index"));
        }
        Ok(())
    }

    fn finish(self, request_id: Option<String>) -> Result<ModelResponse, ProviderError> {
        if !self.started {
            return Err(event_protocol_error("stream has no messageStart event"));
        }
        let mut stop_reason = self
            .stop_reason
            .ok_or_else(|| event_protocol_error("stream has no messageStop event"))?;
        let mut content = Vec::new();
        for (_, block) in self.blocks {
            match block {
                PartialBlock::Text(text) if !text.is_empty() => {
                    content.push(Content::Text { text });
                }
                PartialBlock::Thinking {
                    text,
                    signature,
                    redacted_content,
                    redacted,
                } if !text.is_empty() || !signature.is_empty() || !redacted_content.is_empty() => {
                    let signature = if redacted {
                        (!redacted_content.is_empty()).then(|| BASE64.encode(redacted_content))
                    } else {
                        (!signature.is_empty()).then_some(signature)
                    };
                    content.push(Content::Thinking {
                        text,
                        signature,
                        redacted,
                    });
                }
                PartialBlock::Tool {
                    id,
                    name,
                    arguments,
                } => {
                    let arguments = serde_json::from_str(&arguments).map_err(|error| {
                        ProviderError::Protocol {
                            message: format!("Bedrock tool arguments are invalid JSON: {error}"),
                        }
                    })?;
                    content.push(Content::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
                PartialBlock::Text(_) | PartialBlock::Thinking { .. } => {}
            }
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
            response_id: request_id,
        })
    }
}

fn event_index(value: &Value) -> Result<u64, ProviderError> {
    value
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| event_protocol_error("event has no content block index"))
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| event_protocol_error(&format!("missing {label}")))
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let value = value.unwrap_or(&Value::Null);
    Usage {
        input_tokens: value
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens: value
            .get("cacheReadInputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: value
            .get("cacheWriteInputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn map_stop_reason(value: Option<&str>) -> Result<StopReason, ProviderError> {
    match value {
        Some("end_turn" | "stop_sequence") => Ok(StopReason::Stop),
        Some("max_tokens" | "model_context_window_exceeded") => Ok(StopReason::Length),
        Some("tool_use") => Ok(StopReason::ToolUse),
        _ => Err(event_protocol_error("unknown stop reason")),
    }
}

fn classify_stream_exception(kind: &str, value: &Value, redaction: &Redaction) -> ProviderError {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Bedrock stream failed");
    let message = redaction.sanitize(message);
    match kind {
        "throttlingException" | "ThrottlingException" => ProviderError::RateLimited { message },
        "internalServerException"
        | "InternalServerException"
        | "modelStreamErrorException"
        | "ModelStreamErrorException"
        | "serviceUnavailableException"
        | "ServiceUnavailableException" => ProviderError::Unavailable { message },
        _ => ProviderError::Protocol { message },
    }
}

async fn classify_error_response(
    response: Response,
    cancellation: Option<&CancellationToken>,
    redaction: &Redaction,
) -> Result<ProviderError, ProviderError> {
    let status = response.status();
    let body = read_bounded_body(response, cancellation).await?;
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let message = value
        .get("message")
        .or_else(|| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .unwrap_or("Bedrock returned an error");
    let message = redaction.sanitize(message);
    Ok(match status.as_u16() {
        401 | 403 => ProviderError::Authentication,
        429 => ProviderError::RateLimited { message },
        408 | 424 => ProviderError::Unavailable {
            message: format!("HTTP {status}: {message}"),
        },
        _ if status.is_server_error() => ProviderError::Unavailable {
            message: format!("HTTP {status}: {message}"),
        },
        _ => ProviderError::Protocol {
            message: format!("HTTP {status}: {message}"),
        },
    })
}

async fn send_cancellable(
    request: RequestBuilder,
    cancellation: Option<&CancellationToken>,
) -> Result<Response, ProviderError> {
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ProviderError::Aborted),
            response = request.send() => response.map_err(|error| classify_transport(&error)),
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

async fn retry_delay(
    attempt: u32,
    cancellation: Option<&CancellationToken>,
) -> Result<(), ProviderError> {
    let delay = Duration::from_millis(200_u64.saturating_mul(1_u64 << attempt.min(4)));
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ProviderError::Aborted),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

fn classify_transport(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() || error.is_connect() {
        ProviderError::Unavailable {
            message: "Bedrock endpoint is unavailable".into(),
        }
    } else {
        ProviderError::Protocol {
            message: "Bedrock HTTP transport failed".into(),
        }
    }
}

fn response_too_large() -> ProviderError {
    ProviderError::Protocol {
        message: "Bedrock response exceeds the 8 MiB limit".into(),
    }
}

fn profile_candidates(profile: &str) -> Vec<(PathBuf, String)> {
    let credentials = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| aws_home_file("credentials"));
    let config = std::env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| aws_home_file("config"));
    let mut paths = Vec::new();
    if let Some(path) = credentials {
        paths.push((path, profile.into()));
    }
    if let Some(path) = config {
        paths.push((
            path,
            if profile == "default" {
                "default".into()
            } else {
                format!("profile {profile}")
            },
        ));
    }
    paths
}

fn aws_home_file(name: &str) -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".aws").join(name))
}

fn static_profile_credentials(
    contents: &str,
    wanted_section: &str,
) -> Result<Option<AwsCredentials>, ProviderError> {
    let mut section = "";
    let mut values = Map::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            if section == wanted_section {
                break;
            }
            section = name.trim();
            continue;
        }
        if section != wanted_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(key.trim().into(), Value::String(value.trim().into()));
    }
    let access = values.get("aws_access_key_id").and_then(Value::as_str);
    let secret = values.get("aws_secret_access_key").and_then(Value::as_str);
    match (access, secret) {
        (Some(access), Some(secret)) => AwsCredentials::new(
            access,
            secret,
            values
                .get("aws_session_token")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .map(Some),
        (None, None) => Ok(None),
        _ => Err(ProviderError::Protocol {
            message: format!("AWS profile [{wanted_section}] has incomplete static credentials"),
        }),
    }
}

fn profile_value(contents: &str, wanted_section: &str, wanted_key: &str) -> Option<String> {
    let mut section = "";
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            if section == wanted_section {
                break;
            }
            section = name.trim();
            continue;
        }
        if section != wanted_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == wanted_key {
            return Some(value.trim().into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_matches_deterministic_fixture() {
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None::<String>,
        )
        .unwrap();
        let url = Url::parse(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse-stream",
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let headers =
            sign_request(&url, br#"{"messages":[]}"#, "us-east-1", &credentials, now).unwrap();
        assert_eq!(headers["x-amz-date"], "20150830T123600Z");
        assert_eq!(
            headers["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=a31befc9ea8837ae0e54d2419fb6393b1c54bb56cdd45f77e3f0dfbb6d1d3ed2"
        );
    }

    #[test]
    fn static_profile_parser_is_section_scoped() {
        let contents = "[other]\naws_access_key_id = wrong\naws_secret_access_key = wrong\n\n[dev]\naws_access_key_id = access\naws_secret_access_key = secret\naws_session_token = token\n";
        let credentials = static_profile_credentials(contents, "dev")
            .unwrap()
            .unwrap();
        assert_eq!(credentials.access_key_id.expose_secret(), "access");
        assert_eq!(credentials.session_token.unwrap().expose_secret(), "token");
    }

    #[test]
    fn standard_endpoint_and_profile_region_resolution_are_scoped() {
        let endpoint =
            Url::parse("https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com").unwrap();
        assert_eq!(
            standard_endpoint_region(&endpoint).as_deref(),
            Some("us-gov-west-1")
        );
        let contents = "[default]\nregion = us-east-1\n[profile dev]\nregion = eu-west-2\n";
        assert_eq!(
            profile_value(contents, "profile dev", "region").as_deref(),
            Some("eu-west-2")
        );
    }

    #[test]
    fn model_path_and_sigv4_uri_use_aws_encoding_rules() {
        let endpoint = Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com").unwrap();
        let url = converse_stream_url(&endpoint, "arn:aws:bedrock:test/profile/name").unwrap();
        assert_eq!(
            url.path(),
            "/model/arn:aws:bedrock:test%2Fprofile%2Fname/converse-stream"
        );
        assert_eq!(
            canonical_uri(&url),
            "/model/arn%3Aaws%3Abedrock%3Atest%252Fprofile%252Fname/converse-stream"
        );
    }

    #[test]
    fn event_stream_decoder_rejects_corrupt_prelude() {
        let mut decoder = EventStreamDecoder::default();
        let frame = [0_u8, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let error = decoder.push(&frame).unwrap_err();
        assert!(error.to_string().contains("prelude checksum mismatch"));
    }
}
