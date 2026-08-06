use crate::{
    auth::{AuthCredential, AuthStore},
    error::Result,
    mcp::config::McpServerConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthSource {
    BearerEnv,
    StoredApiKey,
    StoredOAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthStatus {
    pub provider_id: String,
    pub enabled: bool,
    pub source: Option<McpAuthSource>,
    pub expired: bool,
    pub uses_oauth: bool,
    pub bearer_token_env_var: Option<String>,
}

/// Resolves the effective MCP auth status for a server configuration.
///
/// # Errors
///
/// Returns an error when the underlying auth store cannot be read.
pub async fn auth_status(config: &McpServerConfig, store: &AuthStore) -> Result<McpAuthStatus> {
    auth_status_from_parts(
        &config.server,
        config.enabled,
        config.oauth,
        config.bearer_token_env_var.as_deref(),
        store,
        |env_var| std::env::var(env_var).ok(),
    )
    .await
}

/// Resolves the effective MCP auth status using an injected env lookup.
///
/// # Errors
///
/// Returns an error when the underlying auth store cannot be read.
pub async fn auth_status_with_lookup<F>(
    config: &McpServerConfig,
    store: &AuthStore,
    env_lookup: F,
) -> Result<McpAuthStatus>
where
    F: Fn(&str) -> Option<String>,
{
    auth_status_from_parts(
        &config.server,
        config.enabled,
        config.oauth,
        config.bearer_token_env_var.as_deref(),
        store,
        env_lookup,
    )
    .await
}

pub(crate) async fn auth_status_from_parts<F>(
    server: &str,
    enabled: bool,
    oauth: bool,
    bearer_token_env_var: Option<&str>,
    store: &AuthStore,
    env_lookup: F,
) -> Result<McpAuthStatus>
where
    F: Fn(&str) -> Option<String>,
{
    let provider_id = format!("mcp:{server}");
    let mut status = McpAuthStatus {
        provider_id: provider_id.clone(),
        enabled: false,
        source: None,
        expired: false,
        uses_oauth: oauth,
        bearer_token_env_var: bearer_token_env_var.map(str::to_owned),
    };
    if !enabled {
        return Ok(status);
    }
    if let Some(env_var) = bearer_token_env_var
        && env_lookup(env_var).is_some_and(|value| !value.trim().is_empty())
    {
        status.enabled = true;
        status.source = Some(McpAuthSource::BearerEnv);
        return Ok(status);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    if let Some(credential) = store.get(&provider_id).await? {
        match credential {
            AuthCredential::ApiKey { .. } => {
                status.enabled = true;
                status.source = Some(McpAuthSource::StoredApiKey);
            }
            AuthCredential::OAuth(value) => {
                status.enabled = true;
                status.source = Some(McpAuthSource::StoredOAuth);
                status.expired = value.is_expired(now_ms);
            }
        }
    }
    Ok(status)
}
