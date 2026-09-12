use std::{path::Path, process::Stdio, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    error::{MimirError, Result},
    runtime::AgentRuntime,
};

use super::ImageAttachment;

const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 10 * 1024 * 1024;
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

/// Reads a PNG image from the native clipboard without invoking a shell.
///
/// Text clipboard contents return `Ok(None)` so bracketed terminal paste can
/// continue through the normal input path.
pub(super) async fn read_image() -> Result<Option<ImageAttachment>> {
    let Some((bytes, mime_type)) = read_platform_image().await? else {
        return Ok(None);
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(MimirError::Configuration(format!(
            "clipboard image exceeds the {MAX_CLIPBOARD_IMAGE_BYTES}-byte limit"
        )));
    }
    Ok(Some(ImageAttachment {
        byte_size: bytes.len(),
        data: BASE64.encode(bytes),
        mime_type,
    }))
}

#[cfg(target_os = "macos")]
async fn read_platform_image() -> Result<Option<(Vec<u8>, String)>> {
    const SCRIPT: &str = r#"try
return the clipboard as «class PNGf»
on error
return "NO_IMAGE"
end try"#;
    let output = tokio::time::timeout(
        CLIPBOARD_TIMEOUT,
        Command::new("/usr/bin/osascript")
            .args(["-e", SCRIPT])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| MimirError::Configuration("clipboard image read timed out".into()))??;
    let descriptor = String::from_utf8_lossy(&output.stdout);
    let descriptor = descriptor.trim();
    if output.status.success() && descriptor == "NO_IMAGE" {
        return Ok(None);
    }
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(MimirError::Configuration(format!(
            "failed to read clipboard image: {}",
            detail.trim()
        )));
    }
    let bytes = decode_macos_png_descriptor(descriptor)?;
    Ok(Some((bytes, "image/png".into())))
}

#[cfg(target_os = "macos")]
fn decode_macos_png_descriptor(descriptor: &str) -> Result<Vec<u8>> {
    let hex = descriptor
        .strip_prefix("«data PNGf")
        .and_then(|value| value.strip_suffix('»'))
        .ok_or_else(|| {
            MimirError::Configuration("clipboard returned an invalid PNG descriptor".into())
        })?;
    if hex.len() / 2 > MAX_CLIPBOARD_IMAGE_BYTES || hex.len() % 2 != 0 {
        return Err(MimirError::Configuration(
            "clipboard image descriptor is invalid or too large".into(),
        ));
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|error| {
                MimirError::Configuration(format!("clipboard image is not valid hex: {error}"))
            })?;
            u8::from_str_radix(pair, 16).map_err(|error| {
                MimirError::Configuration(format!("clipboard image is not valid hex: {error}"))
            })
        })
        .collect::<Result<Vec<_>>>()
}

#[cfg(not(target_os = "macos"))]
fn read_platform_image() -> std::future::Ready<Result<Option<(Vec<u8>, String)>>> {
    std::future::ready(Ok(None))
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_png_clipboard_descriptor_decodes_without_a_shell_or_temp_file() {
        let bytes =
            decode_macos_png_descriptor("«data PNGf89504E470D0A1A0A»").expect("PNG descriptor");
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
    }
}
