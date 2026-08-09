use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    model::{
        Content, Message, ModelRequest, ModelResponse, StopReason, ThinkingLevel, ToolCall,
        ToolDefinition,
    },
    provider::{GoogleProvider, Provider, ProviderError, ProviderEvent, ProviderEventSink},
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
        model: "gemini-2.5-flash".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: "Be precise".into(),
        messages: vec![
            user,
            Message::assistant(
                vec![
                    Content::Thinking {
                        text: "need the file".into(),
                        signature: Some("signed-thought".into()),
                        redacted: false,
                    },
                    Content::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "README.md"}),
                    }),
                ],
                StopReason::ToolUse,
            ),
            Message::tool_result("call-1", "read_file", r#"{"contents":"hello"}"#, false),
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

fn assert_typed_response_and_signature_replay(provider: &GoogleProvider, response: ModelResponse) {
    assert_eq!(response.response_id.as_deref(), Some("gemini-response-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 11);
    assert_eq!(response.message.usage.output_tokens, 7);
    assert_eq!(response.message.usage.cached_tokens, 3);
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, signature, redacted: false }
            if text == "considering" && signature.as_deref() == Some("sig-1")
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call)
            if call.id == "call-2"
                && call.name == "read_file"
                && call.arguments == json!({"path": "src/main.rs"})
    ));
    assert!(matches!(
        &response.message.content[3],
        Content::Thinking { text, signature, redacted: false }
            if text.is_empty() && signature.as_deref() == Some("sig-call")
    ));

    let replay = provider.request_preview(&ModelRequest {
        model: "gemini-3-flash".into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: String::new(),
        messages: vec![response.message],
        tools: Vec::new(),
        max_output_tokens: 1024,
    });
    assert_eq!(
        replay["contents"][0]["parts"][2]["functionCall"]["name"],
        "read_file"
    );
    assert_eq!(
        replay["contents"][0]["parts"][2]["thoughtSignature"],
        "sig-call"
    );
    assert_eq!(
        replay["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
}

#[tokio::test]
async fn generate_content_translates_typed_messages_tools_and_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_http_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(
            headers.starts_with("post /v1beta/models/gemini-2.5-flash:generatecontent http/1.1")
        );
        assert!(headers.contains("x-goog-api-key: test-secret"));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be precise");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["contents"][1]["parts"][0]["thought"], true);
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            "signed-thought"
        );
        assert_eq!(
            body["contents"][1]["parts"][1]["functionCall"]["name"],
            "read_file"
        );
        assert_eq!(body["contents"][2]["role"], "user");
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["id"],
            "call-1"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["contents"],
            "hello"
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["required"][0],
            "path"
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let response_body = json!({
            "responseId": "gemini-response-1",
            "candidates": [{
                "finishReason": "STOP",
                "content": {"role": "model", "parts": [
                    {"text": "considering", "thought": true, "thoughtSignature": "sig-1"},
                    {"text": "I will inspect it."},
                    {"functionCall": {"id": "call-2", "name": "read_file", "args": {"path": "src/main.rs"}}, "thoughtSignature": "sig-call"}
                ]}
            }],
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 7,
                "cachedContentTokenCount": 3
            }
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

    let provider = GoogleProvider::new(Some(&format!("http://{address}/v1beta")), "test-secret")
        .expect("provider");
    let response = provider.complete(request()).await.expect("completion");
    server.await.expect("server");
    assert_typed_response_and_signature_replay(&provider, response);
}

#[tokio::test]
async fn stream_generate_content_forwards_text_and_thought_chunks_and_collects_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, _) = read_http_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with(
            "post /v1beta/models/gemini-2.5-flash:streamgeneratecontent?alt=sse http/1.1"
        ));
        assert!(headers.contains("accept: text/event-stream"));
        let stream = concat!(
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"plan\",\"thought\":true}]}}],\"usageMetadata\":{\"promptTokenCount\":9}}\n\n",
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"done\"}]}}]}\n\n",
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"id\":\"call-3\",\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}},\"thoughtSignature\":\"call-signature\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":6}}\n\n"
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
        GoogleProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    let sink = Arc::new(Events::default());
    let response = provider
        .stream(request(), sink.as_ref())
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("stream-1"));
    assert_eq!(response.message.usage.input_tokens, 9);
    assert_eq!(response.message.usage.output_tokens, 6);
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::TextDelta("done".into()),
        ]
    );
    assert!(matches!(
        &response.message.content[2],
        Content::ToolCall(call) if call.id == "call-3" && call.name == "read_file"
    ));
    assert!(matches!(
        &response.message.content[3],
        Content::Thinking { text, signature, .. }
            if text.is_empty() && signature.as_deref() == Some("call-signature")
    ));
}

#[tokio::test]
async fn errors_are_bounded_classified_and_do_not_expose_the_api_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let body = json!({"error": {"message": format!("bad credential test-secret {}", "x".repeat(500))}}).to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider =
        GoogleProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    assert!(!format!("{provider:?}").contains("test-secret"));
    let error = provider.complete(request()).await.expect_err("error");
    server.await.expect("server");
    let ProviderError::Protocol { message } = error else {
        panic!("expected protocol error");
    };
    assert!(!message.contains("test-secret"));
    assert!(message.contains("[REDACTED]"));
    assert!(message.len() < 400);
}

#[tokio::test]
async fn oversized_declared_response_is_rejected_without_reading_the_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 9000000\r\nconnection: close\r\n\r\n")
            .await
            .expect("write response");
    });

    let provider =
        GoogleProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider");
    let error = provider.complete(request()).await.expect_err("bounded");
    server.await.expect("server");
    assert!(matches!(error, ProviderError::Protocol { message } if message.contains("8 MiB")));
}

#[tokio::test]
async fn completion_future_is_cancel_safe() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let mut byte = [0_u8; 1];
        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .expect("cancelled client closes promptly")
            .expect("read cancellation EOF")
    });

    let provider = Arc::new(
        GoogleProvider::new(Some(&format!("http://{address}")), "test-secret").expect("provider"),
    );
    let task = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.complete(request()).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    task.abort();
    assert!(task.await.expect_err("task aborted").is_cancelled());
    assert_eq!(server.await.expect("server"), 0);
}

#[test]
fn constructor_rejects_blank_keys_and_unsafe_model_ids() {
    assert!(matches!(
        GoogleProvider::new(None, "  "),
        Err(ProviderError::Authentication)
    ));
    let provider = GoogleProvider::new(Some("http://127.0.0.1:9"), "secret").expect("provider");
    let mut unsafe_request = request();
    unsafe_request.model = "../secrets?key=oops".into();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let error = runtime
        .block_on(provider.complete(unsafe_request))
        .expect_err("unsafe model");
    assert!(matches!(error, ProviderError::Protocol { message } if message.contains("model id")));
}

#[test]
fn system_messages_are_folded_into_system_instruction() {
    let provider = GoogleProvider::new(Some("http://127.0.0.1:9"), "secret").expect("provider");
    let mut input = request();
    input.messages.insert(0, Message::system("Second rule"));
    let body = provider.request_preview(&input);

    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "Be precise\n\nSecond rule"
    );
    assert!(
        body["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .all(|content| content["role"] != "system")
    );
}
