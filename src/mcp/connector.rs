use std::path::Path;

use crate::{
    auth::AuthCredential,
    error::{MimirError, Result},
    mcp::{McpAuthCoordinator, McpClient, McpOAuthClient, McpServerCatalog},
};

/// Resolves one catalog entry, applies its configured authentication precedence, refreshes
/// expired OAuth credentials, and completes the MCP initialize handshake.
///
/// Explicit `Authorization` headers take precedence over bearer-token environment variables,
/// which take precedence over stored API-key or OAuth credentials.
///
/// # Errors
///
/// Returns an error for missing/disabled servers, unavailable credentials, refresh failures, or
/// transport and protocol failures.
pub async fn connect_catalog_client(
    catalog: &McpServerCatalog,
    state_root: &Path,
    server: &str,
) -> Result<McpClient> {
    let config = catalog
        .resolve(server)
        .await?
        .ok_or_else(|| MimirError::Configuration(format!("unknown MCP server: {server}")))?;
    if !config.enabled {
        return Err(MimirError::Configuration(format!(
            "MCP server {server} is disabled"
        )));
    }
    let Some(remote) = config.remote.as_ref() else {
        return McpClient::connect(&config).await;
    };
    let has_authorization_header = config
        .headers
        .keys()
        .chain(remote.headers.keys())
        .any(|header| header.eq_ignore_ascii_case("authorization"));
    if has_authorization_header {
        return McpClient::connect_with_bearer(&config, None).await;
    }

    let bearer_from_env = config
        .bearer_token_env_var
        .as_deref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty());
    if let Some(bearer) = bearer_from_env {
        return McpClient::connect_with_bearer(&config, Some(&bearer)).await;
    }

    let coordinator = McpAuthCoordinator::new(state_root)?;
    let credential = coordinator.store().get(&config.provider_id()).await?;
    let bearer = match credential {
        Some(AuthCredential::ApiKey { key }) => Some(key),
        Some(AuthCredential::OAuth(credential)) if credential.is_expired(current_time_ms()) => {
            let metadata = coordinator
                .oauth_metadata()
                .get(server)
                .await?
                .ok_or_else(|| {
                    MimirError::Configuration(format!(
                        "MCP OAuth metadata is missing for {server}; run mcp login {server}"
                    ))
                })?;
            let oauth = McpOAuthClient::new(remote.io_timeout, remote.max_response_bytes)?;
            let bundle = oauth.refresh(remote, &metadata, &credential).await?;
            let access = bundle.credential.access.clone();
            coordinator.store_oauth_bundle(server, bundle).await?;
            Some(access)
        }
        Some(AuthCredential::OAuth(credential)) => Some(credential.access),
        None if config.oauth || config.bearer_token_env_var.is_some() => {
            return Err(MimirError::Configuration(format!(
                "MCP credentials are unavailable for {server}; run mcp login {server}"
            )));
        }
        None => None,
    };
    McpClient::connect_with_bearer(&config, bearer.as_deref()).await
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
