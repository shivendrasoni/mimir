#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use mimir::daemon::{
    AgentMessageDelivery, AgentMessageRequest, ClientRequest, DaemonClient, DaemonConfig,
    DaemonError, DaemonEventCursor, DaemonHarness, DaemonReplayStatus, DaemonServer,
    DaemonSessionEventKind, DaemonStateStore, PromptHandler, PromptRequest,
    ScheduledPromptDelivery, ServerResponse,
};
use mimir::orchestration::{HeartbeatDeliveryMode, ScheduleStore};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, Notify},
};

struct EchoHandler;

#[derive(Default)]
struct RecordingHandler {
    prompts: Mutex<Vec<(String, String)>>,
}

#[derive(Default)]
struct BlockingHandler {
    started: Notify,
    completed: AtomicBool,
}

#[derive(Default)]
struct DeliveryRecordingHandler {
    prompts: Mutex<Vec<(String, ScheduledPromptDelivery)>>,
}

#[derive(Default)]
struct AgentMessageRecordingHandler {
    requests: Mutex<Vec<AgentMessageRequest>>,
    pending_by_session: Mutex<std::collections::BTreeMap<String, usize>>,
}

struct RejectingAgentMessageHandler;

#[async_trait]
impl PromptHandler for EchoHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        Ok(format!("handled:{}:{}", request.session_id, request.prompt))
    }
}

#[async_trait]
impl PromptHandler for RecordingHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        self.prompts
            .lock()
            .await
            .push((request.session_id, request.prompt));
        Ok("scheduled".into())
    }
}

#[async_trait]
impl PromptHandler for BlockingHandler {
    async fn handle_prompt(&self, _request: PromptRequest) -> Result<String, DaemonError> {
        self.started.notify_one();
        tokio::time::sleep(Duration::from_mins(1)).await;
        self.completed.store(true, Ordering::SeqCst);
        Ok("unexpected completion".into())
    }
}

#[async_trait]
impl PromptHandler for DeliveryRecordingHandler {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError> {
        self.prompts
            .lock()
            .await
            .push((request.prompt, ScheduledPromptDelivery::NewTurn));
        Ok("prompted".into())
    }

    async fn handle_scheduled_prompt(
        &self,
        request: PromptRequest,
        delivery: ScheduledPromptDelivery,
    ) -> Result<String, DaemonError> {
        self.prompts.lock().await.push((request.prompt, delivery));
        Ok("scheduled".into())
    }
}

#[async_trait]
impl PromptHandler for AgentMessageRecordingHandler {
    async fn handle_prompt(&self, _request: PromptRequest) -> Result<String, DaemonError> {
        Ok("unused".into())
    }

    async fn handle_agent_message(
        &self,
        request: AgentMessageRequest,
    ) -> Result<AgentMessageDelivery, DaemonError> {
        self.requests.lock().await.push(request.clone());
        let mut pending = self.pending_by_session.lock().await;
        *pending.entry(request.target_session_id).or_default() += 1;
        Ok(AgentMessageDelivery {
            queued: true,
            target_session_name: Some("Target".into()),
        })
    }

    async fn clear_agent_messages(&self, session_id: &str) -> Result<usize, DaemonError> {
        Ok(self
            .pending_by_session
            .lock()
            .await
            .remove(session_id)
            .unwrap_or(0))
    }

    async fn clear_all_agent_messages(&self) -> Result<usize, DaemonError> {
        let mut pending = self.pending_by_session.lock().await;
        let cleared = pending.values().sum();
        pending.clear();
        Ok(cleared)
    }
}

#[async_trait]
impl PromptHandler for RejectingAgentMessageHandler {
    async fn handle_prompt(&self, _request: PromptRequest) -> Result<String, DaemonError> {
        Ok("unused".into())
    }

    async fn handle_agent_message(
        &self,
        _request: AgentMessageRequest,
    ) -> Result<AgentMessageDelivery, DaemonError> {
        Err(DaemonError::Protocol(
            "provider failed with sensitive-handler-detail".into(),
        ))
    }
}

#[cfg(unix)]
fn config(root: &TempDir) -> DaemonConfig {
    DaemonConfig {
        state_root: root.path().join("state"),
        socket_path: root.path().join("daemon.sock"),
        server_name: "mimir-test".into(),
        lease_ttl: Duration::from_millis(400),
        supported_capabilities: BTreeSet::from([
            "health".into(),
            "prompt".into(),
            "session_catalog".into(),
            "shutdown".into(),
        ]),
    }
}

#[cfg(unix)]
async fn spawn(root: &TempDir) -> (DaemonHarness, DaemonStateStore) {
    let config = config(root);
    let store = DaemonStateStore::new(config.state_root.clone());
    let harness = DaemonHarness::start(config, Arc::new(EchoHandler))
        .await
        .expect("daemon should start");
    (harness, store)
}

#[cfg(unix)]
#[tokio::test]
async fn negotiate_health_and_prompt_round_trip_over_json_lines() {
    let root = TempDir::new().expect("tempdir");
    let (harness, store) = spawn(&root).await;

    let negotiated = harness
        .request(ClientRequest::negotiate(
            "cli-a",
            ["prompt", "health", "unknown"],
        ))
        .await
        .expect("negotiate");
    let ServerResponse::Negotiated(hello) = negotiated else {
        panic!("unexpected response");
    };
    assert_eq!(hello.schema_version, mimir::daemon::IPC_SCHEMA_VERSION);
    assert_eq!(hello.server_name, "mimir-test");
    assert_eq!(
        hello.negotiated_capabilities,
        vec!["health".to_string(), "prompt".to_string()]
    );

    let attached = harness
        .request(ClientRequest::attach("session-a", "cli-a"))
        .await
        .expect("attach");
    let lease = match attached {
        ServerResponse::SessionAttached(attached) => attached.lease,
        other => panic!("unexpected attach response: {other:?}"),
    };

    let prompt = harness
        .request(ClientRequest::prompt(
            &lease.lease_id.to_string(),
            "session-a",
            "hello world",
        ))
        .await
        .expect("prompt");
    let ServerResponse::PromptCompleted(result) = prompt else {
        panic!("unexpected prompt response");
    };
    assert_eq!(result.output, "handled:session-a:hello world");

    let health = harness
        .request(ClientRequest::Health)
        .await
        .expect("health");
    let ServerResponse::Health(status) = health else {
        panic!("unexpected health response");
    };
    assert_eq!(status.active_sessions, 1);
    assert_eq!(status.active_leases, 1);
    assert_eq!(status.launch_count, 1);

    let snapshot = store.load().await.expect("metadata");
    assert_eq!(snapshot.sessions.len(), 1);
    assert!(
        snapshot
            .sessions
            .get("session-a")
            .expect("session entry")
            .last_prompt_at_ms
            .is_some()
    );

    let shutdown = harness
        .request(ClientRequest::Shutdown)
        .await
        .expect("shutdown");
    assert!(matches!(shutdown, ServerResponse::ShutdownAccepted(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn heartbeats_extend_leases_and_detach_keeps_catalog_history() {
    let root = TempDir::new().expect("tempdir");
    let (harness, store) = spawn(&root).await;

    let lease_a = match harness
        .request(ClientRequest::attach("session-shared", "cli-a"))
        .await
        .expect("attach a")
    {
        ServerResponse::SessionAttached(attached) => attached.lease,
        other => panic!("unexpected attach response: {other:?}"),
    };
    let lease_b = match harness
        .request(ClientRequest::attach("session-shared", "cli-b"))
        .await
        .expect("attach b")
    {
        ServerResponse::SessionAttached(attached) => attached.lease,
        other => panic!("unexpected attach response: {other:?}"),
    };
    assert_ne!(lease_a.lease_id, lease_b.lease_id);

    tokio::time::sleep(Duration::from_millis(120)).await;
    let heartbeat = harness
        .request(ClientRequest::heartbeat(&lease_a.lease_id.to_string()))
        .await
        .expect("heartbeat");
    let ServerResponse::LeaseRenewed(renewed) = heartbeat else {
        panic!("unexpected heartbeat response");
    };
    assert!(renewed.lease.expires_at_ms > lease_a.expires_at_ms);

    let detached = harness
        .request(ClientRequest::detach(&lease_b.lease_id.to_string()))
        .await
        .expect("detach");
    let ServerResponse::SessionDetached(detached) = detached else {
        panic!("unexpected detach response");
    };
    assert_eq!(detached.session.active_leases, 1);

    let snapshot = store.load().await.expect("snapshot");
    let session = snapshot.sessions.get("session-shared").expect("session");
    assert_eq!(session.active_leases.len(), 1);
    assert!(session.last_detached_at_ms.is_some());

    let shutdown = harness
        .request(ClientRequest::Shutdown)
        .await
        .expect("shutdown");
    assert!(matches!(shutdown, ServerResponse::ShutdownAccepted(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn restart_recovers_persisted_metadata_and_cleans_expired_leases() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    let store = DaemonStateStore::new(config.state_root.clone());

    let first = DaemonHarness::start(config.clone(), Arc::new(EchoHandler))
        .await
        .expect("first start");
    let lease = match first
        .request(ClientRequest::attach("session-restart", "cli-a"))
        .await
        .expect("attach")
    {
        ServerResponse::SessionAttached(attached) => attached.lease,
        other => panic!("unexpected attach response: {other:?}"),
    };
    assert!(!lease.lease_id.is_nil());
    let shutdown = first
        .request(ClientRequest::Shutdown)
        .await
        .expect("first shutdown");
    assert!(matches!(shutdown, ServerResponse::ShutdownAccepted(_)));

    tokio::time::sleep(Duration::from_millis(500)).await;

    let second = DaemonHarness::start(config, Arc::new(EchoHandler))
        .await
        .expect("restart");
    let health = second.request(ClientRequest::Health).await.expect("health");
    let ServerResponse::Health(status) = health else {
        panic!("unexpected health response");
    };
    assert_eq!(status.launch_count, 2);
    assert_eq!(status.active_leases, 0);
    assert_eq!(status.active_sessions, 0);

    let snapshot = store.load().await.expect("metadata");
    assert_eq!(snapshot.launch_count, 2);
    assert_eq!(
        snapshot
            .sessions
            .get("session-restart")
            .expect("session")
            .active_leases
            .len(),
        0
    );

    let shutdown = second
        .request(ClientRequest::Shutdown)
        .await
        .expect("second shutdown");
    assert!(matches!(shutdown, ServerResponse::ShutdownAccepted(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn attach_snapshot_replays_retained_session_events_from_a_monotonic_cursor() {
    let root = TempDir::new().expect("tempdir");
    let harness = DaemonHarness::start(config(&root), Arc::new(EchoHandler))
        .await
        .expect("daemon should start");

    let negotiated = harness
        .request(ClientRequest::negotiate(
            "reconnect-client",
            ["event_sequence", "attach_snapshot"],
        ))
        .await
        .expect("reconnect capabilities");
    let ServerResponse::Negotiated(negotiated) = negotiated else {
        panic!("unexpected negotiation response");
    };
    assert_eq!(
        negotiated.negotiated_capabilities,
        vec!["attach_snapshot".to_string(), "event_sequence".to_string()]
    );

    let first = harness
        .request(ClientRequest::attach("reconnect", "client-a"))
        .await
        .expect("first attach");
    let ServerResponse::SessionAttached(first) = first else {
        panic!("unexpected first attach response");
    };
    assert_eq!(first.snapshot.cursor.sequence, 0);
    assert!(!first.snapshot.cursor.generation.is_empty());
    assert_eq!(first.replay.status, DaemonReplayStatus::Complete);
    assert!(first.replay.events.is_empty());
    assert!(!first.replay.resync_required);

    harness
        .request(ClientRequest::prompt(
            &first.lease.lease_id.to_string(),
            "reconnect",
            "first turn",
        ))
        .await
        .expect("prompt");

    let second = harness
        .request(ClientRequest::attach_from(
            "reconnect",
            "client-b",
            first.snapshot.cursor.clone(),
        ))
        .await
        .expect("reconnect attach");
    let ServerResponse::SessionAttached(second) = second else {
        panic!("unexpected reconnect response");
    };
    assert_eq!(second.snapshot.cursor.sequence, 1);
    assert_eq!(second.replay.status, DaemonReplayStatus::Complete);
    assert_eq!(second.replay.from_cursor, Some(first.snapshot.cursor));
    assert_eq!(second.replay.to_cursor, second.snapshot.cursor);
    assert_eq!(second.replay.events.len(), 1);
    assert_eq!(second.replay.events[0].cursor.sequence, 1);
    assert_eq!(
        second.replay.events[0].event,
        DaemonSessionEventKind::PromptCompleted
    );
    assert!(!second.replay.resync_required);

    let caught_up = harness
        .request(ClientRequest::attach_from(
            "reconnect",
            "client-c",
            second.snapshot.cursor.clone(),
        ))
        .await
        .expect("caught-up attach");
    let ServerResponse::SessionAttached(caught_up) = caught_up else {
        panic!("unexpected caught-up response");
    };
    assert_eq!(caught_up.replay.status, DaemonReplayStatus::Complete);
    assert!(caught_up.replay.events.is_empty());
    assert!(!caught_up.replay.resync_required);
}

#[cfg(unix)]
#[tokio::test]
async fn attach_snapshot_requires_resync_for_stale_or_ahead_cursors() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    let first = DaemonHarness::start(config.clone(), Arc::new(EchoHandler))
        .await
        .expect("first daemon");
    let attached = first
        .request(ClientRequest::attach("generation", "client-a"))
        .await
        .expect("attach");
    let ServerResponse::SessionAttached(attached) = attached else {
        panic!("unexpected attach response");
    };
    let old_cursor = attached.snapshot.cursor;

    let wrong_session = first
        .request(ClientRequest::attach_from(
            "other-generation",
            "wrong-session-client",
            old_cursor.clone(),
        ))
        .await
        .expect("wrong-session cursor returns a resync snapshot");
    let ServerResponse::SessionAttached(wrong_session) = wrong_session else {
        panic!("unexpected wrong-session response");
    };
    assert_eq!(wrong_session.replay.status, DaemonReplayStatus::Unavailable);
    assert_eq!(
        wrong_session.replay.reason.as_deref(),
        Some("event_generation_changed")
    );
    assert!(wrong_session.replay.resync_required);
    assert_ne!(
        wrong_session.snapshot.cursor.generation,
        old_cursor.generation
    );

    let ahead = first
        .request(ClientRequest::attach_from(
            "generation",
            "ahead-client",
            DaemonEventCursor {
                generation: old_cursor.generation.clone(),
                sequence: 99,
            },
        ))
        .await
        .expect("ahead attach returns snapshot");
    let ServerResponse::SessionAttached(ahead) = ahead else {
        panic!("unexpected ahead response");
    };
    assert_eq!(ahead.replay.status, DaemonReplayStatus::Unavailable);
    assert_eq!(
        ahead.replay.reason.as_deref(),
        Some("resume_cursor_ahead_of_session")
    );
    assert!(ahead.replay.resync_required);
    assert!(ahead.replay.events.is_empty());

    first
        .request(ClientRequest::Shutdown)
        .await
        .expect("first shutdown");
    let second = DaemonHarness::start(config, Arc::new(EchoHandler))
        .await
        .expect("replacement daemon");
    let stale = second
        .request(ClientRequest::attach_from(
            "generation",
            "client-b",
            old_cursor,
        ))
        .await
        .expect("stale attach returns snapshot");
    let ServerResponse::SessionAttached(stale) = stale else {
        panic!("unexpected stale response");
    };
    assert_eq!(stale.replay.status, DaemonReplayStatus::Unavailable);
    assert_eq!(
        stale.replay.reason.as_deref(),
        Some("event_generation_changed")
    );
    assert!(stale.replay.resync_required);
    assert!(stale.replay.events.is_empty());
    assert_ne!(
        stale.snapshot.cursor.generation,
        stale.replay.from_cursor.unwrap().generation
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconnect_history_is_bounded_and_reports_partial_replay_after_truncation() {
    let root = TempDir::new().expect("tempdir");
    let mut daemon_config = config(&root);
    daemon_config.lease_ttl = Duration::from_secs(30);
    let harness = DaemonHarness::start(daemon_config, Arc::new(EchoHandler))
        .await
        .expect("daemon should start");
    let attached = harness
        .request(ClientRequest::attach("bounded", "client-a"))
        .await
        .expect("attach");
    let ServerResponse::SessionAttached(attached) = attached else {
        panic!("unexpected attach response");
    };

    for index in 0..257 {
        harness
            .request(ClientRequest::prompt(
                &attached.lease.lease_id.to_string(),
                "bounded",
                &format!("turn {index}"),
            ))
            .await
            .expect("bounded history prompt");
    }

    let replay = harness
        .request(ClientRequest::attach_from(
            "bounded",
            "client-b",
            attached.snapshot.cursor,
        ))
        .await
        .expect("truncated replay attach");
    let ServerResponse::SessionAttached(replay) = replay else {
        panic!("unexpected replay response");
    };
    assert_eq!(replay.snapshot.cursor.sequence, 257);
    assert_eq!(replay.replay.status, DaemonReplayStatus::Partial);
    assert_eq!(replay.replay.events.len(), 256);
    assert_eq!(replay.replay.events[0].cursor.sequence, 2);
    assert_eq!(replay.replay.events[255].cursor.sequence, 257);
    assert_eq!(
        replay.replay.reason.as_deref(),
        Some("event_history_truncated")
    );
    assert!(replay.replay.resync_required);
}

#[test]
fn legacy_attach_wire_without_a_resume_cursor_stays_compatible() {
    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "attach_session",
        "data": {
            "session_id": "legacy-session",
            "client_name": "legacy-client"
        }
    }))
    .expect("legacy attach wire should deserialize");
    assert_eq!(
        request,
        ClientRequest::attach("legacy-session", "legacy-client")
    );
    assert_eq!(
        serde_json::to_value(&request).expect("serialize legacy attach"),
        serde_json::json!({
            "type": "attach_session",
            "data": {
                "session_id": "legacy-session",
                "client_name": "legacy-client"
            }
        })
    );

    let response: ServerResponse = serde_json::from_value(serde_json::json!({
        "type": "session_attached",
        "data": {
            "session": {
                "session_id": "legacy-session",
                "active_leases": 1,
                "last_prompt_at_ms": null
            },
            "lease": {
                "lease_id": "00000000-0000-0000-0000-000000000000",
                "client_name": "legacy-client",
                "attached_at_ms": 1,
                "expires_at_ms": 2
            }
        }
    }))
    .expect("legacy attach response should deserialize");
    let ServerResponse::SessionAttached(response) = response else {
        panic!("unexpected legacy attach response");
    };
    assert_eq!(response.lease.client_name, "legacy-client");
    assert_eq!(response.session.session_id, "legacy-session");
    assert_eq!(response.snapshot.cursor, DaemonEventCursor::default());
    assert_eq!(response.replay.status, DaemonReplayStatus::Complete);
}

#[cfg(unix)]
#[tokio::test]
async fn startup_refuses_to_unlink_a_non_socket_path() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    std::fs::write(&config.socket_path, "stale").expect("stale file");
    assert!(config.socket_path.exists());

    let error = DaemonServer::prepare_socket_path_for_test(&config.socket_path)
        .await
        .expect_err("regular files must fail closed");
    assert!(error.to_string().contains("non-socket"));
    assert!(config.socket_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn due_schedules_execute_against_the_recorded_session() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    tokio::fs::create_dir_all(&config.state_root)
        .await
        .expect("state root");
    let schedules = ScheduleStore::new(&config.state_root);
    schedules
        .add(
            "heartbeat",
            "scheduled_session",
            "continue the task",
            Utc::now(),
            None,
        )
        .await
        .expect("schedule");
    let handler = Arc::new(RecordingHandler::default());
    let harness = DaemonHarness::start(config, handler.clone())
        .await
        .expect("daemon should start");

    harness
        .run_due_schedules_for_test()
        .await
        .expect("schedule pump");

    let prompts = handler.prompts.lock().await.clone();
    assert_eq!(
        prompts,
        vec![("scheduled_session".into(), "continue the task".into())]
    );
    assert!(
        schedules
            .list()
            .await
            .expect("load schedules")
            .iter()
            .all(|schedule| !schedule.enabled)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn heartbeat_delivery_modes_reach_daemon_dispatch() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    tokio::fs::create_dir_all(&config.state_root)
        .await
        .expect("state root");
    let schedules = ScheduleStore::new(&config.state_root);
    let now = Utc::now();
    schedules
        .add("cron", "cron-session", "cron", now, None)
        .await
        .expect("cron schedule");
    schedules
        .set_heartbeat(
            "steer",
            "steer-session",
            "steer",
            "every 10s",
            Some(HeartbeatDeliveryMode::Steer),
            now - chrono::Duration::seconds(11),
        )
        .await
        .expect("steer heartbeat");
    schedules
        .set_heartbeat(
            "follow-up",
            "follow-session",
            "follow-up",
            "every 10s",
            Some(HeartbeatDeliveryMode::FollowUp),
            now - chrono::Duration::seconds(11),
        )
        .await
        .expect("follow-up heartbeat");
    let handler = Arc::new(DeliveryRecordingHandler::default());
    let harness = DaemonHarness::start(config, handler.clone())
        .await
        .expect("daemon should start");

    harness
        .run_due_schedules_for_test()
        .await
        .expect("schedule pump");

    let prompts = handler.prompts.lock().await;
    assert!(prompts.contains(&("cron".into(), ScheduledPromptDelivery::NewTurn)));
    assert!(prompts.contains(&("steer".into(), ScheduledPromptDelivery::Steer)));
    assert!(prompts.contains(&("follow-up".into(), ScheduledPromptDelivery::FollowUp)));
}

#[cfg(unix)]
async fn attach_message_sessions(harness: &DaemonHarness) {
    for session_id in ["source", "target"] {
        harness
            .request(ClientRequest::attach(session_id, "message-test"))
            .await
            .expect("message session should attach");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn agent_messages_use_the_reference_prompt_envelope_and_receipt_shape() {
    let root = TempDir::new().expect("tempdir");
    let handler = Arc::new(AgentMessageRecordingHandler::default());
    let harness = DaemonHarness::start(config(&root), handler.clone())
        .await
        .expect("daemon should start");
    attach_message_sessions(&harness).await;

    let response = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: "  hello from source  ".into(),
        })
        .await
        .expect("message should be admitted");
    let ServerResponse::AgentMessageSent(receipt) = response else {
        panic!("unexpected send response");
    };

    assert!(receipt.id.starts_with("agentmsg_"));
    assert_eq!(receipt.source, "agent_message");
    assert_eq!(receipt.message, "hello from source");
    assert_eq!(receipt.delivery_status, "queued");
    assert_eq!(receipt.delivery_mode, "steer");
    assert!(receipt.delivered_at.is_none());
    assert!(receipt.queued_at.is_some());
    assert_eq!(receipt.target.active_session_id, "target");
    assert_eq!(receipt.target.session_name.as_deref(), Some("Target"));
    assert_eq!(receipt.from.active_session_id, "source");

    let requests = handler.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].prompt,
        format!(
            "Agent-to-agent message received.\nSource: agent_message\nFrom: active source, session source, client mimir-rpc\nTo: active target, session target\nMessage id: {}\n\nhello from source",
            receipt.id
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn agent_message_safety_limits_are_bounded_and_fail_closed() {
    let root = TempDir::new().expect("tempdir");
    let handler = Arc::new(AgentMessageRecordingHandler::default());
    let harness = DaemonHarness::start(config(&root), handler.clone())
        .await
        .expect("daemon should start");
    attach_message_sessions(&harness).await;

    let self_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "source".into(),
            message: "self".into(),
        })
        .await
        .expect_err("self-targeting must fail");
    assert!(self_error.to_string().contains("sending session"));

    let unknown_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "missing".into(),
            message: "do-not-leak-this-secret".into(),
        })
        .await
        .expect_err("unknown targets must fail before delivery");
    assert!(unknown_error.to_string().contains("unknown target session"));
    assert!(
        !unknown_error
            .to_string()
            .contains("do-not-leak-this-secret")
    );

    let overlong = "é".repeat(16_385);
    let length_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: overlong,
        })
        .await
        .expect_err("the character limit must be enforced");
    assert!(
        length_error
            .to_string()
            .contains("16385 chars exceeds 16384")
    );
    let utf16_length_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: "🦀".repeat(8_193),
        })
        .await
        .expect_err("the reference UTF-16 character count must be enforced");
    assert!(
        utf16_length_error
            .to_string()
            .contains("16386 chars exceeds 16384")
    );
    assert!(handler.requests.lock().await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn agent_message_rate_pause_resume_and_clear_are_enforced() {
    let root = TempDir::new().expect("tempdir");
    let handler = Arc::new(AgentMessageRecordingHandler::default());
    let harness = DaemonHarness::start(config(&root), handler.clone())
        .await
        .expect("daemon should start");
    attach_message_sessions(&harness).await;

    let status = harness
        .request(ClientRequest::AgentMessagesStatus)
        .await
        .expect("status");
    let ServerResponse::AgentMessagesStatus(status) = status else {
        panic!("unexpected status response");
    };
    assert!(!status.paused);
    assert_eq!(status.max_message_chars, 16_384);
    assert_eq!(status.max_pending_per_session, 20);
    assert_eq!(status.rate_limit_capacity, 3);
    assert_eq!(status.rate_limit_refill_ms, 1_000);

    for index in 0..3 {
        harness
            .request(ClientRequest::SendMessage {
                from_session_id: "source".into(),
                target_session_id: "target".into(),
                message: format!("message {index}"),
            })
            .await
            .expect("message inside rate window");
    }
    let rate_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: "over limit".into(),
        })
        .await
        .expect_err("fourth pair message must be rate limited");
    assert!(rate_error.to_string().contains("rate limit exceeded"));

    let cleared = harness
        .request(ClientRequest::AgentMessagesClear {
            session_id: "target".into(),
        })
        .await
        .expect("clear");
    let ServerResponse::AgentMessagesCleared(cleared) = cleared else {
        panic!("unexpected clear response");
    };
    assert_eq!(cleared.cleared, 3);
    harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: "after clear".into(),
        })
        .await
        .expect("clear should reset target rate state");

    let paused = harness
        .request(ClientRequest::AgentMessagesPause)
        .await
        .expect("pause");
    assert!(matches!(
        paused,
        ServerResponse::AgentMessagesStatus(ref status) if status.paused
    ));
    let paused_error = harness
        .request(ClientRequest::SendMessage {
            from_session_id: "source".into(),
            target_session_id: "target".into(),
            message: "while paused".into(),
        })
        .await
        .expect_err("paused messaging must reject sends");
    assert!(paused_error.to_string().contains("paused"));

    let resumed = harness
        .request(ClientRequest::AgentMessagesResume)
        .await
        .expect("resume");
    assert!(matches!(
        resumed,
        ServerResponse::AgentMessagesStatus(ref status) if !status.paused
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn failed_agent_message_delivery_is_redacted_and_refunds_rate_capacity() {
    let root = TempDir::new().expect("tempdir");
    let harness = DaemonHarness::start(config(&root), Arc::new(RejectingAgentMessageHandler))
        .await
        .expect("daemon should start");
    attach_message_sessions(&harness).await;

    for index in 0..4 {
        let error = harness
            .request(ClientRequest::SendMessage {
                from_session_id: "source".into(),
                target_session_id: "target".into(),
                message: format!("attempt {index}"),
            })
            .await
            .expect_err("the handler rejects delivery");
        let message = error.to_string();
        assert!(message.contains("agent message delivery failed"));
        assert!(!message.contains("sensitive-handler-detail"));
        assert!(!message.contains("rate limit exceeded"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_aborts_in_flight_connections_before_wait_returns() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    let handler = Arc::new(BlockingHandler::default());
    let handle = DaemonServer::spawn(config.clone(), handler.clone())
        .await
        .expect("server");
    let mut client = DaemonClient::connect(&config.socket_path)
        .await
        .expect("client");
    let attached = client
        .request(ClientRequest::attach("long-session", "cli"))
        .await
        .expect("attach");
    let ServerResponse::SessionAttached(attached) = attached else {
        panic!("unexpected attach response");
    };
    let socket = config.socket_path.clone();
    let lease = attached.lease.lease_id.to_string();
    let prompt = tokio::spawn(async move {
        let mut client = DaemonClient::connect(&socket).await?;
        client
            .request(ClientRequest::prompt(
                &lease,
                "long-session",
                "wait forever",
            ))
            .await
    });
    handler.started.notified().await;

    let mut stopper = DaemonClient::connect(&config.socket_path)
        .await
        .expect("stop client");
    let shutdown_response = stopper
        .request(ClientRequest::Shutdown)
        .await
        .expect("shutdown response");
    assert!(matches!(
        shutdown_response,
        ServerResponse::ShutdownAccepted(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), handle.wait())
        .await
        .expect("daemon should stop promptly")
        .expect("daemon wait");
    let _ = prompt.await;
    assert!(!handler.completed.load(Ordering::SeqCst));
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_ipc_frame_is_rejected_without_stopping_the_daemon() {
    let root = TempDir::new().expect("tempdir");
    let config = config(&root);
    let handle = DaemonServer::spawn(config.clone(), Arc::new(EchoHandler))
        .await
        .expect("server");
    let mut stream = tokio::net::UnixStream::connect(&config.socket_path)
        .await
        .expect("raw client");
    let oversized = vec![b'x'; 1024 * 1024 + 1];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(&oversized))
        .await
        .expect("oversized writer must not hang");
    let mut probe = [0_u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut probe))
        .await
        .expect("server must close oversized connection")
        .unwrap_or(0);
    assert_eq!(closed, 0);

    let mut client = DaemonClient::connect(&config.socket_path)
        .await
        .expect("health client");
    assert!(matches!(
        client.request(ClientRequest::Health).await.expect("health"),
        ServerResponse::Health(_)
    ));
    handle.shutdown().await.expect("shutdown");
}

#[cfg(not(unix))]
#[test]
fn daemon_reports_clean_unsupported_errors_on_non_unix() {
    let config = DaemonConfig {
        state_root: std::path::PathBuf::from("state"),
        socket_path: std::path::PathBuf::from("daemon.sock"),
        server_name: "mimir-test".into(),
        lease_ttl: Duration::from_secs(30),
        supported_capabilities: BTreeSet::new(),
    };
    let error = DaemonServer::spawn_blocking_for_test(config, Arc::new(EchoHandler))
        .expect_err("non-unix should fail closed");
    assert!(matches!(error, DaemonError::UnsupportedTransport(_)));
}
