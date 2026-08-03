use std::fmt;

use reqwest::Url;
use thiserror::Error;

const ACCOUNT_PLACEHOLDER: &str = "{CLOUDFLARE_ACCOUNT_ID}";
const GATEWAY_PLACEHOLDER: &str = "{CLOUDFLARE_GATEWAY_ID}";
const MAX_IDENTIFIER_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudflareProvider {
    WorkersAi,
    AiGateway,
}

impl CloudflareProvider {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::WorkersAi => "cloudflare-workers-ai",
            Self::AiGateway => "cloudflare-ai-gateway",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudflareConfigError {
    #[error("CLOUDFLARE_ACCOUNT_ID is missing or invalid")]
    InvalidAccountId,
    #[error("CLOUDFLARE_GATEWAY_ID is missing or invalid")]
    InvalidGatewayId,
    #[error("Cloudflare base URL contains unsupported placeholder {0}")]
    UnsupportedPlaceholder(String),
    #[error("Cloudflare base URL is invalid")]
    InvalidBaseUrl,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CloudflareConfig {
    provider: CloudflareProvider,
    account_id: String,
    gateway_id: Option<String>,
}

impl CloudflareConfig {
    /// Builds validated Cloudflare endpoint configuration without retaining API
    /// credentials. Identifiers are restricted to URL-path-safe characters.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when a required identifier is absent, too
    /// long, or contains characters that could alter the endpoint path.
    pub fn new(
        provider: CloudflareProvider,
        account_id: impl Into<String>,
        gateway_id: Option<&str>,
    ) -> Result<Self, CloudflareConfigError> {
        let account_id = account_id.into();
        if !valid_identifier(&account_id) {
            return Err(CloudflareConfigError::InvalidAccountId);
        }
        let gateway_id = gateway_id.map(str::to_owned);
        if provider == CloudflareProvider::AiGateway
            && !gateway_id.as_deref().is_some_and(valid_identifier)
        {
            return Err(CloudflareConfigError::InvalidGatewayId);
        }
        if gateway_id
            .as_deref()
            .is_some_and(|value| !valid_identifier(value))
        {
            return Err(CloudflareConfigError::InvalidGatewayId);
        }
        Ok(Self {
            provider,
            account_id,
            gateway_id,
        })
    }

    /// Loads only the documented Cloudflare endpoint identifiers from the
    /// environment. API credentials are resolved independently by the auth
    /// layer and are never stored in this value.
    ///
    /// # Errors
    ///
    /// Returns the same sanitized validation errors as [`Self::new`].
    pub fn from_environment(provider: CloudflareProvider) -> Result<Self, CloudflareConfigError> {
        let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
        let gateway_id = std::env::var("CLOUDFLARE_GATEWAY_ID").ok();
        Self::new(provider, account_id, gateway_id.as_deref())
    }

    /// Expands the two documented Cloudflare placeholders and validates the
    /// resulting HTTPS URL. Unknown placeholders fail closed rather than
    /// reading arbitrary environment variables.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for unknown placeholders or invalid URLs.
    pub fn resolve_base_url(&self, template: &str) -> Result<String, CloudflareConfigError> {
        if let Some(placeholder) = first_placeholder(template)
            && placeholder != ACCOUNT_PLACEHOLDER
            && placeholder != GATEWAY_PLACEHOLDER
        {
            return Err(CloudflareConfigError::UnsupportedPlaceholder(
                placeholder.to_owned(),
            ));
        }
        let mut resolved = template.replace(ACCOUNT_PLACEHOLDER, &self.account_id);
        if resolved.contains(GATEWAY_PLACEHOLDER) {
            let gateway_id = self
                .gateway_id
                .as_deref()
                .ok_or(CloudflareConfigError::InvalidGatewayId)?;
            resolved = resolved.replace(GATEWAY_PLACEHOLDER, gateway_id);
        }
        if resolved.contains('{') || resolved.contains('}') {
            return Err(CloudflareConfigError::UnsupportedPlaceholder(
                "{...}".into(),
            ));
        }
        let url = Url::parse(&resolved).map_err(|_| CloudflareConfigError::InvalidBaseUrl)?;
        if !self.valid_endpoint(&url) {
            return Err(CloudflareConfigError::InvalidBaseUrl);
        }
        Ok(resolved)
    }

    fn valid_endpoint(&self, url: &Url) -> bool {
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return false;
        }
        match self.provider {
            CloudflareProvider::WorkersAi => {
                url.host_str() == Some("api.cloudflare.com")
                    && url.path() == format!("/client/v4/accounts/{}/ai/v1", self.account_id)
            }
            CloudflareProvider::AiGateway => {
                let Some(gateway_id) = self.gateway_id.as_deref() else {
                    return false;
                };
                url.host_str() == Some("gateway.ai.cloudflare.com")
                    && matches!(
                        url.path()
                            .strip_prefix(&format!("/v1/{}/{gateway_id}/", self.account_id)),
                        Some("compat" | "openai" | "anthropic")
                    )
            }
        }
    }
}

impl fmt::Debug for CloudflareConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudflareConfig")
            .field("provider", &self.provider.id())
            .field("account_id", &"[REDACTED]")
            .field(
                "gateway_id",
                &self.gateway_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn first_placeholder(value: &str) -> Option<&str> {
    let start = value.find('{')?;
    let tail = &value[start..];
    let end = tail.find('}')?;
    Some(&tail[..=end])
}
