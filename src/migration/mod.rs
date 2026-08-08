#![allow(
    clippy::missing_errors_doc,
    reason = "migration operations return typed validation, filesystem, and schema errors"
)]

mod legacy;
mod runtime;
mod safe_fs;
mod session_import;
mod types;

use uuid::Uuid;

pub use runtime::{MigratedPreferences, MigratedRuntimeState};
pub use types::{
    AppliedMigration, ArtifactKind, AuthReport, CompatibilityReport, InventoryReport,
    JournalStatus, MigrationAction, MigrationJournal, MigrationPlan, MigrationReport, ModelsReport,
    PlanStep, RollbackReport, SessionReport, SettingsReport,
};

use crate::error::Result;

#[derive(Default)]
pub struct StateMigrator;

impl StateMigrator {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub async fn plan(
        &self,
        legacy_root: &std::path::Path,
        state_root: &std::path::Path,
    ) -> Result<MigrationPlan> {
        let legacy_root = safe_fs::canonical_legacy_root(legacy_root).await?;
        let state_root = safe_fs::canonical_state_root(state_root).await?;
        let (artifacts, report) = legacy::collect(&legacy_root).await?;
        let mut steps = Vec::new();
        for artifact in artifacts {
            let sha256 = safe_fs::sha256_hex(&artifact.bytes);
            let action = safe_fs::target_action(&state_root, &artifact.target, &sha256).await?;
            steps.push(PlanStep {
                kind: artifact.kind,
                action,
                source: artifact.source,
                target: artifact.target,
                bytes: artifact.bytes.len(),
                sha256,
                summary: artifact.summary,
            });
        }
        Ok(MigrationPlan {
            journal_id: format!("migration-{}", Uuid::new_v4()),
            legacy_root,
            state_root,
            steps,
            report: MigrationReport {
                redacted: true,
                auth: report.auth,
                sessions: report.sessions,
                settings: report.settings,
                models: report.models,
                inventory: report.inventory,
                compatibility: report.compatibility,
            },
        })
    }

    pub async fn apply(&self, plan: &MigrationPlan) -> Result<AppliedMigration> {
        let legacy_root = safe_fs::canonical_legacy_root(&plan.legacy_root).await?;
        let state_root = safe_fs::canonical_state_root(&plan.state_root).await?;
        if legacy_root != plan.legacy_root || state_root != plan.state_root {
            return Err(crate::error::MimirError::Configuration(
                "migration roots changed after planning".into(),
            ));
        }
        let (artifacts, _) = legacy::collect(&legacy_root).await?;
        let mut changed = Vec::new();
        let mut skipped_steps = 0;
        for step in &plan.steps {
            let artifact = artifacts
                .iter()
                .find(|artifact| artifact.target == step.target)
                .ok_or_else(|| {
                    crate::error::MimirError::Configuration(format!(
                        "migration source changed after planning: {} is missing",
                        step.source
                    ))
                })?;
            if safe_fs::sha256_hex(&artifact.bytes) != step.sha256 {
                return Err(crate::error::MimirError::Configuration(format!(
                    "migration source changed after planning: {} no longer matches the plan",
                    step.source
                )));
            }
            let current_action =
                safe_fs::target_action(&state_root, &step.target, &step.sha256).await?;
            if current_action == MigrationAction::Unchanged {
                skipped_steps += 1;
                continue;
            }
            if step.action == MigrationAction::Unchanged || current_action != step.action {
                return Err(crate::error::MimirError::Configuration(format!(
                    "migration target changed after planning: {} no longer matches the plan",
                    step.target
                )));
            }
            changed.push((step, artifact));
        }
        if changed.is_empty() {
            return Ok(AppliedMigration {
                journal_path: None,
                applied_steps: 0,
                skipped_steps,
            });
        }

        let journal_rel = format!("migration/journals/{}.json", plan.journal_id);
        let backup_root = format!("migration/backups/{}", plan.journal_id);
        let staging_root = format!("migration/staging/{}", plan.journal_id);
        let mut journal = MigrationJournal {
            journal_id: plan.journal_id.clone(),
            legacy_root: plan.legacy_root.clone(),
            state_root: plan.state_root.clone(),
            status: JournalStatus::Applying,
            created_at_rfc3339: safe_fs::now_rfc3339(),
            steps: Vec::new(),
        };

        for (_, artifact) in &changed {
            let stage_relative = format!("{staging_root}/{}", artifact.target);
            safe_fs::write_bytes_atomic(&plan.state_root, &stage_relative, &artifact.bytes).await?;
        }

        // Persist the journal before touching any destination. Each step is added before
        // its corresponding replacement so an interrupted apply always has a rollback path.
        safe_fs::write_json_atomic(&plan.state_root, &journal_rel, &journal).await?;

        for (step, artifact) in &changed {
            let target_path = safe_fs::safe_join(&plan.state_root, &artifact.target)?;
            let backup = match tokio::fs::symlink_metadata(&target_path).await {
                Ok(metadata) if metadata.is_file() => {
                    let backup_relative = format!("{backup_root}/{}", artifact.target);
                    safe_fs::copy_file(&plan.state_root, &target_path, &backup_relative).await?;
                    Some(backup_relative)
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            journal.steps.push(types::JournalStep {
                kind: step.kind,
                target: step.target.clone(),
                sha256: step.sha256.clone(),
                backup,
            });
            safe_fs::write_json_atomic(&plan.state_root, &journal_rel, &journal).await?;
            let stage_relative = format!("{staging_root}/{}", artifact.target);
            let staged_path = safe_fs::safe_join(&plan.state_root, &stage_relative)?;
            let bytes = safe_fs::read_path_bounded(&staged_path).await?;
            safe_fs::write_bytes_atomic(&plan.state_root, &artifact.target, &bytes).await?;
            safe_fs::remove_file_if_exists(&plan.state_root, &stage_relative).await?;
        }

        journal.status = JournalStatus::Applied;
        safe_fs::write_json_atomic(&plan.state_root, &journal_rel, &journal).await?;
        Ok(AppliedMigration {
            journal_path: Some(safe_fs::safe_join(&plan.state_root, &journal_rel)?),
            applied_steps: journal.steps.len(),
            skipped_steps,
        })
    }

    pub async fn rollback(&self, journal_path: &std::path::Path) -> Result<RollbackReport> {
        let journal_path = safe_fs::canonical_journal_path(journal_path).await?;
        let bytes = safe_fs::read_path_bounded(&journal_path).await?;
        let mut journal: MigrationJournal = serde_json::from_slice(&bytes)?;
        let state_root = safe_fs::canonical_state_root(&journal.state_root).await?;
        safe_fs::validate_journal_location(&state_root, &journal_path)?;
        if journal.status == JournalStatus::RolledBack {
            return Ok(RollbackReport {
                journal_path,
                restored_steps: 0,
            });
        }
        let mut restored_steps = 0;
        for step in journal.steps.iter().rev() {
            if let Some(backup) = &step.backup {
                let backup_path = safe_fs::safe_join(&state_root, backup)?;
                let bytes = safe_fs::read_path_bounded(&backup_path).await?;
                safe_fs::write_bytes_atomic(&state_root, &step.target, &bytes).await?;
            } else {
                safe_fs::remove_file_if_exists(&state_root, &step.target).await?;
            }
            restored_steps += 1;
        }
        journal.status = JournalStatus::RolledBack;
        let journal_relative = journal_path
            .strip_prefix(&state_root)
            .ok()
            .and_then(|relative| relative.to_str())
            .ok_or_else(|| crate::error::MimirError::Session {
                path: journal_path.clone(),
                message: "journal path must live under the state root".into(),
            })?;
        safe_fs::write_json_atomic(&state_root, journal_relative, &journal).await?;
        Ok(RollbackReport {
            journal_path,
            restored_steps,
        })
    }
}
