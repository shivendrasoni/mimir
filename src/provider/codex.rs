use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::model::{
    Content, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage,
};

use super::{Provider, ProviderError, ProviderEvent, ProviderEventSink};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(300);
type CatalogCacheEntry = (Instant, BTreeSet<String>);
type CatalogCache = tokio::sync::Mutex<HashMap<String, CatalogCacheEntry>>;

pub struct CodexProvider {
    base_url: String,
    token: SecretString,
    account_id: Option<String>,
    codex_backend: bool,
    client: reqwest::Client,
    priority_service_tier: AtomicBool,
}

impl CodexProvider {
    /// Creates the native subscription transport from an `OAuth` access token.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for a blank token, invalid URL, missing account claim,
    /// or an HTTP client configuration failure.
    pub fn new(
        base_url: Option<&str>,
        token: impl Into<String>,
        account_id: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let base_url = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
        reqwest::Url::parse(base_url).map_err(|error| ProviderError::Protocol {
            message: format!("invalid Codex base URL: {error}"),
        })?;
        let account_id = account_id
            .map(str::to_owned)
            .or_else(|| extract_account_id(&token))
            .ok_or_else(|| ProviderError::Protocol {
                message: "Codex OAuth token has no ChatGPT account identifier".into(),
            })?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build Codex HTTP client: {error}"),
            })?;
        Ok(Self {
            base_url: base_url.into(),
            token: SecretString::from(token),
            account_id: Some(account_id),
            codex_backend: true,
            client,
            priority_service_tier: AtomicBool::new(false),
        })
    }

    /// Creates the native `OpenAI` Responses transport from an API key.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for a blank key, invalid URL, or HTTP client
    /// configuration failure.
    pub fn new_openai(
        base_url: Option<&str>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let token = api_key.into();
        if token.trim().is_empty() {
            return Err(ProviderError::Authentication);
        }
        let base_url = base_url
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/');
        reqwest::Url::parse(base_url).map_err(|error| ProviderError::Protocol {
            message: format!("invalid OpenAI base URL: {error}"),
        })?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| ProviderError::Protocol {
                message: format!("cannot build OpenAI HTTP client: {error}"),
            })?;
        Ok(Self {
            base_url: base_url.into(),
            token: SecretString::from(token),
            account_id: None,
            codex_backend: false,
            client,
            priority_service_tier: AtomicBool::new(false),
        })
    }

    fn endpoint(&self) -> String {
        if !self.codex_backend {
            return if self.base_url.ends_with("/responses") {
                self.base_url.clone()
            } else {
                format!("{}/responses", self.base_url)
            };
        }
        if self.base_url.ends_with("/codex/responses") {
            self.base_url.clone()
        } else if self.base_url.ends_with("/codex") {
            format!("{}/responses", self.base_url)
        } else {
            format!("{}/codex/responses", self.base_url)
        }
    }

    fn models_endpoint(&self) -> Result<reqwest::Url, ProviderError> {
        let normalized = self.base_url.trim_end_matches('/');
        let endpoint = if normalized.ends_with("/codex/responses") {
            format!("{}/models", normalized.trim_end_matches("/responses"))
        } else if normalized.ends_with("/codex") {
            format!("{normalized}/models")
        } else {
            format!("{normalized}/codex/models")
        };
        let mut endpoint =
            reqwest::Url::parse(&endpoint).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Codex model catalog URL: {error}"),
            })?;
        endpoint
            .query_pairs_mut()
            .append_pair("client_version", env!("CARGO_PKG_VERSION"));
        Ok(endpoint)
    }

    async fn discover_model_ids(&self) -> Result<Vec<String>, ProviderError> {
        if !self.codex_backend {
            return Ok(Vec::new());
        }
        let account_id = self
            .account_id
            .as_deref()
            .ok_or_else(|| ProviderError::Protocol {
                message: "Codex model discovery requires an account identifier".into(),
            })?;
        let cache_key = format!("{}\0{account_id}", self.base_url);
        let cache = codex_catalog_cache();
        if let Some((recorded, ids)) = cache.lock().await.get(&cache_key)
            && recorded.elapsed() < CATALOG_CACHE_TTL
        {
            return Ok(ids.iter().cloned().collect());
        }
        let response = self
            .client
            .get(self.models_endpoint()?)
            .bearer_auth(self.token.expose_secret())
            .header("chatgpt-account-id", account_id)
            .header("originator", "mimir")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::Authentication);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable {
                message: format!("Codex model discovery returned HTTP {status}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(ProviderError::Protocol {
                message: "Codex model catalog exceeds the 1 MiB limit".into(),
            });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| classify_transport(&error))?;
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(ProviderError::Protocol {
                message: "Codex model catalog exceeds the 1 MiB limit".into(),
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Codex model catalog: {error}"),
            })?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Protocol {
                message: "invalid Codex model catalog shape".into(),
            })?;
        let ids = models
            .iter()
            .filter_map(|model| model.get("slug").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        cache
            .lock()
            .await
            .insert(cache_key, (Instant::now(), ids.clone()));
        Ok(ids.into_iter().collect())
    }

    async fn stream_inner(
        &self,
        request: &ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        let mut body = build_request_body(request);
        if self.priority_service_tier.load(Ordering::Acquire) {
            body["service_tier"] = json!("priority");
        }
        let mut request_builder = self
            .client
            .post(self.endpoint())
            .bearer_auth(self.token.expose_secret())
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(account_id) = &self.account_id {
            request_builder = request_builder
                .header("chatgpt-account-id", account_id)
                .header("originator", "mimir")
                .header("OpenAI-Beta", "responses=experimental");
        }
        let response = request_builder
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::Authentication);
        }
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                message: "Codex rate limit".into(),
            });
        }
        if status.is_server_error() {
            return Err(ProviderError::Unavailable {
                message: format!("HTTP {status}"),
            });
        }
        if !status.is_success() {
            return Err(ProviderError::Protocol {
                message: format!("Codex returned HTTP {status}"),
            });
        }

        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut received = 0_usize;
        let mut accumulator = CodexAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| classify_transport(&error))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_STREAM_BYTES {
                return Err(ProviderError::Protocol {
                    message: "Codex stream exceeds the 8 MiB limit".into(),
                });
            }
            pending.extend_from_slice(&chunk);
            while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<_> = pending.drain(..=position).collect();
                trim_newline(&mut line);
                accumulator.apply_line(&line, sink).await?;
            }
        }
        if !pending.is_empty() {
            trim_newline(&mut pending);
            accumulator.apply_line(&pending, sink).await?;
        }
        accumulator.finish()
    }
}

impl fmt::Debug for CodexProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProvider")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("codex_backend", &self.codex_backend)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn set_service_tier(&self, tier: Option<&str>) -> Result<(), ProviderError> {
        let priority = match tier {
            None | Some("default") => false,
            Some("priority") if !self.codex_backend => true,
            Some("priority") => {
                return Err(ProviderError::Protocol {
                    message: "priority service tier is unavailable for Codex OAuth".into(),
                });
            }
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
        self.stream_inner(&request, &NoopSink).await
    }

    async fn available_model_ids(&self) -> Result<Option<Vec<String>>, ProviderError> {
        if self.codex_backend {
            self.discover_model_ids().await.map(Some)
        } else {
            Ok(None)
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_inner(&request, sink).await
    }
}

fn codex_catalog_cache() -> &'static CatalogCache {
    static CACHE: OnceLock<CatalogCache> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

struct NoopSink;

#[async_trait]
impl ProviderEventSink for NoopSink {
    async fn emit(&self, _event: ProviderEvent) {}
}

#[derive(Default)]
struct CodexAccumulator {
    response_id: Option<String>,
    text: String,
    thinking: String,
    tools: BTreeMap<String, CodexTool>,
    usage: Usage,
    incomplete: bool,
}

#[derive(Default)]
struct CodexTool {
    call_id: String,
    name: String,
    arguments: String,
}

impl CodexAccumulator {
    async fn apply_line(
        &mut self,
        line: &[u8],
        sink: &dyn ProviderEventSink,
    ) -> Result<(), ProviderError> {
        let Some(payload) = line.strip_prefix(b"data: ") else {
            return Ok(());
        };
        if payload == b"[DONE]" {
            return Ok(());
        }
        let value: Value =
            serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
                message: format!("invalid Codex stream event: {error}"),
            })?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    sink.emit(ProviderEvent::TextDelta(delta.into())).await;
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.thinking.push_str(delta);
                    sink.emit(ProviderEvent::ThinkingDelta(delta.into())).await;
                }
            }
            "response.output_item.added" => self.apply_tool_item(&value["item"], false),
            "response.function_call_arguments.delta" => {
                let key = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .map(|_| "0")
                    })
                    .unwrap_or("0")
                    .to_owned();
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.tools.entry(key).or_default().arguments.push_str(delta);
                }
            }
            "response.output_item.done" => self.apply_tool_item(&value["item"], true),
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                self.response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.usage = Usage {
                    input_tokens: response
                        .pointer("/usage/input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output_tokens: response
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cached_tokens: response
                        .pointer("/usage/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                };
                self.incomplete = value["type"] == "response.incomplete";
            }
            "error" | "response.failed" => {
                return Err(ProviderError::Protocol {
                    message: value
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex stream failed")
                        .chars()
                        .take(512)
                        .collect(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_tool_item(&mut self, item: &Value, done: bool) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let key = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("call_id").and_then(Value::as_str))
            .unwrap_or("0")
            .to_owned();
        let tool = self.tools.entry(key).or_default();
        if let Some(value) = item.get("call_id").and_then(Value::as_str) {
            tool.call_id = value.into();
        }
        if let Some(value) = item.get("name").and_then(Value::as_str) {
            tool.name = value.into();
        }
        if done && let Some(value) = item.get("arguments").and_then(Value::as_str) {
            tool.arguments = value.into();
        }
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(Content::Thinking {
                text: self.thinking,
                signature: None,
                redacted: false,
            });
        }
        if !self.text.is_empty() {
            content.push(Content::Text { text: self.text });
        }
        for (_, tool) in self.tools {
            if tool.call_id.is_empty() || tool.name.is_empty() {
                return Err(ProviderError::Protocol {
                    message: "Codex stream ended with an incomplete tool call".into(),
                });
            }
            let arguments =
                serde_json::from_str(&tool.arguments).map_err(|error| ProviderError::Protocol {
                    message: format!("Codex tool arguments are invalid JSON: {error}"),
                })?;
            content.push(Content::ToolCall(ToolCall {
                id: tool.call_id,
                name: tool.name,
                arguments,
            }));
        }
        let stop = if self.incomplete {
            StopReason::Length
        } else if content
            .iter()
            .any(|value| matches!(value, Content::ToolCall(_)))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let mut message = Message::assistant(content, stop);
        message.usage = self.usage;
        Ok(ModelResponse {
            message,
            response_id: self.response_id,
        })
    }
}

fn build_request_body(request: &ModelRequest) -> Value {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::User => {
                let mut content = Vec::new();
                if !message.text().is_empty() {
                    content.push(json!({"type": "input_text", "text": message.text()}));
                }
                if message.role == Role::User {
                    content.extend(message.content.iter().filter_map(|block| match block {
                        Content::Image { data, mime_type } => Some(json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime_type};base64,{data}")
                        })),
                        _ => None,
                    }));
                }
                if !content.is_empty() {
                    input.push(json!({
                        "role": if message.role == Role::System { "system" } else { "user" },
                        "content": content
                    }));
                }
            }
            Role::Assistant => {
                if !message.text().is_empty() {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": message.text(), "annotations": []}]
                    }));
                }
                for block in &message.content {
                    if let Content::ToolCall(call) = block {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments.to_string()
                        }));
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let Content::ToolResult(result) = block {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": result.tool_call_id,
                            "output": result.content
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
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "store": false,
        "stream": true,
        "instructions": request.system_prompt,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "max_output_tokens": request.max_output_tokens
    });
    if let Some(effort) = request.thinking_effort.as_deref().or_else(|| {
        (request.thinking_level != crate::model::ThinkingLevel::Off)
            .then(|| request.thinking_level.as_str())
    }) {
        body["reasoning"] = json!({
            "effort": effort,
            "summary": "auto"
        });
    }
    body
}

fn extract_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| value.pointer("/https:~1~1api.openai.com~1auth/account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
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

fn trim_newline(line: &mut Vec<u8>) {
    while line
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(body["input"][0]["content"][0]["text"], "inspect");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,aW1hZ2U="
        );
    }
}
