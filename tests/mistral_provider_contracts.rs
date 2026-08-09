use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mimir::{
    model::{Content, Message, ModelRequest, StopReason, ThinkingLevel, ToolCall, ToolDefinition},
    provider::{
        MistralProvider, Provider, ProviderError, ProviderEvent, ProviderEventSink,
        registry::{ProviderRegistry, RuntimeSupport},
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
    let mut user = Message::user("inspect the image");
    user.content.push(Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    });
    ModelRequest {
        model: "mistral-small-2603".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: "Be precise".into(),
        messages: vec![
            user,
            Message::assistant(
                vec![
                    Content::Thinking {
                        text: "need the file".into(),
                        signature: None,
                        redacted: false,
                    },
                    Content::ToolCall(ToolCall {
                        id: "call:long/identifier".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "README.md"}),
                    }),
                ],
                StopReason::ToolUse,
            ),
            Message::tool_result(
                "call:long/identifier",
                "read_file",
                "permission denied",
                true,
            ),
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

fn assert_native_request(headers: &str, body: &Value) {
    let headers = headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/chat/completions http/1.1"));
    assert!(headers.contains("authorization: bearer test-secret"));
    assert!(headers.contains("x-client-contract: rust"));
    assert_eq!(body["stream"], false);
    assert_eq!(body["model"], "mistral-small-2603");
    assert_eq!(body["messages"][0]["content"], "Be precise");
    assert_eq!(
        body["messages"][1]["content"][1]["image_url"],
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(
        body["messages"][2]["content"][0]["thinking"][0]["text"],
        "need the file"
    );
    let assistant_call_id = body["messages"][2]["tool_calls"][0]["id"]
        .as_str()
        .expect("assistant tool call id");
    let result_call_id = body["messages"][3]["tool_call_id"]
        .as_str()
        .expect("tool result call id");
    assert_eq!(assistant_call_id, result_call_id);
    assert_eq!(assistant_call_id.len(), 9);
    assert!(
        assistant_call_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    );
    assert_eq!(
        body["messages"][3]["content"][0]["text"],
        "[tool error] permission denied"
    );
    assert_eq!(body["tools"][0]["function"]["strict"], false);
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"][0],
        "path"
    );
    assert_eq!(body["max_tokens"], 8_192);
    assert_eq!(body["reasoning_effort"], "high");
}

#[tokio::test]
async fn native_transport_translates_typed_messages_tools_reasoning_and_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_http_request(&mut socket).await;
        assert_native_request(&headers, &body);

        let response_body = json!({
            "id": "mistral-response-1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": [{"type": "text", "text": "considering"}]},
                        {"type": "text", "text": "I will inspect it."}
                    ],
                    "tool_calls": [{
                        "id": "abc123XYZ",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": {"path": "src/main.rs"}
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let headers = BTreeMap::from([("x-client-contract".into(), "rust".into())]);
    let provider =
        MistralProvider::with_headers(Some(&format!("http://{address}")), "test-secret", &headers)
            .expect("provider");
    let response = provider.complete(request()).await.expect("completion");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("mistral-response-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 11);
    assert_eq!(response.message.usage.output_tokens, 7);
    assert_eq!(response.message.usage.cached_tokens, 0);
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, signature: None, redacted: false } if text == "considering"
    ));
    assert!(matches!(
        &response.message.content[1],
        Content::Text { text } if text == "I will inspect it."
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call)
            if call.id == "abc123XYZ"
                && call.name == "read_file"
                && call.arguments == json!({"path": "src/main.rs"})
    ));
}

#[tokio::test]
async fn stream_preserves_typed_block_order_reassembles_tools_and_emits_deltas() {
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
        let stream = concat!(
            "data: {\"id\":\"mistral-stream-1\",\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"plan\"}]}]},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":9}}\n\n",
            "data:{\"id\":\"mistral-stream-1\",\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"mistral-stream-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tool12345\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"mistral-stream-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"completion_tokens\":6,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n"
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

    let provider =
        MistralProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    let sink = Arc::new(Events::default());
    let response = provider
        .stream(request(), sink.as_ref())
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("mistral-stream-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 9);
    assert_eq!(response.message.usage.output_tokens, 6);
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::TextDelta("done".into())
        ]
    );
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, .. } if text == "plan"
    ));
    assert!(matches!(
        &response.message.content[1],
        Content::Text { text } if text == "done"
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call)
            if call.id == "tool12345"
                && call.name == "read_file"
                && call.arguments == json!({"path": "README.md"})
    ));
}

#[tokio::test]
async fn errors_are_typed_bounded_and_do_not_leak_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let response_body = json!({
            "error": {"message": format!("test-secret:{}", "x".repeat(5_000))}
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    let provider =
        MistralProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    let error = provider.complete(request()).await.expect_err("HTTP 400");
    server.await.expect("server");

    let message = match error {
        ProviderError::Protocol { message } => message,
        other => panic!("unexpected error: {other:?}"),
    };
    assert!(!message.contains("test-secret"));
    assert!(message.contains("[REDACTED]"));
    assert!(message.chars().count() <= 4_030);
    assert!(!format!("{provider:?}").contains("test-secret"));

    assert!(matches!(
        MistralProvider::new(None, "  "),
        Err(ProviderError::Authentication)
    ));
    assert!(matches!(
        MistralProvider::with_headers(
            None,
            "secret",
            &BTreeMap::from([("Authorization".into(), "override".into())])
        ),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        MistralProvider::new(Some("http://example.com"), "secret"),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        MistralProvider::new(Some("https://user:pass@example.com"), "secret"),
        Err(ProviderError::Protocol { .. })
    ));
}

#[tokio::test]
async fn declared_oversize_response_is_rejected_before_allocation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8388609\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write response");
    });
    let provider =
        MistralProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    let error = provider.complete(request()).await.expect_err("oversize");
    server.await.expect("server");
    assert!(matches!(
        error,
        ProviderError::Protocol { message } if message.contains("8 MiB")
    ));
}

#[test]
fn registry_routes_mistral_catalog_models_to_native_runtime() {
    let provider = ProviderRegistry::builtin().get("mistral").expect("Mistral");
    assert_eq!(
        provider.runtime_support,
        RuntimeSupport::MistralConversations
    );
    let model = mimir::provider::registry::model_catalog()
        .iter()
        .find(|model| model.provider == "mistral")
        .expect("cataloged Mistral model");
    assert_eq!(model.api, "mistral-conversations");
    assert_eq!(
        provider.runtime_for_model(model),
        Some(RuntimeSupport::MistralConversations)
    );
}
