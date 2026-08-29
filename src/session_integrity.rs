use std::collections::{BTreeMap, BTreeSet};

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
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceStatus {
    pub tool_call_id: String,
    pub path: String,
    pub available: bool,
    pub reason: String,
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
    tool_call_id: String,
    path: String,
}

/// Validates an explicit source-derived mutation claim against successful,
/// persisted `read_file` evidence. Writes without such a claim remain backward
/// compatible; a required claim fails closed when its source is unavailable.
#[must_use]
pub fn validate_provenance(messages: &[Message], call: &ToolCall) -> Option<ProvenanceCheck> {
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
                warning: Some(format!("invalid provenance claim: {error}")),
            });
        }
    };

    let mut read_calls = BTreeMap::<String, &str>::new();
    let mut results = BTreeMap::<String, bool>::new();
    for message in messages {
        for content in &message.content {
            match content {
                Content::ToolCall(read) if read.name == "read_file" => {
                    if let Some(path) = read.arguments.get("path").and_then(Value::as_str) {
                        read_calls.insert(read.id.clone(), path);
                    }
                }
                Content::ToolResult(result) => {
                    results.insert(result.tool_call_id.clone(), !result.is_error);
                }
                _ => {}
            }
        }
    }

    let evidence = claim
        .derived_from
        .into_iter()
        .map(|reference| {
            let (available, reason) = match read_calls.get(&reference.tool_call_id) {
                None => (false, "referenced read_file call is unavailable".to_owned()),
                Some(path) if *path != reference.path => {
                    (false, "referenced read_file path does not match".to_owned())
                }
                Some(_) if results.get(&reference.tool_call_id) == Some(&true) => {
                    (true, "successful read_file result is available".to_owned())
                }
                Some(_) => (false, "referenced read_file did not succeed".to_owned()),
            };
            EvidenceStatus {
                tool_call_id: reference.tool_call_id,
                path: reference.path,
                available,
                reason,
            }
        })
        .collect::<Vec<_>>();
    let unavailable = evidence.iter().any(|item| !item.available);
    let empty_required = claim.required && evidence.is_empty();
    let allowed = !claim.required || (!unavailable && !empty_required);
    let warning = (!allowed).then(|| {
        "required source evidence is unavailable; mutation paused instead of guessing".into()
    });
    Some(ProvenanceCheck {
        required: claim.required,
        allowed,
        target_path,
        evidence,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{StopReason, ToolCall};

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
        let check = validate_provenance(&messages, &write).expect("claim");
        assert!(!check.allowed);
        assert_eq!(check.evidence.len(), 1);
        assert!(!check.evidence[0].available);
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
