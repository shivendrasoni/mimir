use std::path::PathBuf;

use thiserror::Error;

use crate::budget::BudgetPause;

#[derive(Debug, Error)]
pub enum MimirError {
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("session error at {path}: {message}")]
    Session { path: PathBuf, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("budget paused: {0}")]
    BudgetPaused(BudgetPause),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, MimirError>;
