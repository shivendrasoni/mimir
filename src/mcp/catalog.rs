#![allow(
    clippy::missing_errors_doc,
    reason = "catalog and coordinator APIs mirror existing store patterns with typed errors"
)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    atomic,
    auth::{AuthStore, OAuthCredential},
    error::{MimirError, Result},
    mcp::{
        auth::{McpAuthStatus, auth_status_from_parts},
        config::{
            McpHttpConfig, McpServerConfig, McpStdioConfig, validate_env_key, validate_headers,
            validate_process_value,
        },
        oauth::{McpOAuthClientMetadataStore, McpOAuthCredentialBundle},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogServer {
    pub server: String,
    pub label: String,
    #[serde(default)]
    pub stdio: McpCatalogStdio,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<McpCatalogHttp>,
    #[serde(default)]
    pub oauth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header_env: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogHttp {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    pub io_timeout_ms: u64,
    pub max_response_bytes: usize,
    pub max_tool_payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogStdio {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub startup_timeout_ms: u64,
    pub io_timeout_ms: u64,
    pub max_frame_bytes: usize,
    pub max_tool_payload_bytes: usize,
    pub max_stderr_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerCatalogStatus {
    pub server: String,
    pub label: String,
    pub enabled: bool,
    pub oauth: bool,
    pub bearer_token_env_var: Option<String>,
    pub auth: McpAuthStatus,
}

#[derive(Clone)]
pub struct McpAuthCoordinator {
    catalog: McpServerCatalog,
    store: AuthStore,
    oauth_metadata: McpOAuthClientMetadataStore,
}

#[derive(Debug, Clone)]
pub struct McpServerCatalog {
    root: PathBuf,
    path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogState {
    schema_version: u16,
    #[serde(default)]
    servers: BTreeMap<String, McpCatalogServer>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            servers: BTreeMap::new(),
        }
    }
}

impl McpCatalogServer {
    pub fn new(
        server: impl Into<String>,
        label: impl Into<String>,
        stdio: McpCatalogStdio,
    ) -> Result<Self> {
        let entry = Self {
            server: server.into(),
            label: label.into(),
            stdio,
            remote: None,
            oauth: false,
            bearer_token_env_var: None,
            header_env: BTreeMap::new(),
            enabled: true,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn remote(
        server: impl Into<String>,
        label: impl Into<String>,
        remote: McpCatalogHttp,
    ) -> Result<Self> {
        let entry = Self {
            server: server.into(),
            label: label.into(),
            stdio: McpCatalogStdio::default(),
            remote: Some(remote),
            oauth: false,
            bearer_token_env_var: None,
            header_env: BTreeMap::new(),
            enabled: true,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn provider_id(&self) -> String {
        format!("mcp:{}", self.server)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_base_config()?;
        self.validate_references()?;
        Ok(())
    }

    pub fn to_runtime_config(&self) -> Result<McpServerConfig> {
        self.to_runtime_config_with_lookup(|key| std::env::var(key).ok())
    }

    pub fn to_runtime_config_with_lookup<F>(&self, env_lookup: F) -> Result<McpServerConfig>
    where
        F: Fn(&str) -> Option<String>,
    {
        self.validate_references()?;
        let mut headers = BTreeMap::new();
        for (header, source) in &self.header_env {
            let value =
                resolve_required_value(&self.server, "header", header, source, &env_lookup)?;
            headers.insert(header.clone(), value);
        }
        validate_headers(&headers)?;
        let mut config = if let Some(remote) = &self.remote {
            McpServerConfig::remote(&self.server, &self.label, remote.to_runtime()?)?
        } else {
            let mut env = BTreeMap::new();
            for (target, source) in &self.stdio.env {
                let value =
                    resolve_required_value(&self.server, "stdio env", target, source, &env_lookup)?;
                env.insert(target.clone(), value);
            }
            let stdio = McpStdioConfig {
                program: self.stdio.program.clone(),
                args: self.stdio.args.clone(),
                env,
                startup_timeout: duration_from_millis(
                    "MCP startup timeout",
                    self.stdio.startup_timeout_ms,
                )?,
                io_timeout: duration_from_millis("MCP I/O timeout", self.stdio.io_timeout_ms)?,
                max_frame_bytes: self.stdio.max_frame_bytes,
                max_tool_payload_bytes: self.stdio.max_tool_payload_bytes,
                max_stderr_bytes: self.stdio.max_stderr_bytes,
            };
            McpServerConfig::new(&self.server, &self.label, stdio)?
        };
        config.oauth = self.oauth;
        config
            .bearer_token_env_var
            .clone_from(&self.bearer_token_env_var);
        config.headers = headers;
        config.enabled = self.enabled;
        config.validate()?;
        Ok(config)
    }

    fn validate_references(&self) -> Result<()> {
        if self.remote.is_none() {
            for (target, source) in &self.stdio.env {
                validate_env_key("MCP stdio env key", target)?;
                validate_env_key("MCP stdio env source", source)?;
            }
        }
        for (header, source) in &self.header_env {
            validate_process_value("MCP header name", header)?;
            validate_env_key("MCP header env source", source)?;
        }
        Ok(())
    }

    fn validate_base_config(&self) -> Result<()> {
        if let Some(remote) = &self.remote {
            let mut config =
                McpServerConfig::remote(&self.server, &self.label, remote.to_runtime()?)?;
            config.oauth = self.oauth;
            config
                .bearer_token_env_var
                .clone_from(&self.bearer_token_env_var);
            config.enabled = self.enabled;
            return config.validate();
        }
        let stdio = McpStdioConfig {
            program: self.stdio.program.clone(),
            args: self.stdio.args.clone(),
            env: BTreeMap::new(),
            startup_timeout: duration_from_millis(
                "MCP startup timeout",
                self.stdio.startup_timeout_ms,
            )?,
            io_timeout: duration_from_millis("MCP I/O timeout", self.stdio.io_timeout_ms)?,
            max_frame_bytes: self.stdio.max_frame_bytes,
            max_tool_payload_bytes: self.stdio.max_tool_payload_bytes,
            max_stderr_bytes: self.stdio.max_stderr_bytes,
        };
        let mut config = McpServerConfig::new(&self.server, &self.label, stdio)?;
        config.oauth = self.oauth;
        config
            .bearer_token_env_var
            .clone_from(&self.bearer_token_env_var);
        config.headers = BTreeMap::new();
        config.enabled = self.enabled;
        config.validate()
    }
}

impl McpCatalogHttp {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let runtime = McpHttpConfig::new(url)?;
        Ok(Self::from_runtime(&runtime))
    }

    pub fn from_runtime(remote: &McpHttpConfig) -> Self {
        Self {
            url: remote.url.clone(),
            client_id: remote.client_id.clone(),
            scopes: remote.scopes.clone(),
            io_timeout_ms: u64::try_from(remote.io_timeout.as_millis()).unwrap_or(u64::MAX),
            max_response_bytes: remote.max_response_bytes,
            max_tool_payload_bytes: remote.max_tool_payload_bytes,
        }
    }

    fn to_runtime(&self) -> Result<McpHttpConfig> {
        let mut remote = McpHttpConfig::new(&self.url)?;
        remote.client_id.clone_from(&self.client_id);
        remote.scopes.clone_from(&self.scopes);
        remote.io_timeout = duration_from_millis("MCP remote I/O timeout", self.io_timeout_ms)?;
        remote.max_response_bytes = self.max_response_bytes;
        remote.max_tool_payload_bytes = self.max_tool_payload_bytes;
        remote.validate()?;
        Ok(remote)
    }
}

impl McpCatalogStdio {
    pub fn from_runtime(stdio: &McpStdioConfig) -> Self {
        Self {
            program: stdio.program.clone(),
            args: stdio.args.clone(),
            env: BTreeMap::new(),
            startup_timeout_ms: u64::try_from(stdio.startup_timeout.as_millis())
                .unwrap_or(u64::MAX),
            io_timeout_ms: u64::try_from(stdio.io_timeout.as_millis()).unwrap_or(u64::MAX),
            max_frame_bytes: stdio.max_frame_bytes,
            max_tool_payload_bytes: stdio.max_tool_payload_bytes,
            max_stderr_bytes: stdio.max_stderr_bytes,
        }
    }
}

impl Default for McpCatalogStdio {
    fn default() -> Self {
        let stdio = McpStdioConfig::default();
        Self::from_runtime(&stdio)
    }
}

impl McpServerCatalog {
    pub fn new(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root)?;
        let root = atomic::canonical_state_root(state_root);
        let path = root.join("mcp/catalog.json");
        Ok(Self {
            lock: atomic::path_lock(&path),
            root,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn list(&self) -> Result<Vec<McpCatalogServer>> {
        Ok(self.read_state().await?.servers.into_values().collect())
    }

    pub async fn get(&self, server: &str) -> Result<Option<McpCatalogServer>> {
        validate_server_name(server)?;
        Ok(self.read_state().await?.servers.remove(server))
    }

    pub async fn upsert(&self, entry: McpCatalogServer) -> Result<()> {
        entry.validate()?;
        let mut state = self.read_state().await?;
        state.servers.insert(entry.server.clone(), entry);
        self.write_state(&state).await
    }

    pub async fn enable(&self, server: &str) -> Result<bool> {
        self.set_enabled(server, true).await
    }

    pub async fn disable(&self, server: &str) -> Result<bool> {
        self.set_enabled(server, false).await
    }

    pub async fn remove(&self, server: &str) -> Result<bool> {
        validate_server_name(server)?;
        let mut state = self.read_state().await?;
        let removed = state.servers.remove(server).is_some();
        if removed {
            self.write_state(&state).await?;
        }
        Ok(removed)
    }

    pub async fn resolve(&self, server: &str) -> Result<Option<McpServerConfig>> {
        self.resolve_with_lookup(server, |key| std::env::var(key).ok())
            .await
    }

    pub async fn resolve_with_lookup<F>(
        &self,
        server: &str,
        env_lookup: F,
    ) -> Result<Option<McpServerConfig>>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(entry) = self.get(server).await? else {
            return Ok(None);
        };
        Ok(Some(entry.to_runtime_config_with_lookup(env_lookup)?))
    }

    async fn set_enabled(&self, server: &str, enabled: bool) -> Result<bool> {
        validate_server_name(server)?;
        let mut state = self.read_state().await?;
        let Some(entry) = state.servers.get_mut(server) else {
            return Ok(false);
        };
        if entry.enabled == enabled {
            return Ok(false);
        }
        entry.enabled = enabled;
        self.write_state(&state).await?;
        Ok(true)
    }

    async fn read_state(&self) -> Result<CatalogState> {
        let _guard = self.lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        let state: CatalogState = atomic::read_json(&self.path).await?.unwrap_or_default();
        if state.schema_version != 1 {
            return Err(MimirError::Configuration(format!(
                "unsupported MCP catalog schema version {}",
                state.schema_version
            )));
        }
        Ok(state)
    }

    async fn write_state(&self, state: &CatalogState) -> Result<()> {
        let _guard = self.lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        atomic::write_json(&self.path, state).await
    }
}

impl McpAuthCoordinator {
    pub fn new(state_root: &Path) -> Result<Self> {
        Self::with_auth_store(state_root, AuthStore::new(state_root)?)
    }

    /// Creates a coordinator whose credentials come from the user-global auth file.
    pub fn global(state_root: &Path) -> Result<Self> {
        Self::with_auth_store(state_root, AuthStore::global()?)
    }

    /// Creates a coordinator with an explicit credential store for isolated embedding and tests.
    pub fn with_auth_store(state_root: &Path, store: AuthStore) -> Result<Self> {
        Ok(Self {
            catalog: McpServerCatalog::new(state_root)?,
            store,
            oauth_metadata: McpOAuthClientMetadataStore::new(state_root)?,
        })
    }

    pub fn catalog(&self) -> &McpServerCatalog {
        &self.catalog
    }

    pub fn store(&self) -> &AuthStore {
        &self.store
    }

    pub fn oauth_metadata(&self) -> &McpOAuthClientMetadataStore {
        &self.oauth_metadata
    }

    pub async fn list_statuses(&self) -> Result<Vec<McpServerCatalogStatus>> {
        let entries = self.catalog.list().await?;
        let mut statuses = Vec::with_capacity(entries.len());
        for entry in entries {
            statuses.push(self.status_from_entry(entry).await?);
        }
        Ok(statuses)
    }

    pub async fn status(&self, server: &str) -> Result<Option<McpServerCatalogStatus>> {
        let Some(entry) = self.catalog.get(server).await? else {
            return Ok(None);
        };
        Ok(Some(self.status_from_entry(entry).await?))
    }

    pub async fn store_api_key(&self, server: &str, key: &str) -> Result<()> {
        let entry = self.require_server(server).await?;
        if entry.oauth {
            return Err(MimirError::Configuration(format!(
                "MCP server {server} requires OAuth"
            )));
        }
        self.store.set_api_key(&entry.provider_id(), key).await
    }

    pub async fn store_oauth(&self, server: &str, credential: OAuthCredential) -> Result<()> {
        let entry = self.require_server(server).await?;
        if !entry.oauth {
            return Err(MimirError::Configuration(format!(
                "MCP server {server} does not use OAuth"
            )));
        }
        self.store.set_oauth(&entry.provider_id(), credential).await
    }

    pub async fn store_oauth_bundle(
        &self,
        server: &str,
        bundle: McpOAuthCredentialBundle,
    ) -> Result<()> {
        let entry = self.require_server(server).await?;
        if !entry.oauth {
            return Err(MimirError::Configuration(format!(
                "MCP server {server} does not use OAuth"
            )));
        }
        self.oauth_metadata.set(server, bundle.metadata).await?;
        self.store
            .set_oauth(&entry.provider_id(), bundle.credential)
            .await
    }

    pub async fn logout(&self, server: &str) -> Result<bool> {
        let entry = self.require_server(server).await?;
        let credential_removed = self.store.logout(&entry.provider_id()).await?;
        let metadata_removed = self.oauth_metadata.remove(server).await?;
        Ok(credential_removed || metadata_removed)
    }

    async fn require_server(&self, server: &str) -> Result<McpCatalogServer> {
        self.catalog
            .get(server)
            .await?
            .ok_or_else(|| MimirError::Configuration(format!("unknown MCP server: {server}")))
    }

    async fn status_from_entry(&self, entry: McpCatalogServer) -> Result<McpServerCatalogStatus> {
        let auth = auth_status_from_parts(
            &entry.server,
            entry.enabled,
            entry.oauth,
            entry.bearer_token_env_var.as_deref(),
            &self.store,
            |env_var| std::env::var(env_var).ok(),
        )
        .await?;
        Ok(McpServerCatalogStatus {
            server: entry.server,
            label: entry.label,
            enabled: entry.enabled,
            oauth: entry.oauth,
            bearer_token_env_var: entry.bearer_token_env_var,
            auth,
        })
    }
}

fn default_enabled() -> bool {
    true
}

fn validate_server_name(server: &str) -> Result<()> {
    let stdio = McpStdioConfig {
        program: PathBuf::from("/bin/sh"),
        ..McpStdioConfig::default()
    };
    let mut config = McpServerConfig::new(server, "placeholder", stdio)?;
    config.enabled = false;
    Ok(())
}

fn duration_from_millis(label: &str, value: u64) -> Result<Duration> {
    if value == 0 {
        return Err(MimirError::Configuration(format!(
            "{label} must be greater than zero"
        )));
    }
    Ok(Duration::from_millis(value))
}

fn resolve_required_value<F>(
    server: &str,
    kind: &str,
    target: &str,
    source: &str,
    env_lookup: &F,
) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    let value = env_lookup(source).ok_or_else(|| {
        MimirError::Configuration(format!(
            "MCP server {server} {kind} {target} references missing env var {source}"
        ))
    })?;
    if value.trim().is_empty() {
        return Err(MimirError::Configuration(format!(
            "MCP server {server} {kind} {target} references blank env var {source}"
        )));
    }
    Ok(value)
}

/// Returns the built-in remote MCP integrations without persisting them.
pub fn builtin_mcp_catalog() -> Vec<McpCatalogServer> {
    [
        ("linear", "Linear", "https://mcp.linear.app/mcp"),
        ("notion", "Notion", "https://mcp.notion.com/mcp"),
    ]
    .into_iter()
    .map(|(server, label, url)| McpCatalogServer {
        server: server.into(),
        label: label.into(),
        stdio: McpCatalogStdio::default(),
        remote: Some(McpCatalogHttp {
            url: url.into(),
            client_id: None,
            scopes: Vec::new(),
            io_timeout_ms: 20_000,
            max_response_bytes: crate::mcp::protocol::MAX_FRAME_BYTES,
            max_tool_payload_bytes: 128 * 1024,
        }),
        oauth: true,
        bearer_token_env_var: None,
        header_env: BTreeMap::new(),
        enabled: true,
    })
    .collect()
}
