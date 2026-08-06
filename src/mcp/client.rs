use std::{collections::BTreeMap, sync::Arc};

use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::BufReader,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{
    error::{MimirError, Result},
    mcp::{
        config::{McpHttpConfig, McpServerConfig},
        oauth::{McpAuthorizationChallenge, parse_bearer_challenge},
        protocol::{
            JSONRPC_VERSION, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
            PROTOCOL_VERSION, read_message, write_message,
        },
        tool_names::identifier_map,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDescriptor {
    pub name: String,
    pub identifier: Option<String>,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpToolCallOutput {
    Structured(Value),
    Text(String),
    Blocks(Vec<Value>),
}

pub struct McpClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    remote: Option<HttpTransport>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
    max_frame_bytes: usize,
    max_tool_payload_bytes: usize,
    server_info: McpServerInfo,
}

impl McpClient {
    /// Starts an MCP stdio child process and completes the initialize handshake.
    ///
    /// # Errors
    ///
    /// Returns an error when validation fails, the child process cannot be
    /// spawned, the stdio pipes are unavailable, or the MCP handshake fails.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        Self::connect_with_bearer(config, None).await
    }

    /// Connects through stdio or Streamable HTTP with an optional OAuth bearer token.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, transport failures, or an invalid handshake.
    pub async fn connect_with_bearer(
        config: &McpServerConfig,
        bearer_token: Option<&str>,
    ) -> Result<Self> {
        config.validate()?;
        if let Some(remote) = &config.remote {
            return Self::connect_remote(config, remote, bearer_token).await;
        }
        let mut child = Command::new(&config.stdio.program)
            .args(&config.stdio.args)
            .env_clear()
            .envs(&config.stdio.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| MimirError::Protocol("MCP stdio child did not expose stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MimirError::Protocol("MCP stdio child did not expose stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MimirError::Protocol("MCP stdio child did not expose stderr".into()))?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(drain_stderr(
            stderr,
            config.stdio.max_stderr_bytes,
            stderr_tail.clone(),
        ));
        let mut client = Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            remote: None,
            stderr_tail,
            next_id: 1,
            max_frame_bytes: config.stdio.max_frame_bytes,
            max_tool_payload_bytes: config.stdio.max_tool_payload_bytes,
            server_info: McpServerInfo {
                name: String::new(),
                version: String::new(),
            },
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn connect_remote(
        config: &McpServerConfig,
        remote: &McpHttpConfig,
        bearer_token: Option<&str>,
    ) -> Result<Self> {
        let mut headers = config.headers.clone();
        headers.extend(remote.headers.clone());
        let transport = HttpTransport::new(remote, &headers, bearer_token)?;
        let mut client = Self {
            child: None,
            stdin: None,
            stdout: None,
            remote: Some(transport),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
            next_id: 1,
            max_frame_bytes: remote.max_response_bytes,
            max_tool_payload_bytes: remote.max_tool_payload_bytes,
            server_info: McpServerInfo {
                name: String::new(),
                version: String::new(),
            },
        };
        client.initialize().await?;
        Ok(client)
    }

    pub fn server_info(&self) -> &McpServerInfo {
        &self.server_info
    }

    /// Returns and clears the most recent parsed HTTP authorization challenge.
    pub fn take_authorization_challenge(&mut self) -> Option<McpAuthorizationChallenge> {
        self.remote
            .as_mut()
            .and_then(|remote| remote.authorization_challenge.take())
    }

    /// Lists tools exposed by the connected MCP server.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON-RPC request fails or the server returns
    /// an invalid tool payload.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDescriptor>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ToolEntry {
            name: String,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            input_schema: Option<Value>,
        }
        #[derive(Deserialize)]
        struct ToolsListResult {
            tools: Vec<ToolEntry>,
        }

        let result = self.request("tools/list", json!({})).await?;
        let parsed: ToolsListResult = serde_json::from_value(result)?;
        let tools = parsed.tools;
        let tool_names = tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let identifiers = identifier_map(
            tool_names
                .iter()
                .map(std::string::String::as_str)
                .collect::<Vec<_>>(),
        );
        Ok(tools
            .into_iter()
            .map(|tool| McpToolDescriptor {
                identifier: identifiers.get(tool.name.as_str()).cloned().unwrap_or(None),
                name: tool.name,
                description: tool.description.unwrap_or_default(),
                input_schema: tool.input_schema.unwrap_or_else(|| json!({})),
            })
            .collect())
    }

    /// Invokes a named MCP tool with JSON arguments.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoded arguments exceed the configured
    /// payload limit, the JSON-RPC request fails, or the tool response is
    /// invalid.
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolCallOutput> {
        let encoded = serde_json::to_vec(&arguments)?;
        if encoded.len() > self.max_tool_payload_bytes {
            return Err(MimirError::Protocol(format!(
                "MCP tool payload exceeds the configured limit of {} bytes",
                self.max_tool_payload_bytes
            )));
        }
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": tool_name,
                    "arguments": arguments,
                }),
            )
            .await?;
        parse_tool_result(result)
    }

    async fn initialize(&mut self) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InitializeResult {
            protocol_version: String,
            server_info: InitializeServerInfo,
        }
        #[derive(Deserialize)]
        struct InitializeServerInfo {
            name: String,
            version: String,
        }
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "mimir",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        let parsed: InitializeResult = serde_json::from_value(result)?;
        if parsed.protocol_version != PROTOCOL_VERSION {
            return Err(MimirError::Protocol(format!(
                "unsupported MCP protocol version '{}'",
                bounded_remote_text(&parsed.protocol_version, 128)
            )));
        }
        if let Some(remote) = &mut self.remote {
            remote.protocol_version = parsed.protocol_version;
        }
        self.server_info = McpServerInfo {
            name: parsed.server_info.name,
            version: parsed.server_info.version,
        };
        self.notify(JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION,
            method: "notifications/initialized".into(),
            params: json!({}),
        })
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.into(),
            params,
        };
        let response =
            if let Some(remote) = &mut self.remote {
                remote.post(&request, Some(id)).await?
            } else {
                let stdin = self.stdin.as_mut().ok_or_else(|| {
                    MimirError::Protocol("MCP stdio transport is unavailable".into())
                })?;
                write_message(stdin, &request).await?;
                let stdout = self.stdout.as_mut().ok_or_else(|| {
                    MimirError::Protocol("MCP stdio transport is unavailable".into())
                })?;
                read_message(stdout, self.max_frame_bytes).await?
            };
        let response: JsonRpcResponse = serde_json::from_value(response)?;
        if response.jsonrpc != JSONRPC_VERSION {
            return Err(MimirError::Protocol(format!(
                "unexpected MCP JSON-RPC version '{}'",
                response.jsonrpc
            )));
        }
        if response.id != json!(id) {
            return Err(MimirError::Protocol(format!(
                "unexpected MCP response id {}; expected {id}",
                response.id
            )));
        }
        if let Some(error) = response.error {
            let stderr = self.stderr_snapshot().await;
            return Err(MimirError::Protocol(format!(
                "MCP request '{method}' failed with {}: {}{}",
                error.code,
                bounded_remote_text(&error.message, 1024),
                format_stderr(&stderr)
            )));
        }
        response.result.ok_or_else(|| {
            MimirError::Protocol(format!(
                "MCP request '{method}' returned neither a result nor an error"
            ))
        })
    }

    async fn notify(&mut self, notification: JsonRpcNotification) -> Result<()> {
        if let Some(remote) = &mut self.remote {
            remote.post(&notification, None).await?;
            Ok(())
        } else {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| MimirError::Protocol("MCP stdio transport is unavailable".into()))?;
            write_message(stdin, &notification).await
        }
    }

    async fn stderr_snapshot(&self) -> Vec<u8> {
        self.stderr_tail.lock().await.clone()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

struct HttpTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    session_id: Option<HeaderValue>,
    protocol_version: String,
    max_response_bytes: usize,
    authorization_challenge: Option<McpAuthorizationChallenge>,
}

impl HttpTransport {
    fn new(
        config: &McpHttpConfig,
        configured_headers: &BTreeMap<String, String>,
        bearer_token: Option<&str>,
    ) -> Result<Self> {
        let endpoint = config.validate().and_then(|()| {
            crate::mcp::config::validate_remote_url("MCP remote URL", &config.url)
        })?;
        let mut headers = HeaderMap::new();
        for (name, value) in configured_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                MimirError::Configuration("MCP remote header name is invalid".into())
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                MimirError::Configuration("MCP remote header value is invalid".into())
            })?;
            headers.insert(name, value);
        }
        if let Some(token) = bearer_token {
            if token.trim().is_empty() || token.contains(['\r', '\n', '\0']) {
                return Err(MimirError::Configuration(
                    "MCP bearer token is invalid".into(),
                ));
            }
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| MimirError::Configuration("MCP bearer token is invalid".into()))?;
            headers.insert(AUTHORIZATION, value);
        }
        let client = reqwest::Client::builder()
            .timeout(config.io_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| MimirError::Configuration("MCP HTTP client setup failed".into()))?;
        Ok(Self {
            client,
            endpoint,
            headers,
            session_id: None,
            protocol_version: PROTOCOL_VERSION.into(),
            max_response_bytes: config.max_response_bytes,
            authorization_challenge: None,
        })
    }

    async fn post<T: serde::Serialize>(
        &mut self,
        value: &T,
        expected_id: Option<u64>,
    ) -> Result<Value> {
        self.authorization_challenge = None;
        let payload = serde_json::to_vec(value)?;
        if payload.len() > self.max_response_bytes {
            return Err(MimirError::Protocol(
                "MCP HTTP request exceeds the configured frame limit".into(),
            ));
        }
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .header("MCP-Protocol-Version", &self.protocol_version)
            .body(payload);
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request.send().await.map_err(|_| {
            MimirError::Protocol(format!(
                "MCP HTTP request to {} failed",
                safe_endpoint(&self.endpoint)
            ))
        })?;
        if self.session_id.is_none()
            && let Some(value) = response.headers().get("Mcp-Session-Id")
        {
            validate_session_id(value)?;
            self.session_id = Some(value.clone());
        }
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                self.authorization_challenge = parse_authorization_challenge(response.headers())?;
            }
            let required_scopes = self
                .authorization_challenge
                .as_ref()
                .and_then(McpAuthorizationChallenge::scopes)
                .filter(|scopes| !scopes.is_empty())
                .map(|scopes| format!("; required scopes: {}", scopes.join(" ")))
                .unwrap_or_default();
            return Err(MimirError::Protocol(match status.as_u16() {
                401 => {
                    format!("MCP remote server requires authorization (HTTP 401){required_scopes}")
                }
                403 => format!(
                    "MCP remote server rejected the requested scopes (HTTP 403){required_scopes}"
                ),
                404 if self.session_id.is_some() => {
                    self.session_id = None;
                    "MCP remote session expired; reconnect is required (HTTP 404)".into()
                }
                code => format!("MCP HTTP request failed with status {code}"),
            }));
        }
        if expected_id.is_none() {
            if status.as_u16() != 202 && status.as_u16() != 200 {
                return Err(MimirError::Protocol(
                    "MCP notification returned an unexpected status".into(),
                ));
            }
            return Ok(Value::Null);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let bytes = bounded_response(response, self.max_response_bytes).await?;
        if content_type.starts_with("text/event-stream") {
            parse_sse_response(&bytes, expected_id)
        } else if content_type.starts_with("application/json") {
            Ok(serde_json::from_slice(&bytes)?)
        } else {
            Err(MimirError::Protocol(
                "MCP HTTP response has an unsupported content type".into(),
            ))
        }
    }
}

fn parse_authorization_challenge(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<McpAuthorizationChallenge>> {
    for value in headers.get_all("WWW-Authenticate") {
        let value = value.to_str().map_err(|_| {
            MimirError::Protocol("MCP authorization challenge is not visible ASCII".into())
        })?;
        if let Some(challenge) = parse_bearer_challenge(value)? {
            return Ok(Some(challenge));
        }
    }
    Ok(None)
}

async fn bounded_response(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > limit))
    {
        return Err(MimirError::Protocol(
            "MCP HTTP response exceeds the configured limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| MimirError::Protocol("MCP HTTP response stream failed".into()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(MimirError::Protocol(
                "MCP HTTP response exceeds the configured limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_sse_response(bytes: &[u8], expected_id: Option<u64>) -> Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| MimirError::Protocol("MCP SSE response is not UTF-8".into()))?;
    let normalized = text.replace("\r\n", "\n");
    for event in normalized.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if !data.is_empty()
            && let Ok(value) = serde_json::from_str::<Value>(&data)
            && expected_id.is_none_or(|id| value.get("id") == Some(&json!(id)))
        {
            return Ok(value);
        }
    }
    Err(MimirError::Protocol(
        "MCP SSE response did not contain JSON-RPC data".into(),
    ))
}

fn validate_session_id(value: &HeaderValue) -> Result<()> {
    let value = value
        .to_str()
        .map_err(|_| MimirError::Protocol("MCP session id is not visible ASCII".into()))?;
    if value.is_empty()
        || value.len() > 1024
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(MimirError::Protocol("MCP session id is invalid".into()));
    }
    Ok(())
}

fn safe_endpoint(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("remote");
    format!("{}://{}{}", url.scheme(), host, url.path())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawToolResult {
    #[serde(default)]
    structured_content: Option<Value>,
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    is_error: bool,
}

fn parse_tool_result(value: Value) -> Result<McpToolCallOutput> {
    let result: RawToolResult = serde_json::from_value(value)?;
    let texts = result
        .content
        .iter()
        .filter_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    if result.is_error {
        return Err(MimirError::Tool(if texts.is_empty() {
            "MCP tool returned an error".into()
        } else {
            bounded_remote_text(&texts.join("\n"), 4096)
        }));
    }
    if let Some(structured) = result.structured_content {
        return Ok(McpToolCallOutput::Structured(structured));
    }
    if !texts.is_empty() {
        return Ok(McpToolCallOutput::Text(texts.join("\n")));
    }
    Ok(McpToolCallOutput::Blocks(result.content))
}

async fn drain_stderr(
    mut stderr: ChildStderr,
    max_bytes: usize,
    sink: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut buffer = [0_u8; 256];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let mut sink = sink.lock().await;
        let remaining = max_bytes.saturating_sub(sink.len());
        sink.extend_from_slice(&buffer[..read.min(remaining)]);
        if sink.len() >= max_bytes {
            break;
        }
    }
    Ok(())
}

fn format_stderr(stderr: &[u8]) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(
            "; stderr: {}",
            bounded_remote_text(String::from_utf8_lossy(stderr).trim(), 4096)
        )
    }
}

fn bounded_remote_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}
