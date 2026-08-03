use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::{Content, Message, Role, StopReason, ToolResult, Usage},
    session::{SESSION_SCHEMA_VERSION, SessionPayload, SessionRecord},
};

use super::ReferenceSessionMetadata;

/// Exports native Rust records as a reference-compatible linear v3 session branch.
///
/// # Errors
///
/// Returns an error for invalid metadata, unsupported schema versions, identifier collisions, or
/// runtime events whose preserved legacy JSON is malformed.
pub fn export_jsonl(
    records: &[SessionRecord],
    metadata: &ReferenceSessionMetadata,
) -> Result<String> {
    validate_metadata(metadata)?;
    let ids = reference_ids(records)?;
    let mut values = Vec::with_capacity(records.len().saturating_add(1));
    let mut header = json!({
        "type": "session",
        "version": 3,
        "id": metadata.session_id,
        "timestamp": metadata.timestamp.to_rfc3339(),
        "cwd": path_text(&metadata.cwd)?,
    });
    if let Some(parent) = &metadata.parent_session {
        header["parentSession"] = Value::String(path_text(parent)?);
    }
    values.push(header);

    let mut previous_id: Option<String> = None;
    for record in records {
        if record.schema_version != SESSION_SCHEMA_VERSION {
            return Err(MimirError::Configuration(format!(
                "cannot export session schema version {}",
                record.schema_version
            )));
        }
        let id = ids
            .get(&record.record_id)
            .ok_or_else(|| {
                MimirError::Configuration("reference id map is missing a session record".into())
            })?
            .clone();
        let mut value = export_payload(record, &ids)?;
        let object = value.as_object_mut().ok_or_else(|| {
            MimirError::Configuration("exported session entry must be an object".into())
        })?;
        object.insert("id".into(), Value::String(id.clone()));
        object.insert(
            "parentId".into(),
            previous_id.clone().map_or(Value::Null, Value::String),
        );
        object.insert(
            "timestamp".into(),
            Value::String(record.created_at.to_rfc3339()),
        );
        previous_id = Some(id);
        values.push(value);
    }
    let mut output = String::new();
    for value in values {
        output.push_str(&serde_json::to_string(&value)?);
        output.push('\n');
    }
    Ok(output)
}

fn export_payload(record: &SessionRecord, ids: &HashMap<Uuid, String>) -> Result<Value> {
    match &record.payload {
        SessionPayload::Message(message) => Ok(json!({
            "type": "message",
            "message": export_message(message),
        })),
        SessionPayload::Compaction {
            summary,
            reason,
            first_kept_entry_id,
            tokens_before,
            custom_instructions,
            details,
            ..
        } => {
            let mut object = Map::new();
            object.insert("type".into(), Value::String("compaction".into()));
            object.insert("summary".into(), Value::String(summary.clone()));
            object.insert("tokensBefore".into(), Value::from(*tokens_before));
            insert_optional_string(&mut object, "reason", reason.as_ref());
            let translated_first = translated_first_kept(first_kept_entry_id.as_deref(), ids);
            insert_optional_string(&mut object, "firstKeptEntryId", translated_first.as_ref());
            insert_optional_string(
                &mut object,
                "customInstructions",
                custom_instructions.as_ref(),
            );
            if let Some(details) = details {
                object.insert("details".into(), details.clone());
            }
            Ok(Value::Object(object))
        }
        SessionPayload::RuntimeEvent { name, detail } => export_runtime_event(name, detail),
    }
}

fn export_message(message: &Message) -> Value {
    match message.role {
        Role::User => json!({
            "role": "user",
            "content": export_user_content(&message.content),
            "timestamp": message.timestamp_ms,
        }),
        Role::Assistant => json!({
            "role": "assistant",
            "content": export_content(&message.content),
            "api": "openai-responses",
            "provider": "unknown",
            "model": "unknown",
            "usage": export_usage(message.usage),
            "stopReason": export_stop_reason(message.stop_reason),
            "timestamp": message.timestamp_ms,
        }),
        Role::Tool => export_tool_message(message),
        Role::System => json!({
            "role": "custom",
            "customType": "mimir:system",
            "content": export_content(&message.content),
            "display": false,
            "timestamp": message.timestamp_ms,
        }),
    }
}

fn export_user_content(content: &[Content]) -> Value {
    if let [Content::Text { text }] = content {
        Value::String(text.clone())
    } else {
        Value::Array(export_content(content))
    }
}

fn export_content(content: &[Content]) -> Vec<Value> {
    content
        .iter()
        .map(|block| match block {
            Content::Text { text } => json!({"type":"text", "text":text}),
            Content::Thinking {
                text,
                signature,
                redacted,
            } => {
                if *redacted {
                    json!({
                        "type":"redacted_thinking",
                        "data":signature.as_deref().unwrap_or(text)
                    })
                } else {
                    let mut value = json!({"type":"thinking", "thinking":text});
                    if let Some(signature) = signature {
                        value["signature"] = Value::String(signature.clone());
                    }
                    value
                }
            }
            Content::Image { data, mime_type } => {
                json!({"type":"image", "data":data, "mimeType":mime_type})
            }
            Content::ToolCall(call) => json!({
                "type":"toolCall", "id":call.id, "name":call.name, "arguments":call.arguments
            }),
            Content::ToolResult(result) => json!({"type":"text", "text":result.content}),
        })
        .collect()
}

fn export_tool_message(message: &Message) -> Value {
    let result = message.content.iter().find_map(|content| match content {
        Content::ToolResult(result) => Some(result),
        _ => None,
    });
    match result {
        Some(result) => tool_result_value(result, message),
        None => json!({
            "role": "toolResult",
            "toolCallId": "rust-tool-call",
            "toolName": "unknown",
            "content": export_content(&message.content),
            "isError": false,
            "timestamp": message.timestamp_ms,
        }),
    }
}

fn tool_result_value(result: &ToolResult, message: &Message) -> Value {
    json!({
        "role": "toolResult",
        "toolCallId": result.tool_call_id,
        "toolName": result.tool_name,
        "content": [{"type":"text", "text":result.content}],
        "isError": result.is_error,
        "timestamp": message.timestamp_ms,
    })
}

fn export_usage(usage: Usage) -> Value {
    json!({
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cacheRead": usage.cached_tokens,
        "cacheWrite": 0,
        "totalTokens": usage.total(),
        "cost": {
            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0
        }
    })
}

fn export_stop_reason(reason: Option<StopReason>) -> &'static str {
    match reason.unwrap_or_default() {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::BudgetExhausted => "budgetExhausted",
    }
}

fn export_runtime_event(name: &str, detail: &str) -> Result<Value> {
    if let Some(entry_type) = name.strip_prefix("legacy_") {
        let mut value: Value = serde_json::from_str(detail).map_err(|error| {
            MimirError::Configuration(format!(
                "preserved legacy runtime event is malformed: {error}"
            ))
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            MimirError::Configuration("preserved legacy event must be an object".into())
        })?;
        object.insert("type".into(), Value::String(entry_type.into()));
        object.remove("id");
        object.remove("parentId");
        object.remove("timestamp");
        return Ok(value);
    }
    let data =
        serde_json::from_str::<Value>(detail).unwrap_or_else(|_| Value::String(detail.into()));
    Ok(json!({
        "type": "custom",
        "customType": format!("mimir:{name}"),
        "data": data,
    }))
}

fn translated_first_kept(
    first_kept_entry_id: Option<&str>,
    ids: &HashMap<Uuid, String>,
) -> Option<String> {
    first_kept_entry_id.map(|value| {
        Uuid::parse_str(value)
            .ok()
            .and_then(|id| ids.get(&id).cloned())
            .unwrap_or_else(|| value.to_owned())
    })
}

fn reference_ids(records: &[SessionRecord]) -> Result<HashMap<Uuid, String>> {
    let mut output = HashMap::with_capacity(records.len());
    let mut seen = HashSet::with_capacity(records.len());
    for record in records {
        let id = unique_reference_id(record.record_id, &mut seen)?;
        output.insert(record.record_id, id);
    }
    Ok(output)
}

fn unique_reference_id(record_id: Uuid, seen: &mut HashSet<String>) -> Result<String> {
    for attempt in 0..=u32::MAX {
        let candidate = if attempt == 0 {
            record_id.simple().to_string()[..8].to_owned()
        } else {
            let digest = Sha256::digest(format!("{record_id}:{attempt}").as_bytes());
            format!(
                "{:02x}{:02x}{:02x}{:02x}",
                digest[0], digest[1], digest[2], digest[3]
            )
        };
        if seen.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(MimirError::Configuration(
        "reference session id space is exhausted".into(),
    ))
}

fn insert_optional_string(object: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        object.insert(key.into(), Value::String(value.clone()));
    }
}

fn validate_metadata(metadata: &ReferenceSessionMetadata) -> Result<()> {
    if metadata.session_id.is_empty()
        || metadata.session_id.len() > 128
        || metadata.session_id.chars().any(char::is_control)
    {
        return Err(MimirError::Configuration(
            "reference session id must contain 1-128 non-control characters".into(),
        ));
    }
    if metadata.cwd.as_os_str().is_empty() {
        return Err(MimirError::Configuration(
            "reference session cwd must not be empty".into(),
        ));
    }
    Ok(())
}

fn path_text(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| MimirError::Configuration("session paths must be valid UTF-8".into()))
}
