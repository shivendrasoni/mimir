use crate::error::{MimirError, Result};

pub const MAX_SHARE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_SHARE_VIEWER_BASE: &str = "https://pi.dev/session/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePayload {
    pub filename: &'static str,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
    viewer_base: String,
}

impl SharePayload {
    /// Builds the reference viewer URL for an already-created gist id.
    ///
    /// # Errors
    ///
    /// Returns an error unless the gist id is a bounded hexadecimal identifier.
    pub fn viewer_url(&self, gist_id: &str) -> Result<String> {
        if !(8..=64).contains(&gist_id.len())
            || !gist_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(MimirError::Configuration(
                "gist id must contain 8-64 hexadecimal characters".into(),
            ));
        }
        Ok(format!("{}#{gist_id}", self.viewer_base))
    }
}

/// Prepares bounded HTML bytes for the reference secret-gist share flow without performing I/O.
///
/// # Errors
///
/// Returns an error for an empty/oversized document or an insecure viewer base URL.
pub fn prepare_share_payload(html: &[u8], viewer_base: Option<&str>) -> Result<SharePayload> {
    if html.is_empty() || html.len() > MAX_SHARE_PAYLOAD_BYTES {
        return Err(MimirError::Configuration(format!(
            "share HTML must contain 1-{MAX_SHARE_PAYLOAD_BYTES} bytes"
        )));
    }
    std::str::from_utf8(html)
        .map_err(|_| MimirError::Configuration("share HTML must be valid UTF-8".into()))?;
    let viewer_base = normalize_viewer_base(viewer_base.unwrap_or(DEFAULT_SHARE_VIEWER_BASE))?;
    Ok(SharePayload {
        filename: "session.html",
        content_type: "text/html; charset=utf-8",
        bytes: html.to_vec(),
        viewer_base,
    })
}

fn normalize_viewer_base(value: &str) -> Result<String> {
    let value = value.trim();
    if value.len() > 2_048 || value.chars().any(char::is_control) {
        return Err(MimirError::Configuration(
            "share viewer base must be a bounded HTTPS URL without query or fragment".into(),
        ));
    }
    let parsed = reqwest::Url::parse(value)
        .map_err(|_| MimirError::Configuration("share viewer base must be a valid URL".into()))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(MimirError::Configuration(
            "share viewer base must be HTTPS with a host and no credentials, query, or fragment"
                .into(),
        ));
    }
    Ok(format!("{}/", parsed.as_str().trim_end_matches('/')))
}
