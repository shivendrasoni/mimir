use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    extensions::{RlmHostOperations, RlmRuntime},
    model::ToolDefinition,
};

use super::{
    ObservationStatus, Tool, ToolError, ToolObservation, ToolRegistry, object_schema, parse_input,
};

const SPAWN_AGENT: &str = "spawn_agent";
const FIND_MODELS: &str = "rlm_find_models";
const LIST_SUBAGENTS: &str = "rlm_list_subagents";
const DELETE_SUBAGENT: &str = "rlm_delete_subagent";
const CANCEL_SUBAGENT: &str = "rlm_cancel_subagent";

pub(super) fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        SPAWN_AGENT | FIND_MODELS | LIST_SUBAGENTS | DELETE_SUBAGENT | CANCEL_SUBAGENT
    )
}

pub(super) fn register(
    registry: &mut ToolRegistry,
    runtime: Arc<RlmRuntime>,
) -> Result<(), ToolError> {
    let operations = Arc::new(RlmHostOperations::new(runtime, None));
    let tools = [
        RlmTool::new(Operation::SpawnAgent, Arc::clone(&operations)),
        RlmTool::new(Operation::FindModels, Arc::clone(&operations)),
        RlmTool::new(Operation::ListSubagents, Arc::clone(&operations)),
        RlmTool::new(Operation::DeleteSubagent, Arc::clone(&operations)),
        RlmTool::new(Operation::CancelSubagent, operations),
    ];
    let conflicts = tools
        .iter()
        .map(|tool| tool.operation.name())
        .filter(|name| registry.tools.contains_key(*name))
        .collect::<BTreeSet<_>>();
    if !conflicts.is_empty() {
        return Err(ToolError::Execution {
            tool: "rlm".into(),
            message: format!(
                "reserved RLM tool names conflict with existing tools: {}",
                conflicts.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    for tool in tools {
        registry.register(tool);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    SpawnAgent,
    FindModels,
    ListSubagents,
    DeleteSubagent,
    CancelSubagent,
}

impl Operation {
    const fn name(self) -> &'static str {
        match self {
            Self::SpawnAgent => SPAWN_AGENT,
            Self::FindModels => FIND_MODELS,
            Self::ListSubagents => LIST_SUBAGENTS,
            Self::DeleteSubagent => DELETE_SUBAGENT,
            Self::CancelSubagent => CANCEL_SUBAGENT,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::SpawnAgent => {
                "Start a bounded recursive child agent. Admission returns immediately; inspect progress with rlm_list_subagents."
            }
            Self::FindModels => "Search models whose providers are currently authenticated.",
            Self::ListSubagents => "List recursive child agents created by this parent session.",
            Self::DeleteSubagent => {
                "Cancel if necessary, then remove a recursive child agent from this parent session."
            }
            Self::CancelSubagent => "Cancel an active recursive child agent.",
        }
    }

    const fn host_operation(self) -> &'static str {
        match self {
            Self::SpawnAgent => "agent.spawn",
            Self::FindModels => "rlm.find_models",
            Self::ListSubagents => "rlm.list_subagents",
            Self::DeleteSubagent => "rlm.delete_subagent",
            Self::CancelSubagent => "rlm.cancel_subagent",
        }
    }

    fn parameters(self) -> Value {
        match self {
            Self::SpawnAgent => object_schema(
                &json!({
                    "prompt": {"type": "string", "minLength": 1},
                    "kwargs": {
                        "type": "object",
                        "properties": {
                            "model": {"type": "string", "minLength": 1},
                            "name": {"type": "string", "minLength": 1}
                        },
                        "additionalProperties": false
                    },
                    "cellSourceCode": {"type": "string"}
                }),
                &["prompt"],
            ),
            Self::FindModels => object_schema(
                &json!({
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                }),
                &["query"],
            ),
            Self::ListSubagents => object_schema(&json!({}), &[]),
            Self::DeleteSubagent | Self::CancelSubagent => object_schema(
                &json!({"target": {"type": "string", "minLength": 1}}),
                &["target"],
            ),
        }
    }
}

struct RlmTool {
    operation: Operation,
    operations: Arc<RlmHostOperations>,
}

impl RlmTool {
    fn new(operation: Operation, operations: Arc<RlmHostOperations>) -> Self {
        Self {
            operation,
            operations,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentInput {
    prompt: String,
    #[serde(default)]
    kwargs: SpawnAgentKwargs,
    #[serde(default, rename = "cellSourceCode")]
    cell_source_code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentKwargs {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindModelsInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetInput {
    target: String,
}

#[async_trait]
impl Tool for RlmTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.operation.name().into(),
            description: self.operation.description().into(),
            parameters: self.operation.parameters(),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let payload = normalize_input(self.operation, input)?;
        let output = self
            .operations
            .handle(self.operation.host_operation(), payload)
            .await
            .map_err(|error| ToolError::Execution {
                tool: self.operation.name().into(),
                message: error.to_string(),
            })?;
        let content = serde_json::to_string(&output).map_err(|error| ToolError::Execution {
            tool: self.operation.name().into(),
            message: error.to_string(),
        })?;
        Ok(ToolObservation {
            status: ObservationStatus::Success,
            summary: success_summary(self.operation, &output),
            next_actions: Vec::new(),
            artifacts: Vec::new(),
            content,
        })
    }
}

fn normalize_input(operation: Operation, input: Value) -> Result<Value, ToolError> {
    match operation {
        Operation::SpawnAgent => {
            let input: SpawnAgentInput = parse_input(SPAWN_AGENT, input)?;
            Ok(json!({
                "prompt": input.prompt,
                "kwargs": {
                    "model": input.kwargs.model,
                    "name": input.kwargs.name,
                },
                "cellSourceCode": input.cell_source_code,
            }))
        }
        Operation::FindModels => {
            let input: FindModelsInput = parse_input(FIND_MODELS, input)?;
            Ok(json!({"query": input.query, "limit": input.limit.unwrap_or(8)}))
        }
        Operation::ListSubagents => {
            let _: EmptyInput = parse_input(LIST_SUBAGENTS, input)?;
            Ok(json!({}))
        }
        Operation::DeleteSubagent => {
            let input: TargetInput = parse_input(DELETE_SUBAGENT, input)?;
            Ok(json!({"target": input.target}))
        }
        Operation::CancelSubagent => {
            let input: TargetInput = parse_input(CANCEL_SUBAGENT, input)?;
            Ok(json!({"target": input.target}))
        }
    }
}

fn success_summary(operation: Operation, output: &Value) -> String {
    match operation {
        Operation::SpawnAgent => output.get("name").and_then(Value::as_str).map_or_else(
            || "child agent spawned".into(),
            |name| format!("child agent '{name}' spawned"),
        ),
        Operation::FindModels => {
            let count = output
                .get("models")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("found {count} authenticated RLM models")
        }
        Operation::ListSubagents => {
            let count = output
                .get("subagents")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("listed {count} RLM subagents")
        }
        Operation::DeleteSubagent => "RLM subagent deleted".into(),
        Operation::CancelSubagent => "RLM cancellation processed".into(),
    }
}
