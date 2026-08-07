use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{IPC_SCHEMA_VERSION, state::SessionCatalogEntry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientRequest {
    Negotiate {
        client_name: String,
        capabilities: Vec<String>,
    },
    Health,
    AttachSession {
        session_id: String,
        client_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_cursor: Option<DaemonEventCursor>,
    },
    RenewLease {
        lease_id: String,
    },
    DetachSession {
        lease_id: String,
    },
    Prompt {
        lease_id: String,
        session_id: String,
        prompt: String,
    },
    SendMessage {
        from_session_id: String,
        target_session_id: String,
        message: String,
    },
    AgentMessagesStatus,
    AgentMessagesPause,
    AgentMessagesResume,
    AgentMessagesClear {
        session_id: String,
    },
    Shutdown,
}

impl ClientRequest {
    pub fn negotiate<I, S>(client_name: &str, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let capabilities = capabilities
            .into_iter()
            .map(|value| value.as_ref().trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        Self::Negotiate {
            client_name: client_name.trim().into(),
            capabilities,
        }
    }

    #[must_use]
    pub fn attach(session_id: &str, client_name: &str) -> Self {
        Self::AttachSession {
            session_id: session_id.trim().into(),
            client_name: client_name.trim().into(),
            resume_cursor: None,
        }
    }

    #[must_use]
    pub fn attach_from(
        session_id: &str,
        client_name: &str,
        resume_cursor: DaemonEventCursor,
    ) -> Self {
        Self::AttachSession {
            session_id: session_id.trim().into(),
            client_name: client_name.trim().into(),
            resume_cursor: Some(resume_cursor),
        }
    }

    #[must_use]
    pub fn heartbeat(lease_id: &str) -> Self {
        Self::RenewLease {
            lease_id: lease_id.trim().into(),
        }
    }

    #[must_use]
    pub fn detach(lease_id: &str) -> Self {
        Self::DetachSession {
            lease_id: lease_id.trim().into(),
        }
    }

    #[must_use]
    pub fn prompt(lease_id: &str, session_id: &str, prompt: &str) -> Self {
        Self::Prompt {
            lease_id: lease_id.trim().into(),
            session_id: session_id.trim().into(),
            prompt: prompt.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerResponse {
    Negotiated(NegotiatedResponse),
    Health(DaemonHealth),
    SessionAttached(Box<SessionAttachedResponse>),
    LeaseRenewed(LeaseRenewedResponse),
    SessionDetached(SessionDetachedResponse),
    PromptCompleted(PromptCompletedResponse),
    AgentMessageSent(Box<AgentSessionMessageReceipt>),
    AgentMessagesStatus(AgentMessageSafetyStatus),
    AgentMessagesCleared(AgentMessagesClearedResponse),
    ShutdownAccepted(ShutdownAcceptedResponse),
    Failure(FailureResponse),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedResponse {
    pub schema_version: u16,
    pub server_name: String,
    pub launch_count: u64,
    pub negotiated_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHealth {
    pub server_name: String,
    pub launch_count: u64,
    pub active_sessions: usize,
    pub active_leases: usize,
    pub socket_path: String,
    pub last_started_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachedResponse {
    pub session: SessionCatalogView,
    pub lease: LeaseView,
    #[serde(default)]
    pub snapshot: DaemonAttachSnapshot,
    #[serde(default)]
    pub replay: DaemonReplayInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEventCursor {
    pub generation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonAttachSnapshot {
    pub session: SessionCatalogView,
    pub cursor: DaemonEventCursor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonReplayStatus {
    #[default]
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonSessionEventKind {
    PromptCompleted,
    RuntimeEventPublished,
    AgentMessageDelivered,
    AgentMessageQueued,
    AgentMessagesCleared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSessionEvent {
    pub cursor: DaemonEventCursor,
    pub emitted_at_ms: u64,
    pub event: DaemonSessionEventKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonReplayInfo {
    pub status: DaemonReplayStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cursor: Option<DaemonEventCursor>,
    pub to_cursor: DaemonEventCursor,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<DaemonSessionEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub resync_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRenewedResponse {
    pub lease: LeaseView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetachedResponse {
    pub session: SessionCatalogView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCompletedResponse {
    pub session_id: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageEndpoint {
    pub active_session_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub runtime_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageSender {
    pub active_session_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionMessageReceipt {
    pub id: String,
    pub source: String,
    pub target: AgentMessageEndpoint,
    pub from: AgentMessageSender,
    pub message: String,
    pub delivery_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    pub delivery_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageSafetyStatus {
    pub paused: bool,
    pub max_message_chars: usize,
    pub max_pending_per_session: usize,
    pub rate_limit_capacity: usize,
    pub rate_limit_refill_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessagesClearedResponse {
    pub cleared: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownAcceptedResponse {
    pub server_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureResponse {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogView {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub active_leases: usize,
    pub last_prompt_at_ms: Option<u64>,
}

impl From<&SessionCatalogEntry> for SessionCatalogView {
    fn from(value: &SessionCatalogEntry) -> Self {
        Self {
            session_id: value.session_id.clone(),
            name: value.name.clone(),
            active_leases: value.active_leases.len(),
            last_prompt_at_ms: value.last_prompt_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseView {
    pub lease_id: Uuid,
    pub client_name: String,
    pub attached_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub lease_id: Uuid,
    pub session_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageRequest {
    pub message_id: String,
    pub from_session_id: String,
    pub target_session_id: String,
    pub message: String,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequestEnvelope {
    pub schema_version: u16,
    pub request_id: Uuid,
    pub payload: ClientRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResponseEnvelope {
    pub schema_version: u16,
    pub request_id: Uuid,
    pub payload: ServerResponse,
}

pub(crate) fn negotiate_capabilities(
    server: &BTreeSet<String>,
    requested: &[String],
) -> Vec<String> {
    let mut negotiated: Vec<_> = requested
        .iter()
        .filter(|value| server.contains(value.as_str()))
        .cloned()
        .collect();
    negotiated.sort();
    negotiated.dedup();
    negotiated
}

pub(crate) fn schema_mismatch_error(version: u16) -> FailureResponse {
    FailureResponse {
        code: "schema_mismatch".into(),
        message: format!("unsupported IPC schema version {version}; expected {IPC_SCHEMA_VERSION}"),
    }
}
