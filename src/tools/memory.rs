use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::ToolDefinition,
    refinement::{self, HarnessScope},
};

use super::{Tool, ToolError, ToolObservation, object_schema, parse_input};

pub struct RememberTool {
    state_root: PathBuf,
    workspace: PathBuf,
    session: String,
}

impl RememberTool {
    pub fn new(state_root: &std::path::Path, workspace: &std::path::Path, session: &str) -> Self {
        Self {
            state_root: state_root.to_owned(),
            workspace: workspace.to_owned(),
            session: session.to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberInput {
    memory: String,
    #[serde(default = "default_scope")]
    scope: String,
}

fn default_scope() -> String {
    "project".into()
}

#[async_trait]
impl Tool for RememberTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "remember".into(),
            description: "Persist a concise memory only when the user explicitly asks Mimir to remember, always remember, or save something for later. Rewrite the request as durable guidance rather than copying conversational filler. Default to project scope. Use session only when the user says it is temporary, and user only when they explicitly want it across all projects. Never infer an unrequested memory or store credentials, source code, paths, or tool payloads."
                .into(),
            parameters: object_schema(
                &json!({
                    "memory": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 4096,
                        "description": "Concise standalone guidance to recall later"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "project", "user"],
                        "default": "project",
                        "description": "Persistence boundary; user requires an explicit across-project request"
                    }
                }),
                &["memory"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: RememberInput = parse_input("remember", input)?;
        let scope =
            HarnessScope::parse(&input.scope).ok_or_else(|| ToolError::InvalidArguments {
                tool: "remember".into(),
                message: "scope must be session, project, or user".into(),
            })?;
        if scope == HarnessScope::Fleet {
            return Err(ToolError::InvalidArguments {
                tool: "remember".into(),
                message: "fleet memory is read-only".into(),
            });
        }
        let result = refinement::remember(
            &self.state_root,
            &self.workspace,
            &self.session,
            scope,
            &input.memory,
        )
        .await
        .map_err(|error| ToolError::Execution {
            tool: "remember".into(),
            message: error.to_string(),
        })?;
        let memory_id = result
            .applied_edits
            .first()
            .map(|edit| edit.id.clone())
            .unwrap_or_default();
        let content = serde_json::to_string(&json!({
            "status": "success",
            "scope": scope.as_str(),
            "memory_id": memory_id,
            "refinement_id": result.id.clone(),
        }))
        .map_err(|error| ToolError::Execution {
            tool: "remember".into(),
            message: error.to_string(),
        })?;
        let mut observation =
            ToolObservation::success(format!("memory saved to {} scope", scope.as_str()), content);
        observation.next_actions.push(format!(
            "Use /refine rollback {} to forget this memory",
            result.id
        ));
        observation
            .artifacts
            .push(PathBuf::from(result.harness_state_path));
        Ok(observation)
    }
}
