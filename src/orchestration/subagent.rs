use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, task::JoinHandle};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    runtime::{AgentRuntime, EventSink},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentHandle {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentState {
    pub id: Uuid,
    pub name: String,
    pub status: SubagentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub struct SubagentManager {
    max_children: usize,
    states: Mutex<BTreeMap<Uuid, SubagentState>>,
    tasks: Mutex<BTreeMap<Uuid, JoinHandle<()>>>,
}

impl SubagentManager {
    pub fn new(max_children: usize) -> Self {
        Self {
            max_children: max_children.max(1),
            states: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Admits and starts a child runtime without waiting for its answer.
    ///
    /// # Errors
    ///
    /// Returns an admission error when the concurrency cap is full or the name is blank.
    pub async fn spawn(
        self: &Arc<Self>,
        name: &str,
        prompt: &str,
        runtime: Arc<AgentRuntime>,
        sink: Arc<dyn EventSink>,
    ) -> Result<SubagentHandle> {
        if name.trim().is_empty() || prompt.trim().is_empty() {
            return Err(MimirError::Protocol(
                "subagent name and prompt must not be blank".into(),
            ));
        }
        let mut states = self.states.lock().await;
        let running = states
            .values()
            .filter(|state| state.status == SubagentStatus::Running)
            .count();
        if running >= self.max_children {
            return Err(MimirError::Protocol(format!(
                "subagent concurrency limit {} reached",
                self.max_children
            )));
        }
        let id = Uuid::new_v4();
        let state = SubagentState {
            id,
            name: name.trim().into(),
            status: SubagentStatus::Running,
            result: None,
            error: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        states.insert(id, state.clone());
        drop(states);
        let manager = Arc::clone(self);
        let prompt = prompt.trim().to_owned();
        let task = tokio::spawn(async move {
            let result = runtime.run(&prompt, sink.as_ref()).await;
            let mut states = manager.states.lock().await;
            if let Some(state) = states.get_mut(&id) {
                state.finished_at = Some(Utc::now());
                match result {
                    Ok(result) => {
                        state.status = SubagentStatus::Complete;
                        state.result = Some(result);
                    }
                    Err(error) => {
                        state.status = SubagentStatus::Failed;
                        state.error = Some(error.to_string());
                    }
                }
            }
        });
        self.tasks.lock().await.insert(id, task);
        Ok(SubagentHandle {
            id,
            name: state.name,
        })
    }

    /// Waits for a child and returns its terminal state.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown child, failed task join, or missing terminal state.
    pub async fn wait(&self, id: Uuid) -> Result<SubagentState> {
        let task = self.tasks.lock().await.remove(&id).ok_or_else(|| {
            MimirError::Protocol(format!("unknown or already-waited subagent {id}"))
        })?;
        task.await
            .map_err(|error| MimirError::Protocol(format!("subagent task failed: {error}")))?;
        self.states
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| MimirError::Protocol(format!("missing subagent state {id}")))
    }

    pub async fn list(&self) -> Vec<SubagentState> {
        self.states.lock().await.values().cloned().collect()
    }
}
