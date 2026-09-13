use crate::{
    model::ThinkingLevel,
    orchestration::HeartbeatManagementAction,
    runtime::QueueMode,
    tools::{AgentMode, ApprovalDecision, PermissionRequest},
};
use uuid::Uuid;

use super::commands::McpCommand;

/// Typed, side-effect-free requests emitted by TUI slash commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    SetAgentMode(AgentMode),
    WorkspacePermission {
        request: PermissionRequest,
        decision: ApprovalDecision,
    },
    SetEffort(ThinkingLevel),
    ToggleFast,
    ConfigureScopedModels,
    SetAutoCompaction {
        enabled: bool,
    },
    SetSteeringMode {
        mode: QueueMode,
    },
    /// Persist renderer and TUI-coordinator preferences that do not mutate the
    /// provider runtime directly.
    PersistSettings,
    Resume {
        session: String,
    },
    NewSession {
        name: Option<String>,
        prompt: Option<String>,
    },
    SetSessionName {
        name: Option<String>,
    },
    ShowSessionInfo,
    ShowSessionTree,
    ContinueAt {
        entry_id: Uuid,
    },
    Fork,
    ForkAt {
        entry_id: Uuid,
    },
    Clone,
    Compact {
        instructions: Option<String>,
    },
    Refine {
        arguments: Option<String>,
    },
    Learn {
        arguments: Option<String>,
    },
    CopyLastMessage,
    SideQuestion {
        question: String,
    },
    ExportSession {
        path: Option<String>,
    },
    ImportSession {
        path: String,
    },
    ShareSession,
    ShowChangelog,
    ShowSystemPrompt,
    ShowLogs,
    Update {
        arguments: Option<String>,
    },
    RlmMaxDepth {
        arguments: Option<String>,
    },
    Fullscreen {
        enabled: Option<bool>,
    },
    SetTheme,
    Traces {
        arguments: Option<String>,
    },
    Heartbeat {
        arguments: Option<String>,
    },
    ListHeartbeats,
    ManageHeartbeat {
        session: String,
        id: Uuid,
        action: HeartbeatManagementAction,
    },
    Goal {
        arguments: Option<String>,
    },
    Autonomous {
        arguments: Option<String>,
    },
    Reload,
    ShowContext,
    Mcp(McpCommand),
    ExtensionCommand {
        name: String,
        args: String,
    },
}
