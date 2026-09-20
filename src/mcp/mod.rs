mod auth;
mod client;
mod config;
mod connector;
mod oauth;
mod protocol;
mod tool_names;

mod catalog;

pub use auth::{McpAuthSource, McpAuthStatus, auth_status, auth_status_with_lookup};
pub use catalog::{
    McpAuthCoordinator, McpCatalogHttp, McpCatalogServer, McpCatalogStdio, McpServerCatalog,
    McpServerCatalogStatus, builtin_mcp_catalog,
};
pub use client::{McpClient, McpServerInfo, McpToolCallOutput, McpToolDescriptor};
pub use config::{McpHttpConfig, McpServerConfig, McpStdioConfig};
pub use connector::{connect_catalog_client, connect_catalog_client_with_auth_store};
pub use oauth::{
    McpAuthorizationChallenge, McpOAuthAuthorization, McpOAuthClient, McpOAuthClientMetadata,
    McpOAuthClientMetadataStore, McpOAuthCodeReceiver, McpOAuthCredentialBundle,
};
pub use tool_names::tool_identifier;
