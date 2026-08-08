//! Reference-compatible JSONL RPC primitives shared by the CLI and tests.

#![allow(
    clippy::missing_errors_doc,
    reason = "wire validation errors identify their malformed field directly"
)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::model::{Content, Message};

const MAX_RPC_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Inline image accepted by `prompt`, `steer`, and `follow_up` RPC commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcImageContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub data: String,
    pub mime_type: String,
}

impl RpcImageContent {
    /// Validates the reference image shape and bounds decoded memory use.
    pub fn validate(&self) -> Result<(), String> {
        if self.content_type != "image" {
            return Err("images[].type must be image".into());
        }
        if !matches!(
            self.mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Err(
                "images[].mimeType must be image/png, image/jpeg, image/gif, or image/webp".into(),
            );
        }
        let estimated = self.data.len().saturating_mul(3) / 4;
        if estimated > MAX_RPC_IMAGE_BYTES {
            return Err(format!(
                "image exceeds the {MAX_RPC_IMAGE_BYTES}-byte decoded limit"
            ));
        }
        STANDARD
            .decode(&self.data)
            .map_err(|_| "images[].data must be valid base64".to_string())?;
        Ok(())
    }
}

/// A user RPC input retaining both text and inline image blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcUserInput {
    pub message: String,
    pub images: Vec<RpcImageContent>,
}

impl RpcUserInput {
    /// Creates a validated input. Image-only prompts are supported.
    pub fn new(message: Option<&str>, images: Vec<RpcImageContent>) -> Result<Self, String> {
        images.iter().try_for_each(RpcImageContent::validate)?;
        let message = message.unwrap_or_default().trim().to_owned();
        if message.is_empty() && images.is_empty() {
            return Err("message or images must be provided".into());
        }
        Ok(Self { message, images })
    }

    /// Converts to the core user-message representation without re-encoding data.
    pub fn into_message(self) -> Message {
        let mut content =
            Vec::with_capacity(usize::from(!self.message.is_empty()) + self.images.len());
        if !self.message.is_empty() {
            content.push(Content::Text { text: self.message });
        }
        content.extend(self.images.into_iter().map(|image| Content::Image {
            data: image.data,
            mime_type: image.mime_type,
        }));
        Message::user_content(content)
    }
}

/// Reference queue preview emitted by `session_action_update`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionActionSnapshot {
    pub queued_count: usize,
    pub steering: Vec<String>,
    pub follow_ups: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_only_input_maps_to_a_user_image_block() {
        let input = RpcUserInput::new(
            None,
            vec![RpcImageContent {
                content_type: "image".into(),
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
        )
        .unwrap();
        let message = input.into_message();
        assert!(matches!(message.content[0], Content::Image { .. }));
    }
}
