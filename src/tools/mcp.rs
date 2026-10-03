use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    error::MimirError,
    mcp::{
        McpClient, McpServerCatalog, McpToolCallOutput, McpToolDescriptor, connect_catalog_entry,
    },
    model::ToolDefinition,
};

use super::{
    ObservationStatus, Tool, ToolError, ToolInventoryItem, ToolInventorySource, ToolObservation,
    truncate_utf8,
};

const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_PARALLEL_DISCOVERIES: usize = 8;
const MAX_TOOLS_PER_SERVER: usize = 256;
const MAX_TOTAL_MCP_TOOLS: usize = 1_024;
const MAX_REMOTE_TOOL_NAME_BYTES: usize = 256;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 4 * 1_024;
const MAX_TOOL_SCHEMA_BYTES: usize = 128 * 1_024;

/// An MCP server skipped while the rest of the runtime remained available.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct McpUnavailableServer {
    pub server: String,
    pub reason: String,
}

/// Deterministic MCP discovery outcome used by CLI and test harnesses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegistrationReport {
    pub registered_tools: Vec<String>,
    pub unavailable: Vec<McpUnavailableServer>,
}

pub(super) struct McpDiscovery {
    pub(super) tools: Vec<RegisteredMcpTool>,
    pub(super) report: McpRegistrationReport,
}

struct DiscoveredMcpServer {
    connection: Arc<McpConnection>,
    label: String,
    descriptors: Vec<McpToolDescriptor>,
}

struct McpConnection {
    catalog: McpServerCatalog,
    state_root: PathBuf,
    server: String,
    connect_timeout: Duration,
    io_timeout: Duration,
    client: Mutex<Option<McpClient>>,
    catalog_identity: Value,
    catalog_entry: crate::mcp::McpCatalogServer,
    managed_fingerprint: Option<String>,
}

pub(super) struct RegisteredMcpTool {
    connection: Arc<McpConnection>,
    server: String,
    remote_name: String,
    definition: ToolDefinition,
}

impl RegisteredMcpTool {
    pub(super) fn server_name(&self) -> &str {
        &self.server
    }
}

pub(super) async fn discover(
    state_root: &Path,
    workspace: &Path,
) -> Result<McpDiscovery, ToolError> {
    let catalog = McpServerCatalog::new(state_root).map_err(|error| ToolError::Execution {
        tool: "mcp".into(),
        message: error.to_string(),
    })?;
    let entries = catalog.list().await.map_err(|error| ToolError::Execution {
        tool: "mcp".into(),
        message: error.to_string(),
    })?;
    let mut tools = Vec::new();
    let mut report = McpRegistrationReport::default();

    let project_state = crate::working_environment::native_mcp_state(state_root, workspace)
        .map_err(|error| ToolError::Execution {
            tool: "mcp".into(),
            message: error.to_string(),
        })?;
    let mut catalogs = vec![(catalog, state_root.to_owned(), entries)];
    if project_state.exists() {
        let managed =
            McpServerCatalog::new(&project_state).map_err(|error| ToolError::Execution {
                tool: "mcp".into(),
                message: error.to_string(),
            })?;
        let entries = managed.list().await.map_err(|error| ToolError::Execution {
            tool: "mcp".into(),
            message: error.to_string(),
        })?;
        catalogs.push((managed, project_state, entries));
    }
    let enabled = catalogs.into_iter().flat_map(|(catalog, state, entries)| {
        entries
            .into_iter()
            .filter(|entry| entry.enabled)
            .map(move |entry| (catalog.clone(), state.clone(), entry))
    });
    let discoveries = stream::iter(enabled)
        .map(|(catalog, state, entry)| async move {
            discover_server(&catalog, &state, workspace, entry).await
        })
        .buffered(MAX_PARALLEL_DISCOVERIES);
    futures::pin_mut!(discoveries);
    while let Some(result) = discoveries.next().await {
        match result {
            Ok(discovered) => register_descriptors(
                &mut tools,
                &mut report,
                &discovered.connection,
                &discovered.label,
                discovered.descriptors,
            ),
            Err(unavailable) => report.unavailable.push(unavailable),
        }
    }

    Ok(McpDiscovery { tools, report })
}

async fn discover_server(
    catalog: &McpServerCatalog,
    state_root: &Path,
    workspace: &Path,
    entry: crate::mcp::McpCatalogServer,
) -> Result<DiscoveredMcpServer, McpUnavailableServer> {
    let server = entry.server.clone();
    let unavailable = || McpUnavailableServer {
        server: server.clone(),
        reason: "MCP configuration revoked, changed or unavailable; restart the session".into(),
    };
    if server == "betterloop" && !bridge_workspace_matches(&entry, workspace) {
        return Err(unavailable());
    }
    let catalog_identity = serde_json::to_value(&entry).map_err(|_| unavailable())?;
    let managed_fingerprint = snapshot_managed_identity(state_root, &server, &catalog_identity)
        .map_err(|_| unavailable())?;
    let connect_timeout = connection_timeout(&entry);
    let io_timeout = entry.remote.as_ref().map_or_else(
        || Duration::from_millis(entry.stdio.io_timeout_ms),
        |remote| Duration::from_millis(remote.io_timeout_ms),
    );
    let mut client = match tokio::time::timeout(
        connect_timeout,
        connect_catalog_entry(&entry, state_root, managed_fingerprint.is_some()),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            return Err(McpUnavailableServer {
                server,
                reason: safe_error(&error),
            });
        }
        Err(_) => {
            return Err(McpUnavailableServer {
                server,
                reason: "connection timed out".into(),
            });
        }
    };
    let descriptors = match tokio::time::timeout(io_timeout, client.list_tools()).await {
        Ok(Ok(descriptors)) => descriptors,
        Ok(Err(error)) => {
            return Err(McpUnavailableServer {
                server,
                reason: safe_error(&error),
            });
        }
        Err(_) => {
            return Err(McpUnavailableServer {
                server,
                reason: "tool discovery timed out".into(),
            });
        }
    };
    if catalog
        .get(&server)
        .await
        .map_err(|_| unavailable())?
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| unavailable())?
        .as_ref()
        != Some(&catalog_identity)
        || snapshot_managed_identity(state_root, &server, &catalog_identity)
            .map_err(|_| unavailable())?
            != managed_fingerprint
    {
        return Err(unavailable());
    }
    let connection = Arc::new(McpConnection {
        catalog: catalog.clone(),
        state_root: state_root.to_owned(),
        server,
        connect_timeout,
        io_timeout,
        client: Mutex::new(Some(client)),
        catalog_identity,
        catalog_entry: entry.clone(),
        managed_fingerprint,
    });
    Ok(DiscoveredMcpServer {
        connection,
        label: entry.label,
        descriptors,
    })
}

fn bridge_workspace_matches(entry: &crate::mcp::McpCatalogServer, workspace: &Path) -> bool {
    entry.remote.is_none()
        && entry.stdio.args.len() == 4
        && entry.stdio.args[0] == "--workspace"
        && entry.stdio.args[2..] == ["enterprise", "bridge"]
        && std::fs::canonicalize(&entry.stdio.args[1]).ok().as_deref() == Some(workspace)
}

// An owned catalog entry never becomes unmanaged when its signed grant disappears.
fn snapshot_managed_identity(
    state: &Path,
    server: &str,
    catalog_identity: &Value,
) -> Result<Option<String>, MimirError> {
    let marker = state.join("enterprise-owned-mcp.json");
    let owned: Value = match std::fs::read(marker) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(error.into()),
    };
    if owned.get(server).is_none() && server != "betterloop" && !server.starts_with("bl_") {
        return Ok(None);
    }
    let receipts: Value = serde_json::from_slice(&std::fs::read(
        state.join("enterprise-owned-mcp-grants.json"),
    )?)?;
    let receipt = &receipts[server];
    let identity = crate::working_environment::managed_mcp_fingerprint(server)?;
    if identity.is_none()
        || receipt["catalog"] != *catalog_identity
        || receipt["fingerprint"].as_str() != identity.as_deref()
    {
        return Err(MimirError::Configuration(
            "Owned MCP catalog is not bound to its current signed grant".into(),
        ));
    }
    Ok(identity)
}

impl McpConnection {
    async fn verify_identity(&self) -> Result<(), MimirError> {
        let current = self
            .catalog
            .get(&self.server)
            .await?
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        if current.as_ref() != Some(&self.catalog_identity)
            || snapshot_managed_identity(&self.state_root, &self.server, &self.catalog_identity)?
                != self.managed_fingerprint
        {
            return Err(MimirError::Configuration(
                "MCP configuration revoked or replaced; restart the session".into(),
            ));
        }
        Ok(())
    }
}

fn register_descriptors(
    tools: &mut Vec<RegisteredMcpTool>,
    report: &mut McpRegistrationReport,
    connection: &Arc<McpConnection>,
    label: &str,
    descriptors: Vec<McpToolDescriptor>,
) {
    if descriptors.len() > MAX_TOOLS_PER_SERVER {
        report.unavailable.push(McpUnavailableServer {
            server: connection.server.clone(),
            reason: format!(
                "tool catalog exceeded {MAX_TOOLS_PER_SERVER} entries; additional tools were skipped"
            ),
        });
    }
    for descriptor in descriptors.into_iter().take(MAX_TOOLS_PER_SERVER) {
        if tools.len() >= MAX_TOTAL_MCP_TOOLS {
            report.unavailable.push(McpUnavailableServer {
                server: connection.server.clone(),
                reason: format!(
                    "runtime MCP tool limit of {MAX_TOTAL_MCP_TOOLS} was reached; additional tools were skipped"
                ),
            });
            break;
        }
        if descriptor.name.trim().is_empty()
            || descriptor.name.len() > MAX_REMOTE_TOOL_NAME_BYTES
            || descriptor.name.chars().any(char::is_control)
        {
            report.unavailable.push(McpUnavailableServer {
                server: connection.server.clone(),
                reason: "a tool supplied an invalid or oversized name".into(),
            });
            continue;
        }
        if !descriptor.input_schema.is_object() {
            report.unavailable.push(McpUnavailableServer {
                server: connection.server.clone(),
                reason: format!(
                    "tool '{}' supplied a non-object input schema",
                    descriptor.name
                ),
            });
            continue;
        }
        if serde_json::to_vec(&descriptor.input_schema)
            .map_or(true, |schema| schema.len() > MAX_TOOL_SCHEMA_BYTES)
        {
            report.unavailable.push(McpUnavailableServer {
                server: connection.server.clone(),
                reason: format!(
                    "tool '{}' supplied an oversized input schema",
                    descriptor.name
                ),
            });
            continue;
        }
        let name = stable_tool_name(&connection.server, &descriptor.name);
        let description = if descriptor.description.trim().is_empty() {
            format!("{label} MCP tool '{}'", descriptor.name)
        } else {
            format!("{} ({label} MCP)", descriptor.description)
        };
        let (description, _) = truncate_utf8(&description, MAX_TOOL_DESCRIPTION_BYTES);
        tools.push(RegisteredMcpTool {
            server: connection.server.clone(),
            remote_name: descriptor.name,
            definition: ToolDefinition {
                name,
                description,
                parameters: descriptor.input_schema,
            },
            connection: Arc::clone(connection),
        });
    }
}

#[async_trait]
impl Tool for RegisteredMcpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn inventory(&self) -> ToolInventoryItem {
        ToolInventoryItem {
            id: self.definition.name.clone(),
            label: self.remote_name.clone(),
            source: ToolInventorySource {
                kind: "mcp".into(),
                id: self.server.clone(),
                version: None,
            },
            capabilities: vec!["external_service".into()],
            risk: "external".into(),
            availability: "available".into(),
            availability_reason_code: None,
            required_by_runtime: false,
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        self.connection
            .verify_identity()
            .await
            .map_err(|_| ToolError::Execution {
                tool: self.definition.name.clone(),
                message: "MCP configuration revoked or replaced; restart the session".into(),
            })?;
        if !input.is_object() {
            return Err(ToolError::InvalidArguments {
                tool: self.definition.name.clone(),
                message: "MCP tool arguments must be a JSON object".into(),
            });
        }

        let mut slot = self.connection.client.lock().await;
        let mut client = match slot.take() {
            Some(client) => client,
            None => tokio::time::timeout(
                self.connection.connect_timeout,
                connect_catalog_entry(
                    &self.connection.catalog_entry,
                    &self.connection.state_root,
                    self.connection.managed_fingerprint.is_some(),
                ),
            )
            .await
            .map_err(|_| ToolError::Execution {
                tool: self.definition.name.clone(),
                message: "MCP reconnect timed out".into(),
            })?
            .map_err(|error| ToolError::Execution {
                tool: self.definition.name.clone(),
                message: safe_error(&error),
            })?,
        };

        // Recheck after the slot lock or reconnect await; neither may rebind an old tool.
        self.connection
            .verify_identity()
            .await
            .map_err(|_| ToolError::Execution {
                tool: self.definition.name.clone(),
                message: "MCP configuration revoked or replaced; restart the session".into(),
            })?;
        let result = tokio::time::timeout(
            self.connection.io_timeout,
            client.call_tool(&self.remote_name, input),
        )
        .await;
        match result {
            Ok(Ok(output)) => {
                *slot = Some(client);
                Ok(observation(&self.server, &self.remote_name, output))
            }
            Ok(Err(error)) => Err(ToolError::Execution {
                tool: self.definition.name.clone(),
                message: safe_error(&error),
            }),
            Err(_) => Err(ToolError::Execution {
                tool: self.definition.name.clone(),
                message: "MCP tool call timed out; the connection was discarded".into(),
            }),
        }
    }
}

fn observation(server: &str, tool: &str, output: McpToolCallOutput) -> ToolObservation {
    let content = match output {
        McpToolCallOutput::Structured(value) => value.to_string(),
        McpToolCallOutput::Text(text) => text,
        McpToolCallOutput::Blocks(blocks) => Value::Array(blocks).to_string(),
    };
    ToolObservation {
        status: ObservationStatus::Success,
        summary: format!("MCP server {server} completed {tool}"),
        next_actions: Vec::new(),
        artifacts: Vec::new(),
        content,
    }
}

fn connection_timeout(entry: &crate::mcp::McpCatalogServer) -> Duration {
    entry.remote.as_ref().map_or_else(
        || Duration::from_millis(entry.stdio.startup_timeout_ms),
        |remote| Duration::from_millis(remote.io_timeout_ms),
    )
}

pub(crate) fn stable_tool_name(server: &str, remote_name: &str) -> String {
    let original_server = server;
    let mut server = sanitize_identifier(server);
    server.truncate(20);
    let tool = sanitize_identifier(remote_name);
    let hash = stable_hash(&format!("{original_server}\0{remote_name}"));
    let suffix = format!("_{hash:08x}");
    let prefix = format!("mcp_{server}_");
    let available = MAX_TOOL_NAME_BYTES.saturating_sub(prefix.len() + suffix.len());
    let mut tool = tool;
    tool.truncate(available);
    format!("{prefix}{tool}{suffix}")
}

fn sanitize_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            output.push(character.to_ascii_lowercase());
        } else if !output.ends_with('_') {
            output.push('_');
        }
    }
    let trimmed = output.trim_matches('_');
    if trimmed.is_empty() {
        "tool".into()
    } else {
        trimmed.into()
    }
}

fn stable_hash(value: &str) -> u32 {
    value.as_bytes().iter().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

fn safe_error(error: &MimirError) -> String {
    let message = match error {
        MimirError::Configuration(message) | MimirError::Tool(message) => message,
        MimirError::Protocol(message) if message.contains("requires authorization") => {
            "authorization is required; run `mimir mcp login <server>`"
        }
        MimirError::Protocol(message) if message.contains("rejected the requested scopes") => {
            "the MCP server rejected the configured OAuth scopes"
        }
        MimirError::Protocol(message) if message.contains("session expired") => {
            "the MCP remote session expired; retry to reconnect"
        }
        MimirError::Protocol(_) => "MCP protocol or transport operation failed",
        MimirError::Io(_) => "MCP transport is unavailable",
        MimirError::Json(_) => "MCP server returned invalid JSON",
        MimirError::Provider(_) | MimirError::Session { .. } | MimirError::BudgetPaused(_) => {
            "MCP operation failed"
        }
    };
    bounded_public_text(message)
}

fn bounded_public_text(message: &str) -> String {
    let mut output = String::with_capacity(message.len().min(512));
    for character in message.chars().take(512) {
        if character.is_control() && character != '\n' && character != '\t' {
            continue;
        }
        let _ = output.write_char(character);
    }
    if output.trim().is_empty() {
        "MCP operation failed".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_bridge_discovery_requires_the_runtime_workspace() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let entry = crate::mcp::McpCatalogServer::new(
            "betterloop",
            "Bridge",
            crate::mcp::McpCatalogStdio {
                program: std::env::current_exe().unwrap(),
                args: vec![
                    "--workspace".into(),
                    b.path().to_string_lossy().into_owned(),
                    "enterprise".into(),
                    "bridge".into(),
                ],
                ..crate::mcp::McpCatalogStdio::default()
            },
        )
        .unwrap();
        assert!(!bridge_workspace_matches(
            &entry,
            &std::fs::canonicalize(a.path()).unwrap()
        ));
        assert!(bridge_workspace_matches(
            &entry,
            &std::fs::canonicalize(b.path()).unwrap()
        ));
    }

    #[test]
    fn names_are_stable_bounded_and_collision_safe() {
        let first = stable_tool_name("my-server", "notion-search");
        assert_eq!(first, stable_tool_name("my-server", "notion-search"));
        assert_ne!(first, stable_tool_name("my_server", "notion-search"));
        assert_ne!(first, stable_tool_name("my-server", "notion_search"));
        assert!(first.len() <= MAX_TOOL_NAME_BYTES);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        assert!(
            stable_tool_name(&"server".repeat(20), &"tool".repeat(100)).len()
                <= MAX_TOOL_NAME_BYTES
        );
    }
}
