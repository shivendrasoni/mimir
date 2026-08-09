use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn spawn_openai_server() -> (
    String,
    tokio::sync::oneshot::Receiver<(String, serde_json::Value)>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("address"));
    let (captured_sender, captured) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4_096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.expect("read request");
            assert!(read > 0, "request ended before headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.expect("read body");
            assert!(read > 0, "request ended before body");
            request.extend_from_slice(&buffer[..read]);
        }
        let body: Value = serde_json::from_slice(&request[header_end..header_end + content_length])
            .expect("request JSON");
        captured_sender
            .send((headers, body))
            .expect("capture request");
        let response = json!({
            "id": "custom-response",
            "choices": [{
                "message": {"role": "assistant", "content": "custom-ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write response");
    });
    (base_url, captured)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrated_custom_provider_uses_env_auth_compat_and_catalog_output_limit() {
    let (base_url, captured) = spawn_openai_server().await;
    let state = TempDir::new().expect("state");
    let workspace = TempDir::new().expect("workspace");
    std::fs::create_dir_all(state.path().join("config")).expect("config");
    std::fs::write(
        state.path().join("config/models.json"),
        serde_json::to_vec_pretty(&json!({
            "providers": {
                "local-openai": {
                    "api": "openai-completions",
                    "baseUrl": base_url,
                    "apiKey": "LOCAL_OPENAI_TEST_KEY",
                    "compat": {
                        "maxTokensField": "max_tokens",
                        "supportsReasoningEffort": false,
                        "supportsStrictMode": false
                    },
                    "models": [{"id": "local-model", "maxTokens": 321}]
                }
            }
        }))
        .expect("models"),
    )
    .expect("write models");

    Command::cargo_bin("mimir")
        .expect("binary")
        .env("LOCAL_OPENAI_TEST_KEY", "custom-test-secret")
        .args([
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--provider",
            "local-openai",
            "--model",
            "local-model",
            "--session",
            "custom-provider-runtime",
            "--print",
            "hello",
            "--no-tui",
            "--offline",
        ])
        .assert()
        .success();

    let (headers, body) = captured.await.expect("captured request");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer custom-test-secret")
    );
    assert_eq!(body.get("max_tokens"), Some(&json!(321)));
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("reasoning_effort").is_none());
}
