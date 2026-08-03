mod export;
mod import;
mod message;
mod share;

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::session::SessionRecord;

pub use export::export_jsonl;
pub use import::{import_jsonl, prepare_switch_session};
pub use share::{MAX_SHARE_PAYLOAD_BYTES, SharePayload, prepare_share_payload};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFormat {
    Reference(u16),
    Rust(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceSessionMetadata {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub cwd: PathBuf,
    pub parent_session: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSession {
    pub format: SessionFormat,
    pub metadata: Option<ReferenceSessionMetadata>,
    pub state: SessionCompatibilityState,
    pub records: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCompatibilityState {
    pub thinking_level: Option<String>,
    pub service_tier: Option<String>,
    pub model: Option<SessionModelSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelSelection {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchSessionPlan {
    pub format: SessionFormat,
    pub target_session_id: String,
    pub cwd: PathBuf,
    pub state: SessionCompatibilityState,
    pub records: Vec<SessionRecord>,
}
