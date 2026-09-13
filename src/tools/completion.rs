use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::model::ToolDefinition;

use super::{Tool, ToolError, ToolObservation, WorkspacePathPolicy, object_schema, parse_input};

const MAX_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_ARTIFACTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCompletion {
    pub summary: String,
    pub artifacts: Vec<PathBuf>,
}

#[derive(Default)]
pub struct TaskCompletionSignal {
    enabled: AtomicBool,
    pending: Mutex<Option<TaskCompletion>>,
}

impl TaskCompletionSignal {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub async fn clear(&self) {
        self.pending.lock().await.take();
    }

    pub async fn take(&self) -> Option<TaskCompletion> {
        self.pending.lock().await.take()
    }

    pub async fn get(&self) -> Option<TaskCompletion> {
        self.pending.lock().await.clone()
    }
}

pub struct FinishTaskTool {
    paths: WorkspacePathPolicy,
    signal: Arc<TaskCompletionSignal>,
}

impl FinishTaskTool {
    pub fn new(paths: WorkspacePathPolicy, signal: Arc<TaskCompletionSignal>) -> Self {
        Self { paths, signal }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishTaskInput {
    summary: String,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[async_trait]
impl Tool for FinishTaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "finish_task".into(),
            description: "Signal that an autonomous task is genuinely complete and validated. Call this only after all requested work and checks are finished.".into(),
            parameters: object_schema(
                &json!({
                    "summary": {
                        "type": "string",
                        "description": "Concise completion summary",
                        "minLength": 1,
                        "maxLength": MAX_SUMMARY_BYTES
                    },
                    "artifacts": {
                        "type": "array",
                        "description": "Optional existing workspace-relative artifact paths",
                        "maxItems": MAX_ARTIFACTS,
                        "items": {"type": "string", "minLength": 1}
                    }
                }),
                &["summary"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        if !self.signal.enabled() {
            return Err(ToolError::Disabled {
                tool: "finish_task".into(),
            });
        }
        let input: FinishTaskInput = parse_input("finish_task", input)?;
        let summary = input.summary.trim();
        if summary.is_empty() || summary.len() > MAX_SUMMARY_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "finish_task".into(),
                message: format!("summary must contain 1 to {MAX_SUMMARY_BYTES} bytes"),
            });
        }
        if input.artifacts.len() > MAX_ARTIFACTS {
            return Err(ToolError::InvalidArguments {
                tool: "finish_task".into(),
                message: format!("artifacts are limited to {MAX_ARTIFACTS} paths"),
            });
        }
        let mut artifacts = Vec::with_capacity(input.artifacts.len());
        let mut completion_artifacts = Vec::with_capacity(input.artifacts.len());
        for requested in input.artifacts {
            let resolved = self.paths.resolve_existing(&requested)?;
            completion_artifacts.push(
                resolved
                    .strip_prefix(self.paths.root())
                    .expect("workspace policy keeps resolved artifacts under its root")
                    .to_owned(),
            );
            artifacts.push(resolved);
        }
        let completion = TaskCompletion {
            summary: summary.to_owned(),
            artifacts: completion_artifacts,
        };
        *self.signal.pending.lock().await = Some(completion);
        Ok(ToolObservation {
            status: super::ObservationStatus::Success,
            summary: "autonomous task marked complete".into(),
            next_actions: Vec::new(),
            artifacts,
            content: summary.to_owned(),
        })
    }
}
