use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    session::{SESSION_SCHEMA_VERSION, SessionPayload, SessionRecord},
};

use super::{
    ImportedSession, ReferenceSessionMetadata, SessionCompatibilityState, SessionFormat,
    SessionModelSelection, SwitchSessionPlan, message::reference_message,
};

const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const MAX_SESSION_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct ReferenceEntry {
    line_number: usize,
    id: String,
    parent_id: Option<String>,
    value: Value,
}

/// Parses either a reference v1-v3 session or a native Rust v1 transcript.
///
/// # Errors
///
/// Returns a typed error for oversized, malformed, cyclic, unsupported, or lossy input.
pub fn import_jsonl(source_path: &Path, bytes: &[u8]) -> Result<ImportedSession> {
    validate_input_size(source_path, bytes)?;
    let input = std::str::from_utf8(bytes).map_err(|error| MimirError::Session {
        path: source_path.to_path_buf(),
        message: format!("session is not UTF-8: {error}"),
    })?;
    let lines = nonempty_lines(source_path, input)?;
    let first: Value = serde_json::from_str(lines[0].1).map_err(|error| MimirError::Session {
        path: source_path.to_path_buf(),
        message: format!("invalid session header: {error}"),
    })?;
    if first.get("type").and_then(Value::as_str) == Some("session") {
        import_reference(source_path, &lines, &first)
    } else if first.get("schema_version").is_some() {
        import_rust(source_path, &lines)
    } else {
        Err(MimirError::Session {
            path: source_path.to_path_buf(),
            message: "unrecognized session format".into(),
        })
    }
}

/// Builds a side-effect-free plan for switching to imported session state.
///
/// # Errors
///
/// Returns an error when translation fails, no cwd is available, or no safe target id can be
/// derived from the source filename.
pub fn prepare_switch_session(
    source_path: &Path,
    bytes: &[u8],
    cwd_override: Option<&Path>,
) -> Result<SwitchSessionPlan> {
    let imported = import_jsonl(source_path, bytes)?;
    let cwd = cwd_override
        .map(Path::to_path_buf)
        .or_else(|| {
            imported
                .metadata
                .as_ref()
                .map(|metadata| metadata.cwd.clone())
        })
        .filter(|cwd| !cwd.as_os_str().is_empty())
        .ok_or_else(|| {
            MimirError::Configuration(
                "session cwd is unavailable; provide an explicit cwd override".into(),
            )
        })?;
    if cwd.to_string_lossy().contains('\0') {
        return Err(MimirError::Configuration(
            "session cwd must not contain NUL bytes".into(),
        ));
    }
    let target_session_id = safe_target_session_id(source_path, bytes)?;
    Ok(SwitchSessionPlan {
        format: imported.format,
        target_session_id,
        cwd,
        state: imported.state,
        records: imported.records,
    })
}

fn import_reference(
    source_path: &Path,
    lines: &[(usize, &str)],
    header: &Value,
) -> Result<ImportedSession> {
    let version = required_u16(header, "version", source_path)?;
    if !(1..=3).contains(&version) {
        return Err(session_error(
            source_path,
            "unsupported legacy session header",
        ));
    }
    let session_id = required_string(header, "id", source_path)?.to_owned();
    if session_id.len() > 256 || session_id.chars().any(char::is_control) {
        return Err(session_error(
            source_path,
            "reference session id is invalid",
        ));
    }
    let timestamp = parse_datetime(header.get("timestamp")).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(PathBuf::new, PathBuf::from);
    let parent_session = header
        .get("parentSession")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let entries = parse_reference_entries(source_path, &lines[1..], version)?;
    let branch = active_branch(source_path, &entries)?;
    let state = reference_state(source_path, &branch)?;
    let records = translate_reference_branch(source_path, &session_id, &branch, timestamp)?;
    Ok(ImportedSession {
        format: SessionFormat::Reference(version),
        metadata: Some(ReferenceSessionMetadata {
            session_id,
            timestamp,
            cwd,
            parent_session,
        }),
        state,
        records,
    })
}

fn import_rust(source_path: &Path, lines: &[(usize, &str)]) -> Result<ImportedSession> {
    let mut records = Vec::with_capacity(lines.len());
    for (line_number, line) in lines {
        let record: SessionRecord =
            serde_json::from_str(line).map_err(|error| MimirError::Session {
                path: source_path.to_path_buf(),
                message: format!("invalid Rust record at line {line_number}: {error}"),
            })?;
        if record.schema_version != SESSION_SCHEMA_VERSION {
            return Err(session_error(
                source_path,
                &format!(
                    "unsupported Rust session schema version {}",
                    record.schema_version
                ),
            ));
        }
        records.push(record);
    }
    Ok(ImportedSession {
        format: SessionFormat::Rust(SESSION_SCHEMA_VERSION),
        metadata: None,
        state: SessionCompatibilityState::default(),
        records,
    })
}

fn reference_state(
    source_path: &Path,
    branch: &[ReferenceEntry],
) -> Result<SessionCompatibilityState> {
    let mut state = SessionCompatibilityState {
        thinking_level: Some("off".into()),
        service_tier: Some("default".into()),
        model: None,
    };
    for entry in branch {
        match required_string(&entry.value, "type", source_path)? {
            "thinking_level_change" => {
                state.thinking_level =
                    Some(required_string(&entry.value, "thinkingLevel", source_path)?.to_owned());
            }
            "service_tier_change" => {
                state.service_tier =
                    Some(required_string(&entry.value, "serviceTier", source_path)?.to_owned());
            }
            "model_change" => {
                state.model = Some(SessionModelSelection {
                    provider: required_string(&entry.value, "provider", source_path)?.to_owned(),
                    model: required_string(&entry.value, "modelId", source_path)?.to_owned(),
                });
            }
            "message" => update_state_from_assistant(source_path, &entry.value, &mut state)?,
            _ => {}
        }
    }
    Ok(state)
}

fn update_state_from_assistant(
    source_path: &Path,
    entry: &Value,
    state: &mut SessionCompatibilityState,
) -> Result<()> {
    let Some(message) = entry.get("message") else {
        return Err(session_error(
            source_path,
            "reference message entry is missing message",
        ));
    };
    if message.get("role").and_then(Value::as_str) == Some("assistant") {
        let provider = message.get("provider").and_then(Value::as_str);
        let model = message.get("model").and_then(Value::as_str);
        if let (Some(provider), Some(model)) = (provider, model)
            && !provider.is_empty()
            && !model.is_empty()
        {
            state.model = Some(SessionModelSelection {
                provider: provider.into(),
                model: model.into(),
            });
        }
    }
    Ok(())
}

fn parse_reference_entries(
    source_path: &Path,
    lines: &[(usize, &str)],
    version: u16,
) -> Result<Vec<ReferenceEntry>> {
    let mut entries = Vec::with_capacity(lines.len());
    let mut ids = HashSet::new();
    let mut previous_id = None;
    for (line_number, line) in lines {
        let value: Value = serde_json::from_str(line).map_err(|error| MimirError::Session {
            path: source_path.to_path_buf(),
            message: format!("invalid JSON at line {line_number}: {error}"),
        })?;
        required_string(&value, "type", source_path)?;
        let id = match value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            Some(id) => id.to_owned(),
            None if version == 1 => format!("line-{line_number}"),
            None => {
                return Err(session_error(
                    source_path,
                    "reference v2-v3 entry is missing id",
                ));
            }
        };
        if !ids.insert(id.clone()) {
            return Err(session_error(
                source_path,
                "duplicate reference session entry id",
            ));
        }
        let parent_id = if version == 1 {
            value
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| previous_id.clone())
        } else {
            value
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        previous_id = Some(id.clone());
        entries.push(ReferenceEntry {
            line_number: *line_number,
            id,
            parent_id,
            value,
        });
    }
    Ok(entries)
}

fn active_branch(source_path: &Path, entries: &[ReferenceEntry]) -> Result<Vec<ReferenceEntry>> {
    let Some(mut current) = entries.last() else {
        return Ok(Vec::new());
    };
    let by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut visited = HashSet::new();
    let mut reversed = Vec::new();
    loop {
        if !visited.insert(current.id.as_str()) {
            return Err(session_error(source_path, "cyclic reference session graph"));
        }
        reversed.push(current.clone());
        let Some(parent_id) = current.parent_id.as_deref() else {
            break;
        };
        current = by_id.get(parent_id).copied().ok_or_else(|| {
            session_error(source_path, "reference session entry has an unknown parent")
        })?;
    }
    reversed.reverse();
    Ok(reversed)
}

fn translate_reference_branch(
    source_path: &Path,
    session_id: &str,
    branch: &[ReferenceEntry],
    header_timestamp: DateTime<Utc>,
) -> Result<Vec<SessionRecord>> {
    let mut records = Vec::with_capacity(branch.len());
    let id_to_index = branch
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let translated_ids = branch
        .iter()
        .map(|entry| {
            (
                entry.id.as_str(),
                deterministic_uuid(&format!("{session_id}:{}:{}", entry.id, entry.line_number)),
            )
        })
        .collect::<HashMap<_, _>>();
    for (index, entry) in branch.iter().enumerate() {
        let line_offset = i64::try_from(entry.line_number).unwrap_or(i64::MAX);
        let created_at = parse_datetime(entry.value.get("timestamp"))
            .unwrap_or_else(|| header_timestamp + chrono::Duration::microseconds(line_offset));
        let payload = translate_reference_payload(
            source_path,
            entry,
            branch,
            &id_to_index,
            &translated_ids,
            index,
            created_at.timestamp_millis(),
        )?;
        let record_id = *translated_ids
            .get(entry.id.as_str())
            .ok_or_else(|| session_error(source_path, "translated entry id is unavailable"))?;
        records.push(SessionRecord {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id,
            parent_id: records
                .last()
                .map(|record: &SessionRecord| record.record_id),
            created_at,
            payload,
        });
    }
    Ok(records)
}

fn translate_reference_payload(
    source_path: &Path,
    entry: &ReferenceEntry,
    branch: &[ReferenceEntry],
    id_to_index: &HashMap<&str, usize>,
    translated_ids: &HashMap<&str, Uuid>,
    current_index: usize,
    fallback_timestamp_ms: i64,
) -> Result<SessionPayload> {
    let entry_type = required_string(&entry.value, "type", source_path)?;
    match entry_type {
        "message" => {
            let value = entry.value.get("message").ok_or_else(|| {
                session_error(source_path, "reference message entry is missing message")
            })?;
            match reference_message(source_path, value, fallback_timestamp_ms)? {
                Some(message) => Ok(SessionPayload::Message(message)),
                None => legacy_event(entry_type, &entry.value),
            }
        }
        "custom_message" => {
            translate_context_entry(source_path, &entry.value, "custom", fallback_timestamp_ms)
        }
        "branch_summary" => translate_context_entry(
            source_path,
            &entry.value,
            "branchSummary",
            fallback_timestamp_ms,
        ),
        "compaction" => {
            let reference_first_kept_id = entry
                .value
                .get("firstKeptEntryId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let retained_message_count = reference_first_kept_id
                .as_deref()
                .map(|first_id| {
                    let first_index = id_to_index.get(first_id).copied().ok_or_else(|| {
                        session_error(source_path, "compaction firstKeptEntryId is unknown")
                    })?;
                    if first_index >= current_index {
                        return Err(session_error(
                            source_path,
                            "compaction firstKeptEntryId must precede compaction",
                        ));
                    }
                    Ok(branch[first_index..current_index]
                        .iter()
                        .filter(|candidate| {
                            matches!(
                                candidate.value.get("type").and_then(Value::as_str),
                                Some("message" | "custom_message" | "branch_summary")
                            )
                        })
                        .count())
                })
                .transpose()?
                .unwrap_or_default();
            let first_kept_entry_id = reference_first_kept_id
                .as_deref()
                .map(|first_id| {
                    translated_ids
                        .get(first_id)
                        .map(ToString::to_string)
                        .ok_or_else(|| {
                            session_error(source_path, "compaction firstKeptEntryId is unavailable")
                        })
                })
                .transpose()?;
            Ok(SessionPayload::Compaction {
                summary: required_string(&entry.value, "summary", source_path)?.to_owned(),
                retained_message_count,
                reason: entry
                    .value
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                first_kept_entry_id,
                tokens_before: entry
                    .value
                    .get("tokensBefore")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                custom_instructions: entry
                    .value
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                details: entry.value.get("details").cloned(),
            })
        }
        other => legacy_event(other, &entry.value),
    }
}

fn translate_context_entry(
    source_path: &Path,
    entry: &Value,
    role: &str,
    fallback_timestamp_ms: i64,
) -> Result<SessionPayload> {
    let mut message = entry.clone();
    let object = message
        .as_object_mut()
        .ok_or_else(|| session_error(source_path, "reference context entry must be an object"))?;
    object.insert("role".into(), Value::String(role.into()));
    match reference_message(source_path, &message, fallback_timestamp_ms)? {
        Some(message) => Ok(SessionPayload::Message(message)),
        None => legacy_event(required_string(entry, "type", source_path)?, entry),
    }
}

fn legacy_event(entry_type: &str, value: &Value) -> Result<SessionPayload> {
    Ok(SessionPayload::RuntimeEvent {
        name: format!("legacy_{entry_type}"),
        detail: serde_json::to_string(value)?,
    })
}

fn nonempty_lines<'a>(source_path: &Path, input: &'a str) -> Result<Vec<(usize, &'a str)>> {
    let mut lines = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.len() > MAX_SESSION_LINE_BYTES {
            return Err(session_error(
                source_path,
                "session line exceeds the 4 MiB limit",
            ));
        }
        if !line.trim().is_empty() {
            lines.push((index + 1, line));
        }
    }
    if lines.is_empty() {
        return Err(session_error(source_path, "session is empty"));
    }
    Ok(lines)
}

fn validate_input_size(source_path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_SESSION_BYTES {
        return Err(session_error(
            source_path,
            "session exceeds the 64 MiB limit",
        ));
    }
    Ok(())
}

fn required_string<'a>(value: &'a Value, field: &str, path: &Path) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| session_error(path, &format!("session field {field} must be a string")))
}

fn required_u16(value: &Value, field: &str, path: &Path) -> Result<u16> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| session_error(path, &format!("session field {field} must be an integer")))
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value.and_then(|value| match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|timestamp| timestamp.with_timezone(&Utc)),
        Value::Number(number) => number
            .as_i64()
            .and_then(|timestamp| Utc.timestamp_millis_opt(timestamp).single()),
        _ => None,
    })
}

fn safe_target_session_id(source_path: &Path, bytes: &[u8]) -> Result<String> {
    let stem = source_path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let mut output = String::with_capacity(stem.len().min(64));
    let mut separator = false;
    for character in stem.chars() {
        if output.len() >= 64 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            output.push(character);
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        output = format!("imported-{}", &hex_digest(bytes)[..12]);
    }
    if output.is_empty() {
        return Err(MimirError::Configuration(
            "could not derive a safe session id".into(),
        ));
    }
    Ok(output)
}

fn deterministic_uuid(namespace: &str) -> Uuid {
    let digest = Sha256::digest(namespace.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn session_error(path: &Path, message: &str) -> MimirError {
    MimirError::Session {
        path: path.to_path_buf(),
        message: message.into(),
    }
}
