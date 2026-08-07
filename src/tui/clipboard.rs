use std::{path::Path, process::Stdio, time::Duration};

use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    error::{MimirError, Result},
    runtime::AgentRuntime,
};

const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClipboardCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

/// Copies the most recent non-empty assistant message using a bounded,
/// shell-free platform clipboard helper.
pub(super) async fn copy_last_assistant_message(runtime: &AgentRuntime) -> Result<String> {
    let messages = runtime.messages_snapshot().await;
    let text = messages
        .iter()
        .rev()
        .find(|message| {
            message.role == crate::model::Role::Assistant && !message.text().trim().is_empty()
        })
        .map(crate::model::Message::text)
        .ok_or_else(|| MimirError::Configuration("there is no agent message to copy".into()))?;
    copy_text(&text).await?;
    Ok(format!(
        "Copied the last agent message ({} characters)",
        text.chars().count()
    ))
}

async fn copy_text(text: &str) -> Result<()> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(MimirError::Configuration(format!(
            "clipboard payload exceeds the {MAX_CLIPBOARD_BYTES}-byte limit"
        )));
    }

    let mut failures = Vec::new();
    for candidate in clipboard_commands() {
        if !Path::new(candidate.program).is_file() {
            continue;
        }
        match run_clipboard_command(*candidate, text.as_bytes()).await {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(error.to_string()),
        }
    }
    let detail = if failures.is_empty() {
        "no supported platform clipboard helper was found".into()
    } else {
        failures.join("; ")
    };
    Err(MimirError::Configuration(format!(
        "failed to copy to the clipboard: {detail}"
    )))
}

async fn run_clipboard_command(candidate: ClipboardCommand, payload: &[u8]) -> Result<()> {
    let mut child = Command::new(candidate.program)
        .args(candidate.arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            MimirError::Configuration(format!("{} could not start: {error}", candidate.program))
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        MimirError::Configuration(format!(
            "{} did not expose a clipboard input stream",
            candidate.program
        ))
    })?;
    stdin.write_all(payload).await?;
    drop(stdin);
    let status = tokio::time::timeout(CLIPBOARD_TIMEOUT, child.wait())
        .await
        .map_err(|_| MimirError::Configuration(format!("{} timed out", candidate.program)))??;
    if status.success() {
        Ok(())
    } else {
        Err(MimirError::Configuration(format!(
            "{} exited with {status}",
            candidate.program
        )))
    }
}

#[cfg(target_os = "macos")]
const fn clipboard_commands() -> &'static [ClipboardCommand] {
    &[ClipboardCommand {
        program: "/usr/bin/pbcopy",
        arguments: &[],
    }]
}

#[cfg(target_os = "linux")]
const fn clipboard_commands() -> &'static [ClipboardCommand] {
    &[
        ClipboardCommand {
            program: "/usr/bin/wl-copy",
            arguments: &["--type", "text/plain;charset=utf-8"],
        },
        ClipboardCommand {
            program: "/usr/bin/xclip",
            arguments: &["-selection", "clipboard", "-in"],
        },
        ClipboardCommand {
            program: "/usr/bin/xsel",
            arguments: &["--clipboard", "--input"],
        },
    ]
}

#[cfg(target_os = "windows")]
const fn clipboard_commands() -> &'static [ClipboardCommand] {
    &[ClipboardCommand {
        program: "C:\\Windows\\System32\\clip.exe",
        arguments: &[],
    }]
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const fn clipboard_commands() -> &'static [ClipboardCommand] {
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_helpers_are_absolute_and_never_invoke_a_shell() {
        for candidate in clipboard_commands() {
            assert!(Path::new(candidate.program).is_absolute());
            assert!(!candidate.arguments.iter().any(|argument| argument == &"-c"));
        }
    }

    #[tokio::test]
    async fn clipboard_payload_has_an_explicit_byte_limit() {
        let error = copy_text(&"x".repeat(MAX_CLIPBOARD_BYTES + 1))
            .await
            .expect_err("oversized clipboard payload");
        assert!(error.to_string().contains("1048576-byte limit"));
    }
}
