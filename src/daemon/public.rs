//! Wire-compatible public daemon protocol primitives.
//!
//! The resident Rust daemon uses a smaller typed IPC protocol internally. These
//! types preserve the reference harness's public JSONL v4 command-envelope and
//! snapshot surface without forcing the supervisor to materialize provider or
//! extension-specific state.

#![allow(
    clippy::missing_errors_doc,
    reason = "wire constructors return short, self-describing validation errors"
)]

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};

/// Stable name carried by every public daemon command envelope.
pub const PUBLIC_DAEMON_PROTOCOL_NAME: &str = "mimir.daemon";
/// Oldest public protocol accepted by the compatibility codec.
pub const PUBLIC_DAEMON_PROTOCOL_MIN_VERSION: u16 = 4;
/// Newest reference protocol understood by this migration.
pub const PUBLIC_DAEMON_PROTOCOL_MAX_VERSION: u16 = 7;
/// Target size used by the reference chunked-snapshot transport.
pub const PUBLIC_DAEMON_SNAPSHOT_CHUNK_BYTES: usize = 512 * 1024;

/// Protocol identity embedded in command and event envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicDaemonProtocolInfo {
    pub name: String,
    pub version: u16,
}

impl PublicDaemonProtocolInfo {
    /// Constructs and validates a supported protocol identity.
    pub fn new(version: u16) -> Result<Self, String> {
        if !(PUBLIC_DAEMON_PROTOCOL_MIN_VERSION..=PUBLIC_DAEMON_PROTOCOL_MAX_VERSION)
            .contains(&version)
        {
            return Err(format!(
                "unsupported public daemon protocol {version}; expected {PUBLIC_DAEMON_PROTOCOL_MIN_VERSION}..={PUBLIC_DAEMON_PROTOCOL_MAX_VERSION}"
            ));
        }
        Ok(Self {
            name: PUBLIC_DAEMON_PROTOCOL_NAME.into(),
            version,
        })
    }

    /// Returns an error when a deserialized identity is not compatible.
    pub fn validate(&self) -> Result<(), String> {
        if self.name != PUBLIC_DAEMON_PROTOCOL_NAME {
            return Err(format!("unsupported daemon protocol name: {}", self.name));
        }
        Self::new(self.version).map(|_| ())
    }
}

/// A reference image block accepted by prompt, steer, and follow-up commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicImageContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub data: String,
    pub mime_type: String,
}

impl PublicImageContent {
    /// Creates a validated image block.
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Result<Self, String> {
        let image = Self {
            content_type: "image".into(),
            data: data.into(),
            mime_type: mime_type.into(),
        };
        image.validate()?;
        Ok(image)
    }

    /// Checks the structural wire invariants without decoding or copying image data.
    pub fn validate(&self) -> Result<(), String> {
        if self.content_type != "image" {
            return Err("image content type must be image".into());
        }
        if self.data.is_empty() {
            return Err("image data must not be empty".into());
        }
        if !self.mime_type.starts_with("image/") || self.mime_type.len() <= "image/".len() {
            return Err("image mimeType must start with image/".into());
        }
        Ok(())
    }
}

/// A public daemon command represented losslessly as its JSON object.
///
/// The reference command union is intentionally broad and evolves through
/// compatible field additions. Keeping fields in an ordered map preserves those
/// additions while the validated `type` discriminator prevents accidental
/// forwarding of private or misspelled commands.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicDaemonCommand {
    fields: Map<String, Value>,
}

impl PublicDaemonCommand {
    /// Builds a known command from its discriminator and additional fields.
    pub fn new(
        command_type: impl Into<String>,
        fields: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Self, String> {
        let command_type = command_type.into();
        if !is_public_daemon_command(&command_type) {
            return Err(format!("unknown public daemon command: {command_type}"));
        }
        let mut map: Map<String, Value> = fields.into_iter().collect();
        if map.contains_key("type") {
            return Err("command fields must not replace the type discriminator".into());
        }
        map.insert("type".into(), Value::String(command_type));
        Ok(Self { fields: map })
    }

    /// Returns the command discriminator.
    pub fn command_type(&self) -> &str {
        self.fields
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// Returns a field without cloning its value.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    /// Consumes the command into its exact JSON object.
    #[must_use]
    pub fn into_fields(self) -> Map<String, Value> {
        self.fields
    }

    /// Constructs a prompt with optional reference-format image blocks.
    pub fn prompt(
        active_session_id: &str,
        message: &str,
        images: Vec<PublicImageContent>,
    ) -> Result<Self, String> {
        if active_session_id.trim().is_empty() {
            return Err("activeSessionId must not be blank".into());
        }
        if message.trim().is_empty() && images.is_empty() {
            return Err("prompt requires text or at least one image".into());
        }
        images.iter().try_for_each(PublicImageContent::validate)?;
        let mut fields = vec![
            (
                "activeSessionId".into(),
                Value::String(active_session_id.into()),
            ),
            ("message".into(), Value::String(message.into())),
        ];
        if !images.is_empty() {
            fields.push((
                "images".into(),
                serde_json::to_value(images).map_err(|error| error.to_string())?,
            ));
        }
        Self::new("prompt", fields)
    }
}

impl Serialize for PublicDaemonCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.fields.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PublicDaemonCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = Map::<String, Value>::deserialize(deserializer)?;
        let command_type = fields
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("public daemon command requires a string type"))?;
        if !is_public_daemon_command(command_type) {
            return Err(de::Error::custom(format!(
                "unknown public daemon command: {command_type}"
            )));
        }
        Ok(Self { fields })
    }
}

/// Versioned, idempotency-friendly command envelope from the public daemon socket.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDaemonCommandEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub id: String,
    pub protocol: PublicDaemonProtocolInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub command: PublicDaemonCommand,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPublicDaemonCommandEnvelope {
    #[serde(rename = "type")]
    envelope_type: String,
    id: String,
    protocol: PublicDaemonProtocolInfo,
    client_id: Option<String>,
    command: PublicDaemonCommand,
}

impl<'de> Deserialize<'de> for PublicDaemonCommandEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawPublicDaemonCommandEnvelope::deserialize(deserializer)?;
        let envelope = Self {
            envelope_type: raw.envelope_type,
            id: raw.id,
            protocol: raw.protocol,
            client_id: raw.client_id,
            command: raw.command,
        };
        envelope.validate().map_err(de::Error::custom)?;
        Ok(envelope)
    }
}

impl PublicDaemonCommandEnvelope {
    /// Constructs a validated command envelope.
    pub fn new(
        id: impl Into<String>,
        client_id: Option<String>,
        protocol_version: u16,
        command: PublicDaemonCommand,
    ) -> Result<Self, String> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err("daemon command id must not be blank".into());
        }
        Ok(Self {
            envelope_type: "command".into(),
            id,
            protocol: PublicDaemonProtocolInfo::new(protocol_version)?,
            client_id,
            command,
        })
    }

    /// Validates invariants after deserialization.
    pub fn validate(&self) -> Result<(), String> {
        if self.envelope_type != "command" {
            return Err("daemon envelope type must be command".into());
        }
        if self.id.trim().is_empty() {
            return Err("daemon command id must not be blank".into());
        }
        self.protocol.validate()
    }
}

/// Generation-aware public replay cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicDaemonEventCursor {
    pub generation: String,
    pub sequence: u64,
}

/// Optional parent link in a coherent attach snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDaemonSnapshotParent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
}

/// Coherent public attach/resync snapshot.
///
/// State owned by other subsystems remains JSON to keep this transport layer
/// decoupled and to preserve compatible fields byte-for-byte through proxies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDaemonSessionSnapshot {
    pub active_session_id: String,
    pub summary: Value,
    pub state: Value,
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_context: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_tree: Option<Value>,
    pub last_event_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_cursor: Option<PublicDaemonEventCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<PublicDaemonSnapshotParent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Value>,
}

/// Snapshot streaming records used for bounded attach, replacement, and resync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PublicDaemonSnapshotRecord {
    #[serde(rename = "session_snapshot_begin")]
    Begin {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        snapshot: Value,
        #[serde(rename = "messageCount")]
        message_count: usize,
        #[serde(rename = "targetChunkBytes")]
        target_chunk_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        purpose: Option<String>,
    },
    #[serde(rename = "session_snapshot_chunk")]
    Chunk {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        index: usize,
        messages: Vec<Value>,
    },
    #[serde(rename = "session_snapshot_end")]
    End {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        #[serde(rename = "chunkCount")]
        chunk_count: usize,
        #[serde(rename = "lastEventSequence")]
        last_event_sequence: u64,
        #[serde(rename = "lastEventCursor", skip_serializing_if = "Option::is_none")]
        last_event_cursor: Option<PublicDaemonEventCursor>,
    },
    #[serde(rename = "session_snapshot_failed")]
    Failed {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        error: String,
    },
}

/// Metadata for a public event. Unknown compatible metadata can be retained by
/// putting it into `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDaemonEventMeta {
    pub id: String,
    pub protocol: PublicDaemonProtocolInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PublicDaemonEventCursor>,
    pub emitted_at: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn is_public_daemon_command(command: &str) -> bool {
    matches!(
        command,
        "ack_result"
            | "list"
            | "list_saved_sessions"
            | "create"
            | "attach"
            | "reattach"
            | "detach"
            | "complete_owned_session"
            | "promote_owned_session"
            | "kill"
            | "rename"
            | "prompt"
            | "cancel_prompt_admission"
            | "prompt_and_wait"
            | "steer"
            | "follow_up"
            | "restore_next_turn"
            | "restore_actions"
            | "append_custom_message"
            | "resume_queue"
            | "send_message"
            | "agent_messages_status"
            | "agent_messages_pause"
            | "agent_messages_resume"
            | "agent_messages_clear"
            | "abort"
            | "start_side_question"
            | "abort_side_question"
            | "execute_bash"
            | "abort_bash"
            | "cancel_rlm_child"
            | "delete_rlm_subagent"
            | "wait_for_idle"
            | "wait_for_headless_completion"
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
            | "clear_queue"
            | "abort_and_clear_queue"
            | "cron_list"
            | "heartbeats_list"
            | "heartbeat_manage"
            | "cron_add"
            | "cron_cancel"
            | "heartbeat_get"
            | "heartbeat_set"
            | "heartbeat_update"
            | "set_model"
            | "cycle_model"
            | "set_scoped_models"
            | "set_thinking_level"
            | "set_service_tier"
            | "cycle_thinking_level"
            | "set_transport"
            | "set_steering_mode"
            | "set_follow_up_mode"
            | "set_auto_compaction"
            | "set_auto_retry"
            | "compact"
            | "refine"
            | "abort_compaction"
            | "abort_branch_summary"
            | "abort_retry"
            | "execute_bash_and_wait"
            | "reload"
            | "new_session"
            | "switch_session"
            | "fork"
            | "navigate_tree"
            | "import_jsonl"
            | "export_html"
            | "export_jsonl"
            | "set_session_name"
            | "get_rlm_max_depth_status"
            | "set_rlm_max_depth"
            | "rename_saved_session"
            | "delete_saved_session"
            | "get_session_context"
            | "get_session_tree"
            | "get_user_messages_for_forking"
            | "get_last_assistant_text"
            | "get_system_prompt"
            | "get_tool_definition"
            | "set_session_entry_label"
            | "extension_ui_response"
            | "prepare_update_restart"
            | "retry_worker"
            | "restart"
            | "shutdown"
    )
}
