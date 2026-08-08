use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationAction {
    Create,
    Replace,
    Unchanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Auth,
    Session,
    Settings,
    Preferences,
    Models,
    ModelCatalog,
    Inventory,
    CompatibilityArchive,
    ResourceArchive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub kind: ArtifactKind,
    pub action: MigrationAction,
    pub source: String,
    pub target: String,
    pub bytes: usize,
    pub sha256: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthReport {
    pub provider: String,
    pub auth_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReport {
    pub session_id: String,
    pub records: usize,
    pub source_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub extensions: usize,
    pub packages: usize,
    pub skills: usize,
    pub derived_from_settings: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsReport {
    pub provider_preference: bool,
    pub model_preference: bool,
    pub recent_models: usize,
    pub enabled_models: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsReport {
    pub providers: usize,
    pub models: usize,
    pub request_auth_entries_archived: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    pub schema_version: u16,
    pub archived_files: usize,
    pub archived_bytes: usize,
    pub unrepresentable: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub redacted: bool,
    pub auth: Vec<AuthReport>,
    pub sessions: Vec<SessionReport>,
    pub settings: Option<SettingsReport>,
    pub models: Option<ModelsReport>,
    pub inventory: Option<InventoryReport>,
    pub compatibility: CompatibilityReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub journal_id: String,
    pub legacy_root: PathBuf,
    pub state_root: PathBuf,
    pub steps: Vec<PlanStep>,
    pub report: MigrationReport,
}

impl MigrationPlan {
    pub fn actionable_steps(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.action != MigrationAction::Unchanged)
            .count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalStep {
    pub kind: ArtifactKind,
    pub target: String,
    pub sha256: String,
    pub backup: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Applying,
    Applied,
    RolledBack,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationJournal {
    pub journal_id: String,
    pub legacy_root: PathBuf,
    pub state_root: PathBuf,
    pub status: JournalStatus,
    pub created_at_rfc3339: String,
    pub steps: Vec<JournalStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMigration {
    pub journal_path: Option<PathBuf>,
    pub applied_steps: usize,
    pub skipped_steps: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackReport {
    pub journal_path: PathBuf,
    pub restored_steps: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedArtifact {
    pub kind: ArtifactKind,
    pub source: String,
    pub target: String,
    pub bytes: Vec<u8>,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedReport {
    pub auth: Vec<AuthReport>,
    pub sessions: Vec<SessionReport>,
    pub settings: Option<SettingsReport>,
    pub models: Option<ModelsReport>,
    pub inventory: Option<InventoryReport>,
    pub compatibility: CompatibilityReport,
}
