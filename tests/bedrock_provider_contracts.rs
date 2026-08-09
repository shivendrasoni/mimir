use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;
use mimir::{
    model::{
        Content, Message, ModelRequest, Role, StopReason, ThinkingLevel, ToolCall, ToolDefinition,
        ToolResult, Usage,
    },
    provider::{
        AwsCredentials, BedrockCredentialSource, BedrockProvider, Provider, ProviderError,
        ProviderEvent, ProviderEventSink,
        registry::{AuthKind, ProviderRegistry, RuntimeSupport},
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

fn request() -> ModelRequest {
    ModelRequest {
        model: "anthropic.claude-sonnet-4-6-v1:0".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: Some("high".into()),
        system_prompt: "Be precise".into(),
        messages: vec![
            Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: "inspect".into(),
                    },
                    Content::Image {
                        data: "aW1hZ2U=".into(),
                        mime_type: "image/png".into(),
                    },
                ],
                stop_reason: None,
                usage: Usage::default(),
                timestamp_ms: 1,
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Thinking {
                        text: "plan".into(),
                        signature: Some("signed".into()),
                        redacted: false,
                    },
                    Content::ToolCall(ToolCall {
                        id: "call:unsafe".into(),
                        name: "bash".into(),
                        arguments: json!({"cmd": "pwd"}),
                    }),
                ],
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
                timestamp_ms: 2,
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult {
                    tool_call_id: "call:unsafe".into(),
                    tool_name: "bash".into(),
                    content: "/workspace".into(),
                    is_error: false,
                })],
                stop_reason: None,
                usage: Usage::default(),
                timestamp_ms: 3,
            },
        ],
        tools: vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a command".into(),
            parameters: json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            }),
        }],
        max_output_tokens: 8_192,
    }
}

#[test]
fn registry_advertises_native_bedrock_with_bearer_and_ambient_auth() {
    let provider = ProviderRegistry::builtin()
        .get("amazon-bedrock")
        .expect("Bedrock must be registered");
    assert_eq!(
        provider.runtime_support,
        RuntimeSupport::BedrockConverseStream
    );
    assert_eq!(provider.env_vars, &["AWS_BEARER_TOKEN_BEDROCK"]);
    assert_eq!(provider.auth, &[AuthKind::ApiKey, AuthKind::Ambient]);
    assert!(provider.supports_runtime());
}

#[test]
fn request_translation_preserves_typed_content_tools_and_thinking() {
    let provider = BedrockProvider::new_bearer(
        "us-east-1",
        Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        "test-token",
    )
    .unwrap();
    let body = provider.request_preview(&request()).unwrap();

    assert_eq!(body.pointer("/system/0/text"), Some(&json!("Be precise")));
    assert_eq!(
        body.pointer("/system/1/cachePoint/type"),
        Some(&json!("default"))
    );
    assert_eq!(
        body.pointer("/messages/0/content/1/image/format"),
        Some(&Value::String("png".into()))
    );
    assert_eq!(
        body.pointer("/messages/1/content/0/reasoningContent/reasoningText/signature"),
        Some(&Value::String("signed".into()))
    );
    assert_eq!(
        body.pointer("/messages/1/content/1/toolUse/toolUseId"),
        Some(&Value::String("call_unsafe".into()))
    );
    assert_eq!(
        body.pointer("/messages/2/content/0/toolResult/toolUseId"),
        Some(&Value::String("call_unsafe".into()))
    );
    assert_eq!(
        body.pointer("/messages/2/content/1/cachePoint/type"),
        Some(&json!("default"))
    );
    assert_eq!(
        body.pointer("/toolConfig/tools/0/toolSpec/name"),
        Some(&Value::String("bash".into()))
    );
    assert_eq!(body["inferenceConfig"]["maxTokens"], 8_192);
    assert_eq!(
        body.pointer("/additionalModelRequestFields/thinking/type"),
        Some(&Value::String("adaptive".into()))
    );
    assert_eq!(
        body.pointer("/additionalModelRequestFields/output_config/effort"),
        Some(&Value::String("high".into()))
    );
}

#[derive(Default)]
struct Events(Mutex<Vec<ProviderEvent>>);

#[async_trait]
impl ProviderEventSink for Events {
    async fn emit(&self, event: ProviderEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn bearer_stream_decodes_text_thinking_tools_usage_and_stop() {
    let frames = [
        event_frame("messageStart", &json!({"role": "assistant"})),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think", "signature": "sig"}}}),
        ),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"text": "hello"}}),
        ),
        event_frame(
            "contentBlockStart",
            &json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "call-1", "name": "bash"}}}),
        ),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"cmd\":\"pwd\"}"}}}),
        ),
        event_frame("contentBlockStop", &json!({"contentBlockIndex": 2})),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 3, "delta": {"reasoningContent": {"redactedContent": "YWI="}}}),
        ),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 3, "delta": {"reasoningContent": {"redactedContent": "Y2Q="}}}),
        ),
        event_frame(
            "metadata",
            &json!({"usage": {"inputTokens": 7, "outputTokens": 5, "cacheReadInputTokens": 2, "totalTokens": 12}}),
        ),
        event_frame("messageStop", &json!({"stopReason": "tool_use"})),
    ]
    .concat();
    let (base_url, captured, done) =
        mock_response(200, "application/vnd.amazon.eventstream", frames).await;
    let provider =
        BedrockProvider::new_bearer("us-east-1", Some(&base_url), "bearer-secret").unwrap();
    let sink = Events::default();
    let response = provider.stream(request(), &sink).await.unwrap();
    done.await.unwrap();

    let captured = captured.await.unwrap();
    assert!(
        captured
            .starts_with("POST /model/anthropic.claude-sonnet-4-6-v1:0/converse-stream HTTP/1.1")
    );
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("authorization: bearer bearer-secret")
    );
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 7);
    assert_eq!(response.message.usage.output_tokens, 5);
    assert_eq!(response.message.usage.cached_tokens, 2);
    assert!(response.message.content.iter().any(|block| matches!(
        block,
        Content::Thinking { text, signature: Some(signature), .. }
            if text == "think" && signature == "sig"
    )));
    assert!(response.message.content.iter().any(|block| matches!(
        block,
        Content::ToolCall(ToolCall { id, name, arguments })
            if id == "call-1" && name == "bash" && arguments == &json!({"cmd": "pwd"})
    )));
    assert!(response.message.content.iter().any(|block| matches!(
        block,
        Content::Thinking { text, signature: Some(signature), redacted: true }
            if text.is_empty() && signature == "YWJjZA=="
    )));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        &[
            ProviderEvent::ThinkingDelta("think".into()),
            ProviderEvent::TextDelta("hello".into())
        ]
    );
}

struct FixedCredentials;

#[async_trait]
impl BedrockCredentialSource for FixedCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, ProviderError> {
        AwsCredentials::new("AKIDEXAMPLE", "secret", Some("session-token"))
    }
}

#[tokio::test]
async fn sigv4_signing_includes_region_service_and_session_token() {
    let frames = [
        event_frame("messageStart", &json!({"role": "assistant"})),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "ok"}}),
        ),
        event_frame("messageStop", &json!({"stopReason": "end_turn"})),
    ]
    .concat();
    let (base_url, captured, done) =
        mock_response(200, "application/vnd.amazon.eventstream", frames).await;
    let provider =
        BedrockProvider::new_ambient_with_source("eu-west-1", Some(&base_url), FixedCredentials)
            .unwrap();
    provider.complete(request()).await.unwrap();
    done.await.unwrap();

    let captured = captured.await.unwrap();
    let lower = captured.to_ascii_lowercase();
    assert!(lower.contains("x-amz-security-token: session-token"));
    assert!(captured.contains("Credential=AKIDEXAMPLE/"));
    assert!(captured.contains("/eu-west-1/bedrock/aws4_request"));
    assert!(captured.contains("SignedHeaders="));
    assert!(captured.contains("Signature="));
    assert!(!captured.contains("secret"));
}

#[tokio::test]
async fn cancellation_and_bounded_redacted_errors_fail_closed() {
    let provider =
        BedrockProvider::new_bearer("us-east-1", Some("http://127.0.0.1:9"), "do-not-leak")
            .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = provider
        .stream_with_cancellation(request(), &Events::default(), cancellation)
        .await
        .unwrap_err();
    assert_eq!(error, ProviderError::Aborted);

    let secret = "do-not-leak";
    let body = json!({"message": format!("bad {secret} {}", "x".repeat(2_000))})
        .to_string()
        .into_bytes();
    let (base_url, _captured, done) = mock_response(400, "application/json", body).await;
    let provider = BedrockProvider::new_bearer("us-east-1", Some(&base_url), secret).unwrap();
    let error = provider.complete(request()).await.unwrap_err();
    done.await.unwrap();
    let text = error.to_string();
    assert!(!text.contains(secret));
    assert!(text.len() < 700);
}

async fn mock_response(
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
) -> (String, oneshot::Receiver<String>, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = find(&bytes, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&bytes).into_owned();
        let reason = if status == 200 { "OK" } else { "Bad Request" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
        let _ = socket.shutdown().await;
        let _ = captured_tx.send(request);
        let _ = done_tx.send(());
    });
    (format!("http://{address}"), captured_rx, done_rx)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn event_frame(event_type: &str, payload: &Value) -> Vec<u8> {
    let headers = event_headers(event_type);
    let payload = serde_json::to_vec(&payload).unwrap();
    let total_len = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&u32::try_from(total_len).unwrap().to_be_bytes());
    frame.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
    let prelude_crc = crc32(&frame);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    let message_crc = crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

fn event_headers(event_type: &str) -> Vec<u8> {
    let mut values = BTreeMap::new();
    values.insert(":content-type", "application/json");
    values.insert(":event-type", event_type);
    values.insert(":message-type", "event");
    let mut headers = Vec::new();
    for (name, value) in values {
        headers.push(u8::try_from(name.len()).unwrap());
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // AWS EventStream string
        headers.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    headers
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
