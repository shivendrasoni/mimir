use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use mimir::{
    model::{Content, Message, ModelRequest, ToolDefinition},
    provider::{CodexProvider, Provider, ProviderEvent, ProviderEventSink},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
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

#[tokio::test]
async fn codex_oauth_transport_uses_responses_protocol_and_streams_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 32 * 1024];
        let read = socket.read(&mut request).await.expect("read request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("POST /codex/responses HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: acct-test")
        );
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("\"function_call_output\""));
        assert!(request.contains("\"reasoning\":{\"effort\":\"high\",\"summary\":\"auto\"}"));
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"path\\\":\\\"README.md\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"usage\":{\"input_tokens\":4,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
            "data: [DONE]"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
        }))
        .expect("JWT payload"),
    );
    let token = format!("header.{payload}.signature");
    let provider =
        CodexProvider::new(Some(&format!("http://{address}")), token, None).expect("provider");
    let sink = Arc::new(Events::default());
    let response = provider
        .stream(
            ModelRequest {
                model: "gpt-5.1-codex".into(),
                thinking_level: mimir::model::ThinkingLevel::High,
                thinking_effort: None,
                system_prompt: "be precise".into(),
                messages: vec![
                    Message::user("read it"),
                    Message::tool_result("old-call", "read_file", "old output", false),
                ],
                tools: vec![ToolDefinition {
                    name: "read_file".into(),
                    description: "Read a file".into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }),
                }],
                max_output_tokens: 100,
            },
            sink.as_ref(),
        )
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.message.text(), "done");
    assert_eq!(response.message.usage.input_tokens, 4);
    assert_eq!(response.message.usage.cached_tokens, 2);
    assert_eq!(response.message.usage.uncached_input_tokens(), 2);
    assert!(matches!(response.message.content[1], Content::ToolCall(_)));
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [ProviderEvent::TextDelta("done".into())]
    );
}

#[tokio::test]
async fn openai_api_key_transport_uses_native_responses_without_codex_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 16 * 1024];
        let read = socket.read(&mut request).await.expect("read request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
        let lowercase = request.to_ascii_lowercase();
        assert!(lowercase.contains("authorization: bearer api-test-key"));
        assert!(!lowercase.contains("chatgpt-account-id"));
        assert!(request.contains("\"model\":\"gpt-5-mini\""));
        assert!(request.contains("\"reasoning\":{\"effort\":\"high\",\"summary\":\"auto\"}"));
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"native\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-api\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
            "data: [DONE]"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider = CodexProvider::new_openai(Some(&format!("http://{address}/v1")), "api-test-key")
        .expect("provider");
    let response = provider
        .stream(
            ModelRequest {
                model: "gpt-5-mini".into(),
                thinking_level: mimir::model::ThinkingLevel::High,
                thinking_effort: None,
                system_prompt: "be precise".into(),
                messages: vec![Message::user("hello")],
                tools: Vec::new(),
                max_output_tokens: 100,
            },
            &Events::default(),
        )
        .await
        .expect("stream");
    server.await.expect("server");
    assert_eq!(response.message.text(), "native");
    assert_eq!(response.response_id.as_deref(), Some("resp-api"));
}

#[tokio::test]
async fn codex_model_discovery_filters_by_live_slugs_and_reuses_its_cache() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 8 * 1024];
        let read = socket.read(&mut request).await.expect("read request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("GET /backend-api/codex/models?client_version="));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: acct-discovery")
        );
        let body = r#"{"models":[{"slug":"gpt-5.6-sol"},{"slug":"gpt-5.4"},{"ignored":true}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider = CodexProvider::new(
        Some(&format!("http://{address}/backend-api")),
        "opaque-token",
        Some("acct-discovery"),
    )
    .expect("provider");
    let first = provider
        .available_model_ids()
        .await
        .expect("discovery")
        .expect("Codex IDs");
    assert_eq!(first, ["gpt-5.4", "gpt-5.6-sol"]);
    server.await.expect("server");
    let cached = provider
        .available_model_ids()
        .await
        .expect("cached discovery")
        .expect("cached Codex IDs");
    assert_eq!(cached, first);
}

#[test]
fn codex_debug_output_redacts_the_oauth_token() {
    let provider = CodexProvider::new(
        Some("https://example.invalid"),
        "not-a-jwt-secret",
        Some("acct-test"),
    )
    .expect("provider");
    let debug = format!("{provider:?}");
    assert!(!debug.contains("not-a-jwt-secret"));
    assert!(debug.contains("[REDACTED]"));
}
