use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Content, Message, Role, ToolCall, ToolResult};

pub const INTERRUPTED_TOOL_RESULT_KIND: &str = "session_integrity_repair";

#[derive(Debug, Clone, PartialEq)]
pub struct IntegrityFindings {
    pub missing_calls: Vec<ToolCall>,
    pub unexpected_result_ids: Vec<String>,
    pub duplicate_call_ids: Vec<String>,
    pub duplicate_result_ids: Vec<String>,
}

impl IntegrityFindings {
    #[must_use]
    pub fn requires_pause(&self) -> bool {
        !self.unexpected_result_ids.is_empty()
            || !self.duplicate_call_ids.is_empty()
            || !self.duplicate_result_ids.is_empty()
    }
}

/// Inspects tool-call/result identities independently of message adjacency.
///
/// Missing calls can be repaired deterministically. Duplicate ids are ambiguous
/// and must pause a run instead of guessing which execution produced a result.
#[must_use]
pub fn inspect(messages: &[Message]) -> IntegrityFindings {
    let mut calls = BTreeMap::<String, ToolCall>::new();
    let mut results = BTreeSet::<String>::new();
    let mut duplicate_call_ids = BTreeSet::new();
    let mut duplicate_result_ids = BTreeSet::new();
    let mut unexpected_result_ids = BTreeSet::new();

    for message in messages {
        for content in &message.content {
            match content {
                Content::ToolCall(call) => {
                    if calls.insert(call.id.clone(), call.clone()).is_some() {
                        duplicate_call_ids.insert(call.id.clone());
                    }
                }
                Content::ToolResult(result) => {
                    if !results.insert(result.tool_call_id.clone()) {
                        duplicate_result_ids.insert(result.tool_call_id.clone());
                    }
                }
                Content::Text { .. } | Content::Image { .. } | Content::Thinking { .. } => {}
            }
        }
    }
    for result_id in &results {
        if !calls.contains_key(result_id) {
            unexpected_result_ids.insert(result_id.clone());
        }
    }
    let missing_calls = calls
        .into_values()
        .filter(|call| !results.contains(&call.id))
        .collect();

    IntegrityFindings {
        missing_calls,
        unexpected_result_ids: unexpected_result_ids.into_iter().collect(),
        duplicate_call_ids: duplicate_call_ids.into_iter().collect(),
        duplicate_result_ids: duplicate_result_ids.into_iter().collect(),
    }
}

/// Creates one bounded, durable tool-result message for interrupted calls.
#[must_use]
pub fn interrupted_tool_results(calls: &[ToolCall], reason: &str) -> Message {
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "session interrupted"
    } else {
        reason
    };
    let content = calls
        .iter()
        .map(|call| {
            Content::ToolResult(ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: json!({
                    "kind": INTERRUPTED_TOOL_RESULT_KIND,
                    "status": "error",
                    "summary": "Tool execution was not performed because the session was interrupted before a result was recorded.",
                    "reason": reason,
                    "retryable": true
                })
                .to_string(),
                is_error: true,
            })
        })
        .collect();
    Message::tool_results(content)
}

/// Rebuilds every tool exchange into provider-safe adjacency.
///
/// Durable records stay append-only. This projection moves previously persisted
/// results beside their calls in memory, which also repairs histories written by
/// older versions that allowed a non-tool record between the two sides. Callers
/// must inspect first and pause on unexpected or duplicate ids; normalization
/// intentionally has no policy for ambiguous evidence.
#[must_use]
pub fn normalize(messages: &[Message]) -> Vec<Message> {
    let mut results = BTreeMap::<String, ToolResult>::new();
    for message in messages {
        for content in &message.content {
            if let Content::ToolResult(result) = content {
                results
                    .entry(result.tool_call_id.clone())
                    .or_insert_with(|| result.clone());
            }
        }
    }

    let mut normalized = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::Tool {
            continue;
        }
        normalized.push(message.clone());
        if message.role != Role::Assistant {
            continue;
        }
        let paired = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => results.get(&call.id).cloned().map(Content::ToolResult),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !paired.is_empty() {
            normalized.push(Message::tool_results(paired));
        }
    }
    normalized
}

/// Moves a compaction cut backwards when it would separate a tool result from
/// its assistant tool-call message.
#[must_use]
pub fn safe_compaction_split(messages: &[Message], proposed: usize) -> usize {
    let mut split = proposed.min(messages.len());
    if split == messages.len()
        || messages
            .get(split)
            .is_none_or(|message| message.role != Role::Tool)
    {
        return split;
    }
    while split > 0 && messages[split].role == Role::Tool {
        split -= 1;
    }
    if split > 0 && messages[split - 1].role == Role::Assistant {
        split -= 1;
    }
    split
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceCheck {
    pub required: bool,
    pub allowed: bool,
    pub target_path: Option<String>,
    pub evidence: Vec<EvidenceStatus>,
    pub valid_evidence: Vec<AvailableEvidence>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceStatus {
    pub tool_call_id: String,
    pub requested_tool_call_id: Option<String>,
    pub path: String,
    pub available: bool,
    pub recovered_by_path: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableEvidence {
    pub tool_call_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProvenanceClaim {
    #[serde(default)]
    required: bool,
    #[serde(default)]
    derived_from: Vec<EvidenceReference>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EvidenceReference {
    #[serde(default)]
    tool_call_id: Option<String>,
    path: String,
}

#[derive(Debug)]
struct ReadEvidence {
    tool_call_id: String,
    path: String,
    canonical_path: Option<PathBuf>,
    succeeded: bool,
}

/// Validates an explicit source-derived mutation claim against successful,
/// persisted `read_file` evidence. Writes without such a claim remain backward
/// compatible; a required claim fails closed when its source is unavailable.
#[must_use]
pub fn validate_provenance(
    messages: &[Message],
    call: &ToolCall,
    workspace_root: &Path,
) -> Option<ProvenanceCheck> {
    if !matches!(call.name.as_str(), "write_file" | "edit_file") {
        return None;
    }
    let claim_value = call.arguments.get("provenance")?;
    let target_path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let claim = match serde_json::from_value::<ProvenanceClaim>(claim_value.clone()) {
        Ok(claim) => claim,
        Err(error) => {
            return Some(ProvenanceCheck {
                required: true,
                allowed: false,
                target_path,
                evidence: Vec::new(),
                valid_evidence: Vec::new(),
                warning: Some(format!("invalid provenance claim: {error}")),
            });
        }
    };

    let reads = collect_read_evidence(messages, workspace_root);
    let evidence = claim
        .derived_from
        .into_iter()
        .map(|reference| resolve_evidence_reference(reference, &reads, workspace_root))
        .collect::<Vec<_>>();
    let valid_evidence = valid_evidence_for(&evidence, &reads, workspace_root);
    let unavailable = evidence.iter().any(|item| !item.available);
    let empty_required = claim.required && evidence.is_empty();
    let allowed = !claim.required || (!unavailable && !empty_required);
    let warning = (!allowed).then(|| {
        format!(
            "required source evidence is unavailable; mutation paused instead of guessing; recovery={}",
            json!({
                "code": "required_source_evidence_unavailable",
                "retryable": true,
                "validEvidence": &valid_evidence,
                "instruction": "read each missing path, then retry with its path; toolCallId is optional"
            })
        )
    });
    Some(ProvenanceCheck {
        required: claim.required,
        allowed,
        target_path,
        evidence,
        valid_evidence,
        warning,
    })
}

fn collect_read_evidence(messages: &[Message], workspace_root: &Path) -> Vec<ReadEvidence> {
    let mut read_calls = Vec::<(&str, &str)>::new();
    let mut results = BTreeMap::<String, &ToolResult>::new();
    for message in messages {
        for content in &message.content {
            match content {
                Content::ToolCall(read) if read.name == "read_file" => {
                    if let Some(path) = read.arguments.get("path").and_then(Value::as_str) {
                        read_calls.push((&read.id, path));
                    }
                }
                Content::ToolResult(result) => {
                    results.insert(result.tool_call_id.clone(), result);
                }
                _ => {}
            }
        }
    }
    read_calls
        .into_iter()
        .map(|(tool_call_id, path)| {
            let result = results.get(tool_call_id).copied();
            let canonical_path = result
                .filter(|result| !result.is_error)
                .and_then(|result| result_artifact_path(result, workspace_root))
                .or_else(|| canonical_workspace_path(workspace_root, path));
            ReadEvidence {
                tool_call_id: tool_call_id.to_owned(),
                path: path.to_owned(),
                canonical_path,
                succeeded: result
                    .is_some_and(|result| !result.is_error && result.tool_name == "read_file"),
            }
        })
        .collect()
}

fn resolve_evidence_reference(
    reference: EvidenceReference,
    reads: &[ReadEvidence],
    workspace_root: &Path,
) -> EvidenceStatus {
    let requested_tool_call_id = reference
        .tool_call_id
        .filter(|tool_call_id| !tool_call_id.trim().is_empty());
    let canonical_reference = canonical_workspace_path(workspace_root, &reference.path);
    let requested_read = requested_tool_call_id
        .as_deref()
        .and_then(|tool_call_id| reads.iter().find(|read| read.tool_call_id == tool_call_id));
    let (resolved, available, recovered_by_path, reason) = match (
        requested_tool_call_id.as_deref(),
        requested_read,
        canonical_reference.as_ref(),
    ) {
        (_, _, None) => (
            None,
            false,
            false,
            "source path is not a valid workspace-relative path".to_owned(),
        ),
        (Some(_), Some(read), Some(canonical))
            if read.canonical_path.as_ref() != Some(canonical) =>
        {
            (
                Some(read),
                false,
                false,
                "referenced read_file path does not match".to_owned(),
            )
        }
        (Some(_), Some(read), Some(_)) if !read.succeeded => (
            Some(read),
            false,
            false,
            "referenced read_file did not succeed".to_owned(),
        ),
        (Some(_), Some(read), Some(_)) => (
            Some(read),
            true,
            false,
            "successful referenced read_file result is available".to_owned(),
        ),
        (None, _, Some(canonical)) | (Some(_), None, Some(canonical)) => {
            match reads
                .iter()
                .rev()
                .find(|read| read.succeeded && read.canonical_path.as_ref() == Some(canonical))
            {
                Some(read) => (
                    Some(read),
                    true,
                    true,
                    "bound source path to the latest successful matching read_file result".into(),
                ),
                None => (
                    None,
                    false,
                    false,
                    "no successful read_file result is available for this path".into(),
                ),
            }
        }
    };
    EvidenceStatus {
        tool_call_id: resolved
            .map(|read| read.tool_call_id.clone())
            .or_else(|| requested_tool_call_id.clone())
            .unwrap_or_default(),
        requested_tool_call_id,
        path: reference.path,
        available,
        recovered_by_path,
        reason,
    }
}

fn valid_evidence_for(
    evidence: &[EvidenceStatus],
    reads: &[ReadEvidence],
    workspace_root: &Path,
) -> Vec<AvailableEvidence> {
    let referenced_paths = evidence
        .iter()
        .filter_map(|item| canonical_workspace_path(workspace_root, &item.path))
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    reads
        .iter()
        .rev()
        .filter(|read| {
            read.succeeded
                && read
                    .canonical_path
                    .as_ref()
                    .is_some_and(|path| referenced_paths.contains(path))
                && seen.insert(read.tool_call_id.clone())
        })
        .take(16)
        .map(|read| AvailableEvidence {
            tool_call_id: read.tool_call_id.clone(),
            path: read.path.clone(),
        })
        .collect()
}

fn canonical_workspace_path(workspace_root: &Path, requested: &str) -> Option<PathBuf> {
    let requested = Path::new(requested);
    if requested.as_os_str().is_empty() || requested.is_absolute() {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in requested.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => relative.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if relative.as_os_str().is_empty() {
        return None;
    }
    let candidate = workspace_root.join(relative);
    let canonical = candidate.canonicalize().unwrap_or(candidate);
    canonical.starts_with(workspace_root).then_some(canonical)
}

fn result_artifact_path(result: &ToolResult, workspace_root: &Path) -> Option<PathBuf> {
    let value = serde_json::from_str::<Value>(&result.content).ok()?;
    let artifact = value
        .get("artifacts")?
        .as_array()?
        .iter()
        .find_map(Value::as_str)?;
    let artifact = PathBuf::from(artifact);
    let canonical = artifact.canonicalize().unwrap_or(artifact);
    canonical.starts_with(workspace_root).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{StopReason, ToolCall};

    fn workspace_root() -> PathBuf {
        std::env::current_dir()
            .expect("current dir")
            .canonicalize()
            .expect("canonical current dir")
    }

    #[test]
    fn compaction_split_keeps_tool_exchange_together() {
        let messages = vec![
            Message::user("old"),
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "source.css"}),
                })],
                StopReason::ToolUse,
            ),
            Message::tool_result("call-1", "read_file", "ok", false),
            Message::user("next"),
        ];
        assert_eq!(safe_compaction_split(&messages, 2), 1);
    }

    #[test]
    fn failed_read_cannot_satisfy_required_write_evidence() {
        let read = ToolCall {
            id: "read-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "source.css"}),
        };
        let messages = vec![
            Message::assistant(vec![Content::ToolCall(read)], StopReason::ToolUse),
            Message::tool_result("read-1", "read_file", "denied", true),
        ];
        let write = ToolCall {
            id: "write-1".into(),
            name: "write_file".into(),
            arguments: json!({
                "path": "copy.css",
                "content": "invented",
                "provenance": {
                    "required": true,
                    "derivedFrom": [{"toolCallId": "read-1", "path": "source.css"}]
                }
            }),
        };
        let check = validate_provenance(&messages, &write, &workspace_root()).expect("claim");
        assert!(!check.allowed);
        assert_eq!(check.evidence.len(), 1);
        assert!(!check.evidence[0].available);
        assert!(check.valid_evidence.is_empty());
    }

    #[test]
    fn missing_or_unknown_id_binds_to_latest_successful_matching_read() {
        let messages = vec![
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "read-old".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "source.css"}),
                })],
                StopReason::ToolUse,
            ),
            Message::tool_result("read-old", "read_file", "old", false),
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "read-latest".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "./source.css"}),
                })],
                StopReason::ToolUse,
            ),
            Message::tool_result("read-latest", "read_file", "latest", false),
        ];

        for provenance in [
            json!({"required": true, "derivedFrom": [{"path": "source.css"}]}),
            json!({
                "required": true,
                "derivedFrom": [{"toolCallId": "invented-id", "path": "source.css"}]
            }),
        ] {
            let write = ToolCall {
                id: "write-1".into(),
                name: "write_file".into(),
                arguments: json!({
                    "path": "copy.css",
                    "content": "faithful",
                    "provenance": provenance
                }),
            };
            let check = validate_provenance(&messages, &write, &workspace_root()).expect("claim");
            assert!(check.allowed);
            assert_eq!(check.evidence[0].tool_call_id, "read-latest");
            assert!(check.evidence[0].recovered_by_path);
        }
    }

    #[test]
    fn explicit_tool_id_path_mismatch_is_rejected_without_fallback() {
        let messages = vec![
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "read-other".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "other.css"}),
                })],
                StopReason::ToolUse,
            ),
            Message::tool_result("read-other", "read_file", "ok", false),
            Message::assistant(
                vec![Content::ToolCall(ToolCall {
                    id: "read-source".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "source.css"}),
                })],
                StopReason::ToolUse,
            ),
            Message::tool_result("read-source", "read_file", "ok", false),
        ];
        let write = ToolCall {
            id: "write-1".into(),
            name: "write_file".into(),
            arguments: json!({
                "path": "copy.css",
                "content": "not authorized by the claimed id",
                "provenance": {
                    "required": true,
                    "derivedFrom": [{"toolCallId": "read-other", "path": "source.css"}]
                }
            }),
        };

        let check = validate_provenance(&messages, &write, &workspace_root()).expect("claim");
        assert!(!check.allowed);
        assert!(!check.evidence[0].recovered_by_path);
        assert_eq!(
            check.evidence[0].reason,
            "referenced read_file path does not match"
        );
        assert_eq!(check.valid_evidence[0].tool_call_id, "read-source");
        assert!(
            check
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("validEvidence"))
        );
    }

    #[test]
    fn normalization_groups_results_for_one_multi_tool_assistant_turn() {
        let assistant = Message::assistant(
            vec![
                Content::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "one"}),
                }),
                Content::ToolCall(ToolCall {
                    id: "call-2".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "two"}),
                }),
            ],
            StopReason::ToolUse,
        );
        let messages = vec![
            assistant,
            Message::tool_result("call-1", "read_file", "one", false),
            Message::user("interleaved by an older runtime"),
            Message::tool_result("call-2", "read_file", "two", false),
        ];
        let normalized = normalize(&messages);
        assert_eq!(normalized[1].role, Role::Tool);
        assert_eq!(normalized[1].content.len(), 2);
        assert!(matches!(
            &normalized[1].content[..],
            [Content::ToolResult(first), Content::ToolResult(second)]
                if first.tool_call_id == "call-1" && second.tool_call_id == "call-2"
        ));
    }
}
