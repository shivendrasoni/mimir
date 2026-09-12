use std::sync::Arc;

use async_trait::async_trait;
use mimir::{
    model::{
        Content, Message, ModelRequest, Role, StopReason, ThinkingLevel, ToolCall, ToolDefinition,
    },
    provider::{
        AnthropicCredentialKind, AnthropicProvider, Provider, ProviderError, ProviderEvent,
        ProviderEventSink,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

#[derive(Default)]
struct Events(Mutex<Vec<ProviderEvent>>);

#[async_trait]
impl ProviderEventSink for Events {
    async fn emit(&self, event: ProviderEvent) {
        self.0.lock().await.push(event);
    }
}

fn request() -> ModelRequest {
    let mut user = Message::user("inspect the file");
    user.content.push(Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    });
    ModelRequest {
        model: "claude-sonnet-4-6".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: "Be precise".into(),
        messages: vec![
            user,
            Message::assistant(
                vec![
                    Content::Thinking {
                        text: "need the file".into(),
                        signature: Some("signed-thinking".into()),
                        redacted: false,
                    },
                    Content::ToolCall(ToolCall {
                        id: "call:1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "README.md"}),
                    }),
                ],
                StopReason::ToolUse,
            ),
            Message::tool_result("call:1", "read_file", "contents", false),
        ],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        max_output_tokens: 8_192,
    }
}

async fn read_http_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        assert_ne!(read, 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let headers_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header delimiter")
        + 4;
    let headers = String::from_utf8_lossy(&bytes[..headers_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .expect("content length");
    while bytes.len() - headers_end < content_length {
        let read = socket.read(&mut chunk).await.expect("read body");
        assert_ne!(read, 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[headers_end..headers_end + content_length])
        .expect("request JSON");
    (headers, body)
}

#[tokio::test]
async fn native_messages_transport_uses_anthropic_headers_and_typed_blocks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_http_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/messages http/1.1"));
        assert!(headers.contains("x-api-key: test-secret"));
        assert!(headers.contains("anthropic-version: 2023-06-01"));
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["system"], "Be precise");
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
        assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(
            body["messages"][1]["content"][0]["signature"],
            "signed-thinking"
        );
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][1]["id"], "call_1");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));

        let body = json!({
            "id": "msg-1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "considering", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque-reasoning"},
                {"type": "text", "text": "I will inspect it."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "README.md"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 11, "output_tokens": 7, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2}
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider = AnthropicProvider::new(Some(&format!("http://{address}")), "test-secret")
        .expect("provider");
    let response = provider.complete(request()).await.expect("completion");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("msg-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 16);
    assert_eq!(response.message.usage.output_tokens, 7);
    assert_eq!(response.message.usage.cached_tokens, 3);
    assert_eq!(response.message.usage.uncached_input_tokens(), 13);
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, signature, .. } if text == "considering" && signature.as_deref() == Some("sig")
    ));
    assert!(matches!(
        &response.message.content[3],
        Content::ToolCall(call) if call.name == "read_file" && call.arguments["path"] == "README.md"
    ));
    assert!(matches!(
        &response.message.content[1],
        Content::Thinking { signature, redacted: true, .. }
            if signature.as_deref() == Some("opaque-reasoning")
    ));
}

#[tokio::test]
async fn anthropic_stream_reassembles_tool_json_and_forwards_text_and_thinking() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_http_request(&mut socket).await;
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("accept: text/event-stream")
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-stream\",\"content\":[],\"usage\":{\"input_tokens\":9,\"output_tokens\":1,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"stream-signature\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"README.md\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-stream\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":6}}\n\n",
            "data: {\"type\":\"message_stop\"}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{stream}",
            stream.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider = AnthropicProvider::new(Some(&format!("http://{address}")), "test-secret")
        .expect("provider");
    let sink = Arc::new(Events::default());
    let response = provider
        .stream(request(), sink.as_ref())
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("msg-stream"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 14);
    assert_eq!(response.message.usage.output_tokens, 6);
    assert_eq!(response.message.usage.cached_tokens, 3);
    assert_eq!(response.message.usage.uncached_input_tokens(), 11);
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::TextDelta("done".into()),
        ]
    );
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { signature, .. } if signature.as_deref() == Some("stream-signature")
    ));
    assert!(matches!(
        &response.message.content[3],
        Content::Thinking { signature, redacted: true, .. }
            if signature.as_deref() == Some("opaque-stream")
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call) if call.arguments == json!({"path": "README.md"})
    ));
}

#[tokio::test]
async fn anthropic_auth_failures_are_classified_without_echoing_the_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let body =
            r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    let provider = AnthropicProvider::new(Some(&format!("http://{address}")), "secret-never-print")
        .expect("provider");
    let error = provider
        .complete(request())
        .await
        .expect_err("auth failure");
    server.await.expect("server");
    assert_eq!(error, ProviderError::AuthenticationRejected);
    assert!(!format!("{provider:?}").contains("secret-never-print"));
    assert!(!error.to_string().contains("secret-never-print"));
}

#[tokio::test]
async fn anthropic_permission_failures_are_not_refreshable_authentication_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let body =
            r#"{"type":"error","error":{"type":"permission_error","message":"scope denied"}}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    let provider = AnthropicProvider::new(Some(&format!("http://{address}")), "secret-never-print")
        .expect("provider");

    let error = provider
        .complete(request())
        .await
        .expect_err("permission failure");

    server.await.expect("server");
    assert_eq!(error, ProviderError::Authentication);
    assert!(!error.to_string().contains("secret-never-print"));
}

#[test]
fn anthropic_preview_omits_internal_tool_role_and_normalizes_ids() {
    let provider = AnthropicProvider::new(None, "test-secret").expect("provider");
    let preview = provider.request_preview(&request());
    assert_eq!(preview["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(preview["messages"][2]["role"], "user");
    assert_eq!(preview["messages"][2]["content"][0]["is_error"], false);
    assert!(
        request()
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
    );
}

#[test]
fn anthropic_oauth_keeps_identity_separate_from_agent_system_prompt() {
    let provider = AnthropicProvider::with_credential_kind(
        None,
        "test-oauth-token",
        AnthropicCredentialKind::OAuthToken,
    )
    .expect("provider");

    let preview = provider.request_preview(&request());

    assert_eq!(
        preview["system"],
        json!([
            {
                "type": "text",
                "text": "You are Claude Code, Anthropic's official CLI for Claude."
            },
            {"type": "text", "text": "Be precise"}
        ])
    );
    assert_eq!(preview["cache_control"], json!({"type": "ephemeral"}));
}

#[test]
fn anthropic_preview_omits_cache_control_for_non_claude_compatible_models() {
    let provider = AnthropicProvider::new(None, "test-secret").expect("provider");
    for model in ["MiniMax-M2.7", "kimi-for-coding", "mimo-v2.5-pro"] {
        let mut request = request();
        request.model = model.into();

        let preview = provider.request_preview(&request);

        assert!(
            preview.get("cache_control").is_none(),
            "non-Claude model {model} must not receive Anthropic cache control"
        );
    }
}

#[test]
fn anthropic_preview_replays_redacted_thinking_and_disables_model_default_thinking() {
    let provider = AnthropicProvider::new(None, "test-secret").expect("provider");
    let mut request = request();
    request.model = "claude-sonnet-5".into();
    request.thinking_level = ThinkingLevel::Off;
    request.messages[1].content[0] = Content::Thinking {
        text: "[Reasoning redacted]".into(),
        signature: Some("opaque-data".into()),
        redacted: true,
    };
    let preview = provider.request_preview(&request);
    assert_eq!(preview["thinking"]["type"], "disabled");
    assert_eq!(
        preview["messages"][1]["content"][0],
        json!({"type": "redacted_thinking", "data": "opaque-data"})
    );
}
