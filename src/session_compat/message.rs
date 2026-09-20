use std::{fmt::Write as _, path::Path};

use serde_json::{Map, Value};

use crate::{
    error::{MimirError, Result},
    model::{Content, Message, Role, StopReason, ToolCall, ToolResult, Usage},
};

pub(super) fn reference_message(
    source_path: &Path,
    value: &Value,
    fallback_timestamp: i64,
) -> Result<Option<Message>> {
    let role = required_string(value, "role", source_path)?;
    let timestamp_ms = value
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or(fallback_timestamp);
    let usage = parse_usage(value.get("usage"));
    match role {
        "user" => {
            message_with_content(Role::User, value, usage, timestamp_ms, source_path).map(Some)
        }
        "assistant" => {
            let mut message =
                message_with_content(Role::Assistant, value, usage, timestamp_ms, source_path)?;
            message.stop_reason =
                parse_stop_reason(value.get("stopReason").and_then(Value::as_str));
            Ok(Some(message))
        }
        "toolResult" => Ok(Some(tool_result_message(value, usage, timestamp_ms))),
        "bashExecution" => bash_execution_message(value, usage, timestamp_ms),
        "custom" => custom_message(source_path, value, usage, timestamp_ms),
        "branchSummary" => {
            branch_summary_message(source_path, value, usage, timestamp_ms).map(Some)
        }
        other => Err(session_error(
            source_path,
            &format!("unsupported reference message role {other}"),
        )),
    }
}

fn bash_execution_message(
    value: &Value,
    usage: Usage,
    timestamp_ms: i64,
) -> Result<Option<Message>> {
    if value
        .get("excludeFromContext")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = format!("Ran `{command}`\n{}", bash_output(value, output));
    if text.len() > 4 * 1024 * 1024 {
        return Err(MimirError::Configuration(
            "translated bash message exceeds the 4 MiB limit".into(),
        ));
    }
    Ok(Some(Message {
        role: Role::User,
        content: vec![Content::Text { text }],
        stop_reason: None,
        usage,
        timestamp_ms,
    }))
}

fn bash_output(value: &Value, output: &str) -> String {
    let mut text = if output.is_empty() {
        "(no output)".to_owned()
    } else {
        let longest = output
            .as_bytes()
            .split(|byte| *byte != b'`')
            .map(<[u8]>::len)
            .max()
            .unwrap_or_default();
        let fence = "`".repeat(longest.saturating_add(1).max(3));
        format!("{fence}\n{output}\n{fence}")
    };
    if value.get("cancelled").and_then(Value::as_bool) == Some(true) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = value.get("exitCode").and_then(Value::as_i64)
        && exit_code != 0
    {
        let _ = write!(text, "\n\nCommand exited with code {exit_code}");
    }
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        if let Some(path) = value.get("fullOutputPath").and_then(Value::as_str) {
            let _ = write!(text, "\n\n[Output truncated. Full output: {path}]");
        } else {
            text.push_str("\n\n[Output truncated.]");
        }
    }
    text
}

fn custom_message(
    source_path: &Path,
    value: &Value,
    usage: Usage,
    timestamp_ms: i64,
) -> Result<Option<Message>> {
    let custom_type = required_string(value, "customType", source_path)?;
    if matches!(
        custom_type,
        "session_slash_command" | "session_slash_command_result" | "compaction_outcome"
    ) {
        return Ok(None);
    }
    message_with_content(Role::User, value, usage, timestamp_ms, source_path).map(Some)
}

fn branch_summary_message(
    source_path: &Path,
    value: &Value,
    usage: Usage,
    timestamp_ms: i64,
) -> Result<Message> {
    let summary = required_string(value, "summary", source_path)?;
    Ok(Message {
        role: Role::User,
        content: vec![Content::Text {
            text: format!(
                "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
            ),
        }],
        stop_reason: None,
        usage,
        timestamp_ms,
    })
}

fn message_with_content(
    role: Role,
    value: &Value,
    usage: Usage,
    timestamp_ms: i64,
    source_path: &Path,
) -> Result<Message> {
    Ok(Message {
        role,
        content: reference_content(value.get("content"), source_path)?,
        stop_reason: None,
        usage,
        timestamp_ms,
    })
}

fn tool_result_message(value: &Value, usage: Usage, timestamp_ms: i64) -> Message {
    Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            tool_call_id: value
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("legacy-call")
                .to_owned(),
            tool_name: value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("legacy-tool")
                .to_owned(),
            content: flatten_text(value.get("content")),
            is_error: value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })],
        stop_reason: None,
        usage,
        timestamp_ms,
    }
}

fn reference_content(value: Option<&Value>, source_path: &Path) -> Result<Vec<Content>> {
    let Some(value) = value else {
        return Ok(vec![Content::Text {
            text: String::new(),
        }]);
    };
    if let Some(text) = value.as_str() {
        return Ok(vec![Content::Text {
            text: text.to_owned(),
        }]);
    }
    let blocks = value.as_array().ok_or_else(|| {
        session_error(
            source_path,
            "reference message content must be text or an array",
        )
    })?;
    blocks
        .iter()
        .map(|block| reference_content_block(block, source_path))
        .collect()
}

fn reference_content_block(block: &Value, source_path: &Path) -> Result<Content> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(Content::Text {
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        Some("thinking") => Ok(Content::Thinking {
            text: block
                .get("thinking")
                .or_else(|| block.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            signature: block
                .get("signature")
                .or_else(|| block.get("data"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            redacted: block
                .get("redacted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some("redacted_thinking") => Ok(Content::Thinking {
            text: "[redacted thinking]".into(),
            signature: Some(required_string(block, "data", source_path)?.to_owned()),
            redacted: true,
        }),
        Some("toolCall") => Ok(Content::ToolCall(ToolCall {
            id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("legacy-call")
                .to_owned(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("legacy-tool")
                .to_owned(),
            arguments: block
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::default())),
        })),
        Some("image") => Ok(Content::Image {
            data: required_string(block, "data", source_path)?.to_owned(),
            mime_type: required_string(block, "mimeType", source_path)?.to_owned(),
        }),
        Some(other) => Err(session_error(
            source_path,
            &format!("unsupported reference content block {other}"),
        )),
        None => Err(session_error(source_path, "content block is missing type")),
    }
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    let input = value
        .get("input")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_read = value
        .get("cacheRead")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_write = value
        .get("cacheWrite")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    Usage {
        input_tokens: input.saturating_add(cache_read).saturating_add(cache_write),
        output_tokens: value
            .get("output")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

fn flatten_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn parse_stop_reason(value: Option<&str>) -> Option<StopReason> {
    match value {
        Some("stop") => Some(StopReason::Stop),
        Some("length") => Some(StopReason::Length),
        Some("toolUse") => Some(StopReason::ToolUse),
        Some("error") => Some(StopReason::Error),
        Some("aborted") => Some(StopReason::Aborted),
        Some("budgetExhausted") => Some(StopReason::BudgetExhausted),
        _ => None,
    }
}

fn required_string<'a>(value: &'a Value, field: &str, path: &Path) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| session_error(path, &format!("session field {field} must be a string")))
}

fn session_error(path: &Path, message: &str) -> MimirError {
    MimirError::Session {
        path: path.to_path_buf(),
        message: message.into(),
    }
}
