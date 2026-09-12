//! Bounded prompt-admission and turn-recovery primitives for the public daemon.

use std::collections::HashSet;
#[cfg(unix)]
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, Weak},
};

#[cfg(any(unix, test))]
use serde_json::json;
use serde_json::{Map, Value};
#[cfg(unix)]
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

use crate::model::{Content, Message};

use super::{DaemonError, PublicDaemonCommand};

const MAX_RECOVERY_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECOVERY_ITEMS: usize = 64;
const MAX_RECOVERY_TEXT_CHARS: usize = 262_144;
const MAX_RECOVERY_ID_CHARS: usize = 256;
const MAX_CUSTOM_TYPE_CHARS: usize = 128;

#[cfg(unix)]
type AdmissionKey = (String, String);

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionStatus {
    Waiting,
    Owned,
    Cancelled,
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct AdmissionEntry {
    status: AdmissionStatus,
    cancellation: CancellationToken,
}

/// One validated custom message recovered after a daemon restart.
#[derive(Debug, Clone)]
pub(crate) struct RecoveredCustomMessage {
    pub(crate) custom_type: String,
    pub(crate) content: Vec<Content>,
    pub(crate) raw: Value,
}

impl RecoveredCustomMessage {
    pub(crate) fn into_user_message(self) -> Message {
        Message::user_content(self.content)
    }
}

/// Delivery boundary retained for one recovered queued turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveredDelivery {
    NextTurn,
    WhenIdle,
}

/// One validated recovery action that the native runtime can queue safely.
#[derive(Debug, Clone)]
pub(crate) struct RecoveredAction {
    pub(crate) id: String,
    pub(crate) delivery: RecoveredDelivery,
    pub(crate) payload: RecoveredActionPayload,
    pub(crate) raw: Value,
}

/// Native action payload retained without converting slash commands into prompts.
#[derive(Debug, Clone)]
pub(crate) enum RecoveredActionPayload {
    Turn(Message),
    SessionCommand(RecoveredSessionCommand),
}

/// One supported, bounded session command recovered from the reference queue.
#[derive(Debug, Clone)]
pub(crate) struct RecoveredSessionCommand {
    pub(crate) name: RecoveredSessionCommandName,
    pub(crate) args: String,
}

/// Commands with audited native Rust implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveredSessionCommandName {
    Compact,
    Refine,
    Goal,
    Autonomous,
}

/// Strict, bounded form of a public recovery command.
#[derive(Debug, Clone)]
pub(crate) enum TurnRecovery {
    RestoreNextTurn {
        messages: Vec<RecoveredCustomMessage>,
    },
    RestoreActions {
        actions: Vec<RecoveredAction>,
    },
    AppendCustomMessage {
        message: RecoveredCustomMessage,
    },
    ResumeQueue,
}

#[cfg(unix)]
/// Per-daemon prompt admission gates and recovery validation.
#[derive(Debug, Default)]
pub(crate) struct TurnOps {
    gates: StdMutex<HashMap<String, Weak<Semaphore>>>,
    admissions: Arc<StdMutex<HashMap<AdmissionKey, AdmissionEntry>>>,
}

#[cfg(unix)]
impl TurnOps {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Waits for exclusive prompt ownership for one active session.
    pub(crate) async fn begin_prompt(
        &self,
        session_id: &str,
        admission_id: Option<&str>,
    ) -> Result<PromptAdmissionGuard, DaemonError> {
        validate_identifier("activeSessionId", session_id)?;
        let registration = admission_id
            .map(|id| self.register_admission(session_id, id))
            .transpose()?;
        let gate = self.session_gate(session_id);
        let permit = if let Some((_, cancellation)) = &registration {
            tokio::select! {
                permit = gate.acquire_owned() => permit.map_err(|_| protocol("prompt admission gate closed"))?,
                () = cancellation.cancelled() => {
                    if let Some((key, _)) = &registration { self.remove_admission(key); }
                    return Err(protocol("prompt admission was cancelled before ownership"));
                }
            }
        } else {
            gate.acquire_owned()
                .await
                .map_err(|_| protocol("prompt admission gate closed"))?
        };
        Ok(PromptAdmissionGuard {
            key: registration.map(|(key, _)| key),
            admissions: Arc::clone(&self.admissions),
            _permit: permit,
        })
    }

    /// Cancels a registered prompt only while it is still waiting for ownership.
    pub(crate) fn cancel_prompt_admission(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        let session_id = required_string(command, "activeSessionId")?;
        let admission_id = required_string(command, "admissionId")?;
        validate_identifier("activeSessionId", session_id)?;
        validate_identifier("admissionId", admission_id)?;
        let key = (session_id.to_owned(), admission_id.to_owned());
        let mut admissions = lock(&self.admissions);
        let Some(admission) = admissions.get_mut(&key) else {
            return Ok(json!({"status": "unknown"}));
        };
        match admission.status {
            AdmissionStatus::Owned => Ok(json!({"status": "owned"})),
            AdmissionStatus::Waiting | AdmissionStatus::Cancelled => {
                admission.status = AdmissionStatus::Cancelled;
                admission.cancellation.cancel();
                Ok(json!({"status": "cancelled"}))
            }
        }
    }

    /// Parses the recovery command subset without mutating a runtime.
    pub(crate) fn validate_recovery(
        command: &PublicDaemonCommand,
    ) -> Result<Option<TurnRecovery>, DaemonError> {
        parse_turn_recovery(command)
    }

    fn session_gate(&self, session_id: &str) -> Arc<Semaphore> {
        let mut gates = lock(&self.gates);
        if let Some(gate) = gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Semaphore::new(1));
        gates.insert(session_id.to_owned(), Arc::downgrade(&gate));
        gate
    }

    fn register_admission(
        &self,
        session_id: &str,
        admission_id: &str,
    ) -> Result<(AdmissionKey, CancellationToken), DaemonError> {
        validate_identifier("admissionId", admission_id)?;
        let key = (session_id.to_owned(), admission_id.to_owned());
        let cancellation = CancellationToken::new();
        let mut admissions = lock(&self.admissions);
        if admissions.len() >= 1024 {
            return Err(protocol(
                "prompt admission registry reached its 1024-entry limit",
            ));
        }
        if admissions.contains_key(&key) {
            return Err(protocol(
                "prompt admissionId is already registered for this session",
            ));
        }
        admissions.insert(
            key.clone(),
            AdmissionEntry {
                status: AdmissionStatus::Waiting,
                cancellation: cancellation.clone(),
            },
        );
        Ok((key, cancellation))
    }

    fn remove_admission(&self, key: &AdmissionKey) {
        lock(&self.admissions).remove(key);
    }
}

#[cfg(unix)]
/// Holds one session's prompt gate until the provider run has completed.
pub(crate) struct PromptAdmissionGuard {
    key: Option<AdmissionKey>,
    admissions: Arc<StdMutex<HashMap<AdmissionKey, AdmissionEntry>>>,
    _permit: OwnedSemaphorePermit,
}

#[cfg(unix)]
impl PromptAdmissionGuard {
    /// Commits ownership after the dispatcher has resolved the target runtime.
    pub(crate) fn mark_owned(&mut self) -> Result<(), DaemonError> {
        let Some(key) = &self.key else {
            return Ok(());
        };
        let mut admissions = lock(&self.admissions);
        let admission = admissions
            .get_mut(key)
            .ok_or_else(|| protocol("prompt admission registration disappeared"))?;
        if admission.status == AdmissionStatus::Cancelled {
            return Err(protocol("prompt admission was cancelled before ownership"));
        }
        admission.status = AdmissionStatus::Owned;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for PromptAdmissionGuard {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            lock(&self.admissions).remove(key);
        }
    }
}

pub(crate) fn parse_turn_recovery(
    command: &PublicDaemonCommand,
) -> Result<Option<TurnRecovery>, DaemonError> {
    if serde_json::to_vec(command)?.len() > MAX_RECOVERY_BYTES {
        return Err(protocol("turn recovery command exceeds the 2 MiB limit"));
    }
    let recovery = match command.command_type() {
        "restore_next_turn" => {
            let values = required_array(command, "messages")?;
            ensure_item_bound(values.len(), "next-turn messages")?;
            let messages = values
                .iter()
                .map(|value| parse_custom_message(value, true))
                .collect::<Result<Vec<_>, _>>()?;
            TurnRecovery::RestoreNextTurn { messages }
        }
        "restore_actions" => {
            let snapshot = required_object(command, "snapshot")?;
            if snapshot.get("formatVersion").and_then(Value::as_u64) != Some(1) {
                return Err(protocol(
                    "unsupported session action recovery formatVersion",
                ));
            }
            let values = snapshot
                .get("actions")
                .and_then(Value::as_array)
                .ok_or_else(|| protocol("snapshot.actions must be an array"))?;
            ensure_item_bound(values.len(), "recovery actions")?;
            let mut ids = HashSet::with_capacity(values.len());
            let mut actions = Vec::with_capacity(values.len());
            for value in values {
                let action = parse_recovered_action(value)?;
                if !ids.insert(action.id.clone()) {
                    return Err(protocol(format!(
                        "duplicate session action id: {}",
                        action.id
                    )));
                }
                actions.push(action);
            }
            TurnRecovery::RestoreActions { actions }
        }
        "append_custom_message" => TurnRecovery::AppendCustomMessage {
            message: parse_custom_message(required_value(command, "message")?, false)?,
        },
        "resume_queue" => TurnRecovery::ResumeQueue,
        _ => return Ok(None),
    };
    Ok(Some(recovery))
}

fn parse_recovered_action(value: &Value) -> Result<RecoveredAction, DaemonError> {
    let action = value
        .as_object()
        .ok_or_else(|| protocol("recovery action must be an object"))?;
    let id = object_string(action, "id")?;
    validate_identifier("action.id", id)?;
    validate_identifier("action.source", object_string(action, "source")?)?;
    let delivery = match object_string(action, "delivery")? {
        "next_turn_boundary" => RecoveredDelivery::NextTurn,
        "when_run_idle" => RecoveredDelivery::WhenIdle,
        _ => return Err(protocol("action.delivery is invalid")),
    };
    match object_string(action, "wake")? {
        "immediate" | "on_lower_boundary" | "external_resume" => {}
        _ => return Err(protocol("action.wake is invalid")),
    }
    let payload = action
        .get("payload")
        .and_then(Value::as_object)
        .ok_or_else(|| protocol("action.payload must be an object"))?;
    bounded_text(object_string(payload, "text")?, "action.payload.text")?;
    let recovered_payload = match object_string(payload, "kind")? {
        "turn" => RecoveredActionPayload::Turn(primary_recovery_message(payload, id)?),
        "session_command" => {
            RecoveredActionPayload::SessionCommand(parse_recovered_session_command(payload)?)
        }
        _ => return Err(protocol("action.payload.kind is invalid")),
    };
    Ok(RecoveredAction {
        id: id.to_owned(),
        delivery,
        payload: recovered_payload,
        raw: value.clone(),
    })
}

fn parse_recovered_session_command(
    payload: &Map<String, Value>,
) -> Result<RecoveredSessionCommand, DaemonError> {
    let command = payload
        .get("command")
        .and_then(Value::as_object)
        .ok_or_else(|| protocol("session command recovery payload requires command"))?;
    let name = match object_string(command, "name")? {
        "compact" => RecoveredSessionCommandName::Compact,
        "refine" => RecoveredSessionCommandName::Refine,
        "goal" => RecoveredSessionCommandName::Goal,
        "autonomous" => RecoveredSessionCommandName::Autonomous,
        _ => return Err(protocol("recovered session command name is unsupported")),
    };
    let args = object_string(command, "args")?;
    bounded_text(args, "session command args")?;
    let command_text = object_string(command, "text")?;
    bounded_text(command_text, "session command text")?;
    Ok(RecoveredSessionCommand {
        name,
        args: args.to_owned(),
    })
}

fn primary_recovery_message(
    payload: &Map<String, Value>,
    action_id: &str,
) -> Result<Message, DaemonError> {
    for name in [
        "queueVisible",
        "acceptedAgentMessage",
        "acceptedBeforeCompletion",
    ] {
        if !payload.get(name).is_some_and(Value::is_boolean) {
            return Err(protocol(format!("turn payload {name} must be a boolean")));
        }
    }
    if !payload.get("executionPolicy").is_some_and(Value::is_object) {
        return Err(protocol("turn payload requires executionPolicy"));
    }
    let records = payload
        .get("records")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("turn payload records must be an array"))?;
    ensure_item_bound(records.len(), "recovery delivery records")?;
    let mut primary = None;
    for record in records {
        let record = record
            .as_object()
            .ok_or_else(|| protocol("delivery record must be an object"))?;
        validate_identifier("delivery record id", object_string(record, "id")?)?;
        if object_string(record, "ownerActionId")? != action_id {
            return Err(protocol(
                "delivery record ownerActionId does not match its action",
            ));
        }
        let role = object_string(record, "role")?;
        if !matches!(role, "primary" | "prefix" | "next_turn") {
            return Err(protocol("delivery record role is invalid"));
        }
        if role == "primary" {
            if primary.is_some() {
                return Err(protocol(
                    "turn recovery action must have exactly one primary record",
                ));
            }
            primary =
                Some(parse_queued_message(record.get("message").ok_or_else(
                    || protocol("delivery record requires message"),
                )?)?);
        }
    }
    primary.ok_or_else(|| protocol("turn recovery action requires exactly one primary record"))
}

fn parse_queued_message(value: &Value) -> Result<Message, DaemonError> {
    let message = value
        .as_object()
        .ok_or_else(|| protocol("queued message must be an object"))?;
    match object_string(message, "role")? {
        "custom" => {
            parse_custom_message(value, true).map(RecoveredCustomMessage::into_user_message)
        }
        "user" => {
            if !message.get("timestamp").is_some_and(Value::is_number) {
                return Err(protocol("queued user message requires timestamp"));
            }
            Ok(Message::user_content(parse_content(
                message
                    .get("content")
                    .ok_or_else(|| protocol("queued user message requires content"))?,
            )?))
        }
        _ => Err(protocol("queued message role must be user or custom")),
    }
}

fn parse_custom_message(
    value: &Value,
    require_envelope: bool,
) -> Result<RecoveredCustomMessage, DaemonError> {
    let message = value
        .as_object()
        .ok_or_else(|| protocol("custom message must be an object"))?;
    if require_envelope {
        if message.get("role").and_then(Value::as_str) != Some("custom") {
            return Err(protocol("recovered custom message role must be custom"));
        }
        if !message.get("timestamp").is_some_and(Value::is_number) {
            return Err(protocol("recovered custom message requires timestamp"));
        }
    }
    let custom_type = object_string(message, "customType")?;
    if custom_type.is_empty()
        || custom_type.chars().count() > MAX_CUSTOM_TYPE_CHARS
        || custom_type.chars().any(char::is_control)
    {
        return Err(protocol("customType is invalid or exceeds 128 characters"));
    }
    if !message.get("display").is_some_and(Value::is_boolean) {
        return Err(protocol("custom message display must be a boolean"));
    }
    let content = parse_content(
        message
            .get("content")
            .ok_or_else(|| protocol("custom message requires content"))?,
    )?;
    Ok(RecoveredCustomMessage {
        custom_type: custom_type.to_owned(),
        content,
        raw: value.clone(),
    })
}

fn parse_content(value: &Value) -> Result<Vec<Content>, DaemonError> {
    if let Some(text) = value.as_str() {
        let text = bounded_text(text, "message content")?;
        return Ok(vec![Content::Text { text: text.into() }]);
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| protocol("message content must be a string or content block array"))?;
    ensure_item_bound(blocks.len(), "message content blocks")?;
    if blocks.is_empty() {
        return Err(protocol("message content must not be empty"));
    }
    blocks
        .iter()
        .map(|block| {
            let block = block
                .as_object()
                .ok_or_else(|| protocol("message content block must be an object"))?;
            match object_string(block, "type")? {
                "text" => Ok(Content::Text {
                    text: bounded_text(object_string(block, "text")?, "content block text")?.into(),
                }),
                "image" => {
                    let data = object_string(block, "data")?;
                    let mime_type = object_string(block, "mimeType")?;
                    if data.is_empty() || !mime_type.starts_with("image/") {
                        return Err(protocol("image block data or mimeType is invalid"));
                    }
                    Ok(Content::Image {
                        data: data.into(),
                        mime_type: mime_type.into(),
                    })
                }
                _ => Err(protocol("unsupported message content block type")),
            }
        })
        .collect()
}

fn bounded_text<'a>(value: &'a str, field: &str) -> Result<&'a str, DaemonError> {
    if value.chars().count() > MAX_RECOVERY_TEXT_CHARS || value.contains('\0') {
        return Err(protocol(format!(
            "{field} exceeds the 262144 character limit or contains NUL"
        )));
    }
    Ok(value)
}

fn ensure_item_bound(count: usize, field: &str) -> Result<(), DaemonError> {
    if count > MAX_RECOVERY_ITEMS {
        return Err(protocol(format!("{field} exceeds the 64-item limit")));
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), DaemonError> {
    if value.is_empty()
        || value.chars().count() > MAX_RECOVERY_ID_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(protocol(format!(
            "{field} is invalid or exceeds 256 characters"
        )));
    }
    Ok(())
}

fn required_string<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a str, DaemonError> {
    command
        .field(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| protocol(format!("{field} must be a non-empty string")))
}

fn required_array<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a Vec<Value>, DaemonError> {
    command
        .field(field)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol(format!("{field} must be an array")))
}

fn required_object<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a Map<String, Value>, DaemonError> {
    command
        .field(field)
        .and_then(Value::as_object)
        .ok_or_else(|| protocol(format!("{field} must be an object")))
}

fn required_value<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a Value, DaemonError> {
    command
        .field(field)
        .ok_or_else(|| protocol(format!("{field} is required")))
}

fn object_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, DaemonError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(format!("{field} must be a string")))
}

#[cfg(unix)]
fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn protocol(message: impl Into<String>) -> DaemonError {
    DaemonError::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::time::Duration;

    use super::*;

    fn command(kind: &str, fields: &[(&str, Value)]) -> PublicDaemonCommand {
        PublicDaemonCommand::new(
            kind,
            fields
                .iter()
                .map(|(key, value)| ((*key).into(), value.clone())),
        )
        .expect("command")
    }

    fn custom_message(text: &str) -> Value {
        json!({"role":"custom","customType":"recovery_note","content":text,"display":true,"timestamp":42})
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admission_cancels_waiter_but_never_an_owned_prompt() {
        let ops = Arc::new(TurnOps::new());
        let mut owner_guard = ops
            .begin_prompt("session-a", Some("owner"))
            .await
            .expect("owner guard");
        owner_guard.mark_owned().expect("owned");
        let waiting_ops = Arc::clone(&ops);
        let waiter =
            tokio::spawn(
                async move { waiting_ops.begin_prompt("session-a", Some("waiting")).await },
            );
        tokio::time::sleep(Duration::from_millis(10)).await;
        let cancelled = ops
            .cancel_prompt_admission(&command(
                "cancel_prompt_admission",
                &[
                    ("activeSessionId", json!("session-a")),
                    ("admissionId", json!("waiting")),
                ],
            ))
            .expect("cancel");
        assert_eq!(cancelled, json!({"status": "cancelled"}));
        assert!(waiter.await.expect("join").is_err());
        let owned_status = ops
            .cancel_prompt_admission(&command(
                "cancel_prompt_admission",
                &[
                    ("activeSessionId", json!("session-a")),
                    ("admissionId", json!("owner")),
                ],
            ))
            .expect("owned status");
        assert_eq!(owned_status, json!({"status": "owned"}));
    }

    #[test]
    fn recovery_commands_are_strict_bounded_and_preserve_delivery() {
        let restore = command(
            "restore_actions",
            &[(
                "snapshot",
                json!({"formatVersion":1,"actions":[{
                    "id":"action-1","source":"internal","delivery":"when_run_idle","wake":"external_resume",
                    "payload":{"kind":"turn","text":"resume this","records":[{
                        "id":"record-1","role":"primary","ownerActionId":"action-1","message":custom_message("recovered")
                    }],"executionPolicy":{},"queueVisible":true,"acceptedAgentMessage":false,"acceptedBeforeCompletion":false}
                }]}),
            )],
        );
        let TurnRecovery::RestoreActions { actions } = parse_turn_recovery(&restore)
            .expect("parse")
            .expect("recovery")
        else {
            panic!("wrong recovery type");
        };
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].delivery, RecoveredDelivery::WhenIdle);
        let RecoveredActionPayload::Turn(message) = &actions[0].payload else {
            panic!("expected turn payload");
        };
        assert_eq!(message.text(), "recovered");
        let duplicate_action = json!({
            "id":"same","source":"internal","delivery":"when_run_idle","wake":"immediate",
            "payload":{"kind":"turn","text":"duplicate","records":[{
                "id":"record","role":"primary","ownerActionId":"same","message":custom_message("duplicate")
            }],"executionPolicy":{},"queueVisible":true,"acceptedAgentMessage":false,"acceptedBeforeCompletion":false}
        });
        let duplicate = command(
            "restore_actions",
            &[(
                "snapshot",
                json!({"formatVersion":1,"actions":[
                    duplicate_action.clone(), duplicate_action
                ]}),
            )],
        );
        assert!(parse_turn_recovery(&duplicate).is_err());

        let session_command = command(
            "restore_actions",
            &[(
                "snapshot",
                json!({"formatVersion":1,"actions":[{
                    "id":"compact-action","source":"internal","delivery":"next_turn_boundary","wake":"immediate",
                    "payload":{"kind":"session_command","text":"/compact keep decisions","command":{"name":"compact","args":"keep decisions","text":"/compact keep decisions"}}
                }]}),
            )],
        );
        let TurnRecovery::RestoreActions { actions } = parse_turn_recovery(&session_command)
            .expect("session command parse")
            .expect("recovery")
        else {
            panic!("wrong recovery type");
        };
        let RecoveredActionPayload::SessionCommand(command) = &actions[0].payload else {
            panic!("expected session command");
        };
        assert_eq!(command.name, RecoveredSessionCommandName::Compact);
        assert_eq!(command.args, "keep decisions");
    }

    #[test]
    fn custom_message_validation_rejects_unsafe_payloads() {
        let append = command(
            "append_custom_message",
            &[(
                "message",
                json!({"customType":"note","content":"hello","display":false,"details":{"safe":true}}),
            )],
        );
        let TurnRecovery::AppendCustomMessage { message } = parse_turn_recovery(&append)
            .expect("parse")
            .expect("recovery")
        else {
            panic!("wrong recovery type");
        };
        assert_eq!(message.custom_type, "note");
        assert_eq!(message.into_user_message().text(), "hello");
        let invalid = command(
            "restore_next_turn",
            &[(
                "messages",
                json!([{"role":"custom","customType":"bad\0type","content":"x","display":true,"timestamp":1}]),
            )],
        );
        assert!(parse_turn_recovery(&invalid).is_err());
    }
}
