#[cfg(unix)]
mod journal;
mod protocol;
mod public;
#[cfg(unix)]
mod replay;
pub(crate) mod runtime_ops;
mod server;
#[cfg(unix)]
pub(crate) mod session_ops;
mod state;
pub(crate) mod turn_ops;

pub use protocol::{
    AgentMessageEndpoint, AgentMessageRequest, AgentMessageSafetyStatus, AgentMessageSender,
    AgentMessagesClearedResponse, AgentSessionMessageReceipt, ClientRequest, DaemonAttachSnapshot,
    DaemonEventCursor, DaemonHealth, DaemonReplayInfo, DaemonReplayStatus, DaemonSessionEvent,
    DaemonSessionEventKind, LeaseRenewedResponse, LeaseView, NegotiatedResponse,
    PromptCompletedResponse, PromptRequest, ServerResponse, SessionAttachedResponse,
    SessionCatalogView, SessionDetachedResponse, ShutdownAcceptedResponse,
};
pub use public::{
    PUBLIC_DAEMON_PROTOCOL_MAX_VERSION, PUBLIC_DAEMON_PROTOCOL_MIN_VERSION,
    PUBLIC_DAEMON_PROTOCOL_NAME, PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES, PublicDaemonCommand,
    PublicDaemonCommandEnvelope, PublicDaemonEventCursor, PublicDaemonEventMeta,
    PublicDaemonProtocolInfo, PublicDaemonSessionSnapshot, PublicDaemonSnapshotParent,
    PublicDaemonSnapshotRecord, PublicImageContent,
};
pub use server::{
    AgentMessageDelivery, DaemonClient, DaemonConfig, DaemonError, DaemonHandle, DaemonHarness,
    DaemonServer, PromptHandler, ScheduledPromptDelivery,
};
pub use state::{DaemonMetadataSnapshot, DaemonStateStore, SessionCatalogEntry};

pub const IPC_SCHEMA_VERSION: u16 = 1;
