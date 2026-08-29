use std::{
    collections::BTreeSet,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::ToolError;

const APPROVALS_FILE: &str = ".mimir/workspace-permissions.json";
const AUDIT_FILE: &str = ".mimir/workspace-permissions.audit.jsonl";

/// A destructive operation that needs an explicit workspace-owner decision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveAction {
    Delete,
    GitDestructive,
    ForcePush,
}

impl DestructiveAction {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Delete => "delete files or directories",
            Self::GitDestructive => "perform a destructive Git operation",
            Self::ForcePush => "force-push Git history",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllowWorkspace,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub action: DestructiveAction,
    pub command: String,
}

impl PermissionRequest {
    #[must_use]
    pub fn message(&self) -> String {
        format!("Mimir wants to {}:\n{}", self.action.label(), self.command)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ApprovalState {
    version: u8,
    always_allowed: BTreeSet<DestructiveAction>,
    allow_once: BTreeSet<DestructiveAction>,
}

/// Workspace-local approval state. This deliberately lives inside the workspace
/// so a grant cannot follow Mimir into a different checkout.
#[derive(Debug, Clone)]
pub struct WorkspaceApprovalStore {
    root: Arc<PathBuf>,
}

impl WorkspaceApprovalStore {
    /// Opens the approval ledger for an existing workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace cannot be canonicalized.
    pub fn new(workspace: &Path) -> Result<Self, ToolError> {
        Ok(Self {
            root: Arc::new(workspace.canonicalize()?),
        })
    }

    fn approvals_path(&self) -> PathBuf {
        self.root.join(APPROVALS_FILE)
    }
    fn audit_path(&self) -> PathBuf {
        self.root.join(AUDIT_FILE)
    }

    /// Returns a request when an operation is not covered by a durable grant.
    ///
    /// # Errors
    ///
    /// Returns an error when the approval ledger cannot be read or updated.
    pub fn requires_approval(&self, command: &str) -> Result<Option<PermissionRequest>, ToolError> {
        let Some(action) = classify_command(command) else {
            return Ok(None);
        };
        let mut state = self.load()?;
        if state.always_allowed.contains(&action) {
            Ok(None)
        } else if state.allow_once.remove(&action) {
            self.write_state(&state)?;
            Ok(None)
        } else {
            Ok(Some(PermissionRequest {
                action,
                command: command.into(),
            }))
        }
    }

    /// Records the visible user decision. A one-time grant is retained only
    /// until the next matching operation consumes it; an always grant persists.
    ///
    /// # Errors
    ///
    /// Returns an error when the approval or audit ledger cannot be written.
    pub fn record(
        &self,
        request: &PermissionRequest,
        decision: ApprovalDecision,
    ) -> Result<(), ToolError> {
        let mut state = self.load()?;
        match decision {
            ApprovalDecision::AlwaysAllowWorkspace => {
                state.version = 1;
                state.always_allowed.insert(request.action.clone());
                self.write_state(&state)?;
            }
            ApprovalDecision::AllowOnce => {
                state.version = 1;
                state.allow_once.insert(request.action.clone());
                self.write_state(&state)?;
            }
            ApprovalDecision::Deny => {}
        }
        let audit = serde_json::json!({
            "timestampMs": now_ms(), "action": request.action, "command": request.command,
            "decision": decision,
        });
        let audit_path = self.audit_path();
        if let Some(parent) = audit_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(audit_path)?;
        writeln!(
            file,
            "{}",
            serde_json::to_string(&audit).map_err(|error| ToolError::Execution {
                tool: "workspace_permissions".into(),
                message: error.to_string()
            })?
        )?;
        Ok(())
    }

    fn load(&self) -> Result<ApprovalState, ToolError> {
        let path = self.approvals_path();
        if !path.exists() {
            return Ok(ApprovalState::default());
        }
        serde_json::from_slice(&std::fs::read(path)?).map_err(|error| ToolError::Execution {
            tool: "workspace_permissions".into(),
            message: format!("invalid approval ledger: {error}"),
        })
    }

    fn write_state(&self, state: &ApprovalState) -> Result<(), ToolError> {
        let path = self.approvals_path();
        let parent = path.parent().expect("workspace approval path has a parent");
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(".workspace-permissions.tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec_pretty(state).map_err(|error| ToolError::Execution {
                tool: "workspace_permissions".into(),
                message: error.to_string(),
            })?,
        )?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis())
}

/// Conservative command classification. It recognizes direct destructive
/// utilities and Git's destructive modes; unrecognized commands remain usable.
#[must_use]
pub fn classify_command(command: &str) -> Option<DestructiveAction> {
    let words = shell_words(command);
    let program = words.first()?.as_str();
    if matches!(program, "rm" | "rmdir" | "unlink" | "trash")
        || words.iter().any(|word| word == "-delete")
    {
        return Some(DestructiveAction::Delete);
    }
    if program == "git" {
        let force_push = words.get(1).is_some_and(|word| word == "push")
            && words
                .iter()
                .any(|word| matches!(word.as_str(), "--force" | "-f" | "--force-with-lease"));
        if force_push {
            return Some(DestructiveAction::ForcePush);
        }
        if words
            .iter()
            .any(|word| matches!(word.as_str(), "reset" | "clean" | "restore" | "rebase"))
        {
            return Some(DestructiveAction::GitDestructive);
        }
    }
    None
}

fn shell_words(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|word| word.trim_matches(['\'', '"']))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn destructive_commands_are_classified_without_blocking_normal_git() {
        assert_eq!(classify_command("git status"), None);
        assert_eq!(classify_command("cargo test"), None);
        assert_eq!(
            classify_command("rm -rf generated"),
            Some(DestructiveAction::Delete)
        );
        assert_eq!(
            classify_command("git reset --hard HEAD"),
            Some(DestructiveAction::GitDestructive)
        );
        assert_eq!(
            classify_command("git push --force origin main"),
            Some(DestructiveAction::ForcePush)
        );
    }

    #[test]
    fn workspace_grants_are_durable_and_denials_are_audited() {
        let workspace = TempDir::new().expect("workspace");
        let store = WorkspaceApprovalStore::new(workspace.path()).expect("store");
        let request = store
            .requires_approval("rm obsolete.txt")
            .expect("request")
            .expect("approval required");
        store
            .record(&request, ApprovalDecision::AlwaysAllowWorkspace)
            .expect("grant");
        assert!(
            store
                .requires_approval("rm another.txt")
                .expect("recheck")
                .is_none()
        );

        let force = store
            .requires_approval("git push --force origin main")
            .expect("force request")
            .expect("force approval required");
        store.record(&force, ApprovalDecision::Deny).expect("deny");
        assert!(store.audit_path().exists());
        assert!(
            std::fs::read_to_string(store.audit_path())
                .expect("audit")
                .contains("deny")
        );
    }

    #[test]
    fn allow_once_is_consumed_by_the_next_matching_operation() {
        let workspace = TempDir::new().expect("workspace");
        let store = WorkspaceApprovalStore::new(workspace.path()).expect("store");
        let request = store
            .requires_approval("rm obsolete.txt")
            .expect("request")
            .expect("approval required");
        store
            .record(&request, ApprovalDecision::AllowOnce)
            .expect("allow once");
        assert!(
            store
                .requires_approval("rm replacement.txt")
                .expect("consumed")
                .is_none()
        );
        assert!(
            store
                .requires_approval("rm another.txt")
                .expect("new request")
                .is_some()
        );
    }
}
