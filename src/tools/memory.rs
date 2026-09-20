use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::{Role, ToolDefinition},
    refinement::{self, HarnessScope},
    session::{FileSessionStore, SessionPayload, SessionStore},
};

use super::{Tool, ToolError, ToolObservation, object_schema, parse_input};

pub struct RememberTool {
    state_root: PathBuf,
    session_root: PathBuf,
    workspace: PathBuf,
    session: String,
}

impl RememberTool {
    pub fn new(
        state_root: &std::path::Path,
        session_root: &std::path::Path,
        workspace: &std::path::Path,
        session: &str,
    ) -> Self {
        Self {
            state_root: state_root.to_owned(),
            session_root: session_root.to_owned(),
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
            description: "Persist a concise memory only when the user explicitly asks Mimir to remember or save something for later. Rewrite the request as durable guidance rather than copying conversational filler. Default to project scope and use session only when the user says it is temporary. Use user scope only when the current user message explicitly says the guidance applies across projects or globally; the host independently verifies that intent. Never infer an unrequested memory or store credentials, source code, paths, project status, or tool payloads."
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
                        "description": "Persistence boundary; user requires explicit global or across-project wording in the current user message"
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
        if scope == HarnessScope::User && !self.current_turn_authorizes_user_scope().await? {
            return Err(ToolError::InvalidArguments {
                tool: "remember".into(),
                message: "user scope requires the current user message to explicitly request global or across-project remembrance; otherwise use project scope or /refine --scope user"
                    .into(),
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

impl RememberTool {
    async fn current_turn_authorizes_user_scope(&self) -> Result<bool, ToolError> {
        let store = FileSessionStore::create(&self.session_root, &self.session)
            .await
            .map_err(memory_execution_error)?;
        let loaded = store.load().await.map_err(memory_execution_error)?;
        let request = loaded.records.iter().rev().find_map(|record| {
            let SessionPayload::Message(message) = &record.payload else {
                return None;
            };
            (message.role == Role::User).then(|| message.text())
        });
        Ok(request
            .as_deref()
            .is_some_and(explicit_cross_project_memory_request))
    }
}

fn explicit_cross_project_memory_request(request: &str) -> bool {
    let request = request.to_ascii_lowercase();
    let asks_to_remember = ["remember", "save this", "retain this", "keep this"]
        .iter()
        .any(|phrase| request.contains(phrase));
    let crosses_projects = [
        "across all projects",
        "across projects",
        "all projects",
        "every project",
        "globally",
        "global memory",
        "wherever i use mimir",
    ]
    .iter()
    .any(|phrase| request.contains(phrase));
    asks_to_remember && crosses_projects
}

fn memory_execution_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution {
        tool: "remember".into(),
        message: error.to_string(),
    }
}
