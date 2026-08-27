use std::{
    io::{BufRead, BufReader as StdBufReader},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use mimir::{
    acp::{MIMIR_META_NAMESPACE, acp_updates_for_runtime_event, serve_acp},
    budget::Budget,
    model::{Content, Message, ModelRequest, ModelResponse, StopReason, ToolCall},
    provider::{FakeProvider, Provider, ProviderError, ProviderEvent, ProviderEventSink},
    runtime::{AgentRuntime, RuntimeConfig, RuntimeEvent},
    session::InMemorySessionStore,
    tools::{ObservationStatus, ToolObservation, ToolPolicy, ToolRegistry},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

fn response(content: Vec<Content>, stop_reason: StopReason) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(content, stop_reason),
        response_id: Some("acp-test-response".into()),
    }
}

fn send_sync(writer: &mut impl std::io::Write, frame: &Value) {
    writeln!(writer, "{frame}").expect("write frame");
    writer.flush().expect("flush frame");
}

fn receive_sync(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read frame");
    serde_json::from_str(&line).expect("valid JSON-RPC frame")
}

async fn runtime(root: &Path, provider: Arc<dyn Provider>) -> Arc<AgentRuntime> {
    let tools = ToolRegistry::with_default_tools(root, ToolPolicy::default()).expect("tools");
    runtime_with_tools(provider, tools).await
}

async fn runtime_with_tools(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Arc<AgentRuntime> {
    Arc::new(
        AgentRuntime::resume(
            provider,
            Arc::new(tools),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig {
                provider: "fake".into(),
                model: "fake-model".into(),
                system_prompt: String::new(),
                budget: Budget::default(),
                provider_timeout: Duration::from_secs(5),
                ..RuntimeConfig::default_for_model("fake-model")
            },
        )
        .await
        .expect("runtime"),
    )
}

struct StreamingProvider {
    response: ModelResponse,
    requests: tokio::sync::Mutex<Vec<ModelRequest>>,
}

impl StreamingProvider {
    fn new(response: ModelResponse) -> Self {
        Self {
            response,
            requests: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl Provider for StreamingProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(self.response.clone())
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        self.requests.lock().await.push(request);
        sink.emit(ProviderEvent::TextDelta("hel".into())).await;
        sink.emit(ProviderEvent::TextDelta("lo".into())).await;
        Ok(self.response.clone())
    }
}

struct AcpClient {
    reader: BufReader<ReadHalf<tokio::io::DuplexStream>>,
    writer: WriteHalf<tokio::io::DuplexStream>,
}

impl AcpClient {
    async fn send(&mut self, frame: Value) {
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("write ACP frame");
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .expect("ACP response timeout")
            .expect("read ACP frame");
        serde_json::from_str(&line).expect("valid ACP JSON")
    }

    async fn response(&mut self, id: i64) -> (Value, Vec<Value>) {
        let mut notifications = Vec::new();
        loop {
            let frame = self.recv().await;
            if frame.get("id") == Some(&json!(id)) {
                return (frame, notifications);
            }
            notifications.push(frame);
        }
    }
}

fn start(
    runtime: Arc<AgentRuntime>,
    cwd: &Path,
) -> (AcpClient, tokio::task::JoinHandle<mimir::error::Result<()>>) {
    let (client, server) = tokio::io::duplex(256 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let cwd = cwd.to_path_buf();
    let task =
        tokio::spawn(async move { serve_acp(runtime, cwd, server_read, server_write).await });
    (
        AcpClient {
            reader: BufReader::new(client_read),
            writer: client_write,
        },
        task,
    )
}

async fn initialize_and_create(client: &mut AcpClient, cwd: &Path) -> String {
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }))
        .await;
    let (initialize, notifications) = client.response(1).await;
    assert!(notifications.is_empty());
    assert_eq!(initialize["result"]["protocolVersion"], 1);
    assert_eq!(
        initialize["result"]["agentCapabilities"]["promptCapabilities"],
        json!({"image": true, "embeddedContext": true})
    );
    assert_eq!(
        initialize["result"]["_meta"][MIMIR_META_NAMESPACE],
        json!({})
    );

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": cwd, "mcpServers": []}
        }))
        .await;
    let (created, notifications) = client.response(2).await;
    assert!(notifications.is_empty());
    created["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned()
}

#[tokio::test]
async fn acp_stdio_protocol_streams_updates_and_forwards_all_prompt_content() {
    let root = TempDir::new().expect("tempdir");
    let provider = Arc::new(StreamingProvider::new(response(
        vec![Content::Text {
            text: "hello".into(),
        }],
        StopReason::Stop,
    )));
    let runtime = runtime(root.path(), provider.clone()).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [
                    {"type": "text", "text": "inspect"},
                    {"type": "image", "mimeType": "image/png", "data": "aGVsbG8="},
                    {"type": "resource", "resource": {"uri": "file:///notes", "text": "embedded"}},
                    {"type": "resource_link", "uri": "file:///linked"}
                ]
            }
        }))
        .await;
    let (prompt, notifications) = client.response(3).await;
    assert_eq!(prompt["result"]["stopReason"], "end_turn");
    let streamed = notifications
        .iter()
        .filter(|frame| {
            frame["method"] == "session/update"
                && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        })
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert_eq!(streamed, "hello");

    let requests = provider.requests().await;
    let content = &requests[0].messages.last().expect("user message").content;
    assert!(content.iter().any(
        |block| matches!(block, Content::Text { text } if text.contains("inspect\nfile:///notes\nembedded\nfile:///linked"))
    ));
    assert!(content.iter().any(
        |block| matches!(block, Content::Image { mime_type, data } if mime_type == "image/png" && data == "aGVsbG8=")
    ));

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/close",
            "params": {"sessionId": session_id}
        }))
        .await;
    let (closed, _) = client.response(4).await;
    assert_eq!(closed["result"], json!({}));

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/new",
            "params": {"cwd": root.path(), "mcpServers": []}
        }))
        .await;
    let (replacement, _) = client.response(5).await;
    assert!(replacement["result"]["sessionId"].is_string());
    assert_ne!(replacement["result"]["sessionId"], session_id);
    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_rejects_parallel_turns_and_cancel_stops_only_the_active_session() {
    let root = TempDir::new().expect("tempdir");
    let provider = FakeProvider::new(vec![response(
        vec![Content::Text {
            text: "too late".into(),
        }],
        StopReason::Stop,
    )])
    .with_delay(Duration::from_secs(30));
    let runtime = runtime(root.path(), Arc::new(provider)).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "wait"}]}
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "overlap"}]}
        }))
        .await;
    let (parallel, _) = client.response(4).await;
    assert_eq!(parallel["error"]["code"], -32_000);
    assert!(
        parallel["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("already running"))
    );

    client
        .send(json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}
        }))
        .await;
    let (cancelled, _) = client.response(3).await;
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_forwards_session_lifetime_metadata_and_autonomous_limits() {
    let root = TempDir::new().expect("tempdir");
    let provider = FakeProvider::new(vec![response(
        vec![Content::Text {
            text: "finished".into(),
        }],
        StopReason::Stop,
    )])
    .with_delay(Duration::from_millis(150));
    let runtime = runtime(root.path(), Arc::new(provider)).await;
    let (mut client, server) = start(Arc::clone(&runtime), root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    runtime
        .publish_session_event(json!({
            "type": "goal_update",
            "goal": {"status": "in_progress", "objective": "ship it"}
        }))
        .expect("publish goal");
    let goal = client.recv().await;
    assert_eq!(goal["method"], "session/update");
    assert_eq!(
        goal["params"]["update"]["_meta"][MIMIR_META_NAMESPACE]["goal"]["objective"],
        "ship it"
    );

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "continue"}]}
        }))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    runtime
        .publish_session_event(json!({
            "type": "autonomous_status",
            "status": {"enabled": true, "limitReason": "maxTurns", "turnsUsed": 12}
        }))
        .expect("publish autonomous status");
    let (completed, notifications) = client.response(3).await;
    assert_eq!(completed["result"]["stopReason"], "max_turn_requests");
    assert!(notifications.iter().any(|frame| {
        frame["params"]["update"]["_meta"][MIMIR_META_NAMESPACE]["autonomous"]["limitReason"]
            == "maxTurns"
    }));

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn runtime_event_stream_is_bounded_monotonic_and_reports_lag() {
    let root = TempDir::new().expect("tempdir");
    let runtime = runtime(root.path(), Arc::new(FakeProvider::new(Vec::new()))).await;
    let mut events = runtime.subscribe_events();
    for index in 0..300_u64 {
        runtime
            .publish_session_event(json!({"type": "goal_update", "index": index}))
            .expect("publish event");
    }
    let lagged = events
        .recv()
        .await
        .expect_err("bounded receiver must report lag");
    assert!(matches!(
        lagged,
        tokio::sync::broadcast::error::RecvError::Lagged(count) if count > 0
    ));
    let first = events.recv().await.expect("newest retained event");
    let second = events.recv().await.expect("next retained event");
    assert_eq!(second.sequence, first.sequence + 1);
    assert!(first.source.is_none());
}

#[test]
fn acp_event_mapping_preserves_thoughts_tools_and_ipython_metadata() {
    let thinking = Message::assistant(
        vec![Content::Thinking {
            text: "reasoning".into(),
            signature: None,
            redacted: false,
        }],
        StopReason::Stop,
    );
    assert_eq!(
        acp_updates_for_runtime_event(&RuntimeEvent::MessageCompleted { message: thinking }),
        vec![json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "reasoning"}
        })]
    );

    assert_eq!(
        acp_updates_for_runtime_event(&RuntimeEvent::ToolStarted {
            id: "cell-1".into(),
            name: "ipython".into(),
            arguments: json!({"code": "print(1)"}),
        }),
        vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "cell-1",
            "title": "IPython cell",
            "kind": "execute",
            "status": "in_progress",
            "rawInput": {"code": "print(1)"}
        })]
    );

    let updates = acp_updates_for_runtime_event(&RuntimeEvent::ToolFinished {
        id: "cell-1".into(),
        name: "ipython".into(),
        observation: ToolObservation {
            status: ObservationStatus::Success,
            summary: "cell complete".into(),
            next_actions: Vec::new(),
            artifacts: Vec::new(),
            content: json!({
                "output": "done",
                "details": {
                    "attachments": [{"mimeType": "image/png", "path": "/tmp/a.png", "data": "aGVsbG8="}],
                    "diffs": [{"path": "a.rs"}]
                }
            })
            .to_string(),
        },
    });
    assert_eq!(updates[0]["status"], "completed");
    assert_eq!(
        updates[0]["_meta"][MIMIR_META_NAMESPACE]["ipython"]["attachments"][0]["bytes"],
        5
    );
    assert_eq!(
        updates[0]["_meta"][MIMIR_META_NAMESPACE]["ipython"]["diffCount"],
        1
    );
}

#[test]
fn acp_event_mapping_preserves_control_plane_metadata() {
    let cases = [
        (json!({"type": "heartbeats_changed"}), "heartbeatsChanged"),
        (
            json!({"type": "compaction_end", "result": {"tokensBefore": 42, "summary": "short"}}),
            "compaction",
        ),
        (
            json!({"type": "rlm_child_update", "child": {"id": "child-1", "status": "running"}}),
            "subagents",
        ),
        (
            json!({"type": "refine_failed", "error": "no edit"}),
            "refinement",
        ),
        (
            json!({"type": "inbound_agent_message", "message": {"messageId": "agentmsg_1"}}),
            "agentMessage",
        ),
    ];
    for (event, key) in cases {
        let updates = acp_updates_for_runtime_event(&RuntimeEvent::SessionEvent { event });
        assert_eq!(updates.len(), 1);
        assert!(updates[0]["_meta"][MIMIR_META_NAMESPACE].get(key).is_some());
    }
}

#[tokio::test]
async fn acp_provider_failures_are_returned_with_sanitized_json_rpc_errors() {
    let root = TempDir::new().expect("tempdir");
    let provider = FakeProvider::with_results(vec![Err(ProviderError::Protocol {
        message: "upstream rejected the test request".into(),
    })]);
    let runtime = runtime(root.path(), Arc::new(provider)).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "fail"}]}
        }))
        .await;
    let (failure, _) = client.response(3).await;
    assert_eq!(failure["error"]["code"], -32_603);
    assert!(
        failure["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("upstream rejected the test request"))
    );
    assert!(failure["error"].get("stack").is_none());

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_error_stop_reason_is_not_reported_as_a_clean_turn() {
    let root = TempDir::new().expect("tempdir");
    let provider = FakeProvider::new(vec![response(Vec::new(), StopReason::Error)]);
    let runtime = runtime(root.path(), Arc::new(provider)).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "fail"}]}
        }))
        .await;
    let (failure, _) = client.response(3).await;
    assert_eq!(failure["error"]["code"], -32_603);
    assert!(
        failure["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("turn failed"))
    );

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_never_bypasses_runtime_tool_permissions() {
    let root = TempDir::new().expect("tempdir");
    let provider = FakeProvider::new(vec![
        response(
            vec![Content::ToolCall(ToolCall {
                id: "process-1".into(),
                name: "run_process".into(),
                arguments: json!({"program": "echo", "args": ["unsafe"]}),
            })],
            StopReason::ToolUse,
        ),
        response(
            vec![Content::Text {
                text: "permission preserved".into(),
            }],
            StopReason::Stop,
        ),
    ]);
    let tools = ToolRegistry::with_default_tools(
        root.path(),
        ToolPolicy {
            allow_process: false,
            ..ToolPolicy::default()
        },
    )
    .expect("tools");
    let runtime = runtime_with_tools(Arc::new(provider), tools).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "try process"}]}
        }))
        .await;
    let (completed, notifications) = client.response(3).await;
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    assert!(notifications.iter().any(|frame| {
        frame["params"]["update"]["sessionUpdate"] == "tool_call"
            && frame["params"]["update"]["kind"] == "execute"
    }));
    assert!(notifications.iter().any(|frame| {
        frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && frame["params"]["update"]["status"] == "failed"
    }));

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_preserves_persistent_ipython_state_and_execute_updates() {
    let root = TempDir::new().expect("tempdir");
    let state = TempDir::new().expect("state");
    let policy = ToolPolicy {
        allow_process: true,
        ..ToolPolicy::default()
    };
    let mut tools = ToolRegistry::with_default_tools(root.path(), policy.clone()).expect("tools");
    tools
        .register_ipython_kernel(root.path(), state.path(), "acp-kernel", policy)
        .expect("IPython kernel tool");
    let provider = FakeProvider::new(vec![
        response(
            vec![Content::ToolCall(ToolCall {
                id: "cell-1".into(),
                name: "ipython".into(),
                arguments: json!({"code": "acp_state = 41\nprint('set')"}),
            })],
            StopReason::ToolUse,
        ),
        response(
            vec![Content::ToolCall(ToolCall {
                id: "cell-2".into(),
                name: "ipython".into(),
                arguments: json!({"code": "print(acp_state + 1)"}),
            })],
            StopReason::ToolUse,
        ),
        response(
            vec![Content::Text {
                text: "kernel state preserved".into(),
            }],
            StopReason::Stop,
        ),
    ]);
    let runtime = runtime_with_tools(Arc::new(provider), tools).await;
    let (mut client, server) = start(runtime, root.path());
    let session_id = initialize_and_create(&mut client, root.path()).await;

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "use persistent Python"}]}
        }))
        .await;
    let (completed, notifications) = client.response(3).await;
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    let cells = notifications
        .iter()
        .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call")
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 2);
    assert!(
        cells
            .iter()
            .all(|frame| frame["params"]["update"]["kind"] == "execute")
    );
    let second_result = notifications.iter().find(|frame| {
        frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && frame["params"]["update"]["toolCallId"] == "cell-2"
    });
    assert!(second_result.is_some_and(|frame| frame.to_string().contains("42")));

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn acp_reports_a_cwd_mismatch_only_in_namespaced_metadata() {
    let root = TempDir::new().expect("tempdir");
    let other = TempDir::new().expect("other tempdir");
    let provider = FakeProvider::new(Vec::new());
    let runtime = runtime(root.path(), Arc::new(provider)).await;
    let (mut client, server) = start(runtime, root.path());

    client
        .send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }))
        .await;
    let _ = client.response(1).await;
    client
        .send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": other.path(), "mcpServers": []}
        }))
        .await;
    let (created, _) = client.response(2).await;
    assert!(created["result"]["sessionId"].is_string());
    assert_eq!(
        created["result"]["_meta"][MIMIR_META_NAMESPACE]["cwd"]["requested"],
        other.path().to_string_lossy().as_ref()
    );
    assert_eq!(
        created["result"]["_meta"][MIMIR_META_NAMESPACE]["cwd"]["actual"],
        root.path()
            .canonicalize()
            .expect("canonical root")
            .to_string_lossy()
            .as_ref()
    );
    assert!(created["result"].get("cwd").is_none());

    client.writer.shutdown().await.expect("close stdin");
    server.await.expect("server join").expect("server result");
}

#[test]
fn cold_cli_accepts_reference_mode_flag_and_keeps_protocol_on_stdout() {
    let root = TempDir::new().expect("tempdir");
    let state = TempDir::new().expect("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mimir"))
        .args([
            "--workspace",
            root.path().to_str().expect("workspace path"),
            "--state-dir",
            state.path().to_str().expect("state path"),
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--fake-response",
            "hello from cold ACP",
            "--no-session",
            "--offline",
            "--mode",
            "acp",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ACP CLI");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = StdBufReader::new(child.stdout.take().expect("child stdout"));

    send_sync(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }),
    );
    let initialize = receive_sync(&mut stdout);
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["agentInfo"]["name"], "mimir");

    send_sync(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": root.path(), "mcpServers": []}
        }),
    );
    let created = receive_sync(&mut stdout);
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    send_sync(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}
        }),
    );
    let mut text = String::new();
    loop {
        let frame = receive_sync(&mut stdout);
        if frame["id"] == 3 {
            assert_eq!(frame["result"]["stopReason"], "end_turn");
            break;
        }
        if frame["method"] == "session/update" {
            text.push_str(
                frame["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or(""),
            );
        }
    }
    assert_eq!(text, "hello from cold ACP");

    drop(stdin);
    let status = child.wait().expect("wait for ACP CLI");
    assert!(status.success());
}
