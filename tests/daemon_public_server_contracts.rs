#![cfg(unix)]

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::daemon::{
    DaemonConfig, DaemonError, DaemonServer, PUBLIC_DAEMON_PROTOCOL_NAME, PromptHandler,
    PromptRequest, PublicDaemonCommand,
};
use mimir::{
    model::Message,
    runtime::RuntimeEvent,
    runtime_events::{RuntimeEventBus, RuntimeEventEnvelope},
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    tools::{ObservationStatus, ToolObservation},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

struct EchoHandler;

#[derive(Default)]
struct MultimodalHandler {
    received: tokio::sync::Mutex<Vec<Message>>,
}

#[derive(Default)]
struct LiveEventHandler {
    events: RuntimeEventBus,
}

#[derive(Default)]
struct ControlHandler {
    steered: tokio::sync::Mutex<Vec<Message>>,
    follow_ups: tokio::sync::Mutex<Vec<Message>>,
    rebinds: tokio::sync::Mutex<Vec<(String, String)>>,
    controls: tokio::sync::Mutex<Vec<String>>,
    navigation_slices: tokio::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl PromptHandler for EchoHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        Ok(format!("handled:{}:{}", request.session_id, request.prompt))
    }

    async fn session_system_prompt(
        &self,
        _session_id: &str,
    ) -> Result<Option<String>, DaemonError> {
        Ok(Some("test system prompt".into()))
    }
}

#[async_trait]
impl PromptHandler for MultimodalHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        Ok(request.prompt)
    }

    async fn handle_prompt_message(
        &self,
        _request: PromptRequest,
        message: Message,
    ) -> Result<String, DaemonError> {
        self.received.lock().await.push(message);
        Ok("multimodal handled".into())
    }
}

#[async_trait]
impl PromptHandler for LiveEventHandler {
    async fn handle_prompt(&self, _request: PromptRequest) -> Result<String, DaemonError> {
        let assistant = Message::assistant(
            vec![mimir::model::Content::Text {
                text: "streamed".into(),
            }],
            mimir::model::StopReason::Stop,
        );
        self.events.publish(None, RuntimeEvent::RunStarted);
        self.events.publish(
            None,
            RuntimeEvent::MessageStarted {
                message: Message::assistant(
                    vec![mimir::model::Content::Text {
                        text: String::new(),
                    }],
                    mimir::model::StopReason::Stop,
                ),
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::TextDelta {
                text: "streamed".into(),
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::ToolStarted {
                id: "tool-1".into(),
                name: "read".into(),
                arguments: json!({"path": "README.md"}),
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::ToolFinished {
                id: "tool-1".into(),
                name: "read".into(),
                observation: ToolObservation {
                    status: ObservationStatus::Success,
                    summary: "read".into(),
                    next_actions: Vec::new(),
                    artifacts: Vec::new(),
                    content: "ok".into(),
                },
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::MessageCompleted {
                message: assistant.clone(),
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::TurnCompleted {
                message: assistant,
                tool_results: Vec::new(),
            },
        );
        self.events.publish(
            None,
            RuntimeEvent::Completed {
                text: "streamed".into(),
            },
        );
        Ok("streamed".into())
    }

    async fn subscribe_session_events(
        &self,
        _session_id: &str,
    ) -> Result<Option<tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>>, DaemonError> {
        Ok(Some(self.events.subscribe()))
    }
}

#[async_trait]
impl PromptHandler for ControlHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        Ok(request.prompt)
    }

    async fn steer_session(
        &self,
        _session_id: &str,
        message: Message,
        _command: &PublicDaemonCommand,
    ) -> Result<bool, DaemonError> {
        self.steered.lock().await.push(message);
        Ok(true)
    }

    async fn follow_up_session(
        &self,
        _session_id: &str,
        message: Message,
        _command: &PublicDaemonCommand,
    ) -> Result<Option<bool>, DaemonError> {
        self.follow_ups.lock().await.push(message);
        Ok(Some(true))
    }

    async fn rebind_session(
        &self,
        active_session_id: &str,
        durable_session_id: &str,
    ) -> Result<bool, DaemonError> {
        self.rebinds
            .lock()
            .await
            .push((active_session_id.into(), durable_session_id.into()));
        Ok(true)
    }

    async fn handle_session_control(
        &self,
        _session_id: &str,
        command: &PublicDaemonCommand,
    ) -> Result<Option<Value>, DaemonError> {
        self.controls
            .lock()
            .await
            .push(command.command_type().into());
        Ok(Some(match command.command_type() {
            "cycle_thinking_level" => json!({"level": "high"}),
            "compact" => json!({"summary": "compacted"}),
            _ => Value::Null,
        }))
    }

    async fn summarize_navigation(
        &self,
        _session_id: &str,
        _command: &PublicDaemonCommand,
        messages: Vec<Message>,
    ) -> Result<Option<String>, DaemonError> {
        self.navigation_slices
            .lock()
            .await
            .push(messages.into_iter().map(|message| message.text()).collect());
        Ok(Some("durable navigation summary".into()))
    }
}

fn config(root: &TempDir) -> DaemonConfig {
    DaemonConfig {
        state_root: root.path().join("state"),
        socket_path: root.path().join("daemon.sock"),
        server_name: "public-v4-test".into(),
        lease_ttl: Duration::from_secs(30),
        supported_capabilities: BTreeSet::new(),
    }
}

fn envelope(id: &str, command: Value) -> Value {
    envelope_for_client(id, "public-client", command)
}

fn envelope_for_client(id: &str, client_id: &str, command: Value) -> Value {
    let mut envelope = json!({
        "type": "command",
        "id": id,
        "protocol": {"name": PUBLIC_DAEMON_PROTOCOL_NAME, "version": 7},
        "clientId": client_id,
        "command": null
    });
    envelope["command"] = command;
    envelope
}

async fn write_frame(writer: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) {
    writer
        .write_all(value.to_string().as_bytes())
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_frame(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        !line.is_empty(),
        "daemon closed before returning a public frame"
    );
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one persistent socket transcript proves ordering across response, snapshot, execution, and unsupported frames"
)]
async fn public_socket_executes_core_commands_streams_attach_and_never_silences_unsupported() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);

    let hello = read_frame(&mut read).await;
    assert_eq!(hello["type"], "daemon_hello");
    assert_eq!(hello["protocol"]["version"], 7);
    assert_eq!(hello["schemaId"], "protocol-7-schema-13-816309b1cd50");
    assert_eq!(hello["schemaRevision"], 13);
    assert!(
        hello["serverCapabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("heartbeat_management"))
    );
    assert_eq!(
        hello["socketPath"],
        daemon_config.socket_path.to_string_lossy().as_ref()
    );

    write_frame(&mut write, &envelope("list", json!({"type": "list"}))).await;
    let listed = read_frame(&mut read).await;
    assert_eq!(listed["success"], true);
    assert_eq!(listed["data"]["sessions"], json!([]));

    write_frame(&mut write, &json!({"id": "bare-list", "type": "list"})).await;
    let bare_list = read_frame(&mut read).await;
    assert_eq!(bare_list["id"], "bare-list");
    assert_eq!(bare_list["success"], true);

    write_frame(
        &mut write,
        &envelope(
            "attach",
            json!({
                "type": "attach",
                "activeSessionId": "session-a",
                "capabilities": ["attach_snapshot", "event_sequence", "chunked_snapshot"]
            }),
        ),
    )
    .await;
    let attached = read_frame(&mut read).await;
    assert_eq!(attached["success"], true);
    assert_eq!(attached["data"]["activeSessionId"], "session-a");
    assert_eq!(attached["data"]["snapshot"]["messages"], json!([]));
    assert_eq!(attached["data"]["snapshotStream"]["messageCount"], 0);
    let begin = read_frame(&mut read).await;
    let end = read_frame(&mut read).await;
    assert_eq!(begin["type"], "session_snapshot_begin");
    assert!(begin["snapshot"].get("messages").is_none());
    assert_eq!(end["type"], "session_snapshot_end");
    assert_eq!(end["chunkCount"], 0);

    write_frame(
        &mut write,
        &envelope(
            "prompt",
            json!({"type": "prompt_and_wait", "activeSessionId": "session-a", "message": "hello"}),
        ),
    )
    .await;
    let prompted = read_frame(&mut read).await;
    assert_eq!(prompted["success"], true);
    assert_eq!(prompted["data"]["text"], "handled:session-a:hello");
    let event = read_frame(&mut read).await;
    assert_eq!(event["type"], "session_event");
    assert_eq!(event["activeSessionId"], "session-a");
    assert_eq!(event["event"]["type"], "turn_end");

    write_frame(
        &mut write,
        &envelope(
            "unsupported",
            json!({
                "type": "set_model",
                "activeSessionId": "session-a",
                "provider": "fake",
                "modelId": "fake/model"
            }),
        ),
    )
    .await;
    let unsupported = read_frame(&mut read).await;
    assert_eq!(unsupported["success"], false);
    assert_eq!(unsupported["errorInfo"]["code"], "unsupported_command");
    assert!(
        unsupported["error"]
            .as_str()
            .unwrap()
            .contains("recognized public daemon command 'set_model'")
    );

    write_frame(
        &mut write,
        &envelope(
            "unsupported-feature",
            json!({
                "type": "prompt",
                "activeSessionId": "session-a",
                "message": "do not silently steer",
                "streamingBehavior": "steer"
            }),
        ),
    )
    .await;
    let unsupported_feature = read_frame(&mut read).await;
    assert_eq!(unsupported_feature["success"], false);
    assert_eq!(
        unsupported_feature["errorInfo"]["code"],
        "unsupported_command_feature"
    );
    assert!(unsupported_feature.get("data").is_none());

    for (id, command) in [
        ("header", "get_session_header"),
        ("state", "get_state"),
        ("connection", "get_connection_state"),
        ("messages", "get_messages"),
        ("stats", "get_session_stats"),
        ("context-tree", "get_context_tree"),
        ("context", "get_session_context"),
        ("tree", "get_session_tree"),
        ("fork-messages", "get_user_messages_for_forking"),
        ("queue", "get_queue"),
        ("last", "get_last_assistant_text"),
        ("system", "get_system_prompt"),
    ] {
        write_frame(
            &mut write,
            &envelope(id, json!({"type": command, "activeSessionId": "session-a"})),
        )
        .await;
        let response = read_frame(&mut read).await;
        assert_eq!(response["success"], true, "{command}");
        assert!(response.get("data").is_some(), "{command}");
        match command {
            "get_session_header" => {
                assert_eq!(response["data"]["header"]["type"], "session");
                assert_eq!(response["data"]["header"]["id"], "session-a");
            }
            "get_state" => assert_eq!(response["data"]["activeSessionId"], "session-a"),
            "get_connection_state" => {
                assert_eq!(response["data"]["activeSessionId"], "session-a");
                assert_eq!(response["data"]["messageCount"], 2);
            }
            "get_messages" => assert_eq!(response["data"]["messages"].as_array().unwrap().len(), 2),
            "get_session_stats" => assert_eq!(response["data"]["totalMessages"], 2),
            "get_context_tree" => {
                assert_eq!(response["data"]["id"], "root");
                assert_eq!(response["data"]["status"], "active");
                assert_eq!(response["data"]["children"], json!([]));
            }
            "get_session_context" => {
                assert_eq!(
                    response["data"]["context"]["messages"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
                assert!(response["data"]["context"]["model"].is_null());
            }
            "get_session_tree" => {
                assert_eq!(response["data"]["flatNodes"].as_array().unwrap().len(), 2);
                assert!(response["data"]["leafId"].is_string());
            }
            "get_user_messages_for_forking" => {
                let messages = response["data"]["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0]["text"], "hello");
                assert!(messages[0]["entryId"].is_string());
            }
            "get_queue" => {
                assert_eq!(response["data"]["steering"], json!([]));
                assert_eq!(response["data"]["followUp"], json!([]));
            }
            "get_last_assistant_text" => {
                assert_eq!(response["data"]["text"], "handled:session-a:hello");
            }
            "get_system_prompt" => {
                assert_eq!(response["data"]["systemPrompt"], "test system prompt");
            }
            _ => unreachable!(),
        }
    }

    write_frame(
        &mut write,
        &envelope("shutdown", json!({"type": "shutdown"})),
    )
    .await;
    let shutdown = read_frame(&mut read).await;
    assert_eq!(shutdown["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn public_attach_reports_generation_resync_and_streams_a_recovery_snapshot() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);

    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-first",
            json!({"type": "attach", "activeSessionId": "resync"}),
        ),
    )
    .await;
    let first = read_frame(&mut read).await;
    assert_eq!(first["data"]["replay"]["status"], "complete");

    write_frame(
        &mut write,
        &envelope(
            "attach-stale",
            json!({
                "type": "attach",
                "activeSessionId": "resync",
                "capabilities": ["chunked_snapshot"],
                "resumeCursor": {"generation": "retired-generation", "sequence": 0}
            }),
        ),
    )
    .await;
    let stale = read_frame(&mut read).await;
    assert_eq!(stale["data"]["replay"]["status"], "unavailable");
    assert_eq!(
        stale["data"]["replay"]["reason"],
        "event_generation_changed"
    );
    assert_eq!(
        read_frame(&mut read).await["type"],
        "session_snapshot_begin"
    );
    assert_eq!(read_frame(&mut read).await["type"], "session_snapshot_end");

    write_frame(
        &mut write,
        &envelope("shutdown", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one restart transcript proves import, durable replay, acknowledgement, re-execution, and chunk ordering"
)]
async fn create_imports_a_durable_transcript_and_command_results_survive_restart_until_ack() {
    let root = TempDir::new().unwrap();
    let source = FileSessionStore::create(&root.path().join("source"), "imported")
        .await
        .unwrap();
    source
        .append(SessionRecord::new(SessionPayload::Message(Message::user(
            "durable source message",
        ))))
        .await
        .unwrap();
    let daemon_config = config(&root);

    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    let create = envelope(
        "durable-create",
        json!({
            "type": "create",
            "sessionPath": source.path(),
            "name": "original"
        }),
    );
    write_frame(&mut write, &create).await;
    let created = read_frame(&mut read).await;
    assert_eq!(created["success"], true);
    assert_eq!(created["data"]["activeSessionId"], "imported");
    assert_eq!(created["data"]["messageCount"], 1);
    assert_eq!(created["data"]["sessionName"], "original");

    write_frame(
        &mut write,
        &envelope("shutdown-first", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();

    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "durable-create",
            json!({"type": "create", "sessionPath": source.path(), "name": "changed"}),
        ),
    )
    .await;
    let replayed = read_frame(&mut read).await;
    assert_eq!(replayed, created);

    write_frame(
        &mut write,
        &envelope(
            "ack-create",
            json!({"type": "ack_result", "commandId": "durable-create"}),
        ),
    )
    .await;
    write_frame(
        &mut write,
        &envelope(
            "durable-create",
            json!({"type": "create", "sessionPath": source.path(), "name": "changed"}),
        ),
    )
    .await;
    let after_ack = read_frame(&mut read).await;
    assert_eq!(after_ack["success"], true);
    assert_eq!(after_ack["data"]["sessionName"], "changed");

    write_frame(
        &mut write,
        &envelope(
            "attach-imported",
            json!({
                "type": "attach",
                "activeSessionId": "imported",
                "capabilities": ["chunked_snapshot"]
            }),
        ),
    )
    .await;
    let attached = read_frame(&mut read).await;
    assert_eq!(attached["data"]["snapshotStream"]["messageCount"], 1);
    assert_eq!(
        read_frame(&mut read).await["type"],
        "session_snapshot_begin"
    );
    let chunk = read_frame(&mut read).await;
    assert_eq!(chunk["type"], "session_snapshot_chunk");
    assert_eq!(chunk["messages"][0]["role"], "user");
    assert_eq!(read_frame(&mut read).await["type"], "session_snapshot_end");

    write_frame(
        &mut write,
        &envelope("shutdown-second", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn client_owned_sessions_stay_out_of_the_public_catalog_and_reject_other_clients() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();

    let owner = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (owner_read, mut owner_write) = owner.into_split();
    let mut owner_read = BufReader::new(owner_read);
    assert_eq!(read_frame(&mut owner_read).await["type"], "daemon_hello");
    write_frame(
        &mut owner_write,
        &envelope_for_client(
            "create-private",
            "owner-client",
            json!({
                "type": "create",
                "activeSessionId": "private-session",
                "lifecycle": "client_owned"
            }),
        ),
    )
    .await;
    let created = read_frame(&mut owner_read).await;
    assert_eq!(created["success"], true);
    assert_eq!(created["data"]["lifecycle"], "client_owned");

    write_frame(
        &mut owner_write,
        &envelope_for_client("list-private", "owner-client", json!({"type": "list"})),
    )
    .await;
    assert_eq!(
        read_frame(&mut owner_read).await["data"]["sessions"],
        json!([])
    );

    let intruder = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (intruder_read, mut intruder_write) = intruder.into_split();
    let mut intruder_read = BufReader::new(intruder_read);
    assert_eq!(read_frame(&mut intruder_read).await["type"], "daemon_hello");
    write_frame(
        &mut intruder_write,
        &envelope_for_client(
            "attach-private-intruder",
            "intruder-client",
            json!({"type": "attach", "activeSessionId": "private-session"}),
        ),
    )
    .await;
    let denied = read_frame(&mut intruder_read).await;
    assert_eq!(denied["success"], false);
    assert!(
        denied["error"]
            .as_str()
            .unwrap()
            .contains("unknown session")
    );

    write_frame(
        &mut owner_write,
        &envelope_for_client(
            "attach-private-owner",
            "owner-client",
            json!({"type": "attach", "activeSessionId": "private-session"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut owner_read).await["success"], true);

    write_frame(
        &mut owner_write,
        &envelope_for_client(
            "shutdown-private",
            "owner-client",
            json!({"type": "shutdown"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut owner_read).await["success"], true);
    drop(owner_write);
    drop(intruder_write);
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn completed_turns_fan_out_to_every_attached_public_socket() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();

    let first = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let second = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (first_read, mut first_write) = first.into_split();
    let (second_read, mut second_write) = second.into_split();
    let mut first_read = BufReader::new(first_read);
    let mut second_read = BufReader::new(second_read);
    assert_eq!(read_frame(&mut first_read).await["type"], "daemon_hello");
    assert_eq!(read_frame(&mut second_read).await["type"], "daemon_hello");

    let attach = |id: &str| envelope(id, json!({"type": "attach", "activeSessionId": "shared"}));
    write_frame(&mut first_write, &attach("attach-first")).await;
    assert_eq!(read_frame(&mut first_read).await["success"], true);
    write_frame(&mut second_write, &attach("attach-second")).await;
    assert_eq!(read_frame(&mut second_read).await["success"], true);

    write_frame(
        &mut first_write,
        &envelope(
            "shared-prompt",
            json!({"type": "prompt", "activeSessionId": "shared", "message": "fan out"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut first_read).await["success"], true);
    let first_event = read_frame(&mut first_read).await;
    let second_event = read_frame(&mut second_read).await;
    assert_eq!(first_event["type"], "session_event");
    assert_eq!(second_event, first_event);
    assert_eq!(second_event["event"]["message"]["role"], "assistant");

    write_frame(
        &mut first_write,
        &envelope("shutdown-fanout", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut first_read).await["success"], true);
    drop(first_write);
    drop(second_write);
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn runtime_events_stream_as_sequenced_public_session_events_without_a_duplicate_turn() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(LiveEventHandler::default()))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-live-events",
            json!({"type": "attach", "activeSessionId": "live-events"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);

    write_frame(
        &mut write,
        &envelope(
            "prompt-live-events",
            json!({
                "type": "prompt",
                "activeSessionId": "live-events",
                "message": "stream"
            }),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["data"]["text"], "streamed");

    let mut event_types = Vec::new();
    let mut daemon_sequences = Vec::new();
    let mut runtime_sequences = Vec::new();
    for _ in 0..8 {
        let frame = read_frame(&mut read).await;
        assert_eq!(frame["type"], "session_event");
        event_types.push(frame["event"]["type"].as_str().unwrap().to_owned());
        daemon_sequences.push(frame["meta"]["sequence"].as_u64().unwrap());
        runtime_sequences.push(frame["meta"]["runtimeEventSequence"].as_u64().unwrap());
    }
    assert_eq!(
        event_types,
        [
            "agent_start",
            "message_start",
            "message_update",
            "tool_execution_start",
            "tool_execution_end",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
    assert!(daemon_sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(runtime_sequences.windows(2).all(|pair| pair[0] < pair[1]));

    write_frame(
        &mut write,
        &envelope("shutdown-live-events", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one socket transcript proves control routing, durable rebinding, and active-session deletion safety"
)]
async fn runtime_control_commands_reach_the_bound_session_handler() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handler = Arc::new(ControlHandler::default());
    let handle = DaemonServer::spawn(daemon_config.clone(), handler.clone())
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-controls",
            json!({"type": "attach", "activeSessionId": "controls"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);

    write_frame(
        &mut write,
        &envelope(
            "rich-follow-up",
            json!({
                "type": "follow_up",
                "activeSessionId": "controls",
                "message": "replayed",
                "content": [{"type":"text","text":"replayed content"}],
                "expandPromptTemplates": false,
                "queueKey": "status-refresh",
                "agentMessageId": "agent-message-1",
                "customMessage": {"role":"custom","customType":"agent","content":"metadata","display":true,"timestamp":1},
                "prefixMessages": [{"role":"custom","customType":"context","content":"prefix","display":false,"timestamp":1}]
            }),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);

    write_frame(
        &mut write,
        &envelope(
            "invalid-rich-steer",
            json!({
                "type": "steer",
                "activeSessionId": "controls",
                "message": "invalid replay",
                "customMessage": {"role":"custom","customType":"agent","content":"metadata","display":true,"timestamp":1}
            }),
        ),
    )
    .await;
    let invalid = read_frame(&mut read).await;
    assert_eq!(invalid["success"], false);
    assert!(
        invalid["error"]
            .as_str()
            .is_some_and(|error| error.contains("expandPromptTemplates=false"))
    );

    let commands = [
        json!({"type": "steer", "activeSessionId": "controls", "message": "redirect"}),
        json!({"type": "follow_up", "activeSessionId": "controls", "message": "after that"}),
        json!({"type": "abort", "activeSessionId": "controls"}),
        json!({"type": "clear_queue", "activeSessionId": "controls"}),
        json!({"type": "abort_and_clear_queue", "activeSessionId": "controls"}),
        json!({"type": "set_thinking_level", "activeSessionId": "controls", "level": "high"}),
        json!({"type": "set_service_tier", "activeSessionId": "controls", "serviceTier": "priority"}),
        json!({"type": "cycle_thinking_level", "activeSessionId": "controls"}),
        json!({"type": "set_steering_mode", "activeSessionId": "controls", "mode": "all"}),
        json!({"type": "set_follow_up_mode", "activeSessionId": "controls", "mode": "all"}),
        json!({"type": "set_auto_compaction", "activeSessionId": "controls", "enabled": false}),
        json!({"type": "set_auto_retry", "activeSessionId": "controls", "enabled": false}),
        json!({"type": "compact", "activeSessionId": "controls", "customInstructions": "brief"}),
        json!({"type": "abort_retry", "activeSessionId": "controls"}),
        json!({"type": "wait_for_idle", "activeSessionId": "controls"}),
        json!({"type": "wait_for_headless_completion", "activeSessionId": "controls"}),
        json!({"type": "execute_bash_and_wait", "activeSessionId": "controls", "command": "printf ok"}),
        json!({"type": "abort_bash", "activeSessionId": "controls"}),
        json!({"type": "set_model", "activeSessionId": "controls", "provider": "fake", "modelId": "fake/model"}),
        json!({"type": "cycle_model", "activeSessionId": "controls", "direction": "forward"}),
        json!({"type": "set_scoped_models", "activeSessionId": "controls", "scopedModels": []}),
        json!({"type": "set_transport", "activeSessionId": "controls", "transport": "sse"}),
        json!({"type": "extension_ui_response", "activeSessionId": "controls", "requestId": "ui-1", "response": {"confirmed": true}}),
    ];
    for (index, command) in commands.iter().enumerate() {
        write_frame(
            &mut write,
            &envelope(&format!("control-{index}"), command.clone()),
        )
        .await;
        let response = read_frame(&mut read).await;
        assert_eq!(response["success"], true, "{command}");
    }

    let steered = handler.steered.lock().await;
    assert_eq!(steered.len(), 1);
    assert_eq!(steered[0].text(), "redirect");
    drop(steered);
    let follow_ups = handler.follow_ups.lock().await;
    assert_eq!(follow_ups.len(), 2);
    assert_eq!(follow_ups[0].text(), "replayed content");
    assert_eq!(follow_ups[1].text(), "after that");
    drop(follow_ups);
    assert_eq!(
        handler.controls.lock().await.as_slice(),
        [
            "abort",
            "clear_queue",
            "abort_and_clear_queue",
            "set_thinking_level",
            "set_service_tier",
            "cycle_thinking_level",
            "set_steering_mode",
            "set_follow_up_mode",
            "set_auto_compaction",
            "set_auto_retry",
            "compact",
            "abort_retry",
            "wait_for_idle",
            "wait_for_headless_completion",
            "execute_bash_and_wait",
            "abort_bash",
            "set_model",
            "cycle_model",
            "set_scoped_models",
            "set_transport",
            "extension_ui_response"
        ]
    );

    write_frame(
        &mut write,
        &envelope(
            "new-session-control",
            json!({"type": "new_session", "activeSessionId": "controls"}),
        ),
    )
    .await;
    let replacement = read_frame(&mut read).await;
    assert_eq!(replacement["success"], true);
    let durable_session_id = replacement["data"]["sessionId"]
        .as_str()
        .expect("durable session id")
        .to_owned();
    let durable_session_file = replacement["data"]["sessionFile"]
        .as_str()
        .expect("durable session file")
        .to_owned();
    assert_ne!(durable_session_id, "controls");
    assert_eq!(
        handler.rebinds.lock().await.as_slice(),
        [("controls".into(), durable_session_id.clone())]
    );
    write_frame(
        &mut write,
        &envelope(
            "state-after-new-session",
            json!({"type": "get_state", "activeSessionId": "controls"}),
        ),
    )
    .await;
    let state = read_frame(&mut read).await;
    assert_eq!(state["data"]["activeSessionId"], "controls");
    assert_eq!(state["data"]["sessionId"], durable_session_id);
    write_frame(
        &mut write,
        &envelope(
            "header-after-new-session",
            json!({"type": "get_session_header", "activeSessionId": "controls"}),
        ),
    )
    .await;
    let header = read_frame(&mut read).await;
    assert_eq!(header["data"]["header"]["id"], durable_session_id);
    write_frame(
        &mut write,
        &envelope(
            "messages-after-new-session",
            json!({"type": "get_messages", "activeSessionId": "controls"}),
        ),
    )
    .await;
    let messages = read_frame(&mut read).await;
    assert_eq!(messages["data"]["messages"], json!([]));

    let store = FileSessionStore::create(&daemon_config.state_root, &durable_session_id)
        .await
        .expect("durable store");
    let shared_user = SessionRecord::new(SessionPayload::Message(Message::user("shared")));
    let shared_answer = SessionRecord::new(SessionPayload::Message(Message::assistant(
        vec![mimir::model::Content::Text {
            text: "shared answer".into(),
        }],
        mimir::model::StopReason::Stop,
    )))
    .with_parent(shared_user.record_id);
    let target = SessionRecord::new(SessionPayload::Message(Message::user("target sibling")))
        .with_parent(shared_answer.record_id);
    let abandoned_user =
        SessionRecord::new(SessionPayload::Message(Message::user("abandoned request")))
            .with_parent(shared_answer.record_id);
    let abandoned_answer = SessionRecord::new(SessionPayload::Message(Message::assistant(
        vec![mimir::model::Content::Text {
            text: "abandoned answer".into(),
        }],
        mimir::model::StopReason::Stop,
    )))
    .with_parent(abandoned_user.record_id);
    let target_id = target.record_id;
    for record in [
        shared_user,
        shared_answer,
        target,
        abandoned_user,
        abandoned_answer,
    ] {
        store.append(record).await.expect("branch record");
    }

    write_frame(
        &mut write,
        &envelope(
            "tree-before-navigation",
            json!({"type": "get_session_tree", "activeSessionId": "controls"}),
        ),
    )
    .await;
    let tree = read_frame(&mut read).await;
    assert_ne!(tree["data"]["leafId"], target_id.to_string());
    write_frame(
        &mut write,
        &envelope(
            "summarized-navigation",
            json!({
                "type": "navigate_tree",
                "activeSessionId": "controls",
                "targetId": target_id,
                "summarize": true,
                "customInstructions": "preserve decisions",
                "label": "checkpoint"
            }),
        ),
    )
    .await;
    let navigated = read_frame(&mut read).await;
    assert_eq!(navigated["success"], true);
    assert!(navigated["data"]["summaryEntryId"].is_string());
    assert_eq!(
        handler.navigation_slices.lock().await.as_slice(),
        [vec![
            String::from("abandoned request"),
            String::from("abandoned answer"),
        ]]
    );
    assert_eq!(
        handler.rebinds.lock().await.as_slice(),
        [
            ("controls".into(), durable_session_id.clone()),
            ("controls".into(), durable_session_id.clone())
        ]
    );

    write_frame(
        &mut write,
        &envelope(
            "delete-bound-session",
            json!({
                "type": "delete_saved_session",
                "sessionPath": durable_session_file
            }),
        ),
    )
    .await;
    let delete_bound = read_frame(&mut read).await;
    assert_eq!(delete_bound["success"], false);
    assert!(
        delete_bound["error"]
            .as_str()
            .expect("delete error")
            .contains("bound to an active daemon session")
    );

    write_frame(
        &mut write,
        &envelope("shutdown-controls", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn update_restart_commands_checkpoint_retry_and_stop_with_state_preserved() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handler = Arc::new(ControlHandler::default());
    let handle = DaemonServer::spawn(daemon_config.clone(), handler.clone())
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-restart",
            json!({"type": "attach", "activeSessionId": "restartable"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    write_frame(
        &mut write,
        &envelope(
            "persist-restart",
            json!({"type": "new_session", "activeSessionId": "restartable"}),
        ),
    )
    .await;
    let created = read_frame(&mut read).await;
    assert_eq!(created["success"], true);

    write_frame(
        &mut write,
        &envelope("prepare-restart", json!({"type": "prepare_update_restart"})),
    )
    .await;
    let prepared = read_frame(&mut read).await;
    assert_eq!(prepared["success"], true);
    assert_eq!(prepared["data"]["formatVersion"], 1);
    assert_eq!(
        prepared["data"]["sessions"][0]["activeSessionId"],
        "restartable"
    );
    let manifest_path = daemon_config.state_root.join("daemon-update-restart.json");
    let manifest: Value = serde_json::from_slice(
        &tokio::fs::read(&manifest_path)
            .await
            .expect("restart manifest"),
    )
    .expect("valid restart manifest");
    assert_eq!(manifest, prepared["data"]);

    write_frame(
        &mut write,
        &envelope(
            "retry-worker",
            json!({"type": "retry_worker", "activeSessionId": "restartable"}),
        ),
    )
    .await;
    let retried = read_frame(&mut read).await;
    assert_eq!(retried["success"], true);
    assert_eq!(retried["data"]["activeSessionId"], "restartable");
    assert_eq!(
        handler.controls.lock().await.as_slice(),
        ["wait_for_idle", "reload"]
    );

    write_frame(&mut write, &envelope("restart", json!({"type": "restart"}))).await;
    let restarted = read_frame(&mut read).await;
    assert_eq!(restarted["success"], true);
    assert_eq!(restarted["data"]["restartRequired"], true);
    assert_eq!(restarted["data"]["statePreserved"], true);
    drop(write);
    handle.wait().await.unwrap();
    assert!(tokio::fs::try_exists(manifest_path).await.unwrap());
}

#[tokio::test]
async fn public_prompt_images_reach_the_multimodal_prompt_handler() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handler = Arc::new(MultimodalHandler::default());
    let handle = DaemonServer::spawn(daemon_config.clone(), handler.clone())
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-images",
            json!({"type": "attach", "activeSessionId": "images"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    write_frame(
        &mut write,
        &envelope(
            "prompt-images",
            json!({
                "type": "prompt",
                "activeSessionId": "images",
                "message": "inspect",
                "images": [{"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}]
            }),
        ),
    )
    .await;
    let response = read_frame(&mut read).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["text"], "multimodal handled");
    assert_eq!(read_frame(&mut read).await["type"], "session_event");

    let received = handler.received.lock().await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].content.len(), 2);
    assert!(matches!(
        received[0].content[0],
        mimir::model::Content::Text { .. }
    ));
    assert!(matches!(
        received[0].content[1],
        mimir::model::Content::Image { .. }
    ));
    drop(received);

    write_frame(
        &mut write,
        &envelope("shutdown-images", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one persistent socket transcript proves the complete cron and heartbeat lifecycle"
)]
async fn public_schedule_commands_manage_the_durable_schedule_store() {
    let root = TempDir::new().unwrap();
    let daemon_config = config(&root);
    let handle = DaemonServer::spawn(daemon_config.clone(), Arc::new(EchoHandler))
        .await
        .unwrap();
    let stream = tokio::net::UnixStream::connect(&daemon_config.socket_path)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    assert_eq!(read_frame(&mut read).await["type"], "daemon_hello");

    write_frame(
        &mut write,
        &envelope(
            "attach-scheduled",
            json!({"type": "attach", "activeSessionId": "scheduled"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);

    write_frame(
        &mut write,
        &envelope(
            "cron-add",
            json!({
                "type": "cron_add",
                "activeSessionId": "scheduled",
                "schedule": "every 1h",
                "prompt": "check status"
            }),
        ),
    )
    .await;
    let added = read_frame(&mut read).await;
    assert_eq!(added["success"], true);
    assert_eq!(added["data"]["job"]["status"], "active");
    assert_eq!(added["data"]["job"]["source"], "cron");
    assert_eq!(added["data"]["job"]["activeSessionId"], "scheduled");
    let cron_id = added["data"]["job"]["id"].as_str().unwrap();

    write_frame(
        &mut write,
        &envelope(
            "cron-list",
            json!({"type": "cron_list", "activeSessionId": "scheduled"}),
        ),
    )
    .await;
    let listed = read_frame(&mut read).await;
    assert_eq!(listed["data"]["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["jobs"][0]["id"], cron_id);

    write_frame(
        &mut write,
        &envelope(
            "cron-cancel",
            json!({"type": "cron_cancel", "jobId": cron_id}),
        ),
    )
    .await;
    assert_eq!(
        read_frame(&mut read).await["data"]["job"]["status"],
        "cancelled"
    );

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-set",
            json!({
                "type": "heartbeat_set",
                "activeSessionId": "scheduled",
                "schedule": "every 5m",
                "prompt": "still alive?",
                "deliveryMode": "follow_up"
            }),
        ),
    )
    .await;
    let heartbeat = read_frame(&mut read).await;
    assert_eq!(heartbeat["data"]["heartbeat"]["status"], "active");
    assert_eq!(heartbeat["data"]["heartbeat"]["deliveryMode"], "follow_up");
    let heartbeat_id = heartbeat["data"]["heartbeat"]["id"].as_str().unwrap();

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-list",
            json!({"type": "heartbeats_list", "activeSessionId": "scheduled"}),
        ),
    )
    .await;
    let listed = read_frame(&mut read).await;
    assert_eq!(listed["data"]["heartbeats"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["heartbeats"][0]["job"]["id"], heartbeat_id);

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-pause",
            json!({
                "type": "heartbeat_manage",
                "activeSessionId": "scheduled",
                "jobId": heartbeat_id,
                "action": "pause"
            }),
        ),
    )
    .await;
    assert_eq!(
        read_frame(&mut read).await["data"]["heartbeat"]["status"],
        "paused"
    );

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-resume",
            json!({
                "type": "heartbeat_update",
                "activeSessionId": "scheduled",
                "action": "resume"
            }),
        ),
    )
    .await;
    assert_eq!(
        read_frame(&mut read).await["data"]["heartbeat"]["status"],
        "active"
    );

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-get",
            json!({"type": "heartbeat_get", "activeSessionId": "scheduled"}),
        ),
    )
    .await;
    assert_eq!(
        read_frame(&mut read).await["data"]["heartbeat"]["id"],
        heartbeat_id
    );

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-clear",
            json!({
                "type": "heartbeat_update",
                "activeSessionId": "scheduled",
                "action": "clear"
            }),
        ),
    )
    .await;
    assert_eq!(
        read_frame(&mut read).await["data"]["heartbeat"]["status"],
        "cancelled"
    );

    write_frame(
        &mut write,
        &envelope(
            "heartbeat-list-empty",
            json!({"type": "heartbeats_list", "activeSessionId": "scheduled"}),
        ),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["data"]["heartbeats"], json!([]));

    write_frame(
        &mut write,
        &envelope("shutdown-schedules", json!({"type": "shutdown"})),
    )
    .await;
    assert_eq!(read_frame(&mut read).await["success"], true);
    drop(write);
    handle.wait().await.unwrap();
}
