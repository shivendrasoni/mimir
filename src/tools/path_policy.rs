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
        let path = Path::new(requested);
        let unsafe_component = path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if unsafe_component || requested.is_empty() {
            return Err(ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: "path must be a non-empty relative path without parent traversal".into(),
            });
        }
        Ok(self.root.join(path))
    }

    fn ensure_inside(&self, requested: &str, resolved: PathBuf) -> Result<PathBuf, ToolError> {
        if resolved.starts_with(self.root.as_ref()) {
            Ok(resolved)
        } else {
            Err(ToolError::WorkspaceDenied {
                path: requested.into(),
                reason: "canonical target escapes the workspace".into(),
            })
        }
    }
}
