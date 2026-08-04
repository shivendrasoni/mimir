use std::{collections::BTreeMap, path::Path, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    extensions::{
        Capability, CatalogEntry, ExtensionCallStatus, ExtensionManager, HostLimits, HostRequest,
        HostResponseStatus, JsonLineExtensionHost, ToolDescriptor,
    },
    model::ToolDefinition,
};

pub(super) struct RegisteredExtensionTool {
    manager: Arc<ExtensionManager>,
    descriptor: ToolDescriptor,
}

impl RegisteredExtensionTool {
    pub(super) fn new(manager: Arc<ExtensionManager>, descriptor: ToolDescriptor) -> Self {
        Self {
            manager,
            descriptor,
        }
    }
}

#[async_trait]
impl Tool for RegisteredExtensionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.descriptor.name.clone(),
            description: self.descriptor.description.clone(),
            parameters: self.descriptor.parameters.clone(),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let result = self
            .manager
            .invoke_tool(&self.descriptor.name, &Uuid::new_v4().to_string(), input)
            .await
            .map_err(|error| ToolError::Execution {
                tool: self.descriptor.name.clone(),
                message: error.to_string(),
            })?;
        let status = match result.status {
            ExtensionCallStatus::Ok => ObservationStatus::Success,
            ExtensionCallStatus::Warning => ObservationStatus::Warning,
            ExtensionCallStatus::Error => ObservationStatus::Error,
        };
        Ok(ToolObservation {
            status,
            summary: result.summary,
            next_actions: result.next_actions,
            artifacts: Vec::new(),
            content: serde_json::to_string(&result.content).map_err(|error| {
                ToolError::Execution {
                    tool: self.descriptor.name.clone(),
                    message: error.to_string(),
                }
            })?,
        })
    }
}

use super::{ObservationStatus, Tool, ToolError, ToolObservation, object_schema, parse_input};

pub struct ExtensionInvokeTool {
    hosts: BTreeMap<String, JsonLineExtensionHost>,
}

impl ExtensionInvokeTool {
    pub fn from_catalog(entries: Vec<CatalogEntry>, workspace: &Path) -> Result<Self, ToolError> {
        let mut hosts = BTreeMap::new();
        for entry in entries.into_iter().filter(|entry| {
            entry.enabled && entry.manifest.capabilities.contains(&Capability::Tools)
        }) {
            let name = entry.manifest.name.clone();
            let host = JsonLineExtensionHost::new(entry.manifest, workspace, HostLimits::default())
                .map_err(|error| ToolError::Execution {
                    tool: "extension_invoke".into(),
                    message: error.to_string(),
                })?;
            hosts.insert(name, host);
        }
        Ok(Self { hosts })
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionInput {
    extension: String,
    command: String,
    #[serde(default)]
    payload: Value,
}

#[async_trait]
impl Tool for ExtensionInvokeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "extension_invoke".into(),
            description: "Invoke an enabled capability-scoped extension tool".into(),
            parameters: object_schema(
                &json!({
                    "extension": {"type": "string"},
                    "command": {"type": "string"},
                    "payload": {}
                }),
                &["extension", "command"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: ExtensionInput = parse_input("extension_invoke", input)?;
        let host = self
            .hosts
            .get(&input.extension)
            .ok_or_else(|| ToolError::Disabled {
                tool: format!("extension_invoke:{}", input.extension),
            })?;
        let response = host
            .invoke(HostRequest {
                schema_version: 1,
                id: Uuid::new_v4().to_string(),
                command: input.command,
                payload: input.payload,
            })
            .await
            .map_err(|error| ToolError::Execution {
                tool: format!("extension_invoke:{}", input.extension),
                message: error.to_string(),
            })?;
        let status = match response.status {
            HostResponseStatus::Ok => ObservationStatus::Success,
            HostResponseStatus::Error => ObservationStatus::Error,
        };
        Ok(ToolObservation {
            status,
            summary: response
                .message
                .unwrap_or_else(|| format!("extension {} completed", input.extension)),
            next_actions: if status == ObservationStatus::Error {
                vec!["Inspect the extension response and retry with corrected input".into()]
            } else {
                Vec::new()
            },
            artifacts: Vec::new(),
            content: serde_json::to_string(&response.output).map_err(|error| {
                ToolError::Execution {
                    tool: "extension_invoke".into(),
                    message: error.to_string(),
                }
            })?,
        })
    }
}
