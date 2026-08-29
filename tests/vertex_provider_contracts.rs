use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    model::{Content, Message, ModelRequest, StopReason, ThinkingLevel, ToolCall, ToolDefinition},
    provider::{
        Provider, ProviderError, ProviderEvent, ProviderEventSink, VertexProvider,
        registry::{AuthKind, ModelDefinition, ProviderRegistry, RuntimeSupport},
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

fn request(model: &str) -> ModelRequest {
    let mut user = Message::user("inspect the image");
    user.content.push(Content::Image {
        data: "aW1hZ2U=".into(),
        mime_type: "image/png".into(),
    });
    ModelRequest {
        model: model.into(),
        thinking_level: ThinkingLevel::High,
        thinking_effort: None,
        system_prompt: "Be precise".into(),
        messages: vec![
            user,
            Message::assistant(
                vec![
                    Content::ToolCall(ToolCall {
                        id: "local-call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "README.md"}),
                    }),
                    Content::Thinking {
                        text: String::new(),
                        signature: Some("c2lnbmVkLXRvb2w=".into()),
                        redacted: false,
                    },
                ],
                StopReason::ToolUse,
            ),
            Message::tool_result(
                "local-call-1",
                "read_file",
                r#"{"contents":"hello"}"#,
                false,
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

#[tokio::test]
async fn api_key_mode_uses_express_resource_and_preserves_typed_parts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, body) = read_http_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with(
            "post /v1/publishers/google/models/gemini-3-flash-preview:generatecontent http/1.1"
        ));
        assert!(headers.contains("x-goog-api-key: vertex-secret"));
        assert!(!headers.contains("authorization:"));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be precise");
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["name"],
            "read_file"
        );
        assert!(body["contents"][1]["parts"][0]["functionCall"]["id"].is_null());
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            "c2lnbmVkLXRvb2w="
        );
        assert!(body["contents"][2]["parts"][0]["functionResponse"]["id"].is_null());
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["required"][0],
            "path"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );

        let response_body = json!({
            "responseId": "vertex-response-1",
            "candidates": [{
                "finishReason": "STOP",
                "content": {"role": "model", "parts": [
                    {"text": "considering", "thought": true, "thoughtSignature": "dGhvdWdodA=="},
                    {"text": "I will inspect it."},
                    {"inlineData": {"mimeType": "image/png", "data": "b3V0"}},
                    {"functionCall": {"name": "read_file", "args": {"path": "src/main.rs"}}, "thoughtSignature": "dG9vbA=="}
                ]}
            }],
            "usageMetadata": {
                "promptTokenCount": 14,
                "candidatesTokenCount": 6,
                "thoughtsTokenCount": 4,
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
            .expect("response");
    });

    let provider =
        VertexProvider::with_api_key(Some(&format!("http://{address}")), "vertex-secret")
            .expect("provider");
    let response = provider
        .complete(request("gemini-3-flash-preview"))
        .await
        .expect("completion");
    server.await.expect("server");

    assert_eq!(response.response_id.as_deref(), Some("vertex-response-1"));
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.message.usage.input_tokens, 14);
    assert_eq!(response.message.usage.output_tokens, 10);
    assert_eq!(response.message.usage.cached_tokens, 3);
    assert!(matches!(
        &response.message.content[0],
        Content::Thinking { text, signature, redacted: false }
            if text == "considering" && signature.as_deref() == Some("dGhvdWdodA==")
    ));
    assert!(matches!(
        &response.message.content[2],
        Content::Image { mime_type, data } if mime_type == "image/png" && data == "b3V0"
    ));
    assert!(matches!(
        &response.message.content[3],
        Content::ToolCall(call)
            if call.name == "read_file" && call.arguments == json!({"path": "src/main.rs"})
    ));
    assert!(matches!(
        &response.message.content[4],
        Content::Thinking { text, signature, .. }
            if text.is_empty() && signature.as_deref() == Some("dG9vbA==")
    ));
}

#[tokio::test]
async fn bearer_mode_uses_project_location_resource_and_sse_streaming() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let (headers, _) = read_http_request(&mut socket).await;
        let headers = headers.to_ascii_lowercase();
        assert!(headers.starts_with(
            "post /v1/projects/test-project/locations/us-central1/publishers/google/models/gemini-2.5-flash:streamgeneratecontent?alt=sse http/1.1"
        ));
        assert!(headers.contains("authorization: bearer adc-access-token"));
        assert!(headers.contains("x-goog-user-project: billing-project"));
        assert!(headers.contains("accept: text/event-stream"));
        assert!(!headers.contains("x-goog-api-key:"));

        let stream = concat!(
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"plan\",\"thought\":true}]}}],\"usageMetadata\":{\"promptTokenCount\":12,\"cachedContentTokenCount\":2}}\n\n",
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"do\"}]}}]}\n\n",
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ne\"},{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}},\"thoughtSignature\":\"c3RyZWFt\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3,\"cachedContentTokenCount\":2}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{stream}",
            stream.len()
        );
        socket.write_all(response.as_bytes()).await.expect("stream");
    });

    let provider = VertexProvider::with_bearer_token_and_quota_project(
        Some(&format!("http://{address}")),
        "test-project",
        "us-central1",
        "adc-access-token",
        Some("billing-project"),
    )
    .expect("provider");
    let events = Arc::new(Events::default());
    let response = provider
        .stream(request("gemini-2.5-flash"), events.as_ref())
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(
        events.0.lock().await.as_slice(),
        [
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::TextDelta("do".into()),
            ProviderEvent::TextDelta("ne".into())
        ]
    );
    assert_eq!(response.message.usage.input_tokens, 12);
    assert_eq!(response.message.usage.output_tokens, 8);
    assert_eq!(response.message.usage.cached_tokens, 2);
    assert_eq!(response.message.stop_reason, Some(StopReason::ToolUse));
    assert!(matches!(
        &response.message.content[1],
        Content::Text { text } if text == "done"
    ));
}

#[test]
fn endpoint_and_thinking_rules_are_explicit_and_safe() {
    let bearer = VertexProvider::with_bearer_token(None, "project-123", "europe-west4", "token")
        .expect("bearer provider");
    assert_eq!(
        bearer
            .endpoint_preview("models/gemini-2.5-pro", false)
            .expect("endpoint"),
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/project-123/locations/europe-west4/publishers/google/models/gemini-2.5-pro:generateContent"
    );
    let global = VertexProvider::with_bearer_token(None, "project-123", "global", "token")
        .expect("global provider");
    assert!(
        global
            .endpoint_preview("gemini-2.5-flash", false)
            .expect("endpoint")
            .starts_with("https://aiplatform.googleapis.com/v1/")
    );

    let api_key = VertexProvider::with_api_key(None, "key").expect("API-key provider");
    let mut off = request("gemini-2.5-pro");
    off.thinking_level = ThinkingLevel::Off;
    assert_eq!(
        api_key.request_preview(&off)["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
    off.model = "gemini-3.1-pro-preview".into();
    assert_eq!(
        api_key.request_preview(&off)["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
    off.model = "gemini-3-flash-preview".into();
    assert_eq!(
        api_key.request_preview(&off)["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MINIMAL"
    );
    assert_eq!(
        VertexProvider::with_api_key(
            Some("https://{location}-aiplatform.googleapis.com/v1beta1"),
            " key ",
        )
        .expect("templated base")
        .endpoint_preview("gemini-2.5-flash", false)
        .expect("versioned endpoint"),
        "https://aiplatform.googleapis.com/v1beta1/publishers/google/models/gemini-2.5-flash:generateContent"
    );

    let mut invalid_signature = request("gemini-3-flash-preview");
    invalid_signature.messages[1].content[1] = Content::Thinking {
        text: String::new(),
        signature: Some("not base64!".into()),
        redacted: false,
    };
    let preview = api_key.request_preview(&invalid_signature);
    assert!(preview["contents"][1]["parts"][0]["thoughtSignature"].is_null());

    assert!(matches!(
        bearer.endpoint_preview("../secrets?token=x", false),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        VertexProvider::with_bearer_token(None, "../project", "global", "token"),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        VertexProvider::with_api_key(Some("https://user:pass@example.com"), "key"),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        VertexProvider::with_api_key(Some("http://example.com"), "key"),
        Err(ProviderError::Protocol { .. })
    ));
    assert!(matches!(
        VertexProvider::with_api_key(None, "key with whitespace"),
        Err(ProviderError::Authentication)
    ));

    let mut invalid_tool = request("gemini-2.5-flash");
    invalid_tool.tools[0].name = "../unsafe".into();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(matches!(
        runtime.block_on(api_key.complete(invalid_tool)),
        Err(ProviderError::Protocol { message }) if message.contains("function name")
    ));
}

#[tokio::test]
async fn declared_oversized_response_is_rejected_before_body_buffering() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 9000000\r\nconnection: close\r\n\r\n")
            .await
            .expect("response");
    });
    let provider = VertexProvider::with_api_key(Some(&format!("http://{address}")), "secret")
        .expect("provider");
    let error = provider
        .complete(request("gemini-2.5-flash"))
        .await
        .expect_err("bounded");
    server.await.expect("server");
    assert!(matches!(
        error,
        ProviderError::Protocol { message } if message.contains("8 MiB")
    ));
}

#[tokio::test]
async fn errors_are_bounded_classified_and_redacted_for_both_auth_modes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        let body = json!({
            "error": {
                "status": "INVALID_ARGUMENT",
                "message": format!("bad credential adc-secret {}", "x".repeat(500))
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.expect("error");
    });
    let provider = VertexProvider::with_bearer_token(
        Some(&format!("http://{address}")),
        "project",
        "global",
        "adc-secret",
    )
    .expect("provider");
    assert!(!format!("{provider:?}").contains("adc-secret"));
    let error = provider
        .complete(request("gemini-2.5-flash"))
        .await
        .expect_err("error");
    server.await.expect("server");
    let ProviderError::Protocol { message } = error else {
        panic!("expected protocol error");
    };
    assert!(message.contains("[REDACTED]"));
    assert!(!message.contains("adc-secret"));
    assert!(message.len() < 400);
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
        VertexProvider::with_api_key(Some(&format!("http://{address}")), "secret")
            .expect("provider"),
    );
    let task = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.complete(request("gemini-2.5-flash")).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    task.abort();
    assert!(task.await.expect_err("task aborted").is_cancelled());
    assert_eq!(server.await.expect("server"), 0);
}

#[test]
fn registry_marks_vertex_native_with_api_key_and_ambient_auth() {
    let definition = ProviderRegistry::builtin()
        .get("google-vertex")
        .expect("Vertex definition");
    assert_eq!(definition.runtime_support, RuntimeSupport::GoogleVertex);
    assert_eq!(definition.auth, &[AuthKind::ApiKey, AuthKind::Ambient]);
    assert_eq!(
        definition.env_vars,
        &["GOOGLE_CLOUD_API_KEY", "GOOGLE_API_KEY"]
    );
    assert_eq!(definition.default_model, Some("gemini-2.5-flash"));

    let fallback = ModelDefinition::from_runtime(
        "google-vertex",
        "gemini-3-future",
        None,
        RuntimeSupport::GoogleVertex,
    );
    assert_eq!(fallback.api, "google-vertex");
    assert!(fallback.reasoning);
    assert_eq!(fallback.input, ["text", "image"]);
}
