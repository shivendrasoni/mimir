#![allow(
    clippy::missing_errors_doc,
    reason = "daemon errors are exhaustively represented by DaemonError"
)]

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;
const MAX_PUBLIC_FRAME_BYTES: usize = 8 * 1024 * 1024;
const AGENT_MESSAGE_RATE_CAPACITY: usize = 3;
const AGENT_MESSAGE_RATE_REFILL_MS: u64 = 1_000;
const RECONNECT_CAPABILITIES: [&str; 2] = ["attach_snapshot", "event_sequence"];

use crate::{
    model::{Content, Message, Role, StopReason},
    orchestration::{
        HeartbeatDeliveryMode, HeartbeatManagementAction, Schedule, ScheduleSource, ScheduleStore,
    },
    runtime::RuntimeEvent,
    runtime_events::RuntimeEventEnvelope,
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    session_compat::import_jsonl,
    session_tree::SessionBranchCatalog,
};

use super::{
    IPC_SCHEMA_VERSION,
    journal::{JournalLookup, PublicCommandJournal},
    protocol::{
        AgentMessageEndpoint, AgentMessageRequest, AgentMessageSafetyStatus, AgentMessageSender,
        AgentMessagesClearedResponse, AgentSessionMessageReceipt, ClientRequest,
        DaemonAttachSnapshot, DaemonEventCursor, DaemonHealth, DaemonReplayInfo,
        DaemonReplayStatus, DaemonSessionEventKind, FailureResponse, LeaseRenewedResponse,
        LeaseView, NegotiatedResponse, PromptCompletedResponse, PromptRequest, RequestEnvelope,
        ResponseEnvelope, ServerResponse, SessionAttachedResponse, SessionCatalogView,
        SessionDetachedResponse, ShutdownAcceptedResponse, negotiate_capabilities,
        schema_mismatch_error,
    },
    public::{
        PUBLIC_DAEMON_PROTOCOL_MAX_VERSION, PUBLIC_DAEMON_PROTOCOL_NAME,
        PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES, PublicDaemonCommand, PublicDaemonCommandEnvelope,
        PublicDaemonEventCursor, PublicDaemonSessionSnapshot, PublicDaemonSnapshotRecord,
        PublicImageContent,
    },
    replay::ReplayJournal,
    session_ops::SessionOps,
    state::{DaemonMetadataSnapshot, DaemonStateStore, SessionCatalogEntry, now_ms},
    turn_ops::TurnOps,
};

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon configuration error: {0}")]
    Configuration(String),
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    #[error("daemon transport unsupported on this platform: {0}")]
    UnsupportedTransport(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub state_root: PathBuf,
    pub socket_path: PathBuf,
    pub server_name: String,
    pub lease_ttl: Duration,
    pub supported_capabilities: BTreeSet<String>,
}

#[async_trait]
pub trait PromptHandler: Send + Sync {
    async fn handle_prompt(&self, request: PromptRequest) -> Result<String, DaemonError>;

    async fn handle_prompt_message(
        &self,
        request: PromptRequest,
        message: Message,
    ) -> Result<String, DaemonError> {
        if message.content.len() == 1
            && let Content::Text { text } = &message.content[0]
        {
            return self
                .handle_prompt(PromptRequest {
                    prompt: text.clone(),
                    ..request
                })
                .await;
        }
        Err(DaemonError::Protocol(
            "multimodal prompts are not supported by this prompt handler".into(),
        ))
    }

    async fn session_messages(
        &self,
        _session_id: &str,
    ) -> Result<Option<Vec<Message>>, DaemonError> {
        Ok(None)
    }

    async fn session_system_prompt(
        &self,
        _session_id: &str,
    ) -> Result<Option<String>, DaemonError> {
        Ok(None)
    }

    async fn session_queue(&self, _session_id: &str) -> Result<Option<Value>, DaemonError> {
        Ok(None)
    }

    async fn session_runtime_state(&self, _session_id: &str) -> Result<Option<Value>, DaemonError> {
        Ok(None)
    }

    async fn subscribe_session_events(
        &self,
        _session_id: &str,
    ) -> Result<Option<broadcast::Receiver<RuntimeEventEnvelope>>, DaemonError> {
        Ok(None)
    }

    async fn steer_session(
        &self,
        _session_id: &str,
        _message: Message,
        _command: &PublicDaemonCommand,
    ) -> Result<bool, DaemonError> {
        Ok(false)
    }

    async fn follow_up_session(
        &self,
        _session_id: &str,
        _message: Message,
        _command: &PublicDaemonCommand,
    ) -> Result<Option<bool>, DaemonError> {
        Ok(None)
    }

    async fn rebind_session(
        &self,
        _active_session_id: &str,
        _durable_session_id: &str,
    ) -> Result<bool, DaemonError> {
        Ok(false)
    }

    async fn close_session(&self, _session_id: &str) -> Result<bool, DaemonError> {
        Ok(false)
    }

    async fn handle_session_control(
        &self,
        _session_id: &str,
        _command: &PublicDaemonCommand,
    ) -> Result<Option<Value>, DaemonError> {
        Ok(None)
    }

    async fn handle_runtime_operation(
        &self,
        _client_id: &str,
        _session_id: &str,
        _command: &PublicDaemonCommand,
    ) -> Result<Option<Value>, DaemonError> {
        Ok(None)
    }

    async fn summarize_navigation(
        &self,
        _session_id: &str,
        _command: &PublicDaemonCommand,
        _messages: Vec<Message>,
    ) -> Result<Option<String>, DaemonError> {
        Ok(None)
    }

    async fn handle_scheduled_prompt(
        &self,
        request: PromptRequest,
        _delivery: ScheduledPromptDelivery,
    ) -> Result<String, DaemonError> {
        self.handle_prompt(request).await
    }

    async fn handle_agent_message(
        &self,
        _request: AgentMessageRequest,
    ) -> Result<AgentMessageDelivery, DaemonError> {
        Err(DaemonError::Protocol(
            "agent messaging is not supported by this prompt handler".into(),
        ))
    }

    async fn clear_agent_messages(&self, _session_id: &str) -> Result<usize, DaemonError> {
        Ok(0)
    }

    async fn clear_all_agent_messages(&self) -> Result<usize, DaemonError> {
        Ok(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageDelivery {
    pub queued: bool,
    pub target_session_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledPromptDelivery {
    NewTurn,
    Steer,
    FollowUp,
}

pub struct DaemonServer;

pub struct DaemonHandle {
    socket_path: PathBuf,
    join: JoinHandle<Result<(), DaemonError>>,
}

#[cfg(unix)]
pub struct DaemonHarness {
    core: Arc<ServerCore>,
}

impl DaemonHandle {
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn shutdown(self) -> Result<(), DaemonError> {
        let mut client = DaemonClient::connect(&self.socket_path).await?;
        let response = client.request(ClientRequest::Shutdown).await?;
        if !matches!(response, ServerResponse::ShutdownAccepted(_)) {
            return Err(DaemonError::Protocol(
                "shutdown was not acknowledged".into(),
            ));
        }
        self.wait().await
    }

    pub async fn wait(self) -> Result<(), DaemonError> {
        self.join
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
    }
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    #[cfg(unix)]
    pub async fn connect(socket_path: &Path) -> Result<Self, DaemonError> {
        let path = socket_path.to_path_buf();
        let _ = tokio::net::UnixStream::connect(&path).await?;
        Ok(Self { socket_path: path })
    }

    #[cfg(not(unix))]
    pub async fn connect(_socket_path: &Path) -> Result<Self, DaemonError> {
        Err(DaemonError::UnsupportedTransport(
            "Unix sockets are required".into(),
        ))
    }

    #[cfg(unix)]
    pub async fn request(&mut self, payload: ClientRequest) -> Result<ServerResponse, DaemonError> {
        let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
        let request_id = Uuid::new_v4();
        let encoded = serde_json::to_vec(&RequestEnvelope {
            schema_version: IPC_SCHEMA_VERSION,
            request_id,
            payload,
        })?;
        ensure_frame_size(encoded.len())?;
        stream.write_all(&encoded).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let frame = read_frame(&mut reader).await?;
        let envelope: ResponseEnvelope = serde_json::from_slice(&frame)?;
        if envelope.schema_version != IPC_SCHEMA_VERSION {
            return Err(DaemonError::Protocol(format!(
                "unexpected response schema {}",
                envelope.schema_version
            )));
        }
        if envelope.request_id != request_id {
            return Err(DaemonError::Protocol("response/request ID mismatch".into()));
        }
        match envelope.payload {
            ServerResponse::Failure(FailureResponse { code, message }) => {
                Err(DaemonError::Protocol(format!("{code}: {message}")))
            }
            response => Ok(response),
        }
    }

    #[cfg(not(unix))]
    pub async fn request(
        &mut self,
        _payload: ClientRequest,
    ) -> Result<ServerResponse, DaemonError> {
        Err(DaemonError::UnsupportedTransport(
            "Unix sockets are required".into(),
        ))
    }
}

#[cfg(unix)]
struct ServerCore {
    config: DaemonConfig,
    state_store: DaemonStateStore,
    schedule_store: ScheduleStore,
    session_ops: SessionOps,
    turn_ops: TurnOps,
    snapshot: Mutex<DaemonMetadataSnapshot>,
    schedule_runner: Mutex<()>,
    replay: Arc<Mutex<ReplayJournal>>,
    public_commands: Mutex<PublicCommandJournal>,
    public_events: broadcast::Sender<Value>,
    runtime_event_sessions: Arc<Mutex<BTreeSet<String>>>,
    supervisor_generation: String,
    agent_messages_paused: AtomicBool,
    agent_message_rate: Mutex<HashMap<String, AgentMessageRateBucket>>,
    handler: Arc<dyn PromptHandler>,
    shutdown: CancellationToken,
}

#[cfg(unix)]
struct AgentMessageRateBucket {
    tokens: usize,
    updated_at_ms: u64,
}

#[cfg(unix)]
struct PublicDispatch {
    frames: Vec<Value>,
    shutdown: bool,
}

#[cfg(unix)]
impl ServerCore {
    #[allow(
        clippy::too_many_lines,
        reason = "the protocol dispatcher intentionally keeps all request state transitions together"
    )]
    async fn process(&self, payload: ClientRequest) -> Result<ServerResponse, DaemonError> {
        let now = now_ms();
        let mut snapshot = self.snapshot.lock().await;
        snapshot.expire_leases(now);
        snapshot.last_seen_at_ms = now;
        self.replay
            .lock()
            .await
            .retain_sessions(|session_id| snapshot.sessions.contains_key(session_id));

        let response = match payload {
            ClientRequest::Negotiate {
                client_name: _,
                capabilities,
            } => {
                let mut supported = self.config.supported_capabilities.clone();
                supported.extend(RECONNECT_CAPABILITIES.map(str::to_owned));
                ServerResponse::Negotiated(NegotiatedResponse {
                    schema_version: IPC_SCHEMA_VERSION,
                    server_name: self.config.server_name.clone(),
                    launch_count: snapshot.launch_count,
                    negotiated_capabilities: negotiate_capabilities(&supported, &capabilities),
                })
            }
            ClientRequest::Health => ServerResponse::Health(DaemonHealth {
                server_name: self.config.server_name.clone(),
                launch_count: snapshot.launch_count,
                active_sessions: snapshot.active_sessions(),
                active_leases: snapshot.active_leases(),
                socket_path: self.config.socket_path.display().to_string(),
                last_started_at_ms: snapshot.last_started_at_ms,
            }),
            ClientRequest::AttachSession {
                session_id,
                client_name,
                resume_cursor,
            } => {
                validate_identifier("session_id", &session_id)?;
                validate_identifier("client_name", &client_name)?;
                let lease = LeaseView {
                    lease_id: Uuid::new_v4(),
                    client_name,
                    attached_at_ms: now,
                    expires_at_ms: now.saturating_add(duration_ms(self.config.lease_ttl)),
                };
                let session = snapshot.session_mut(&session_id, now);
                session.last_attached_at_ms = Some(now);
                session.active_leases.push(lease.clone());
                let session = SessionCatalogView::from(&*session);
                let (cursor, replay) = self.replay.lock().await.attach(&session_id, resume_cursor);
                ServerResponse::SessionAttached(Box::new(SessionAttachedResponse {
                    session: session.clone(),
                    lease,
                    snapshot: DaemonAttachSnapshot { session, cursor },
                    replay,
                }))
            }
            ClientRequest::RenewLease { lease_id } => {
                let lease_id = parse_uuid("lease_id", &lease_id)?;
                let mut renewed = None;
                for session in snapshot.sessions.values_mut() {
                    if let Some(lease) = session
                        .active_leases
                        .iter_mut()
                        .find(|lease| lease.lease_id == lease_id)
                    {
                        lease.expires_at_ms =
                            now.saturating_add(duration_ms(self.config.lease_ttl));
                        renewed = Some(lease.clone());
                        break;
                    }
                }
                let lease = renewed
                    .ok_or_else(|| DaemonError::Protocol(format!("unknown lease {lease_id}")))?;
                ServerResponse::LeaseRenewed(LeaseRenewedResponse { lease })
            }
            ClientRequest::DetachSession { lease_id } => {
                let lease_id = parse_uuid("lease_id", &lease_id)?;
                let mut detached = None;
                for session in snapshot.sessions.values_mut() {
                    let before = session.active_leases.len();
                    session
                        .active_leases
                        .retain(|lease| lease.lease_id != lease_id);
                    if before != session.active_leases.len() {
                        session.last_detached_at_ms = Some(now);
                        detached = Some(SessionCatalogView::from(&*session));
                        break;
                    }
                }
                ServerResponse::SessionDetached(SessionDetachedResponse {
                    session: detached.ok_or_else(|| {
                        DaemonError::Protocol(format!("unknown lease {lease_id}"))
                    })?,
                })
            }
            ClientRequest::Prompt {
                lease_id,
                session_id,
                prompt,
            } => {
                validate_identifier("session_id", &session_id)?;
                let lease_id = parse_uuid("lease_id", &lease_id)?;
                let session = snapshot.sessions.get_mut(&session_id).ok_or_else(|| {
                    DaemonError::Protocol(format!("unknown session {session_id}"))
                })?;
                if !session
                    .active_leases
                    .iter()
                    .any(|lease| lease.lease_id == lease_id)
                {
                    return Err(DaemonError::Protocol(format!(
                        "lease {lease_id} is not attached to {session_id}"
                    )));
                }
                let request = PromptRequest {
                    lease_id,
                    session_id: session_id.clone(),
                    prompt: prompt.clone(),
                };
                let handler = self.handler.clone();
                drop(snapshot);
                let output = handler.handle_prompt(request).await?;
                let mut snapshot = self.snapshot.lock().await;
                snapshot.expire_leases(now_ms());
                let session = snapshot.session_mut(&session_id, now_ms());
                session.last_prompt_at_ms = Some(now_ms());
                snapshot.last_seen_at_ms = now_ms();
                self.state_store.save(&snapshot).await?;
                self.replay.lock().await.record(
                    &session_id,
                    DaemonSessionEventKind::PromptCompleted,
                    now_ms(),
                );
                return Ok(ServerResponse::PromptCompleted(PromptCompletedResponse {
                    session_id,
                    output,
                }));
            }
            ClientRequest::SendMessage {
                from_session_id,
                target_session_id,
                message,
            } => {
                validate_identifier("from_session_id", &from_session_id)?;
                validate_identifier("target_session_id", &target_session_id)?;
                if from_session_id == target_session_id {
                    return Err(DaemonError::Protocol(
                        "agent messaging cannot target the sending session".into(),
                    ));
                }
                if !snapshot.sessions.contains_key(&from_session_id) {
                    return Err(DaemonError::Protocol(format!(
                        "unknown sending session {from_session_id}"
                    )));
                }
                if !snapshot.sessions.contains_key(&target_session_id) {
                    return Err(DaemonError::Protocol(format!(
                        "unknown target session {target_session_id}"
                    )));
                }
                if self.agent_messages_paused.load(Ordering::Acquire) {
                    return Err(DaemonError::Protocol("agent messaging is paused".into()));
                }
                let message = normalize_agent_message(&message)?;
                self.consume_agent_message_rate(&from_session_id, &target_session_id, now)
                    .await?;
                let message_id = format!("agentmsg_{}", Uuid::new_v4());
                let request = AgentMessageRequest {
                    prompt: agent_message_prompt(
                        &from_session_id,
                        &target_session_id,
                        &message_id,
                        &message,
                    ),
                    message_id: message_id.clone(),
                    from_session_id: from_session_id.clone(),
                    target_session_id: target_session_id.clone(),
                    message: message.clone(),
                };
                let handler = self.handler.clone();
                drop(snapshot);
                let Ok(delivery) = handler.handle_agent_message(request).await else {
                    self.refund_agent_message_rate(&from_session_id, &target_session_id)
                        .await;
                    return Err(DaemonError::Protocol(
                        "agent message delivery failed".into(),
                    ));
                };
                let timestamp = Utc::now().to_rfc3339();
                self.replay.lock().await.record(
                    &target_session_id,
                    if delivery.queued {
                        DaemonSessionEventKind::AgentMessageQueued
                    } else {
                        DaemonSessionEventKind::AgentMessageDelivered
                    },
                    now_ms(),
                );
                return Ok(ServerResponse::AgentMessageSent(Box::new(
                    AgentSessionMessageReceipt {
                        id: message_id,
                        source: "agent_message".into(),
                        target: AgentMessageEndpoint {
                            active_session_id: target_session_id.clone(),
                            session_id: target_session_id,
                            session_name: delivery.target_session_name,
                            runtime_kind: "top-level".into(),
                        },
                        from: AgentMessageSender {
                            active_session_id: from_session_id.clone(),
                            session_id: from_session_id,
                            session_name: None,
                            client_id: "mimir-rpc".into(),
                        },
                        message,
                        delivery_status: if delivery.queued {
                            "queued".into()
                        } else {
                            "delivered".into()
                        },
                        delivered_at: (!delivery.queued).then_some(timestamp.clone()),
                        queued_at: delivery.queued.then_some(timestamp),
                        delivery_mode: "steer".into(),
                    },
                )));
            }
            ClientRequest::AgentMessagesStatus => {
                ServerResponse::AgentMessagesStatus(self.agent_message_safety_status())
            }
            ClientRequest::AgentMessagesPause => {
                self.agent_messages_paused.store(true, Ordering::Release);
                self.agent_message_rate.lock().await.clear();
                let handler = self.handler.clone();
                drop(snapshot);
                handler.clear_all_agent_messages().await?;
                return Ok(ServerResponse::AgentMessagesStatus(
                    self.agent_message_safety_status(),
                ));
            }
            ClientRequest::AgentMessagesResume => {
                self.agent_messages_paused.store(false, Ordering::Release);
                ServerResponse::AgentMessagesStatus(self.agent_message_safety_status())
            }
            ClientRequest::AgentMessagesClear { session_id } => {
                validate_identifier("session_id", &session_id)?;
                if !snapshot.sessions.contains_key(&session_id) {
                    return Err(DaemonError::Protocol(format!(
                        "unknown target session {session_id}"
                    )));
                }
                self.clear_agent_message_rate_for_target(&session_id).await;
                let handler = self.handler.clone();
                drop(snapshot);
                let cleared = handler.clear_agent_messages(&session_id).await?;
                self.replay.lock().await.record(
                    &session_id,
                    DaemonSessionEventKind::AgentMessagesCleared,
                    now_ms(),
                );
                return Ok(ServerResponse::AgentMessagesCleared(
                    AgentMessagesClearedResponse { cleared },
                ));
            }
            ClientRequest::Shutdown => {
                snapshot.last_stopped_at_ms = Some(now);
                ServerResponse::ShutdownAccepted(ShutdownAcceptedResponse {
                    server_name: self.config.server_name.clone(),
                })
            }
        };

        self.state_store.save(&snapshot).await?;
        Ok(response)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the public command matrix remains linear so implemented and explicit-unsupported behavior is auditable"
    )]
    async fn process_public(&self, envelope: PublicDaemonCommandEnvelope) -> PublicDispatch {
        let command_name = envelope.command.command_type().to_owned();
        let client_id = envelope
            .client_id
            .clone()
            .unwrap_or_else(|| format!("public-{}", envelope.id));
        if command_name == "ack_result" {
            let result = match required_string(&envelope.command, "commandId") {
                Ok(command_id) => {
                    self.public_commands
                        .lock()
                        .await
                        .acknowledge(&client_id, command_id)
                        .await
                }
                Err(error) => Err(error),
            };
            return match result {
                Ok(()) => PublicDispatch {
                    frames: Vec::new(),
                    shutdown: false,
                },
                Err(error) => PublicDispatch {
                    frames: vec![public_failure(
                        &envelope.id,
                        &command_name,
                        "command_failed",
                        &error.to_string(),
                    )],
                    shutdown: false,
                },
            };
        }

        let journaled = is_public_mutating_command(&command_name);
        if journaled {
            let mut journal = self.public_commands.lock().await;
            match journal.lookup(&client_id, &envelope.id) {
                Some(JournalLookup::Complete(response)) => {
                    return PublicDispatch {
                        frames: vec![response],
                        shutdown: false,
                    };
                }
                Some(JournalLookup::Pending) => {
                    return PublicDispatch {
                        frames: vec![public_failure(
                            &envelope.id,
                            &command_name,
                            "command_result_uncertain",
                            "The previous command result is uncertain and was not replayed",
                        )],
                        shutdown: false,
                    };
                }
                None => {
                    if let Err(error) = journal.begin(&client_id, &envelope.id, &command_name).await
                    {
                        return PublicDispatch {
                            frames: vec![public_failure(
                                &envelope.id,
                                &command_name,
                                "command_journal_failed",
                                &error.to_string(),
                            )],
                            shutdown: false,
                        };
                    }
                }
            }
        }
        let result = self
            .process_public_inner(&envelope, &client_id, &command_name)
            .await;
        let mut dispatch = match result {
            Ok((data, mut trailing, shutdown)) => {
                let mut frames = vec![public_success(&envelope.id, &command_name, data)];
                frames.append(&mut trailing);
                PublicDispatch { frames, shutdown }
            }
            Err(error) => {
                let message = error.to_string();
                let code = if message.contains("recognized public daemon command") {
                    "unsupported_command"
                } else if message.contains("recognized but not supported") {
                    "unsupported_command_feature"
                } else {
                    "command_failed"
                };
                PublicDispatch {
                    frames: vec![public_failure(&envelope.id, &command_name, code, &message)],
                    shutdown: false,
                }
            }
        };
        if journaled
            && let Some(response) = dispatch.frames.first().cloned()
            && let Err(error) = self
                .public_commands
                .lock()
                .await
                .record_result(&client_id, &envelope.id, response)
                .await
        {
            dispatch = PublicDispatch {
                frames: vec![public_failure(
                    &envelope.id,
                    &command_name,
                    "command_journal_failed",
                    &error.to_string(),
                )],
                shutdown: false,
            };
        }
        dispatch
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the public command matrix remains linear so implemented and explicit-unsupported behavior is auditable"
    )]
    async fn process_public_inner(
        &self,
        envelope: &PublicDaemonCommandEnvelope,
        client_id: &str,
        command_name: &str,
    ) -> Result<(Option<Value>, Vec<Value>, bool), DaemonError> {
        let command = &envelope.command;
        match command_name {
            "list" => {
                reject_present_fields(
                    command,
                    &["all", "cwd", "sessionDir", "includeClientOwned"],
                )?;
                let now = now_ms();
                let mut snapshot = self.snapshot.lock().await;
                snapshot.expire_leases(now);
                let sessions = snapshot
                    .sessions
                    .values()
                    .filter(|session| session.lifecycle != "client_owned")
                    .map(public_session_summary)
                    .collect::<Vec<_>>();
                self.state_store.save(&snapshot).await?;
                Ok((Some(json!({"sessions": sessions})), Vec::new(), false))
            }
            "create" => {
                reject_present_fields(
                    command,
                    &[
                        "continueRecent",
                        "noSession",
                        "config",
                        "runtimeMetadata",
                        "env",
                        "launchEnv",
                    ],
                )?;
                let session_path = optional_string(command, "sessionPath")?;
                let requested = session_path
                    .as_deref()
                    .and_then(public_session_id_from_path)
                    .or_else(|| {
                        command
                            .field("activeSessionId")
                            .and_then(Value::as_str)
                            .and_then(public_session_id_from_path)
                    });
                let session_id = requested.unwrap_or_else(|| Uuid::new_v4().to_string());
                validate_identifier("activeSessionId", &session_id)?;
                let lifecycle =
                    optional_string(command, "lifecycle")?.unwrap_or_else(|| "resident".into());
                if !matches!(lifecycle.as_str(), "resident" | "client_owned") {
                    return Err(DaemonError::Protocol(
                        "lifecycle must be resident or client_owned".into(),
                    ));
                }
                let durable_path = self
                    .prepare_public_session(&session_id, session_path.as_deref())
                    .await?;
                let message_count = self.load_public_messages(&session_id).await?.len();
                let now = now_ms();
                let mut snapshot = self.snapshot.lock().await;
                let session = snapshot.session_mut(&session_id, now);
                if let Some(name) = optional_string(command, "name")? {
                    session.name = Some(name);
                }
                session.session_path = Some(durable_path);
                session.owner_client_id =
                    (lifecycle == "client_owned").then(|| client_id.to_owned());
                session.lifecycle = lifecycle;
                session.message_count = message_count;
                let summary = public_session_summary(session);
                self.state_store.save(&snapshot).await?;
                Ok((Some(summary), Vec::new(), false))
            }
            "attach" => {
                reject_present_fields(command, &["supportsExtensionUi", "env", "launchEnv"])?;
                self.public_attach(envelope, client_id, "attach").await
            }
            "reattach" => {
                reject_present_fields(command, &["supportsExtensionUi", "env", "launchEnv"])?;
                let previous = required_string(command, "activeSessionId")?;
                self.public_detach(client_id, Some(previous)).await?;
                let target = required_string(command, "targetActiveSessionId")?;
                let replacement = PublicDaemonCommand::new(
                    "attach",
                    command
                        .clone()
                        .into_fields()
                        .into_iter()
                        .filter(|(key, _)| key != "type" && key != "activeSessionId")
                        .map(|(key, value)| {
                            if key == "targetActiveSessionId" {
                                ("activeSessionId".into(), Value::String(target.to_owned()))
                            } else {
                                (key, value)
                            }
                        }),
                )
                .map_err(DaemonError::Protocol)?;
                let replacement = PublicDaemonCommandEnvelope::new(
                    envelope.id.clone(),
                    envelope.client_id.clone(),
                    envelope.protocol.version,
                    replacement,
                )
                .map_err(DaemonError::Protocol)?;
                self.public_attach(&replacement, client_id, "reattach")
                    .await
            }
            "detach" => {
                let active = optional_string(command, "activeSessionId")?;
                let detached = self.public_detach(client_id, active.as_deref()).await?;
                Ok((
                    Some(json!({"cancelled": false, "detached": detached})),
                    Vec::new(),
                    false,
                ))
            }
            "complete_owned_session" | "promote_owned_session" | "kill" => {
                let session_id = required_string(command, "activeSessionId")?;
                let session = self.public_session_access(client_id, session_id).await?;
                if command_name != "kill"
                    && (session.lifecycle != "client_owned"
                        || session.owner_client_id.as_deref() != Some(client_id))
                {
                    return Err(DaemonError::Protocol(
                        "session is not owned by this client".into(),
                    ));
                }
                if command_name == "promote_owned_session" {
                    let mut snapshot = self.snapshot.lock().await;
                    let session = snapshot.sessions.get_mut(session_id).ok_or_else(|| {
                        DaemonError::Protocol(format!("unknown session {session_id}"))
                    })?;
                    session.lifecycle = "resident".into();
                    session.owner_client_id = None;
                    let summary = public_session_summary(session);
                    self.state_store.save(&snapshot).await?;
                    Ok((Some(summary), Vec::new(), false))
                } else {
                    self.handler.close_session(session_id).await?;
                    let mut snapshot = self.snapshot.lock().await;
                    snapshot.sessions.remove(session_id);
                    self.state_store.save(&snapshot).await?;
                    Ok((None, Vec::new(), false))
                }
            }
            "rename" | "set_session_name" => {
                reject_present_fields(command, &["workerToken"])?;
                let session_id = required_string(command, "activeSessionId")?;
                let name = required_string(command, "name")?.trim();
                if name.is_empty() {
                    return Err(DaemonError::Protocol("name must not be blank".into()));
                }
                let mut snapshot = self.snapshot.lock().await;
                let session = snapshot.sessions.get_mut(session_id).ok_or_else(|| {
                    DaemonError::Protocol(format!("unknown session {session_id}"))
                })?;
                session.name = Some(name.to_owned());
                let summary = public_session_summary(session);
                self.state_store.save(&snapshot).await?;
                Ok((Some(summary), Vec::new(), false))
            }
            "prompt" | "prompt_and_wait" => {
                reject_present_fields(
                    command,
                    &[
                        "streamingBehavior",
                        "queueIfBusy",
                        "expandPromptTemplates",
                        "source",
                        "agentMessageId",
                        "customMessage",
                    ],
                )?;
                let session_id = required_string(command, "activeSessionId")?;
                let message = public_prompt_message(command)?;
                let admission_id = optional_string(command, "admissionId")?;
                let mut admission = self
                    .turn_ops
                    .begin_prompt(session_id, admission_id.as_deref())
                    .await?;
                let lease_id = self.public_lease(client_id, session_id).await?;
                admission.mark_owned()?;
                let output = self
                    .process_public_prompt(lease_id, session_id, message)
                    .await?;
                let messages = self.load_public_messages(session_id).await?;
                {
                    let mut snapshot = self.snapshot.lock().await;
                    if let Some(session) = snapshot.sessions.get_mut(session_id) {
                        session.message_count = messages.len();
                    }
                    self.state_store.save(&snapshot).await?;
                }
                self.publish_public_turn(session_id, messages.last().cloned())
                    .await;
                Ok((Some(json!({"text": output})), Vec::new(), false))
            }
            "cancel_prompt_admission" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(self.turn_ops.cancel_prompt_admission(command)?),
                    Vec::new(),
                    false,
                ))
            }
            "get_session_header" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(json!({"header": self.public_session_header(session_id).await?})),
                    Vec::new(),
                    false,
                ))
            }
            "get_state" => {
                let session_id = required_string(command, "activeSessionId")?;
                let mut session = self.public_session_access(client_id, session_id).await?;
                session.message_count = self.load_public_messages(session_id).await?.len();
                let mut state = public_session_summary(&session);
                if let Some(runtime_state) = self.handler.session_runtime_state(session_id).await? {
                    merge_public_object(&mut state, &runtime_state)?;
                }
                Ok((Some(state), Vec::new(), false))
            }
            "get_connection_state" => {
                let session_id = required_string(command, "activeSessionId")?;
                let mut session = self.public_session_access(client_id, session_id).await?;
                session.message_count = self.load_public_messages(session_id).await?.len();
                let summary = public_session_summary(&session);
                let mut state = public_connection_state(&summary);
                if let Some(runtime_state) = self.handler.session_runtime_state(session_id).await? {
                    merge_public_object(&mut state, &runtime_state)?;
                }
                Ok((Some(state), Vec::new(), false))
            }
            "get_messages" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(json!({"messages": self.load_public_messages(session_id).await?})),
                    Vec::new(),
                    false,
                ))
            }
            "get_session_stats" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(self.public_session_stats(session_id).await?),
                    Vec::new(),
                    false,
                ))
            }
            "get_context_tree" => {
                let session_id = required_string(command, "activeSessionId")?;
                let session = self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(self.public_context_tree(session_id, &session).await?),
                    Vec::new(),
                    false,
                ))
            }
            "get_session_context" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let messages = self.load_public_messages(session_id).await?;
                let runtime_state = self.handler.session_runtime_state(session_id).await?;
                let thinking_level = runtime_state
                    .as_ref()
                    .and_then(|state| state.get("thinkingLevel"))
                    .cloned()
                    .unwrap_or_else(|| Value::String("off".into()));
                let service_tier = runtime_state
                    .as_ref()
                    .and_then(|state| state.get("serviceTier"))
                    .cloned()
                    .unwrap_or_else(|| Value::String("auto".into()));
                let model = runtime_state
                    .as_ref()
                    .and_then(|state| state.get("model"))
                    .and_then(Value::as_object)
                    .map_or(Value::Null, |model| {
                        json!({
                            "provider": model.get("provider"),
                            "modelId": model.get("id")
                        })
                    });
                Ok((
                    Some(json!({
                        "context": {
                            "messages": messages,
                            "thinkingLevel": thinking_level,
                            "serviceTier": service_tier,
                            "model": model
                        }
                    })),
                    Vec::new(),
                    false,
                ))
            }
            "get_session_tree" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let tree = self.public_session_tree(session_id).await?;
                Ok((
                    Some(json!({
                        "flatNodes": tree["flatNodes"],
                        "leafId": tree["leafId"]
                    })),
                    Vec::new(),
                    false,
                ))
            }
            "get_user_messages_for_forking" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                Ok((
                    Some(json!({
                        "messages": self.public_user_messages_for_forking(session_id).await?
                    })),
                    Vec::new(),
                    false,
                ))
            }
            "get_queue" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let queue = self
                    .handler
                    .session_queue(session_id)
                    .await?
                    .unwrap_or_else(|| json!({"steering": [], "followUp": []}));
                Ok((Some(queue), Vec::new(), false))
            }
            "get_last_assistant_text" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let messages = self.load_public_messages(session_id).await?;
                let text = messages.iter().rev().find_map(public_assistant_text);
                Ok((Some(json!({"text": text})), Vec::new(), false))
            }
            "get_system_prompt" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let system_prompt = self
                    .handler
                    .session_system_prompt(session_id)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(
                            "system prompt is unavailable for this session runtime".into(),
                        )
                    })?;
                Ok((
                    Some(json!({"systemPrompt": system_prompt})),
                    Vec::new(),
                    false,
                ))
            }
            "steer" => {
                validate_queued_prompt_features(command)?;
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let message = public_prompt_message(command)?;
                if !self
                    .handler
                    .steer_session(session_id, message, command)
                    .await?
                {
                    return Err(DaemonError::Protocol(
                        "recognized public daemon command 'steer' is not implemented by this session runtime"
                            .into(),
                    ));
                }
                Ok((None, Vec::new(), false))
            }
            "follow_up" => {
                validate_queued_prompt_features(command)?;
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let message = public_prompt_message(command)?;
                let queued = self
                    .handler
                    .follow_up_session(session_id, message, command)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(
                            "recognized public daemon command 'follow_up' is not implemented by this session runtime"
                                .into(),
                        )
                    })?;
                Ok((Some(json!({"queued": queued})), Vec::new(), false))
            }
            "abort"
            | "clear_queue"
            | "abort_and_clear_queue"
            | "set_thinking_level"
            | "set_service_tier"
            | "cycle_thinking_level"
            | "set_steering_mode"
            | "set_follow_up_mode"
            | "set_auto_compaction"
            | "set_auto_retry"
            | "compact"
            | "abort_retry"
            | "wait_for_idle"
            | "wait_for_headless_completion"
            | "execute_bash_and_wait"
            | "abort_bash"
            | "get_commands"
            | "get_resource_snapshot"
            | "get_model_catalog"
            | "get_available_models"
            | "get_tool_definition"
            | "reload"
            | "set_model"
            | "cycle_model"
            | "set_scoped_models"
            | "set_transport"
            | "extension_ui_response" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let result = self
                    .handler
                    .handle_session_control(session_id, command)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(format!(
                            "recognized public daemon command '{command_name}' is not implemented by this session runtime"
                        ))
                    })?;
                Ok(((!result.is_null()).then_some(result), Vec::new(), false))
            }
            "restore_next_turn" | "restore_actions" | "append_custom_message" | "resume_queue" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                TurnOps::validate_recovery(command)?.ok_or_else(|| {
                    DaemonError::Protocol(format!(
                        "recognized public daemon command '{command_name}' was not accepted by the recovery service"
                    ))
                })?;
                let result = self
                    .handler
                    .handle_session_control(session_id, command)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(format!(
                            "recognized public daemon command '{command_name}' is not implemented by this session runtime"
                        ))
                    })?;
                Ok(((!result.is_null()).then_some(result), Vec::new(), false))
            }
            "execute_bash"
            | "start_side_question"
            | "abort_side_question"
            | "cancel_rlm_child"
            | "delete_rlm_subagent"
            | "get_rlm_max_depth_status"
            | "set_rlm_max_depth"
            | "refine"
            | "abort_compaction"
            | "abort_branch_summary" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let result = self
                    .handler
                    .handle_runtime_operation(client_id, session_id, command)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(format!(
                            "recognized public daemon command '{command_name}' is not implemented by this session runtime"
                        ))
                    })?;
                Ok(((!result.is_null()).then_some(result), Vec::new(), false))
            }
            "cron_list" => {
                let active_session_id = optional_string(command, "activeSessionId")?;
                let include_inactive = optional_bool(command, "includeInactive")?.unwrap_or(false);
                let visible_sessions = self.public_visible_session_ids(client_id).await;
                if let Some(session_id) = active_session_id.as_deref()
                    && !visible_sessions.contains(session_id)
                {
                    return Err(DaemonError::Protocol(format!(
                        "unknown session {session_id}"
                    )));
                }
                let jobs = self
                    .schedule_store
                    .list()
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .into_iter()
                    .filter(|schedule| schedule.source == ScheduleSource::Cron)
                    .filter(|schedule| visible_sessions.contains(&schedule.session_id))
                    .filter(|schedule| {
                        active_session_id
                            .as_deref()
                            .is_none_or(|session_id| schedule.session_id == session_id)
                    })
                    .filter(|schedule| include_inactive || schedule.enabled)
                    .map(|schedule| public_schedule(&schedule, &self.config.state_root))
                    .collect::<Vec<_>>();
                Ok((Some(json!({"jobs": jobs})), Vec::new(), false))
            }
            "heartbeats_list" => {
                let active_session_id = optional_string(command, "activeSessionId")?;
                let visible_sessions = self.public_visible_session_ids(client_id).await;
                if let Some(session_id) = active_session_id.as_deref()
                    && !visible_sessions.contains(session_id)
                {
                    return Err(DaemonError::Protocol(format!(
                        "unknown session {session_id}"
                    )));
                }
                let heartbeats = self
                    .schedule_store
                    .list_heartbeats()
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .into_iter()
                    .filter(|schedule| visible_sessions.contains(&schedule.session_id))
                    .filter(|schedule| {
                        active_session_id
                            .as_deref()
                            .is_none_or(|session_id| schedule.session_id == session_id)
                    })
                    .map(|schedule| {
                        json!({"job": public_schedule(&schedule, &self.config.state_root)})
                    })
                    .collect::<Vec<_>>();
                Ok((Some(json!({"heartbeats": heartbeats})), Vec::new(), false))
            }
            "cron_add" => {
                reject_present_fields(command, &["promoteOwnedSession"])?;
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let schedule = required_string(command, "schedule")?;
                let prompt = required_string(command, "prompt")?;
                let job = self
                    .schedule_store
                    .add_text("cron", session_id, prompt, schedule, Utc::now())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Ok((
                    Some(json!({"job": public_schedule(&job, &self.config.state_root)})),
                    Vec::new(),
                    false,
                ))
            }
            "cron_cancel" => {
                let job_id = required_uuid(command, "jobId")?;
                let job_session_id = self
                    .schedule_store
                    .list()
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .into_iter()
                    .find(|schedule| {
                        schedule.id == job_id && schedule.source == ScheduleSource::Cron
                    })
                    .map(|schedule| schedule.session_id)
                    .ok_or_else(|| DaemonError::Protocol(format!("unknown cron job {job_id}")))?;
                self.public_session_access(client_id, &job_session_id)
                    .await?;
                let job = self
                    .schedule_store
                    .cancel(job_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Ok((
                    Some(json!({"job": public_schedule(&job, &self.config.state_root)})),
                    Vec::new(),
                    false,
                ))
            }
            "heartbeat_get" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let heartbeat = self
                    .schedule_store
                    .get_heartbeat(session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .as_ref()
                    .map(|schedule| public_schedule(schedule, &self.config.state_root));
                Ok((Some(json!({"heartbeat": heartbeat})), Vec::new(), false))
            }
            "heartbeat_set" => {
                reject_present_fields(command, &["promoteOwnedSession"])?;
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let schedule = required_string(command, "schedule")?;
                let prompt = required_string(command, "prompt")?;
                let delivery_mode = optional_string(command, "deliveryMode")?
                    .map(|mode| parse_heartbeat_delivery_mode(&mode))
                    .transpose()?;
                let heartbeat = self
                    .schedule_store
                    .set_heartbeat(
                        "heartbeat",
                        session_id,
                        prompt,
                        schedule,
                        delivery_mode,
                        Utc::now(),
                    )
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                Ok((
                    Some(json!({
                        "heartbeat": public_schedule(&heartbeat, &self.config.state_root)
                    })),
                    Vec::new(),
                    false,
                ))
            }
            "heartbeat_update" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let action = parse_heartbeat_update_action(required_string(command, "action")?)?;
                let heartbeat = match action {
                    HeartbeatManagementAction::Pause => {
                        self.schedule_store
                            .pause_heartbeat(session_id, Utc::now())
                            .await
                    }
                    HeartbeatManagementAction::Resume => {
                        self.schedule_store
                            .resume_heartbeat(session_id, Utc::now())
                            .await
                    }
                    HeartbeatManagementAction::Stop => {
                        self.schedule_store
                            .clear_heartbeat(session_id, Utc::now())
                            .await
                    }
                }
                .map_err(|error| DaemonError::Protocol(error.to_string()))?
                .as_ref()
                .map(|schedule| public_schedule(schedule, &self.config.state_root));
                Ok((Some(json!({"heartbeat": heartbeat})), Vec::new(), false))
            }
            "heartbeat_manage" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let job_id = required_uuid(command, "jobId")?;
                let action =
                    parse_heartbeat_management_action(required_string(command, "action")?)?;
                let heartbeat = self
                    .schedule_store
                    .manage_heartbeat(session_id, job_id, action, Utc::now())
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?
                    .ok_or_else(|| DaemonError::Protocol(format!("unknown heartbeat {job_id}")))?;
                Ok((
                    Some(json!({
                        "heartbeat": public_schedule(&heartbeat, &self.config.state_root)
                    })),
                    Vec::new(),
                    false,
                ))
            }
            "send_message" => {
                reject_present_fields(command, &["agentOrigin", "deliveryMode"])?;
                let target = required_string(command, "targetActiveSessionId")?;
                let message = required_string(command, "message")?;
                let source = if let Some(source) = optional_string(command, "fromActiveSessionId")?
                {
                    source
                } else {
                    self.first_public_session(client_id, Some(target)).await?
                };
                let response = self
                    .process(ClientRequest::SendMessage {
                        from_session_id: source,
                        target_session_id: target.to_owned(),
                        message: message.to_owned(),
                    })
                    .await?;
                let ServerResponse::AgentMessageSent(receipt) = response else {
                    return Err(DaemonError::Protocol(
                        "public send_message returned an invalid internal response".into(),
                    ));
                };
                Ok((Some(serde_json::to_value(receipt)?), Vec::new(), false))
            }
            "agent_messages_status" | "agent_messages_pause" | "agent_messages_resume" => {
                let request = match command_name {
                    "agent_messages_pause" => ClientRequest::AgentMessagesPause,
                    "agent_messages_resume" => ClientRequest::AgentMessagesResume,
                    _ => ClientRequest::AgentMessagesStatus,
                };
                let response = self.process(request).await?;
                let ServerResponse::AgentMessagesStatus(status) = response else {
                    return Err(DaemonError::Protocol(
                        "public agent message status returned an invalid internal response".into(),
                    ));
                };
                Ok((Some(serde_json::to_value(status)?), Vec::new(), false))
            }
            "agent_messages_clear" => {
                let session_id = required_string(command, "activeSessionId")?;
                let response = self
                    .process(ClientRequest::AgentMessagesClear {
                        session_id: session_id.to_owned(),
                    })
                    .await?;
                let ServerResponse::AgentMessagesCleared(cleared) = response else {
                    return Err(DaemonError::Protocol(
                        "public agent message clear returned an invalid internal response".into(),
                    ));
                };
                Ok((Some(json!({"cleared": cleared.cleared})), Vec::new(), false))
            }
            "new_session"
            | "switch_session"
            | "import_jsonl"
            | "fork"
            | "navigate_tree"
            | "export_html"
            | "export_jsonl"
            | "set_session_entry_label" => {
                let session_id = required_string(command, "activeSessionId")?;
                let bound_command = self
                    .public_bound_session_command(client_id, command)
                    .await?;
                let data = if command_name == "navigate_tree"
                    && command.field("summarize") == Some(&Value::Bool(true))
                {
                    let messages = self
                        .session_ops
                        .navigation_summary_messages(&bound_command)
                        .await?;
                    let summary = self
                        .handler
                        .summarize_navigation(session_id, command, messages)
                        .await?
                        .ok_or_else(|| {
                            DaemonError::Protocol(
                                "summarized navigation is not implemented by this session runtime"
                                    .into(),
                            )
                        })?;
                    self.session_ops
                        .execute_navigation(&bound_command, Some(&summary))
                        .await?
                } else {
                    self.session_ops.execute(&bound_command).await?.ok_or_else(|| {
                        DaemonError::Protocol(format!(
                            "recognized public daemon command '{command_name}' is not implemented by the durable session service"
                        ))
                    })?
                };
                if matches!(
                    command_name,
                    "new_session" | "switch_session" | "import_jsonl" | "fork" | "navigate_tree"
                ) {
                    self.public_rebind_session(session_id, &data).await?;
                }
                Ok((Some(data), Vec::new(), false))
            }
            "list_saved_sessions" | "rename_saved_session" | "delete_saved_session" => {
                if command_name == "delete_saved_session" {
                    self.public_assert_saved_session_inactive(command).await?;
                }
                let data = self.session_ops.execute(command).await?.ok_or_else(|| {
                    DaemonError::Protocol(format!(
                        "recognized public daemon command '{command_name}' is not implemented by the durable session service"
                    ))
                })?;
                Ok((Some(data), Vec::new(), false))
            }
            "prepare_update_restart" => Ok((
                Some(self.public_update_restart_manifest().await?),
                Vec::new(),
                false,
            )),
            "retry_worker" => {
                let session_id = required_string(command, "activeSessionId")?;
                self.public_session_access(client_id, session_id).await?;
                let reload = PublicDaemonCommand::new(
                    "reload",
                    [("activeSessionId".into(), Value::String(session_id.into()))],
                )
                .map_err(DaemonError::Protocol)?;
                self.handler
                    .handle_session_control(session_id, &reload)
                    .await?
                    .ok_or_else(|| {
                        DaemonError::Protocol(
                            "session runtime cannot recover an unavailable worker".into(),
                        )
                    })?;
                let mut snapshot = self.snapshot.lock().await;
                let session = snapshot.sessions.get_mut(session_id).ok_or_else(|| {
                    DaemonError::Protocol(format!("unknown session {session_id}"))
                })?;
                session.lifecycle = "resident".into();
                let summary = public_session_summary(session);
                self.state_store.save(&snapshot).await?;
                Ok((Some(summary), Vec::new(), false))
            }
            "restart" => Ok((
                Some(json!({
                    "serverName": self.config.server_name,
                    "restartRequired": true,
                    "statePreserved": true
                })),
                Vec::new(),
                true,
            )),
            "shutdown" => Ok((
                Some(json!({"serverName": self.config.server_name})),
                Vec::new(),
                true,
            )),
            _ => Err(DaemonError::Protocol(format!(
                "recognized public daemon command '{command_name}' is not implemented by the Rust supervisor"
            ))),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "restart checkpoint assembly keeps each session's queue and in-flight flags in one auditable wire-format transaction"
    )]
    async fn public_update_restart_manifest(&self) -> Result<Value, DaemonError> {
        let sessions = self
            .snapshot
            .lock()
            .await
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut restart_sessions = Vec::new();
        let mut discarded = Vec::new();
        for session in sessions {
            let Some(session_file) = session.session_path.clone() else {
                discarded.push(session.session_id);
                continue;
            };
            let wait = PublicDaemonCommand::new(
                "wait_for_idle",
                [(
                    "activeSessionId".into(),
                    Value::String(session.session_id.clone()),
                )],
            )
            .map_err(DaemonError::Protocol)?;
            self.handler
                .handle_session_control(&session.session_id, &wait)
                .await?;
            let state = self
                .handler
                .session_runtime_state(&session.session_id)
                .await?
                .unwrap_or(Value::Null);
            let queue = self
                .handler
                .session_queue(&session.session_id)
                .await?
                .unwrap_or_else(|| json!({"steering": [], "followUp": []}));
            let pending = queue
                .get("steering")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    queue
                        .get("followUp")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
                .filter_map(Value::as_str)
                .map(|text| {
                    json!({
                        "role": "custom",
                        "customType": "mimir.restart_queue",
                        "display": false,
                        "content": [{"type": "text", "text": text}],
                        "timestamp": now_ms()
                    })
                })
                .collect::<Vec<_>>();
            let is_streaming = state
                .get("isStreaming")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let is_compacting = state
                .get("isCompacting")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let is_bash_running = state
                .get("isBashRunning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let cwd = state.get("cwd").and_then(Value::as_str).map_or_else(
                || {
                    std::env::current_dir()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                },
                str::to_owned,
            );
            restart_sessions.push(json!({
                "activeSessionId": session.session_id,
                "sessionId": public_session_id_from_path(&session_file)
                    .unwrap_or_else(|| session.session_id.clone()),
                "sessionFile": session_file,
                "cwd": cwd,
                "config": {"cwd": cwd},
                "runtimeMetadata": {"kind": "top-level"},
                "queue": {
                    "actions": {"formatVersion": 1, "actions": []},
                    "nextTurn": pending
                },
                "shouldResume": is_streaming || is_compacting || is_bash_running || !pending.is_empty(),
                "wasStreaming": is_streaming,
                "wasCompacting": is_compacting,
                "wasBashRunning": is_bash_running,
                "hadRunningRlmChildren": false,
                "wasRetrying": false,
                "hadAcceptedPromptInFlight": false
            }));
        }
        let manifest = json!({
            "formatVersion": 1,
            "createdAt": Utc::now().to_rfc3339(),
            "sessions": restart_sessions,
            "discardedActiveSessionIds": discarded
        });
        let path = self.config.state_root.join("daemon-update-restart.json");
        crate::atomic::prepare_state_path(&self.config.state_root, &path)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        crate::atomic::write_json(&path, &manifest)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        Ok(manifest)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "attach assembly keeps response, coherent snapshot, and ordered chunk records in one auditable transaction"
    )]
    async fn public_attach(
        &self,
        envelope: &PublicDaemonCommandEnvelope,
        client_id: &str,
        response_command: &str,
    ) -> Result<(Option<Value>, Vec<Value>, bool), DaemonError> {
        validate_identifier("clientId", client_id)?;
        let session_id = required_string(&envelope.command, "activeSessionId")?;
        {
            let snapshot = self.snapshot.lock().await;
            if snapshot.sessions.get(session_id).is_some_and(|session| {
                session.lifecycle == "client_owned"
                    && session.owner_client_id.as_deref() != Some(client_id)
            }) {
                return Err(DaemonError::Protocol(format!(
                    "unknown session {session_id}"
                )));
            }
        }
        let resume_cursor = public_resume_cursor(&envelope.command)?;
        let response = self
            .process(ClientRequest::AttachSession {
                session_id: session_id.to_owned(),
                client_name: client_id.to_owned(),
                resume_cursor,
            })
            .await?;
        let ServerResponse::SessionAttached(attached) = response else {
            return Err(DaemonError::Protocol(
                "public attach returned an invalid internal response".into(),
            ));
        };
        let capabilities = public_capabilities(&envelope.command)?;
        let capabilities = capabilities
            .into_iter()
            .filter(|capability| {
                matches!(
                    capability.as_str(),
                    "attach_snapshot" | "event_sequence" | "chunked_snapshot"
                )
            })
            .collect::<Vec<_>>();
        let messages = self.load_public_messages(session_id).await?;
        let session_tree = self.public_session_tree(session_id).await?;
        let cursor = PublicDaemonEventCursor {
            generation: attached.snapshot.cursor.generation.clone(),
            sequence: attached.snapshot.cursor.sequence,
        };
        let summary = {
            let mut snapshot = self.snapshot.lock().await;
            let entry = snapshot
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| DaemonError::Protocol(format!("unknown session {session_id}")))?;
            entry.message_count = messages.len();
            if entry.session_path.is_none() {
                let store = FileSessionStore::create(&self.config.state_root, session_id)
                    .await
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                entry.session_path = Some(store.path().display().to_string());
            }
            let summary = public_session_summary(entry);
            self.state_store.save(&snapshot).await?;
            summary
        };
        let mut state = public_connection_state(&summary);
        if let Some(runtime_state) = self.handler.session_runtime_state(session_id).await? {
            merge_public_object(&mut state, &runtime_state)?;
        }
        let streaming_snapshot = capabilities.iter().any(|value| value == "chunked_snapshot");
        let public_snapshot = PublicDaemonSessionSnapshot {
            active_session_id: session_id.to_owned(),
            summary: summary.clone(),
            state,
            messages: if streaming_snapshot {
                Vec::new()
            } else {
                messages.clone()
            },
            session_context: None,
            session_tree: Some(session_tree),
            last_event_sequence: cursor.sequence,
            last_event_cursor: Some(cursor.clone()),
            parent: None,
            children: Vec::new(),
        };
        let mut data = json!({
            "protocol": {
                "name": PUBLIC_DAEMON_PROTOCOL_NAME,
                "version": envelope.protocol.version
            },
            "activeSessionId": session_id,
            "state": summary,
            "messages": if streaming_snapshot { Vec::new() } else { messages.clone() },
            "snapshot": public_snapshot,
            "replay": public_replay(&attached.replay),
            "lastEventSequence": cursor.sequence,
            "lastEventCursor": cursor,
            "client": {"id": client_id, "capabilities": capabilities}
        });
        let mut trailing = Vec::new();
        if streaming_snapshot {
            let snapshot_id = Uuid::new_v4().to_string();
            let chunks = public_snapshot_chunks(&messages)?;
            data["snapshotStream"] = json!({
                "id": snapshot_id,
                "messageCount": messages.len(),
                "targetChunkBytes": PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES
            });
            let mut snapshot_without_messages = serde_json::to_value(&public_snapshot)?;
            if let Some(object) = snapshot_without_messages.as_object_mut() {
                object.remove("messages");
            }
            trailing.push(serde_json::to_value(PublicDaemonSnapshotRecord::Begin {
                active_session_id: session_id.to_owned(),
                snapshot_id: snapshot_id.clone(),
                snapshot: snapshot_without_messages,
                message_count: messages.len(),
                target_chunk_bytes: PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES,
                purpose: Some(if response_command == "reattach" {
                    "replacement".into()
                } else {
                    "attach".into()
                }),
            })?);
            for (index, messages) in chunks.iter().enumerate() {
                trailing.push(serde_json::to_value(PublicDaemonSnapshotRecord::Chunk {
                    active_session_id: session_id.to_owned(),
                    snapshot_id: snapshot_id.clone(),
                    index,
                    messages: messages.clone(),
                })?);
            }
            trailing.push(serde_json::to_value(PublicDaemonSnapshotRecord::End {
                active_session_id: session_id.to_owned(),
                snapshot_id,
                chunk_count: chunks.len(),
                last_event_sequence: cursor.sequence,
                last_event_cursor: Some(cursor),
            })?);
        }
        self.ensure_public_event_subscription(session_id).await?;
        Ok((Some(data), trailing, false))
    }

    async fn prepare_public_session(
        &self,
        session_id: &str,
        source_path: Option<&str>,
    ) -> Result<String, DaemonError> {
        let store = FileSessionStore::create(&self.config.state_root, session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let destination_exists = tokio::fs::try_exists(store.path()).await?;
        if let Some(source_path) = source_path {
            let source = Path::new(source_path);
            let same_path = match (
                tokio::fs::canonicalize(source).await,
                tokio::fs::canonicalize(store.path()).await,
            ) {
                (Ok(source), Ok(destination)) => source == destination,
                _ => false,
            };
            if !same_path && !destination_exists {
                let bytes = tokio::fs::read(source).await?;
                let imported = import_jsonl(source, &bytes)
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                for record in imported.records {
                    store
                        .append(record)
                        .await
                        .map_err(|error| DaemonError::Protocol(error.to_string()))?;
                }
            } else if !same_path && destination_exists {
                // Create-with-sessionPath is intentionally idempotent. Once the
                // canonical destination exists, a later create reuses it.
            } else if !destination_exists {
                return Err(DaemonError::Protocol(format!(
                    "session path does not exist: {}",
                    source.display()
                )));
            }
        } else if !destination_exists {
            store
                .append(SessionRecord::new(SessionPayload::RuntimeEvent {
                    name: "session_created".into(),
                    detail: String::new(),
                }))
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        Ok(store.path().display().to_string())
    }

    async fn load_public_records(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionRecord>, DaemonError> {
        let durable_session_id = self
            .snapshot
            .lock()
            .await
            .sessions
            .get(session_id)
            .and_then(|session| session.session_path.as_deref())
            .and_then(public_session_id_from_path)
            .unwrap_or_else(|| session_id.to_owned());
        let store = FileSessionStore::create(&self.config.state_root, &durable_session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        Ok(store
            .load()
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
            .records)
    }

    async fn load_public_messages(&self, session_id: &str) -> Result<Vec<Value>, DaemonError> {
        if let Some(messages) = self.handler.session_messages(session_id).await? {
            return messages
                .iter()
                .map(public_message)
                .collect::<Result<Vec<_>, _>>();
        }
        self.load_public_records(session_id)
            .await?
            .into_iter()
            .filter_map(|record| match record.payload {
                SessionPayload::Message(message) => Some(public_message(&message)),
                _ => None,
            })
            .collect()
    }

    async fn public_session_header(&self, session_id: &str) -> Result<Value, DaemonError> {
        let records = self.load_public_records(session_id).await?;
        let durable_session_id = self
            .snapshot
            .lock()
            .await
            .sessions
            .get(session_id)
            .and_then(|session| session.session_path.as_deref())
            .and_then(public_session_id_from_path)
            .unwrap_or_else(|| session_id.to_owned());
        let timestamp = records.first().map_or_else(
            || Utc::now().to_rfc3339(),
            |record| record.created_at.to_rfc3339(),
        );
        let cwd = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        Ok(json!({
            "type": "session",
            "version": 1,
            "id": durable_session_id,
            "timestamp": timestamp,
            "cwd": cwd
        }))
    }

    async fn public_user_messages_for_forking(
        &self,
        session_id: &str,
    ) -> Result<Vec<Value>, DaemonError> {
        let catalog =
            SessionBranchCatalog::from_records(self.load_public_records(session_id).await?)
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        Ok(catalog
            .user_message_selectors()
            .into_iter()
            .map(|selector| {
                json!({
                    "entryId": selector.entry_id,
                    "text": selector.text
                })
            })
            .collect())
    }

    async fn public_context_tree(
        &self,
        session_id: &str,
        session: &SessionCatalogEntry,
    ) -> Result<Value, DaemonError> {
        let stats = self.public_session_stats(session_id).await?;
        let tokens = &stats["tokens"];
        let usage = json!({
            "input": tokens["input"],
            "output": tokens["output"],
            "cacheRead": tokens["cacheRead"],
            "cacheWrite": tokens["cacheWrite"],
            "totalTokens": tokens["total"],
            "cost": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "total": 0
            }
        });
        Ok(json!({
            "id": "root",
            "label": session.name.as_deref().unwrap_or("main agent"),
            "status": "active",
            "ownUsage": usage.clone(),
            "totalUsage": usage,
            "children": []
        }))
    }

    async fn public_session_tree(&self, session_id: &str) -> Result<Value, DaemonError> {
        let records = self.load_public_records(session_id).await?;
        let catalog = SessionBranchCatalog::from_records(records.clone())
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let leaf_id = catalog.leaf_id().map(|record_id| record_id.to_string());
        let mut entries = HashMap::with_capacity(records.len());
        let mut flat_nodes = Vec::with_capacity(records.len());
        for record in records {
            let mut entry = json!({
                "id": record.record_id,
                "parentId": catalog.node(record.record_id).and_then(|node| node.parent_id),
                "timestamp": record.created_at.to_rfc3339()
            });
            match record.payload {
                SessionPayload::Message(message) => {
                    entry["type"] = Value::String("message".into());
                    entry["message"] = public_message(&message)?;
                }
                SessionPayload::Compaction {
                    summary,
                    retained_message_count,
                    reason,
                    ..
                } => {
                    entry["type"] = Value::String("compaction".into());
                    entry["summary"] = Value::String(summary);
                    entry["retainedMessageCount"] = json!(retained_message_count);
                    if let Some(reason) = reason {
                        entry["reason"] = Value::String(reason);
                    }
                }
                SessionPayload::RuntimeEvent { name, detail } => {
                    entry["type"] = Value::String("custom".into());
                    entry["customType"] = Value::String(name);
                    entry["data"] = Value::String(detail);
                }
            }
            entries.insert(record.record_id, entry.clone());
            flat_nodes.push(json!({"entry": entry}));
        }
        let mut built = HashMap::with_capacity(entries.len());
        for root in catalog.roots() {
            let mut stack = vec![(*root, false)];
            while let Some((record_id, expanded)) = stack.pop() {
                let node = catalog.node(record_id).ok_or_else(|| {
                    DaemonError::Protocol(format!("session tree node {record_id} disappeared"))
                })?;
                if expanded {
                    let children = node
                        .children
                        .iter()
                        .filter_map(|child_id| built.remove(child_id))
                        .collect::<Vec<_>>();
                    let entry = entries.remove(&record_id).ok_or_else(|| {
                        DaemonError::Protocol(format!("session tree entry {record_id} disappeared"))
                    })?;
                    built.insert(record_id, json!({"entry": entry, "children": children}));
                } else {
                    stack.push((record_id, true));
                    stack.extend(
                        node.children
                            .iter()
                            .rev()
                            .map(|child_id| (*child_id, false)),
                    );
                }
            }
        }
        let tree = catalog
            .roots()
            .iter()
            .filter_map(|root| built.remove(root))
            .collect::<Vec<_>>();
        Ok(json!({
            "tree": tree,
            "flatNodes": flat_nodes,
            "leafId": leaf_id
        }))
    }

    async fn public_session_stats(&self, session_id: &str) -> Result<Value, DaemonError> {
        let messages = if let Some(messages) = self.handler.session_messages(session_id).await? {
            messages
        } else {
            self.load_public_records(session_id)
                .await?
                .into_iter()
                .filter_map(|record| match record.payload {
                    SessionPayload::Message(message) => Some(message),
                    _ => None,
                })
                .collect()
        };
        let mut user_messages = 0_usize;
        let mut assistant_messages = 0_usize;
        let mut tool_results = 0_usize;
        let mut tool_calls = 0_usize;
        let mut input = 0_u64;
        let mut output = 0_u64;
        let mut cache_read = 0_u64;
        let mut total_messages = 0_usize;
        for message in &messages {
            total_messages = total_messages.saturating_add(1);
            match message.role {
                Role::User => user_messages = user_messages.saturating_add(1),
                Role::Assistant => assistant_messages = assistant_messages.saturating_add(1),
                Role::Tool => tool_results = tool_results.saturating_add(1),
                Role::System => {}
            }
            tool_calls = tool_calls.saturating_add(
                message
                    .content
                    .iter()
                    .filter(|content| matches!(content, Content::ToolCall(_)))
                    .count(),
            );
            input = input.saturating_add(message.usage.input_tokens);
            output = output.saturating_add(message.usage.output_tokens);
            cache_read = cache_read.saturating_add(message.usage.cached_tokens);
        }
        let store = FileSessionStore::create(&self.config.state_root, session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        Ok(json!({
            "sessionFile": store.path(),
            "sessionId": session_id,
            "userMessages": user_messages,
            "assistantMessages": assistant_messages,
            "toolCalls": tool_calls,
            "toolResults": tool_results,
            "totalMessages": total_messages,
            "tokens": {
                "input": input,
                "output": output,
                "cacheRead": cache_read,
                "cacheWrite": 0,
                "total": input.saturating_add(output).saturating_add(cache_read)
            },
            "cost": 0
        }))
    }

    async fn process_public_prompt(
        &self,
        lease_id: Uuid,
        session_id: &str,
        message: Message,
    ) -> Result<String, DaemonError> {
        {
            let mut snapshot = self.snapshot.lock().await;
            snapshot.expire_leases(now_ms());
            let session = snapshot
                .sessions
                .get(session_id)
                .ok_or_else(|| DaemonError::Protocol(format!("unknown session {session_id}")))?;
            if !session
                .active_leases
                .iter()
                .any(|lease| lease.lease_id == lease_id)
            {
                return Err(DaemonError::Protocol(format!(
                    "lease {lease_id} is not attached to {session_id}"
                )));
            }
        }
        let output = self
            .handler
            .handle_prompt_message(
                PromptRequest {
                    lease_id,
                    session_id: session_id.to_owned(),
                    prompt: message.text(),
                },
                message.clone(),
            )
            .await?;
        if self.handler.session_messages(session_id).await?.is_none() {
            let store = FileSessionStore::create(&self.config.state_root, session_id)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            store
                .append(SessionRecord::new(SessionPayload::Message(message)))
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            store
                .append(SessionRecord::new(SessionPayload::Message(
                    Message::assistant(
                        vec![Content::Text {
                            text: output.clone(),
                        }],
                        StopReason::Stop,
                    ),
                )))
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        let now = now_ms();
        let mut snapshot = self.snapshot.lock().await;
        snapshot.last_seen_at_ms = now;
        snapshot.session_mut(session_id, now).last_prompt_at_ms = Some(now);
        self.state_store.save(&snapshot).await?;
        drop(snapshot);
        self.replay
            .lock()
            .await
            .record(session_id, DaemonSessionEventKind::PromptCompleted, now);
        Ok(output)
    }

    async fn ensure_public_event_subscription(&self, session_id: &str) -> Result<(), DaemonError> {
        {
            let subscriptions = self.runtime_event_sessions.lock().await;
            if subscriptions.contains(session_id) {
                return Ok(());
            }
        }
        let Some(mut runtime_events) = self.handler.subscribe_session_events(session_id).await?
        else {
            return Ok(());
        };
        {
            let mut subscriptions = self.runtime_event_sessions.lock().await;
            if !subscriptions.insert(session_id.to_owned()) {
                return Ok(());
            }
        }

        let session_id = session_id.to_owned();
        let events = self.public_events.clone();
        let replay = self.replay.clone();
        let subscriptions = self.runtime_event_sessions.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut streamed_text = String::new();
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    next_event = runtime_events.recv() => match next_event {
                        Ok(envelope) => {
                            let Ok(Some((event, runtime_sequence))) =
                                public_runtime_event(envelope, &mut streamed_text)
                            else {
                                continue;
                            };
                            let cursor = replay.lock().await.record(
                                &session_id,
                                DaemonSessionEventKind::RuntimeEventPublished,
                                now_ms(),
                            );
                            let _ = events.send(public_session_event_frame(
                                &session_id,
                                &event,
                                &cursor,
                                Some(runtime_sequence),
                                false,
                            ));
                        }
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            let cursor = replay.lock().await.cursor(&session_id);
                            let event = json!({
                                "type": "runtime_event_lagged",
                                "droppedEventCount": dropped
                            });
                            let _ = events.send(public_session_event_frame(
                                &session_id,
                                &event,
                                &cursor,
                                None,
                                true,
                            ));
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            subscriptions.lock().await.remove(&session_id);
        });
        Ok(())
    }

    async fn publish_public_turn(&self, session_id: &str, message: Option<Value>) {
        if self
            .runtime_event_sessions
            .lock()
            .await
            .contains(session_id)
        {
            return;
        }
        let Some(message) = message else {
            return;
        };
        let cursor = self.replay.lock().await.cursor(session_id);
        let emitted_at = Utc::now().to_rfc3339();
        let meta = json!({
            "id": format!("{}:{}", session_id, cursor.sequence),
            "protocol": {
                "name": PUBLIC_DAEMON_PROTOCOL_NAME,
                "version": PUBLIC_DAEMON_PROTOCOL_MAX_VERSION
            },
            "activeSessionId": session_id,
            "sequence": cursor.sequence,
            "cursor": cursor,
            "emittedAt": emitted_at
        });
        let _ = self.public_events.send(json!({
            "type": "session_event",
            "activeSessionId": session_id,
            "event": {"type": "turn_end", "message": message},
            "meta": meta
        }));
    }

    async fn public_detach(
        &self,
        client_id: &str,
        active_session_id: Option<&str>,
    ) -> Result<usize, DaemonError> {
        let now = now_ms();
        let mut snapshot = self.snapshot.lock().await;
        let mut detached = 0;
        for (session_id, session) in &mut snapshot.sessions {
            if active_session_id.is_some_and(|requested| requested != session_id) {
                continue;
            }
            let before = session.active_leases.len();
            session
                .active_leases
                .retain(|lease| lease.client_name != client_id);
            detached += before.saturating_sub(session.active_leases.len());
            if before != session.active_leases.len() {
                session.last_detached_at_ms = Some(now);
            }
        }
        self.state_store.save(&snapshot).await?;
        Ok(detached)
    }

    async fn public_lease(&self, client_id: &str, session_id: &str) -> Result<Uuid, DaemonError> {
        let snapshot = self.snapshot.lock().await;
        snapshot
            .sessions
            .get(session_id)
            .and_then(|session| {
                session
                    .active_leases
                    .iter()
                    .find(|lease| lease.client_name == client_id)
            })
            .map(|lease| lease.lease_id)
            .ok_or_else(|| {
                DaemonError::Protocol(format!(
                    "session {session_id} is not attached by client {client_id}"
                ))
            })
    }

    async fn public_session_access(
        &self,
        client_id: &str,
        session_id: &str,
    ) -> Result<SessionCatalogEntry, DaemonError> {
        let snapshot = self.snapshot.lock().await;
        let session = snapshot
            .sessions
            .get(session_id)
            .filter(|session| {
                session.lifecycle != "client_owned"
                    || session.owner_client_id.as_deref() == Some(client_id)
            })
            .ok_or_else(|| DaemonError::Protocol(format!("unknown session {session_id}")))?;
        Ok(session.clone())
    }

    async fn public_bound_session_command(
        &self,
        client_id: &str,
        command: &PublicDaemonCommand,
    ) -> Result<PublicDaemonCommand, DaemonError> {
        let active_session_id = required_string(command, "activeSessionId")?;
        let session = self
            .public_session_access(client_id, active_session_id)
            .await?;
        let durable_session_id = session
            .session_path
            .as_deref()
            .and_then(public_session_id_from_path)
            .unwrap_or_else(|| active_session_id.to_owned());
        PublicDaemonCommand::new(
            command.command_type(),
            command
                .clone()
                .into_fields()
                .into_iter()
                .filter(|(key, _)| key != "type" && key != "activeSessionId")
                .chain([("activeSessionId".into(), Value::String(durable_session_id))]),
        )
        .map_err(DaemonError::Protocol)
    }

    async fn public_rebind_session(
        &self,
        active_session_id: &str,
        data: &Value,
    ) -> Result<(), DaemonError> {
        let durable_session_id = data
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Protocol("session operation omitted sessionId".into()))?;
        let session_file = data
            .get("sessionFile")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Protocol("session operation omitted sessionFile".into()))?;
        let store = FileSessionStore::create(&self.config.state_root, durable_session_id)
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let message_count = store
            .load()
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
            .records
            .iter()
            .filter(|record| matches!(record.payload, SessionPayload::Message(_)))
            .count();
        self.handler
            .rebind_session(active_session_id, durable_session_id)
            .await?;
        let mut snapshot = self.snapshot.lock().await;
        let session = snapshot
            .sessions
            .get_mut(active_session_id)
            .ok_or_else(|| DaemonError::Protocol(format!("unknown session {active_session_id}")))?;
        session.session_path = Some(session_file.to_owned());
        session.message_count = message_count;
        self.state_store.save(&snapshot).await
    }

    async fn public_assert_saved_session_inactive(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<(), DaemonError> {
        let requested = tokio::fs::canonicalize(required_string(command, "sessionPath")?).await?;
        let active_paths = self
            .snapshot
            .lock()
            .await
            .sessions
            .values()
            .filter_map(|session| session.session_path.clone())
            .collect::<Vec<_>>();
        for path in active_paths {
            if tokio::fs::canonicalize(path)
                .await
                .is_ok_and(|path| path == requested)
            {
                return Err(DaemonError::Protocol(
                    "cannot delete a session transcript bound to an active daemon session".into(),
                ));
            }
        }
        Ok(())
    }

    async fn public_visible_session_ids(&self, client_id: &str) -> BTreeSet<String> {
        let snapshot = self.snapshot.lock().await;
        snapshot
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.lifecycle != "client_owned"
                    || session.owner_client_id.as_deref() == Some(client_id)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    async fn first_public_session(
        &self,
        client_id: &str,
        excluding: Option<&str>,
    ) -> Result<String, DaemonError> {
        let snapshot = self.snapshot.lock().await;
        snapshot
            .sessions
            .iter()
            .find(|(session_id, session)| {
                excluding.is_none_or(|excluded| excluded != session_id.as_str())
                    && session
                        .active_leases
                        .iter()
                        .any(|lease| lease.client_name == client_id)
            })
            .map(|(session_id, _)| session_id.clone())
            .ok_or_else(|| {
                DaemonError::Protocol(format!("client {client_id} has no attached source session"))
            })
    }

    fn agent_message_safety_status(&self) -> AgentMessageSafetyStatus {
        AgentMessageSafetyStatus {
            paused: self.agent_messages_paused.load(Ordering::Acquire),
            max_message_chars: 16_384,
            max_pending_per_session: 20,
            rate_limit_capacity: AGENT_MESSAGE_RATE_CAPACITY,
            rate_limit_refill_ms: AGENT_MESSAGE_RATE_REFILL_MS,
        }
    }

    async fn consume_agent_message_rate(
        &self,
        from_session_id: &str,
        target_session_id: &str,
        now: u64,
    ) -> Result<(), DaemonError> {
        let key = format!("{from_session_id}->{target_session_id}");
        let mut rates = self.agent_message_rate.lock().await;
        let bucket = rates.entry(key).or_insert(AgentMessageRateBucket {
            tokens: AGENT_MESSAGE_RATE_CAPACITY,
            updated_at_ms: now,
        });
        let elapsed = now.saturating_sub(bucket.updated_at_ms);
        let refilled =
            usize::try_from(elapsed / AGENT_MESSAGE_RATE_REFILL_MS).unwrap_or(usize::MAX);
        if refilled > 0 {
            bucket.tokens = bucket
                .tokens
                .saturating_add(refilled)
                .min(AGENT_MESSAGE_RATE_CAPACITY);
            bucket.updated_at_ms = bucket.updated_at_ms.saturating_add(
                u64::try_from(refilled)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(AGENT_MESSAGE_RATE_REFILL_MS),
            );
        }
        if bucket.tokens == 0 {
            let retry_after = bucket
                .updated_at_ms
                .saturating_add(AGENT_MESSAGE_RATE_REFILL_MS)
                .saturating_sub(now)
                .max(1);
            return Err(DaemonError::Protocol(format!(
                "agent messaging rate limit exceeded; retry after {retry_after}ms"
            )));
        }
        bucket.tokens -= 1;
        Ok(())
    }

    async fn refund_agent_message_rate(&self, from_session_id: &str, target_session_id: &str) {
        let key = format!("{from_session_id}->{target_session_id}");
        let mut rates = self.agent_message_rate.lock().await;
        if let Some(bucket) = rates.get_mut(&key) {
            bucket.tokens = bucket
                .tokens
                .saturating_add(1)
                .min(AGENT_MESSAGE_RATE_CAPACITY);
        }
    }

    async fn clear_agent_message_rate_for_target(&self, target_session_id: &str) {
        let suffix = format!("->{target_session_id}");
        self.agent_message_rate
            .lock()
            .await
            .retain(|key, _| !key.ends_with(&suffix));
    }

    async fn process_due_schedules(&self) -> Result<(), DaemonError> {
        let Ok(_guard) = self.schedule_runner.try_lock() else {
            return Ok(());
        };
        let due = self
            .schedule_store
            .due(Utc::now())
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        for schedule in due {
            let delivery = match (schedule.source, schedule.delivery_mode) {
                (ScheduleSource::Heartbeat, Some(HeartbeatDeliveryMode::FollowUp)) => {
                    ScheduledPromptDelivery::FollowUp
                }
                (ScheduleSource::Heartbeat, _) => ScheduledPromptDelivery::Steer,
                (ScheduleSource::Cron, _) => ScheduledPromptDelivery::NewTurn,
            };
            if self
                .handler
                .handle_scheduled_prompt(
                    PromptRequest {
                        lease_id: Uuid::nil(),
                        session_id: schedule.session_id.clone(),
                        prompt: schedule.prompt.clone(),
                    },
                    delivery,
                )
                .await
                .is_err()
            {
                continue;
            }
            self.schedule_store
                .mark_run(schedule.id)
                .await
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            let now = now_ms();
            let mut snapshot = self.snapshot.lock().await;
            snapshot.expire_leases(now);
            snapshot.last_seen_at_ms = now;
            let session = snapshot.session_mut(&schedule.session_id, now);
            session.last_prompt_at_ms = Some(now);
            self.state_store.save(&snapshot).await?;
            self.replay.lock().await.record(
                &schedule.session_id,
                DaemonSessionEventKind::PromptCompleted,
                now,
            );
        }
        Ok(())
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive runtime-to-reference event mapping is intentionally linear and field-explicit"
)]
fn public_runtime_event(
    envelope: RuntimeEventEnvelope,
    streamed_text: &mut String,
) -> Result<Option<(Value, u64)>, DaemonError> {
    let sequence = envelope.sequence;
    let event = match envelope.event {
        RuntimeEvent::RunStarted => json!({"type": "agent_start"}),
        RuntimeEvent::ProviderRequest { turn } => json!({
            "type": "turn_start",
            "turnIndex": turn,
            "timestamp": now_ms()
        }),
        RuntimeEvent::MessageStarted { message } => {
            if message.role == Role::Assistant {
                streamed_text.clear();
            }
            json!({"type": "message_start", "message": public_message(&message)?})
        }
        RuntimeEvent::MessageCompleted { message } => {
            if message.role == Role::Assistant {
                streamed_text.clear();
            }
            json!({"type": "message_end", "message": public_message(&message)?})
        }
        RuntimeEvent::TurnCompleted {
            message,
            tool_results,
        } => json!({
            "type": "turn_end",
            "turnIndex": 0,
            "message": public_message(&message)?,
            "toolResults": tool_results
                .iter()
                .map(public_message)
                .collect::<Result<Vec<_>, _>>()?
        }),
        RuntimeEvent::ToolStarted {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_execution_start",
            "toolCallId": id,
            "toolName": name,
            "args": arguments
        }),
        RuntimeEvent::PermissionRequested { request } => json!({
            "type": "workspace_permission_required",
            "request": request
        }),
        RuntimeEvent::ToolUpdated {
            id,
            name,
            arguments,
            observation,
        } => json!({
            "type": "tool_execution_update",
            "toolCallId": id,
            "toolName": name,
            "args": arguments,
            "partialResult": observation
        }),
        RuntimeEvent::ToolFinished {
            id,
            name,
            observation,
        } => {
            let is_error = matches!(observation.status, crate::tools::ObservationStatus::Error);
            json!({
                "type": "tool_execution_end",
                "toolCallId": id,
                "toolName": name,
                "result": observation,
                "isError": is_error
            })
        }
        RuntimeEvent::TextDelta { text } => {
            streamed_text.push_str(&text);
            let partial = json!({
                "role": "assistant",
                "content": [{"type": "text", "text": streamed_text}],
                "stopReason": null,
                "usage": {
                    "input": 0,
                    "output": 0,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 0,
                    "cost": {
                        "input": 0,
                        "output": 0,
                        "cacheRead": 0,
                        "cacheWrite": 0,
                        "total": 0
                    }
                }
            });
            json!({
                "type": "message_update",
                "message": partial.clone(),
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "contentIndex": 0,
                    "delta": text,
                    "partial": partial
                }
            })
        }
        RuntimeEvent::AutoRetryStarted {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message
        }),
        RuntimeEvent::AutoRetryFinished {
            success,
            attempt,
            final_error,
        } => json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": attempt,
            "finalError": final_error
        }),
        RuntimeEvent::Completed { text } => {
            json!({"type": "agent_end", "messages": [], "text": text})
        }
        RuntimeEvent::Failed { message } => {
            json!({"type": "agent_end", "messages": [], "errorMessage": message})
        }
        RuntimeEvent::ExtensionUi { extension, request } => json!({
            "type": "extension_ui_request",
            "extension": extension,
            "request": request
        }),
        RuntimeEvent::ExtensionRendered { custom_type, lines } => json!({
            "type": "extension_rendered",
            "customType": custom_type,
            "lines": lines
        }),
        RuntimeEvent::ExtensionError {
            extension_path,
            event,
            error,
        } => json!({
            "type": "extension_error",
            "extensionPath": extension_path,
            "event": event,
            "error": error
        }),
        RuntimeEvent::SessionEvent { event } => {
            if !event.is_object() {
                return Ok(None);
            }
            event
        }
    };
    Ok(Some((event, sequence)))
}

fn public_session_event_frame(
    session_id: &str,
    event: &Value,
    cursor: &DaemonEventCursor,
    runtime_sequence: Option<u64>,
    resync_required: bool,
) -> Value {
    json!({
        "type": "session_event",
        "activeSessionId": session_id,
        "event": event,
        "meta": {
            "id": format!("{}:{}", session_id, cursor.sequence),
            "protocol": {
                "name": PUBLIC_DAEMON_PROTOCOL_NAME,
                "version": PUBLIC_DAEMON_PROTOCOL_MAX_VERSION
            },
            "activeSessionId": session_id,
            "sequence": cursor.sequence,
            "cursor": cursor,
            "runtimeEventSequence": runtime_sequence,
            "emittedAt": Utc::now().to_rfc3339(),
            "resyncRequired": resync_required
        }
    })
}

fn public_success(id: &str, command: &str, data: Option<Value>) -> Value {
    let mut response = json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": true
    });
    if let Some(data) = data {
        response["data"] = data;
    }
    response
}

fn public_daemon_hello(core: &ServerCore, client_id: &str) -> Value {
    json!({
        "type": "daemon_hello",
        "socketPath": core.config.socket_path,
        "protocol": {
            "name": PUBLIC_DAEMON_PROTOCOL_NAME,
            "version": PUBLIC_DAEMON_PROTOCOL_MAX_VERSION
        },
        "schemaId": "protocol-7-schema-13-816309b1cd50",
        "schemaRevision": 13,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "supervisorGeneration": core.supervisor_generation,
        "supervisorPid": std::process::id(),
        "supervisorSocketPath": core.config.socket_path,
        "clientId": client_id,
        "serverCapabilities": [
            "attach_snapshot",
            "event_sequence",
            "chunked_snapshot",
            "heartbeat_catalog",
            "heartbeat_management"
        ]
    })
}

enum PublicCommandRouting {
    Attach(String),
    Reattach { previous: String, target: String },
    Detach(Option<String>),
    None,
}

fn public_command_routing(envelope: &PublicDaemonCommandEnvelope) -> PublicCommandRouting {
    match envelope.command.command_type() {
        "attach" => envelope
            .command
            .field("activeSessionId")
            .and_then(Value::as_str)
            .map_or(PublicCommandRouting::None, |session_id| {
                PublicCommandRouting::Attach(session_id.to_owned())
            }),
        "reattach" => match (
            envelope
                .command
                .field("activeSessionId")
                .and_then(Value::as_str),
            envelope
                .command
                .field("targetActiveSessionId")
                .and_then(Value::as_str),
        ) {
            (Some(previous), Some(target)) => PublicCommandRouting::Reattach {
                previous: previous.to_owned(),
                target: target.to_owned(),
            },
            _ => PublicCommandRouting::None,
        },
        "detach" => PublicCommandRouting::Detach(
            envelope
                .command
                .field("activeSessionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        _ => PublicCommandRouting::None,
    }
}

fn apply_public_routing(attached: &mut BTreeSet<String>, routing: PublicCommandRouting) {
    match routing {
        PublicCommandRouting::Attach(session_id) => {
            attached.insert(session_id);
        }
        PublicCommandRouting::Reattach { previous, target } => {
            attached.remove(&previous);
            attached.insert(target);
        }
        PublicCommandRouting::Detach(Some(session_id)) => {
            attached.remove(&session_id);
        }
        PublicCommandRouting::Detach(None) => attached.clear(),
        PublicCommandRouting::None => {}
    }
}

fn public_failure(id: &str, command: &str, code: &str, error: &str) -> Value {
    json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": false,
        "error": error,
        "errorInfo": {"code": code}
    })
}

fn required_string<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a str, DaemonError> {
    command
        .field(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a non-empty string")))
}

fn public_prompt_message(command: &PublicDaemonCommand) -> Result<Message, DaemonError> {
    let mut content = Vec::new();
    if let Some(blocks) = command.field("content") {
        let blocks = blocks
            .as_array()
            .ok_or_else(|| DaemonError::Protocol("content must be an array".into()))?;
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                        DaemonError::Protocol("text content requires text".into())
                    })?;
                    content.push(Content::Text { text: text.into() });
                }
                Some("image") => content.push(public_image_content(block.clone())?),
                Some(other) => {
                    return Err(DaemonError::Protocol(format!(
                        "unsupported public prompt content type {other}"
                    )));
                }
                None => {
                    return Err(DaemonError::Protocol(
                        "prompt content requires a string type".into(),
                    ));
                }
            }
        }
    } else {
        if let Some(message) = command.field("message") {
            let message = message
                .as_str()
                .ok_or_else(|| DaemonError::Protocol("message must be a string".into()))?;
            if !message.trim().is_empty() {
                content.push(Content::Text {
                    text: message.to_owned(),
                });
            }
        }
        if let Some(images) = command.field("images") {
            let images = images
                .as_array()
                .ok_or_else(|| DaemonError::Protocol("images must be an array".into()))?;
            for image in images {
                content.push(public_image_content(image.clone())?);
            }
        }
    }
    if content.is_empty() {
        return Err(DaemonError::Protocol(
            "prompt requires text or at least one image".into(),
        ));
    }
    Ok(Message::user_content(content))
}

fn validate_queued_prompt_features(command: &PublicDaemonCommand) -> Result<(), DaemonError> {
    let expand = match command.field("expandPromptTemplates") {
        None | Some(Value::Bool(true)) => true,
        Some(Value::Bool(false)) => false,
        Some(_) => {
            return Err(DaemonError::Protocol(
                "expandPromptTemplates must be a boolean".into(),
            ));
        }
    };
    for field in ["queueKey", "agentMessageId"] {
        if let Some(value) = command.field(field) {
            let value = value
                .as_str()
                .ok_or_else(|| DaemonError::Protocol(format!("{field} must be a string")))?;
            if value.trim().is_empty() || value.len() > 128 {
                return Err(DaemonError::Protocol(format!(
                    "{field} must contain 1 to 128 bytes"
                )));
            }
        }
    }
    let replay_fields = ["content", "customMessage", "prefixMessages"]
        .into_iter()
        .filter(|field| command.field(field).is_some())
        .collect::<Vec<_>>();
    if expand && !replay_fields.is_empty() {
        return Err(DaemonError::Protocol(format!(
            "{} replay fields ({}) require expandPromptTemplates=false",
            command.command_type(),
            replay_fields.join(", ")
        )));
    }
    if let Some(custom) = command.field("customMessage") {
        validate_custom_message(custom)?;
    }
    if let Some(prefixes) = command.field("prefixMessages") {
        let prefixes = prefixes
            .as_array()
            .ok_or_else(|| DaemonError::Protocol("prefixMessages must be an array".into()))?;
        if prefixes.len() > 64 {
            return Err(DaemonError::Protocol(
                "prefixMessages exceeds the 64-message limit".into(),
            ));
        }
        for prefix in prefixes {
            validate_custom_message(prefix)?;
        }
    }
    if serde_json::to_vec(command)?.len() > 2 * 1024 * 1024 {
        return Err(DaemonError::Protocol(
            "queued prompt payload exceeds the 2 MiB limit".into(),
        ));
    }
    Ok(())
}

fn validate_custom_message(value: &Value) -> Result<(), DaemonError> {
    let object = value
        .as_object()
        .ok_or_else(|| DaemonError::Protocol("custom message must be an object".into()))?;
    if object.get("role").and_then(Value::as_str) != Some("custom") {
        return Err(DaemonError::Protocol(
            "custom message role must be custom".into(),
        ));
    }
    let custom_type = object
        .get("customType")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 128)
        .ok_or_else(|| DaemonError::Protocol("customType must contain 1 to 128 bytes".into()))?;
    let _ = custom_type;
    if object.get("display").and_then(Value::as_bool).is_none()
        || object.get("timestamp").and_then(Value::as_f64).is_none()
    {
        return Err(DaemonError::Protocol(
            "custom message requires boolean display and numeric timestamp".into(),
        ));
    }
    match object.get("content") {
        Some(Value::String(_)) => {}
        Some(Value::Array(blocks)) if blocks.len() <= 128 => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") if block.get("text").and_then(Value::as_str).is_some() => {}
                    Some("image") => {
                        public_image_content(block.clone())?;
                    }
                    _ => {
                        return Err(DaemonError::Protocol(
                            "custom message content contains an invalid block".into(),
                        ));
                    }
                }
            }
        }
        _ => {
            return Err(DaemonError::Protocol(
                "custom message content must be text or bounded content blocks".into(),
            ));
        }
    }
    Ok(())
}

fn public_image_content(value: Value) -> Result<Content, DaemonError> {
    let image: PublicImageContent = serde_json::from_value(value)?;
    image.validate().map_err(DaemonError::Protocol)?;
    Ok(Content::Image {
        data: image.data,
        mime_type: image.mime_type,
    })
}

fn public_message(message: &Message) -> Result<Value, DaemonError> {
    if message.role == Role::Tool
        && let Some(Content::ToolResult(result)) = message.content.first()
    {
        return Ok(json!({
            "role": "toolResult",
            "toolCallId": result.tool_call_id,
            "toolName": result.tool_name,
            "content": [{"type": "text", "text": result.content}],
            "isError": result.is_error,
            "timestamp": message.timestamp_ms
        }));
    }
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "toolResult",
    };
    let content = message
        .content
        .iter()
        .map(|content| match content {
            Content::Text { text } => json!({"type": "text", "text": text}),
            Content::Image { data, mime_type } => {
                json!({"type": "image", "data": data, "mimeType": mime_type})
            }
            Content::Thinking {
                text,
                signature,
                redacted,
            } => {
                if *redacted {
                    json!({"type": "redacted_thinking", "data": signature})
                } else {
                    json!({"type": "thinking", "thinking": text, "signature": signature})
                }
            }
            Content::ToolCall(call) => json!({
                "type": "toolCall",
                "id": call.id,
                "name": call.name,
                "arguments": call.arguments
            }),
            Content::ToolResult(result) => json!({
                "type": "toolResult",
                "toolCallId": result.tool_call_id,
                "toolName": result.tool_name,
                "content": result.content,
                "isError": result.is_error
            }),
        })
        .collect::<Vec<_>>();
    let mut value = json!({
        "role": role,
        "content": content,
        "timestamp": message.timestamp_ms
    });
    if let Some(stop_reason) = message.stop_reason {
        value["stopReason"] = serde_json::to_value(stop_reason)?;
    }
    Ok(value)
}

fn public_assistant_text(message: &Value) -> Option<String> {
    (message.get("role").and_then(Value::as_str) == Some("assistant")).then(|| {
        message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn public_snapshot_chunks(messages: &[Value]) -> Result<Vec<Vec<Value>>, DaemonError> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_bytes = 0_usize;
    for message in messages {
        let message_bytes = serde_json::to_vec(message)?.len();
        let additional = message_bytes.saturating_add(1);
        if !chunk.is_empty()
            && chunk_bytes.saturating_add(additional) > PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES
        {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 0;
        }
        chunk.push(message.clone());
        chunk_bytes = chunk_bytes.saturating_add(additional);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn optional_string(
    command: &PublicDaemonCommand,
    field: &str,
) -> Result<Option<String>, DaemonError> {
    match command.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().into())),
        Some(Value::String(_)) => Err(DaemonError::Protocol(format!("{field} must not be blank"))),
        Some(_) => Err(DaemonError::Protocol(format!("{field} must be a string"))),
    }
}

fn optional_bool(command: &PublicDaemonCommand, field: &str) -> Result<Option<bool>, DaemonError> {
    match command.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(DaemonError::Protocol(format!("{field} must be a boolean"))),
    }
}

fn required_uuid(command: &PublicDaemonCommand, field: &str) -> Result<Uuid, DaemonError> {
    Uuid::parse_str(required_string(command, field)?)
        .map_err(|error| DaemonError::Protocol(format!("invalid {field}: {error}")))
}

fn parse_heartbeat_delivery_mode(value: &str) -> Result<HeartbeatDeliveryMode, DaemonError> {
    match value {
        "steer" => Ok(HeartbeatDeliveryMode::Steer),
        "follow_up" => Ok(HeartbeatDeliveryMode::FollowUp),
        _ => Err(DaemonError::Protocol(
            "deliveryMode must be steer or follow_up".into(),
        )),
    }
}

fn parse_heartbeat_update_action(value: &str) -> Result<HeartbeatManagementAction, DaemonError> {
    match value {
        "pause" => Ok(HeartbeatManagementAction::Pause),
        "resume" => Ok(HeartbeatManagementAction::Resume),
        "clear" => Ok(HeartbeatManagementAction::Stop),
        _ => Err(DaemonError::Protocol(
            "heartbeat update action must be pause, resume, or clear".into(),
        )),
    }
}

fn parse_heartbeat_management_action(
    value: &str,
) -> Result<HeartbeatManagementAction, DaemonError> {
    match value {
        "pause" => Ok(HeartbeatManagementAction::Pause),
        "resume" => Ok(HeartbeatManagementAction::Resume),
        "stop" => Ok(HeartbeatManagementAction::Stop),
        _ => Err(DaemonError::Protocol(
            "heartbeat management action must be pause, resume, or stop".into(),
        )),
    }
}

fn public_schedule(schedule: &Schedule, state_root: &Path) -> Value {
    let status = if schedule.cancelled {
        "cancelled"
    } else if schedule.paused {
        "paused"
    } else if schedule.enabled {
        "active"
    } else {
        "completed"
    };
    let kind = match schedule.schedule_kind {
        crate::orchestration::ScheduleKind::Once => "once",
        crate::orchestration::ScheduleKind::Cron => "cron",
        crate::orchestration::ScheduleKind::Interval => "interval",
    };
    let source = match schedule.source {
        ScheduleSource::Cron => "cron",
        ScheduleSource::Heartbeat => "heartbeat",
    };
    let delivery_mode = schedule.delivery_mode.map(|mode| match mode {
        HeartbeatDeliveryMode::Steer => "steer",
        HeartbeatDeliveryMode::FollowUp => "follow_up",
    });
    let session_file = crate::atomic::canonical_state_root(state_root)
        .join("sessions")
        .join(format!("{}.jsonl", schedule.session_id));
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    json!({
        "id": schedule.id,
        "status": status,
        "source": source,
        "runtimeKind": "top-level",
        "deliveryMode": delivery_mode,
        "activeSessionId": schedule.session_id,
        "sessionId": schedule.session_id,
        "sessionFile": session_file,
        "cwd": cwd,
        "label": schedule.name,
        "prompt": schedule.prompt,
        "schedule": {
            "kind": kind,
            "expression": schedule.schedule_expression,
            "intervalMs": schedule.every_seconds.map(|seconds| seconds.saturating_mul(1000))
        },
        "createdAt": schedule.created_at.map(|value| value.to_rfc3339()),
        "updatedAt": schedule.updated_at.map(|value| value.to_rfc3339()),
        "nextRunAt": schedule.enabled.then(|| schedule.next_run.to_rfc3339()),
        "lastRunAt": schedule.last_run.map(|value| value.to_rfc3339()),
        "runCount": schedule.run_count
    })
}

fn reject_present_fields(
    command: &PublicDaemonCommand,
    fields: &[&str],
) -> Result<(), DaemonError> {
    if let Some(field) = fields.iter().find(|field| command.field(field).is_some()) {
        return Err(DaemonError::Protocol(format!(
            "public daemon command feature '{field}' is recognized but not supported by the Rust supervisor"
        )));
    }
    Ok(())
}

fn is_public_mutating_command(command: &str) -> bool {
    !matches!(
        command,
        "ack_result"
            | "list"
            | "list_saved_sessions"
            | "attach"
            | "reattach"
            | "agent_messages_status"
            | "wait_for_idle"
            | "get_session_header"
            | "get_state"
            | "get_connection_state"
            | "get_messages"
            | "get_session_stats"
            | "get_context_tree"
            | "get_commands"
            | "get_resource_snapshot"
            | "get_model_catalog"
            | "get_available_models"
            | "get_queue"
            | "cron_list"
            | "heartbeats_list"
            | "heartbeat_get"
            | "get_session_context"
            | "get_session_tree"
            | "get_user_messages_for_forking"
            | "get_last_assistant_text"
            | "get_system_prompt"
            | "get_rlm_max_depth_status"
            | "get_tool_definition"
    )
}

fn public_capabilities(command: &PublicDaemonCommand) -> Result<Vec<String>, DaemonError> {
    let Some(value) = command.field("capabilities") else {
        return Ok(vec!["attach_snapshot".into(), "event_sequence".into()]);
    };
    let values = value
        .as_array()
        .ok_or_else(|| DaemonError::Protocol("capabilities must be an array".into()))?;
    let mut capabilities = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| DaemonError::Protocol("capabilities entries must be strings".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    capabilities.sort();
    capabilities.dedup();
    Ok(capabilities)
}

fn public_resume_cursor(
    command: &PublicDaemonCommand,
) -> Result<Option<super::protocol::DaemonEventCursor>, DaemonError> {
    let Some(value) = command.field("resumeCursor") else {
        return Ok(None);
    };
    let cursor = value
        .as_object()
        .ok_or_else(|| DaemonError::Protocol("resumeCursor must be an object".into()))?;
    let generation = cursor
        .get("generation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let sequence = cursor
        .get("sequence")
        .or_else(|| cursor.get("eventSequence"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            DaemonError::Protocol("resumeCursor requires a non-negative sequence".into())
        })?;
    Ok(Some(super::protocol::DaemonEventCursor {
        generation,
        sequence,
    }))
}

fn public_session_id_from_path(path: &str) -> Option<String> {
    let candidate = std::path::Path::new(path)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)?;
    (!candidate.is_empty()
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| candidate.to_owned())
}

fn public_session_summary(session: &SessionCatalogEntry) -> Value {
    let durable_session_id = session
        .session_path
        .as_deref()
        .and_then(public_session_id_from_path)
        .unwrap_or_else(|| session.session_id.clone());
    let mut summary = json!({
        "id": session.session_id,
        "activeSessionId": session.session_id,
        "sessionId": durable_session_id,
        "runtimeKind": "top-level",
        "lifecycle": session.lifecycle,
        "status": if session.active_leases.is_empty() { "idle" } else { "active" },
        "messageCount": session.message_count,
        "attachedClients": session.active_leases.len(),
        "sessionActions": {
            "queuedCount": 0,
            "steering": [],
            "followUps": []
        }
    });
    if let Some(name) = &session.name {
        summary["sessionName"] = Value::String(name.clone());
    }
    if let Some(path) = &session.session_path {
        summary["sessionFile"] = Value::String(path.clone());
    }
    summary
}

fn public_connection_state(summary: &Value) -> Value {
    json!({
        "activeSessionId": summary["activeSessionId"],
        "sessionId": summary["sessionId"],
        "sessionFile": summary.get("sessionFile").cloned().unwrap_or(Value::Null),
        "messageCount": summary["messageCount"],
        "model": null,
        "thinkingLevel": "off",
        "isStreaming": false,
        "isCompacting": false,
        "steeringMode": "one-at-a-time",
        "followUpMode": "one-at-a-time",
        "autoCompactionEnabled": true,
        "autoRetryEnabled": true,
        "sessionActions": {
            "queuedCount": 0,
            "steering": [],
            "followUps": []
        },
        "summary": summary
    })
}

fn merge_public_object(target: &mut Value, patch: &Value) -> Result<(), DaemonError> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| DaemonError::Protocol("public state target must be an object".into()))?;
    let patch = patch
        .as_object()
        .ok_or_else(|| DaemonError::Protocol("runtime state must be an object".into()))?;
    target.extend(patch.clone());
    Ok(())
}

fn public_replay(replay: &DaemonReplayInfo) -> Value {
    let status = match replay.status {
        DaemonReplayStatus::Complete => "complete",
        DaemonReplayStatus::Partial => "partial",
        DaemonReplayStatus::Unavailable => "unavailable",
    };
    let mut value = json!({
        "status": status,
        "toSequence": replay.to_cursor.sequence,
        "toCursor": replay.to_cursor,
    });
    if let Some(from) = &replay.from_cursor {
        value["fromSequence"] = json!(from.sequence);
        value["fromCursor"] = json!(from);
    }
    if let Some(reason) = &replay.reason {
        value["reason"] = Value::String(reason.clone());
    }
    value
}

#[cfg(unix)]
impl DaemonHarness {
    pub async fn start(
        config: DaemonConfig,
        handler: Arc<dyn PromptHandler>,
    ) -> Result<Self, DaemonError> {
        let state_store = DaemonStateStore::new(config.state_root.clone());
        let schedule_store = ScheduleStore::new(&config.state_root);
        let snapshot = initialize_snapshot(&config, &state_store).await?;
        let public_commands = PublicCommandJournal::load(&config.state_root).await?;
        let session_ops = SessionOps::new(config.state_root.clone());
        let (public_events, _) = broadcast::channel(256);
        Ok(Self {
            core: Arc::new(ServerCore {
                config,
                state_store,
                schedule_store,
                session_ops,
                turn_ops: TurnOps::new(),
                snapshot: Mutex::new(snapshot),
                schedule_runner: Mutex::new(()),
                replay: Arc::new(Mutex::new(ReplayJournal::new())),
                public_commands: Mutex::new(public_commands),
                public_events,
                runtime_event_sessions: Arc::new(Mutex::new(BTreeSet::new())),
                supervisor_generation: Uuid::new_v4().to_string(),
                agent_messages_paused: AtomicBool::new(false),
                agent_message_rate: Mutex::new(HashMap::new()),
                handler,
                shutdown: CancellationToken::new(),
            }),
        })
    }

    pub async fn request(&self, payload: ClientRequest) -> Result<ServerResponse, DaemonError> {
        let (client, server) = tokio::net::UnixStream::pair()?;
        let core = self.core.clone();
        let server_task = tokio::spawn(async move { handle_stream(server, core).await });
        let response = request_over_stream(client, payload).await?;
        server_task
            .await
            .map_err(|error| DaemonError::Protocol(error.to_string()))??;
        Ok(response)
    }

    pub async fn metadata(&self) -> Result<DaemonMetadataSnapshot, DaemonError> {
        self.core.state_store.load().await
    }

    pub async fn run_due_schedules_for_test(&self) -> Result<(), DaemonError> {
        self.core.process_due_schedules().await
    }
}

#[cfg(unix)]
impl DaemonServer {
    pub async fn spawn(
        config: DaemonConfig,
        handler: Arc<dyn PromptHandler>,
    ) -> Result<DaemonHandle, DaemonError> {
        validate_config(&config)?;
        if let Some(parent) = config.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        reclaim_stale_socket(&config.socket_path).await?;
        let listener = tokio::net::UnixListener::bind(&config.socket_path)?;

        let state_store = DaemonStateStore::new(config.state_root.clone());
        let schedule_store = ScheduleStore::new(&config.state_root);
        let snapshot = initialize_snapshot(&config, &state_store).await?;
        let public_commands = PublicCommandJournal::load(&config.state_root).await?;
        let (public_events, _) = broadcast::channel(256);

        let core = Arc::new(ServerCore {
            config: config.clone(),
            state_store,
            schedule_store,
            session_ops: SessionOps::new(config.state_root.clone()),
            turn_ops: TurnOps::new(),
            snapshot: Mutex::new(snapshot),
            schedule_runner: Mutex::new(()),
            replay: Arc::new(Mutex::new(ReplayJournal::new())),
            public_commands: Mutex::new(public_commands),
            public_events,
            runtime_event_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            supervisor_generation: Uuid::new_v4().to_string(),
            agent_messages_paused: AtomicBool::new(false),
            agent_message_rate: Mutex::new(HashMap::new()),
            handler,
            shutdown: CancellationToken::new(),
        });
        let socket_path = config.socket_path.clone();
        let core_for_task = core.clone();
        let join = tokio::spawn(async move {
            let result = tokio::try_join!(
                accept_loop(listener, core_for_task.clone()),
                schedule_loop(core_for_task.clone()),
            )
            .map(|_| ());
            let _ = tokio::fs::remove_file(&socket_path).await;
            if result.is_ok() {
                let mut snapshot = core_for_task.snapshot.lock().await;
                snapshot.last_stopped_at_ms = Some(now_ms());
                core_for_task.state_store.save(&snapshot).await?;
            }
            result
        });

        Ok(DaemonHandle {
            socket_path: config.socket_path,
            join,
        })
    }

    pub fn spawn_blocking_for_test(
        _config: DaemonConfig,
        _handler: Arc<dyn PromptHandler>,
    ) -> Result<DaemonHandle, DaemonError> {
        Err(DaemonError::UnsupportedTransport(
            "spawn_blocking_for_test is only for non-Unix builds".into(),
        ))
    }

    pub async fn prepare_socket_path_for_test(socket_path: &Path) -> Result<(), DaemonError> {
        reclaim_stale_socket(socket_path).await
    }
}

#[cfg(not(unix))]
impl DaemonServer {
    pub async fn spawn(
        _config: DaemonConfig,
        _handler: Arc<dyn PromptHandler>,
    ) -> Result<DaemonHandle, DaemonError> {
        Err(DaemonError::UnsupportedTransport(
            "Unix sockets are required".into(),
        ))
    }

    pub fn spawn_blocking_for_test(
        _config: DaemonConfig,
        _handler: Arc<dyn PromptHandler>,
    ) -> Result<DaemonHandle, DaemonError> {
        Err(DaemonError::UnsupportedTransport(
            "Unix sockets are required".into(),
        ))
    }
}

#[cfg(unix)]
async fn accept_loop(
    listener: tokio::net::UnixListener,
    core: Arc<ServerCore>,
) -> Result<(), DaemonError> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            () = core.shutdown.cancelled() => {
                connections.shutdown().await;
                return Ok(());
            },
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let core = core.clone();
                connections.spawn(async move {
                    let _ = handle_stream(stream, core).await;
                });
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

#[cfg(unix)]
async fn schedule_loop(core: Arc<ServerCore>) -> Result<(), DaemonError> {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            () = core.shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => core.process_due_schedules().await?,
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
async fn handle_stream(
    stream: tokio::net::UnixStream,
    core: Arc<ServerCore>,
) -> Result<(), DaemonError> {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let connection_client_id = Uuid::new_v4().to_string();
    let first = tokio::time::timeout(Duration::from_millis(10), read_frame(&mut reader)).await;
    let (frame, hello_sent) = if let Ok(frame) = first {
        (frame?, false)
    } else {
        write_public_frame(
            &mut writer_half,
            &public_daemon_hello(&core, &connection_client_id),
        )
        .await?;
        (read_frame(&mut reader).await?, true)
    };
    if frame.is_empty() {
        return Ok(());
    }
    let decoded: Value = serde_json::from_slice(&frame)?;
    if decoded.get("schema_version").is_none()
        && decoded.get("type").and_then(Value::as_str).is_some()
    {
        if !hello_sent {
            write_public_frame(
                &mut writer_half,
                &public_daemon_hello(&core, &connection_client_id),
            )
            .await?;
        }
        return handle_public_stream(
            &mut reader,
            &mut writer_half,
            core,
            decoded,
            &connection_client_id,
        )
        .await;
    }
    let request: RequestEnvelope = serde_json::from_value(decoded)?;
    let payload = if request.schema_version == IPC_SCHEMA_VERSION {
        match core.process(request.payload).await {
            Ok(response) => response,
            Err(error) => ServerResponse::Failure(FailureResponse {
                code: "request_failed".into(),
                message: error.to_string(),
            }),
        }
    } else {
        ServerResponse::Failure(schema_mismatch_error(request.schema_version))
    };
    let shutdown_accepted = matches!(&payload, ServerResponse::ShutdownAccepted(_));
    let encoded = serde_json::to_vec(&ResponseEnvelope {
        schema_version: IPC_SCHEMA_VERSION,
        request_id: request.request_id,
        payload,
    })?;
    ensure_frame_size(encoded.len())?;
    writer_half.write_all(&encoded).await?;
    writer_half.write_all(b"\n").await?;
    writer_half.flush().await?;
    if shutdown_accepted {
        core.shutdown.cancel();
    }
    Ok(())
}

#[cfg(unix)]
async fn handle_public_stream(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    core: Arc<ServerCore>,
    first: Value,
    connection_client_id: &str,
) -> Result<(), DaemonError> {
    let mut events = core.public_events.subscribe();
    let mut attached_sessions = BTreeSet::new();
    let mut next = Some(first);
    loop {
        if let Some(value) = next.take() {
            let decoded = decode_public_envelope(value.clone(), connection_client_id);
            let routing = decoded.as_ref().ok().map(public_command_routing);
            let dispatch = match decoded {
                Ok(envelope) => core.process_public(envelope).await,
                Err(error) => {
                    let id = value.get("id").and_then(Value::as_str).unwrap_or("unknown");
                    let command = value
                        .get("command")
                        .and_then(|command| command.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    PublicDispatch {
                        frames: vec![public_failure(
                            id,
                            command,
                            "invalid_command_envelope",
                            &error.to_string(),
                        )],
                        shutdown: false,
                    }
                }
            };
            let succeeded = dispatch
                .frames
                .first()
                .and_then(|frame| frame.get("success"))
                .and_then(Value::as_bool)
                .unwrap_or(dispatch.frames.is_empty());
            for frame in dispatch.frames {
                write_public_frame(writer, &frame).await?;
            }
            if succeeded && let Some(routing) = routing {
                apply_public_routing(&mut attached_sessions, routing);
            }
            if dispatch.shutdown {
                core.shutdown.cancel();
                return Ok(());
            }
            continue;
        }
        tokio::select! {
            frame = read_frame(reader) => {
                let frame = frame?;
                if frame.is_empty() {
                    return Ok(());
                }
                match serde_json::from_slice::<Value>(&frame) {
                    Ok(value) => next = Some(value),
                    Err(error) => {
                        write_public_frame(
                            writer,
                            &public_failure("unknown", "unknown", "invalid_json", &error.to_string()),
                        ).await?;
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) if event
                        .get("activeSessionId")
                        .and_then(Value::as_str)
                        .is_some_and(|session_id| attached_sessions.contains(session_id)) => {
                            write_public_frame(writer, &event).await?;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        for session_id in &attached_sessions {
                            let cursor = core.replay.lock().await.cursor(session_id);
                            let event = json!({
                                "type": "daemon_event_lagged",
                                "droppedEventCount": dropped
                            });
                            write_public_frame(
                                writer,
                                &public_session_event_frame(
                                    session_id,
                                    &event,
                                    &cursor,
                                    None,
                                    true,
                                ),
                            ).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

fn decode_public_envelope(
    value: Value,
    connection_client_id: &str,
) -> Result<PublicDaemonCommandEnvelope, DaemonError> {
    if value.get("type").and_then(Value::as_str) == Some("command") {
        let mut envelope = serde_json::from_value::<PublicDaemonCommandEnvelope>(value)?;
        if envelope.client_id.is_none() {
            envelope.client_id = Some(connection_client_id.to_owned());
        }
        return Ok(envelope);
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
    let client_id = value
        .get("clientId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| Some(connection_client_id.to_owned()));
    let command = serde_json::from_value::<PublicDaemonCommand>(value)?;
    PublicDaemonCommandEnvelope::new(id, client_id, PUBLIC_DAEMON_PROTOCOL_MAX_VERSION, command)
        .map_err(DaemonError::Protocol)
}

#[cfg(unix)]
async fn write_public_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &Value,
) -> Result<(), DaemonError> {
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > MAX_PUBLIC_FRAME_BYTES {
        return Err(DaemonError::Protocol(format!(
            "public daemon frame exceeds {MAX_PUBLIC_FRAME_BYTES} bytes"
        )));
    }
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(unix)]
async fn request_over_stream(
    mut stream: tokio::net::UnixStream,
    payload: ClientRequest,
) -> Result<ServerResponse, DaemonError> {
    let request_id = Uuid::new_v4();
    let encoded = serde_json::to_vec(&RequestEnvelope {
        schema_version: IPC_SCHEMA_VERSION,
        request_id,
        payload,
    })?;
    ensure_frame_size(encoded.len())?;
    stream.write_all(&encoded).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let frame = read_frame(&mut reader).await?;
    let envelope: ResponseEnvelope = serde_json::from_slice(&frame)?;
    if envelope.schema_version != IPC_SCHEMA_VERSION {
        return Err(DaemonError::Protocol(format!(
            "unexpected response schema {}",
            envelope.schema_version
        )));
    }
    if envelope.request_id != request_id {
        return Err(DaemonError::Protocol("response/request ID mismatch".into()));
    }
    match envelope.payload {
        ServerResponse::Failure(FailureResponse { code, message }) => {
            Err(DaemonError::Protocol(format!("{code}: {message}")))
        }
        response => Ok(response),
    }
}

#[cfg(unix)]
async fn initialize_snapshot(
    config: &DaemonConfig,
    state_store: &DaemonStateStore,
) -> Result<DaemonMetadataSnapshot, DaemonError> {
    validate_config(config)?;
    let mut snapshot = state_store.load().await?;
    let now = now_ms();
    snapshot.expire_leases(now);
    snapshot.server_name = config.server_name.clone();
    snapshot.socket_path = config.socket_path.display().to_string();
    snapshot.launch_count = snapshot.launch_count.saturating_add(1);
    snapshot.last_started_at_ms = now;
    snapshot.last_seen_at_ms = now;
    snapshot.last_stopped_at_ms = None;
    state_store.save(&snapshot).await?;
    Ok(snapshot)
}

#[cfg(unix)]
async fn reclaim_stale_socket(socket_path: &Path) -> Result<(), DaemonError> {
    use std::os::unix::fs::FileTypeExt;

    match tokio::fs::symlink_metadata(socket_path).await {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(DaemonError::Configuration(format!(
                    "refusing to remove non-socket daemon path: {}",
                    socket_path.display()
                )));
            }
            if tokio::net::UnixStream::connect(socket_path).await.is_ok() {
                return Err(DaemonError::Configuration(format!(
                    "daemon socket already in use: {}",
                    socket_path.display()
                )));
            }
            tokio::fs::remove_file(socket_path).await?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_frame_size(size: usize) -> Result<(), DaemonError> {
    if size > MAX_IPC_FRAME_BYTES {
        Err(DaemonError::Protocol(format!(
            "IPC frame exceeds the {MAX_IPC_FRAME_BYTES}-byte limit"
        )))
    } else {
        Ok(())
    }
}

async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, DaemonError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        ensure_frame_size(frame.len().saturating_add(take))?;
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    while frame
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        frame.pop();
    }
    Ok(frame)
}

fn validate_config(config: &DaemonConfig) -> Result<(), DaemonError> {
    if config.server_name.trim().is_empty() {
        return Err(DaemonError::Configuration(
            "server_name must not be blank".into(),
        ));
    }
    if config.lease_ttl.is_zero() {
        return Err(DaemonError::Configuration(
            "lease_ttl must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_identifier(name: &str, value: &str) -> Result<(), DaemonError> {
    if value.trim().is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DaemonError::Protocol(format!(
            "{name} must contain only letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

fn normalize_agent_message(message: &str) -> Result<String, DaemonError> {
    let message = message.trim();
    if message.is_empty() {
        return Err(DaemonError::Protocol(
            "agent session message cannot be empty".into(),
        ));
    }
    // Match the reference JavaScript `String.length` contract (UTF-16 code units).
    let chars = message.encode_utf16().count();
    if chars > 16_384 {
        return Err(DaemonError::Protocol(format!(
            "agent session message is too long: {chars} chars exceeds 16384"
        )));
    }
    Ok(message.into())
}

fn agent_message_prompt(
    from_session_id: &str,
    target_session_id: &str,
    message_id: &str,
    message: &str,
) -> String {
    format!(
        "Agent-to-agent message received.\nSource: agent_message\nFrom: active {from_session_id}, session {from_session_id}, client mimir-rpc\nTo: active {target_session_id}, session {target_session_id}\nMessage id: {message_id}\n\n{message}"
    )
}

fn parse_uuid(name: &str, value: &str) -> Result<Uuid, DaemonError> {
    Uuid::parse_str(value)
        .map_err(|error| DaemonError::Protocol(format!("invalid {name}: {error}")))
}
