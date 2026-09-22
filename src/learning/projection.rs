//! Privacy-minimised projections of local session spans for Jev.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    model::{Content, Message, Role},
    session::{SessionPayload, SessionRecord},
};

pub const MAX_PROJECTION_BYTES: usize = 96 * 1024;
const MAX_TEXT_BYTES: usize = 12 * 1024;
const MAX_TOOL_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogicalTaskProjection {
    pub schema: u16,
    pub session_id: String,
    pub first_record_id: Uuid,
    pub last_record_id: Uuid,
    pub messages: Vec<ProjectedMessage>,
    pub tool_events: Vec<ProjectedToolEvent>,
    pub validation_evidence: Vec<String>,
    pub terminal_outcome: String,
    pub exposed_candidate_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectedMessage {
    pub role: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectedToolEvent {
    pub name: String,
    pub result: String,
    pub is_error: bool,
}

/// Projects a record range without retaining thinking, images, tool arguments, or binary data.
pub fn project_records(
    session_id: &str,
    records: &[SessionRecord],
    first_record_id: Uuid,
    last_record_id: Uuid,
    workspace_root: Option<&str>,
    exposed_candidate_ids: Vec<Uuid>,
) -> Option<LogicalTaskProjection> {
    let first = records
        .iter()
        .position(|record| record.record_id == first_record_id)?;
    let last = records
        .iter()
        .position(|record| record.record_id == last_record_id)?;
    if first > last {
        return None;
    }
    let mut messages = Vec::new();
    let mut tool_events = Vec::new();
    let mut validation_evidence = Vec::new();
    let mut terminal_outcome = "unknown".to_owned();
    for record in &records[first..=last] {
        match &record.payload {
            SessionPayload::Message(message) => {
                project_message(message, workspace_root, &mut messages, &mut tool_events);
            }
            SessionPayload::RuntimeEvent { name, detail } => {
                if matches!(
                    name.as_str(),
                    "finish_task" | "quality_gate" | "validator" | "task_completion"
                ) {
                    validation_evidence.push(bound(&scrub(detail, workspace_root), MAX_TOOL_BYTES));
                    terminal_outcome.clone_from(name);
                }
            }
            SessionPayload::Compaction { summary, .. } => messages.push(ProjectedMessage {
                role: "summary".into(),
                text: bound(&scrub(summary, workspace_root), MAX_TEXT_BYTES),
            }),
        }
    }
    let mut projection = LogicalTaskProjection {
        schema: 1,
        session_id: session_id.to_owned(),
        first_record_id,
        last_record_id,
        messages,
        tool_events,
        validation_evidence,
        terminal_outcome,
        exposed_candidate_ids,
    };
    while serde_json::to_vec(&projection).map_or(usize::MAX, |value| value.len())
        > MAX_PROJECTION_BYTES
    {
        if !projection.tool_events.is_empty() {
            projection.tool_events.remove(0);
        } else if projection.messages.len() > 1 {
            projection.messages.remove(0);
        } else {
            break;
        }
    }
    Some(projection)
}

fn project_message(
    message: &Message,
    root: Option<&str>,
    messages: &mut Vec<ProjectedMessage>,
    tools: &mut Vec<ProjectedToolEvent>,
) {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    };
    let mut text = Vec::new();
    for block in &message.content {
        match block {
            Content::Text { text: value } => text.push(scrub(value, root)),
            Content::ToolCall(call) => tools.push(ProjectedToolEvent {
                name: call.name.clone(),
                result: "tool call recorded; arguments excluded".into(),
                is_error: false,
            }),
            Content::ToolResult(result) => tools.push(ProjectedToolEvent {
                name: result.tool_name.clone(),
                result: bound(&scrub(&result.content, root), MAX_TOOL_BYTES),
                is_error: result.is_error,
            }),
            Content::Image { .. } | Content::Thinking { .. } => {}
        }
    }
    if !text.is_empty() {
        messages.push(ProjectedMessage {
            role: role.into(),
            text: bound(&text.join("\n"), MAX_TEXT_BYTES),
        });
    }
}

/// Removes actual authorization material while leaving ordinary task text intact.
pub fn scrub(value: &str, workspace_root: Option<&str>) -> String {
    let mut output = workspace_root.filter(|root| !root.is_empty()).map_or_else(
        || value.to_owned(),
        |root| value.replace(root, "$WORKSPACE"),
    );
    for marker in ["sk-", "Bearer ", "ghp_", "github_pat_", "AKIA"] {
        let mut start = 0;
        while let Some(relative) = output[start..].find(marker) {
            let at = start + relative;
            let end = output[at..]
                .find(char::is_whitespace)
                .map_or(output.len(), |offset| at + offset);
            output.replace_range(at..end, "[REDACTED_CREDENTIAL]");
            start = at + "[REDACTED_CREDENTIAL]".len();
        }
    }
    output
}

fn bound(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let head = max / 2;
    let tail = max.saturating_sub(head + 40);
    let tail_start = ceil_boundary(value, value.len().saturating_sub(tail));
    format!(
        "{}\n...[bounded output omitted]...\n{}",
        &value[..floor_boundary(value, head)],
        &value[tail_start..]
    )
}

fn floor_boundary(value: &str, limit: usize) -> usize {
    let mut boundary = limit.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn ceil_boundary(value: &str, limit: usize) -> usize {
    let mut boundary = limit.min(value.len());
    while boundary < value.len() && !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrubs_credentials_and_normalizes_root() {
        let output = scrub("/work/a sk-secret Bearer token", Some("/work/a"));
        assert!(output.contains("$WORKSPACE"));
        assert!(!output.contains("sk-secret"));
    }
    #[test]
    fn bounds_with_error_tail() {
        let value = format!("{}ERROR", "x".repeat(MAX_TOOL_BYTES + 200));
        assert!(bound(&value, MAX_TOOL_BYTES).ends_with("ERROR"));
    }
    #[test]
    fn empty_workspace_root_does_not_corrupt_text() {
        assert_eq!(scrub("plain text", None), "plain text");
        assert_eq!(scrub("plain text", Some("")), "plain text");
    }
    #[test]
    fn bounds_unicode_without_splitting_codepoints() {
        let value = format!("{}終", "é".repeat(MAX_TOOL_BYTES));
        let bounded = bound(&value, MAX_TOOL_BYTES);
        assert!(bounded.ends_with('終'));
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }
}
