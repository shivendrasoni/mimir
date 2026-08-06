use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    atomic,
    error::{MimirError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub schema_version: u16,
    pub id: Uuid,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub used_tokens: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct GoalStore {
    root: PathBuf,
    path: PathBuf,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl GoalStore {
    pub fn new(state_root: &Path) -> Self {
        let root = atomic::canonical_state_root(state_root);
        let path = root.join("goals/goal.json");
        Self {
            root,
            mutation_lock: atomic::path_lock(&path),
            path,
        }
    }

    /// Creates the one active goal.
    ///
    /// # Errors
    ///
    /// Returns an error for a blank objective, zero budget, existing active goal, or persistence failure.
    pub async fn create(&self, objective: &str, token_budget: Option<u64>) -> Result<Goal> {
        if objective.trim().is_empty() {
            return Err(MimirError::Configuration(
                "goal objective must not be blank".into(),
            ));
        }
        if token_budget == Some(0) {
            return Err(MimirError::Configuration(
                "goal token budget must be positive".into(),
            ));
        }
        let _guard = self.mutation_lock.lock().await;
        if self
            .load_unlocked()
            .await?
            .is_some_and(|goal| goal.status == GoalStatus::Active)
        {
            return Err(MimirError::Configuration(
                "an active goal already exists".into(),
            ));
        }
        let now = Utc::now();
        let goal = Goal {
            schema_version: 1,
            id: Uuid::new_v4(),
            objective: objective.trim().into(),
            status: GoalStatus::Active,
            token_budget,
            used_tokens: 0,
            created_at: now,
            updated_at: now,
        };
        self.write_unlocked(&goal).await?;
        Ok(goal)
    }

    /// Loads the current goal when present.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error.
    pub async fn load(&self) -> Result<Option<Goal>> {
        atomic::prepare_state_path(&self.root, &self.path).await?;
        atomic::read_json(&self.path).await
    }

    /// Adds model usage to the active goal.
    ///
    /// # Errors
    ///
    /// Returns an error when no goal exists or persistence fails.
    pub async fn record_tokens(&self, tokens: u64) -> Result<Goal> {
        let _guard = self.mutation_lock.lock().await;
        let mut goal = self.load_unlocked().await?.ok_or_else(|| {
            MimirError::Configuration("cannot record usage without a goal".into())
        })?;
        goal.used_tokens = goal.used_tokens.saturating_add(tokens);
        goal.updated_at = Utc::now();
        if goal
            .token_budget
            .is_some_and(|budget| goal.used_tokens >= budget)
        {
            goal.status = GoalStatus::Blocked;
        }
        self.write_unlocked(&goal).await?;
        Ok(goal)
    }

    /// Changes goal status without replacing its objective or accounting.
    ///
    /// # Errors
    ///
    /// Returns an error when no goal exists or persistence fails.
    pub async fn set_status(&self, status: GoalStatus) -> Result<Goal> {
        let _guard = self.mutation_lock.lock().await;
        let mut goal = self
            .load_unlocked()
            .await?
            .ok_or_else(|| MimirError::Configuration("cannot update a missing goal".into()))?;
        goal.status = status;
        goal.updated_at = Utc::now();
        self.write_unlocked(&goal).await?;
        Ok(goal)
    }

    /// Removes current goal state idempotently.
    ///
    /// # Errors
    ///
    /// Returns an I/O error other than a missing file.
    pub async fn clear(&self) -> Result<()> {
        let _guard = self.mutation_lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn load_unlocked(&self) -> Result<Option<Goal>> {
        atomic::prepare_state_path(&self.root, &self.path).await?;
        atomic::read_json(&self.path).await
    }

    async fn write_unlocked(&self, goal: &Goal) -> Result<()> {
        atomic::prepare_state_path(&self.root, &self.path).await?;
        atomic::write_json(&self.path, goal).await
    }
}
