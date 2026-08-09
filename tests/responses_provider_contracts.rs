use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    model::{Content, Message, ModelRequest, StopReason, ThinkingLevel, ToolCall, ToolDefinition},
    provider::{Provider, ProviderError, ProviderEvent, ProviderEventSink, ResponsesProvider},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Events(Mutex<Vec<ProviderEvent>>);

#[async_trait]
impl ProviderEventSink for Events {
    async fn emit(&self, event: ProviderEvent) {
        self.0.lock().await.push(event);
    }
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
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
        .expect("headers")
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

fn request() -> ModelRequest {
    let mut user = Message::user("inspect this");
    user.content.push(Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    });
    ModelRequest {
        model: "gpt-5-mini".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: "Be precise".into(),
        messages: vec![
            Message::system("Follow workspace policy"),
            user,
            Message::assistant(
                vec![
                    Content::Text {
                        text: "I will read it".into(),
                    },
                    Content::ToolCall(ToolCall {
                        id: "call-old".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "README.md"}),
                    }),
                ],
                StopReason::ToolUse,
            ),
            Message::tool_result("call-old", "read_file", "old output", false),
        ],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        max_output_tokens: 512,
    }
}

#[tokio::test]
async fn responses_maps_typed_input_and_streams_text_reasoning_and_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/responses http/1.1"));
        assert!(headers.contains("authorization: bearer responses-secret"));
        assert!(headers.contains("accept: text/event-stream"));
        assert_eq!(body["model"], "gpt-5-mini");
        assert_eq!(body["instructions"], "Be precise");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["role"], "system");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
        assert_eq!(
            body["input"][1]["content"][1]["image_url"],
            "data:image/png;base64,aW1hZ2U="
        );
        assert_eq!(body["input"][2]["role"], "assistant");
        assert_eq!(body["input"][3]["type"], "function_call");
        assert_eq!(body["input"][4]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 512);

        let stream = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"plan\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc-1\",\"delta\":\"{\\\"path\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc-1\",\"delta\":\"\\\"src/main.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":3}}}}\n\n",
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
        ResponsesProvider::new(Some(&format!("http://{address}/v1")), "responses-secret")
            .expect("provider");
    let events = Arc::new(Events::default());
    let response = provider
        .stream(request(), events.as_ref())
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("resp-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 11);
    assert_eq!(response.message.usage.output_tokens, 7);
    assert_eq!(response.message.usage.cached_tokens, 3);
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, .. } if text == "plan"
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call)
            if call.id == "call-1"
                && call.name == "read_file"
                && call.arguments == json!({"path": "src/main.rs"})
    ));
    assert_eq!(
        events.0.lock().await.as_slice(),
        [
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::TextDelta("done".into()),
        ]
    );
}

#[tokio::test]
async fn azure_responses_uses_api_key_header_and_preserves_api_version() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, _) = read_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(
            headers
                .starts_with("post /openai/v1/responses?api-version=2025-04-01-preview http/1.1")
        );
        assert!(headers.contains("api-key: azure-secret"));
        assert!(!headers.contains("authorization:"));
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"azure\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-azure\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
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

    let provider = ResponsesProvider::new_azure(
        Some(&format!("http://{address}/openai/v1")),
        "azure-secret",
        Some("2025-04-01-preview"),
    )
    .expect("provider");
    let response = provider.complete(request()).await.expect("Azure response");
    server.await.expect("server");
    assert_eq!(response.message.text(), "azure");
}

#[tokio::test]
async fn diagnostics_redact_credentials_and_bound_declared_responses() {
    let provider = ResponsesProvider::new(Some("https://example.invalid/v1"), "never-print-me")
        .expect("provider");
    let debug = format!("{provider:?}");
    assert!(!debug.contains("never-print-me"));
    assert!(debug.contains("[REDACTED]"));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        let body = r#"{"error":{"message":"credential leaked-secret is invalid"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    let provider = ResponsesProvider::new(Some(&format!("http://{address}")), "leaked-secret")
        .expect("provider");
    let error = provider.complete(request()).await.expect_err("rate limit");
    server.await.expect("server");
    assert!(matches!(error, ProviderError::RateLimited { .. }));
    assert!(!error.to_string().contains("leaked-secret"));
    assert!(error.to_string().contains("[REDACTED]"));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 9000000\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write response");
    });
    let provider =
        ResponsesProvider::new(Some(&format!("http://{address}")), "secret").expect("provider");
    let error = provider
        .complete(request())
        .await
        .expect_err("bounded body");
    server.await.expect("server");
    assert!(matches!(error, ProviderError::Protocol { .. }));
    assert!(error.to_string().contains("8 MiB"));
}

#[tokio::test]
async fn cancellation_aborts_an_in_flight_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let accepted = Arc::new(tokio::sync::Notify::new());
    let server_accepted = Arc::clone(&accepted);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        server_accepted.notify_one();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let provider =
        ResponsesProvider::new(Some(&format!("http://{address}")), "secret").expect("provider");
    let cancellation = CancellationToken::new();
    let sink = Events::default();
    let future = provider.stream_with_cancellation(request(), &sink, cancellation.clone());
    tokio::pin!(future);
    tokio::select! {
        () = accepted.notified() => cancellation.cancel(),
        result = &mut future => panic!("request completed before cancellation: {result:?}"),
    }
    let result = tokio::time::timeout(Duration::from_secs(1), &mut future)
        .await
        .expect("prompt cancellation");
    assert_eq!(result, Err(ProviderError::Aborted));
    server.abort();
}

#[test]
fn responses_rejects_blank_credentials_and_invalid_urls() {
    assert!(matches!(
        ResponsesProvider::new(None, "  "),
        Err(ProviderError::Authentication)
    ));
    assert!(matches!(
        ResponsesProvider::new(Some("not a URL"), "secret"),
        Err(ProviderError::Protocol { .. })
    ));
}
