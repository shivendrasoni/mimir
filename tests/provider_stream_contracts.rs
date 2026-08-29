#[path = "support/reliability.rs"]
#[allow(dead_code)]
mod reliability;

use std::sync::Arc;

use async_trait::async_trait;
use mimir::{
    config::ProviderConfig,
    model::{Message, ModelRequest},
    provider::{OpenAiProvider, Provider, ProviderEvent, ProviderEventSink},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

use reliability::{minimal_request, serve_sse};

#[derive(Default)]
struct Events(Mutex<Vec<ProviderEvent>>);

#[async_trait]
impl ProviderEventSink for Events {
    async fn emit(&self, event: ProviderEvent) {
        self.0.lock().await.push(event);
    }
}

#[tokio::test]
async fn openai_sse_stream_forwards_text_and_reassembles_tool_arguments() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 16 * 1024];
        let read = socket.read(&mut request).await.expect("read request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("\"stream\":true"));
        let body = concat!(
            "data: {\"id\":\"resp-1\",\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });

    let provider = OpenAiProvider::new(
        ProviderConfig::openai(format!("http://{address}"), "test", "secret").expect("config"),
    )
    .expect("provider");
    let sink = Arc::new(Events::default());
    let response = provider
        .stream(
            ModelRequest {
                model: "test".into(),
                thinking_level: mimir::model::ThinkingLevel::Off,
                thinking_effort: None,
                system_prompt: String::new(),
                messages: vec![Message::user("hello")],
                tools: Vec::new(),
                max_output_tokens: 100,
            },
            sink.as_ref(),
        )
        .await
        .expect("stream");
    server.await.expect("server");

    assert_eq!(response.message.text(), "hello");
    assert_eq!(response.message.usage.input_tokens, 4);
    assert_eq!(response.message.content.len(), 2);
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [
            ProviderEvent::TextDelta("hel".into()),
            ProviderEvent::TextDelta("lo".into()),
        ]
    );
}

#[tokio::test]
async fn openai_sse_stream_keeps_a_final_frame_without_a_newline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 16 * 1024];
        let _ = socket.read(&mut request).await.expect("read request");
        let body = "data: {\"id\":\"final-eof\",\"choices\":[{\"delta\":{\"content\":\"kept\"},\"finish_reason\":\"stop\"}]}";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    let provider = OpenAiProvider::new(
        ProviderConfig::openai(format!("http://{address}"), "test-model", "test-key")
            .expect("config"),
    )
    .expect("provider");
    let sink = Events::default();
    let result = provider
        .stream(
            ModelRequest {
                model: "test-model".into(),
                thinking_level: mimir::model::ThinkingLevel::Off,
                thinking_effort: None,
                system_prompt: String::new(),
                messages: vec![Message::user("hello")],
                tools: Vec::new(),
                max_output_tokens: 100,
            },
            &sink,
        )
        .await
        .expect("stream");
    server.await.expect("server task");

    assert_eq!(result.message.text(), "kept");
    assert_eq!(
        sink.0.lock().await.as_slice(),
        [ProviderEvent::TextDelta("kept".into())]
    );
}

#[tokio::test]
async fn malformed_sse_event_is_a_protocol_error_and_emits_no_partial_text() {
    let server = serve_sse(b"data: {\"choices\": [not-json}\n\ndata: [DONE]\n\n".to_vec()).await;
    let provider = OpenAiProvider::new(
        ProviderConfig::openai(&server.base_url, "reliability-fixture", "fixture-secret")
            .expect("config"),
    )
    .expect("provider");
    let sink = Events::default();

    let error = provider
        .stream(minimal_request(), &sink)
        .await
        .expect_err("malformed event must fail closed");
    server.finish().await;

    assert!(error.to_string().contains("protocol error"));
    assert!(error.to_string().contains("invalid provider stream event"));
    assert!(sink.0.lock().await.is_empty());
}

#[tokio::test]
async fn truncated_tool_arguments_never_become_an_executable_tool_call() {
    let body = concat!(
        "data: {\"id\":\"truncated\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = serve_sse(body.as_bytes().to_vec()).await;
    let provider = OpenAiProvider::new(
        ProviderConfig::openai(&server.base_url, "reliability-fixture", "fixture-secret")
            .expect("config"),
    )
    .expect("provider");
    let sink = Events::default();

    let error = provider
        .stream(minimal_request(), &sink)
        .await
        .expect_err("incomplete tool JSON must fail before tool execution");
    server.finish().await;

    assert!(error.to_string().contains("protocol error"));
    assert!(
        error
            .to_string()
            .contains("tool arguments are invalid JSON")
    );
    assert!(sink.0.lock().await.is_empty());
}
