use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ThinkingLevel;

/// Bounded, immutable host state exposed to one extension invocation.
///
/// Embedded extensions receive a copy rather than references to runtime state,
/// so JavaScript cannot retain locks or bypass the host action validator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionHostSnapshot {
    pub cwd: String,
    pub session_id: String,
    pub session_name: Option<String>,
    pub provider: String,
    pub model: String,
    pub thinking_level: ThinkingLevel,
    pub active_tools: Vec<String>,
    pub all_tools: Vec<ExtensionToolInfo>,
    pub commands: Vec<ExtensionCommandInfo>,
    pub flags: Vec<ExtensionFlagValue>,
    pub is_idle: bool,
    pub has_pending_messages: bool,
    pub system_prompt: String,
    pub context_usage: ExtensionContextUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionFlagValue {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionContextUsage {
    pub messages: usize,
    pub estimated_tokens: u64,
    pub max_messages: usize,
}

/// A capability-checked request emitted by extension JavaScript and applied by
/// `AgentRuntime` only after the isolate returns control to Rust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionHostAction {
    SendMessage {
        custom_type: String,
        content: Value,
        #[serde(default)]
        display: bool,
        #[serde(default)]
        details: Option<Value>,
        #[serde(default)]
        trigger_turn: bool,
        #[serde(default)]
        deliver_as: ExtensionDelivery,
    },
    SendUserMessage {
        content: Value,
        #[serde(default)]
        deliver_as: ExtensionDelivery,
    },
    AppendEntry {
        custom_type: String,
        #[serde(default)]
        data: Value,
    },
    SetSessionName {
        name: String,
    },
    SetLabel {
        entry_id: String,
        #[serde(default)]
        label: Option<String>,
    },
    SetActiveTools {
        names: Vec<String>,
    },
    SetModel {
        provider: String,
        model: String,
    },
    SetThinkingLevel {
        level: ThinkingLevel,
    },
    PublishEvent {
        topic: String,
        data: Value,
    },
    Session {
        action: ExtensionSessionAction,
    },
    Abort,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionSessionAction {
    New {
        #[serde(default)]
        parent_session: Option<String>,
    },
    Fork {
        entry_id: String,
        #[serde(default)]
        position: ExtensionForkPosition,
    },
    NavigateTree {
        target_id: String,
        #[serde(default)]
        summarize: bool,
        #[serde(default)]
        custom_instructions: Option<String>,
        #[serde(default)]
        replace_instructions: bool,
        #[serde(default)]
        label: Option<String>,
    },
    Switch {
        session_path: String,
    },
    Reload,
    Compact {
        #[serde(default)]
        custom_instructions: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionForkPosition {
    #[default]
    Before,
    At,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionDelivery {
    Steer,
    FollowUp,
    #[default]
    NextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShortcutDescriptor {
    pub shortcut: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    Boolean,
    String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlagDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub kind: FlagKind,
    #[serde(default)]
    pub default: Option<Value>,
}
