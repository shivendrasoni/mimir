use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use super::ToolError;

#[derive(Debug, Clone)]
pub struct WorkspacePathPolicy {
    root: Arc<PathBuf>,
}

impl WorkspacePathPolicy {
    /// Canonicalizes a workspace root for subsequent path checks.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the root does not exist or cannot be canonicalized.
    pub fn new(root: &Path) -> Result<Self, ToolError> {
        Ok(Self {
            root: Arc::new(root.canonicalize()?),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_ref()
    }

    /// Returns model-facing guidance for paths accepted by workspace tools.
    #[must_use]
    pub fn path_guidance(&self) -> String {
        "Non-empty $WORKSPACE-relative path; absolute paths and '..' are rejected.".into()
    }

    /// Rejects path-like process arguments that obviously escape the workspace contract.
    ///
    /// This is intentionally a conservative argument check, not an operating-system sandbox.
    /// It catches standalone absolute paths and relative paths containing `..`, while leaving
    /// flags, URLs, expressions, and source-code arguments untouched.
    ///
    /// # Errors
    ///
    /// Returns a workspace policy error for an obvious absolute path or parent traversal.
    pub fn validate_obvious_process_path_argument(&self, argument: &str) -> Result<(), ToolError> {
        if argument.is_empty()
            || (argument.starts_with('-') && argument != "-")
            || argument.contains("://")
            || !looks_like_literal_path(argument)
        {
            return Ok(());
        }
        let path = Path::new(argument);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            self.validate_requested_path(argument)?;
        }
        Ok(())
    }

    /// Resolves an existing relative path and denies canonical workspace escape.
    ///
    /// # Errors
    ///
    /// Returns a policy or I/O error for traversal, missing targets, and escaping symlinks.
    pub fn resolve_existing(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.candidate(requested)?;
        let resolved = candidate
            .canonicalize()
            .map_err(|error| ToolError::Execution {
                tool: "path".into(),
                message: format!("{}: {error}", candidate.display()),
            })?;
        self.ensure_inside(requested, resolved)
    }

    /// Resolves a writable relative target through its nearest canonical existing ancestor.
    ///
    /// # Errors
    ///
    /// Returns a policy or I/O error for traversal and escaping symlinks.
    /// Missing parent directories are permitted so a caller can create them
    /// after this policy check.
    pub fn resolve_for_write(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.candidate(requested)?;
        if candidate.exists() {
            return self.ensure_inside(requested, candidate.canonicalize()?);
        }
        let mut parent = candidate
            .parent()
            .ok_or_else(|| ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: "path has no parent".into(),
            })?
            .to_owned();
        while !parent.exists() {
            parent = parent
                .parent()
                .ok_or_else(|| ToolError::WorkspaceDenied {
                    path: requested.into(),
                    reason: "path has no existing workspace ancestor".into(),
                })?
                .to_owned();
        }
        let resolved_parent = parent
            .canonicalize()
            .map_err(|error| ToolError::Execution {
                tool: "path".into(),
                message: format!("{}: {error}", parent.display()),
            })?;
        self.ensure_inside(requested, resolved_parent)?;
        Ok(candidate)
    }

    fn candidate(&self, requested: &str) -> Result<PathBuf, ToolError> {
        self.validate_requested_path(requested)?;
        let path = Path::new(requested);
        Ok(self.root.join(path))
    }

    fn validate_requested_path(&self, requested: &str) -> Result<(), ToolError> {
        if requested.is_empty() {
            return Err(ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: format!(
                    "path must be non-empty and relative to workspace root '{}'",
                    self.root.display()
                ),
            });
        }
        let path = Path::new(requested);
        if path.is_absolute() {
            return Err(self.absolute_path_error(requested, path));
        }
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: format!(
                    "'..' parent traversal is not allowed; use a path relative to workspace root '{}'. If the target is outside that root, restart Mimir with a broader --workspace or copy it into the workspace",
                    self.root.display()
                ),
            });
        }
        Ok(())
    }

    fn absolute_path_error(&self, requested: &str, path: &Path) -> ToolError {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let relative = resolved
            .strip_prefix(self.root.as_ref())
            .ok()
            .filter(|relative| {
                !relative.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            });
        let reason = if let Some(relative) = relative {
            let suggestion = if relative.as_os_str().is_empty() {
                ".".into()
            } else {
                relative.display().to_string()
            };
            format!(
                "absolute paths are not accepted; this target is inside workspace root '{}', so use the relative path '{suggestion}'",
                self.root.display()
            )
        } else {
            self.outside_workspace_reason(&resolved)
        };
        ToolError::WorkspaceDenied {
            path: requested.into(),
            reason,
        }
    }

    fn outside_workspace_reason(&self, resolved: &Path) -> String {
        format!(
            "target '{}' is outside workspace root '{}'; restart Mimir with a broader --workspace that contains the target or copy it into the workspace",
            resolved.display(),
            self.root.display()
        )
    }

    fn ensure_inside(&self, requested: &str, resolved: PathBuf) -> Result<PathBuf, ToolError> {
        if resolved.starts_with(self.root.as_ref()) {
            Ok(resolved)
        } else {
            Err(ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: self.outside_workspace_reason(&resolved),
            })
        }
    }
}

fn looks_like_literal_path(argument: &str) -> bool {
    !argument.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '^' | '$' | '\'' | '"'
            )
    })
}
