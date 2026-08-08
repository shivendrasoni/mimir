use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::{MimirError, Result};

const MAX_SOURCE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_RESOURCE_FILE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_RESOURCE_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_RESOURCE_FILES: usize = 512;
const MAX_DIRECTORY_ENTRIES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFile {
    pub relative: String,
    pub bytes: Vec<u8>,
}

pub(crate) async fn canonical_legacy_root(path: &Path) -> Result<PathBuf> {
    reject_symlink(path).await?;
    let canonical = tokio::fs::canonicalize(path).await?;
    ensure_directory(&canonical).await?;
    Ok(canonical)
}

pub(crate) async fn canonical_state_root(path: &Path) -> Result<PathBuf> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MimirError::Session {
                path: path.to_path_buf(),
                message: "state root must not be a symlink".into(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path).await?;
        }
        Err(error) => return Err(error.into()),
    }
    let canonical = tokio::fs::canonicalize(path).await?;
    ensure_directory(&canonical).await?;
    Ok(canonical)
}

pub(crate) fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative.is_empty()
        || relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(MimirError::Session {
            path: root.join(relative_path),
            message: "invalid relative path".into(),
        });
    }
    Ok(root.join(relative_path))
}

pub(crate) async fn read_optional_bytes(root: &Path, relative: &str) -> Result<Option<Vec<u8>>> {
    let path = safe_join(root, relative)?;
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Session {
            path,
            message: "migration source must not be a symlink".into(),
        }),
        Ok(metadata) if metadata.is_file() => {
            enforce_size_limit(&path, metadata.len(), MAX_SOURCE_FILE_BYTES)?;
            Ok(Some(tokio::fs::read(path).await?))
        }
        Ok(metadata) if metadata.is_dir() => Err(MimirError::Session {
            path,
            message: "expected a file but found a directory".into(),
        }),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn read_path_bounded(path: &Path) -> Result<Vec<u8>> {
    reject_symlink(path).await?;
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() {
        return Err(MimirError::Session {
            path: path.to_path_buf(),
            message: "expected a regular file".into(),
        });
    }
    enforce_size_limit(path, metadata.len(), MAX_SOURCE_FILE_BYTES)?;
    Ok(tokio::fs::read(path).await?)
}

pub(crate) async fn canonical_journal_path(path: &Path) -> Result<PathBuf> {
    reject_symlink(path).await?;
    let canonical = tokio::fs::canonicalize(path).await?;
    let metadata = tokio::fs::metadata(&canonical).await?;
    if !metadata.is_file() {
        return Err(MimirError::Session {
            path: canonical,
            message: "migration journal must be a regular file".into(),
        });
    }
    Ok(canonical)
}

pub(crate) fn validate_journal_location(state_root: &Path, journal_path: &Path) -> Result<()> {
    let relative = journal_path
        .strip_prefix(state_root)
        .map_err(|_| MimirError::Session {
            path: journal_path.to_path_buf(),
            message: "journal path must live under the state root".into(),
        })?;
    let components = relative.components().collect::<Vec<_>>();
    let valid = components.len() == 3
        && components[0].as_os_str() == "migration"
        && components[1].as_os_str() == "journals"
        && Path::new(components[2].as_os_str()).extension() == Some(OsStr::new("json"));
    if !valid {
        return Err(MimirError::Session {
            path: journal_path.to_path_buf(),
            message: "journal path must live in migration/journals".into(),
        });
    }
    Ok(())
}

pub(crate) async fn collect_resource_files(root: &Path, relative: &str) -> Result<Vec<SourceFile>> {
    let start = safe_join(root, relative)?;
    match tokio::fs::symlink_metadata(&start).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MimirError::Session {
                path: start,
                message: "migration resource directory must not be a symlink".into(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(MimirError::Session {
                path: start,
                message: "migration resource path must be a directory".into(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }

    let mut pending = vec![start];
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            if metadata.file_type().is_symlink() {
                return Err(MimirError::Session {
                    path,
                    message: "migration resource entry must not be a symlink".into(),
                });
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(MimirError::Session {
                    path,
                    message: "migration resource entry must be a regular file".into(),
                });
            }
            enforce_size_limit(&path, metadata.len(), MAX_RESOURCE_FILE_BYTES)?;
            if files.len() >= MAX_RESOURCE_FILES {
                return Err(MimirError::Configuration(format!(
                    "migration resource archive exceeds file limit of {MAX_RESOURCE_FILES}"
                )));
            }
            let bytes = tokio::fs::read(&path).await?;
            total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
                MimirError::Configuration("migration resource archive size overflow".into())
            })?;
            if total_bytes > MAX_RESOURCE_ARCHIVE_BYTES {
                return Err(MimirError::Configuration(format!(
                    "migration resource archive exceeds size limit of {MAX_RESOURCE_ARCHIVE_BYTES} bytes"
                )));
            }
            let relative_path = path.strip_prefix(root).map_err(|_| MimirError::Session {
                path: path.clone(),
                message: "migration resource escaped the legacy root".into(),
            })?;
            let relative = relative_path.to_str().ok_or_else(|| MimirError::Session {
                path: path.clone(),
                message: "migration resource path must be valid UTF-8".into(),
            })?;
            files.push(SourceFile {
                relative: relative.replace(std::path::MAIN_SEPARATOR, "/"),
                bytes,
            });
        }
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

pub(crate) async fn list_regular_files(
    root: &Path,
    relative: &str,
    extension: &str,
) -> Result<Vec<PathBuf>> {
    let directory = safe_join(root, relative)?;
    match tokio::fs::symlink_metadata(&directory).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MimirError::Session {
                path: directory,
                message: "migration source directory must not be a symlink".into(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(MimirError::Session {
                path: directory,
                message: "expected a directory".into(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let mut entries = tokio::fs::read_dir(&directory).await?;
    let mut files = Vec::new();
    let mut scanned = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        scanned += 1;
        if scanned > MAX_DIRECTORY_ENTRIES {
            return Err(MimirError::Configuration(format!(
                "migration source directory exceeds entry limit of {MAX_DIRECTORY_ENTRIES}"
            )));
        }
        let path = entry.path();
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if metadata.file_type().is_symlink() {
            return Err(MimirError::Session {
                path,
                message: "migration source entry must not be a symlink".into(),
            });
        }
        if metadata.is_file() && path.extension() == Some(OsStr::new(extension)) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) async fn write_bytes_atomic(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let path = safe_join(root, relative)?;
    let parent = path.parent().ok_or_else(|| MimirError::Session {
        path: path.clone(),
        message: "target path has no parent".into(),
    })?;
    create_dir_tree(parent).await?;
    reject_symlink(&path).await?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    set_private_if_sensitive(&temporary, relative).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, &path).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_private_if_sensitive(path: &Path, _relative: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_if_sensitive(_path: &Path, _relative: &str) -> Result<()> {
    Ok(())
}

pub(crate) async fn write_json_atomic<T: Serialize>(
    root: &Path,
    relative: &str,
    value: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_bytes_atomic(root, relative, &bytes).await
}

pub(crate) async fn remove_file_if_exists(root: &Path, relative: &str) -> Result<()> {
    let path = safe_join(root, relative)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn copy_file(root: &Path, from: &Path, to_relative: &str) -> Result<()> {
    let bytes = read_path_bounded(from).await?;
    write_bytes_atomic(root, to_relative, &bytes).await
}

pub(crate) async fn target_action(
    root: &Path,
    relative: &str,
    desired_hash: &str,
) -> Result<crate::migration::MigrationAction> {
    let path = safe_join(root, relative)?;
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Session {
            path,
            message: "state target must not be a symlink".into(),
        }),
        Ok(metadata) if metadata.is_file() => {
            enforce_size_limit(&path, metadata.len(), MAX_SOURCE_FILE_BYTES)?;
            let current = tokio::fs::read(path).await?;
            Ok(if sha256_hex(&current) == desired_hash {
                crate::migration::MigrationAction::Unchanged
            } else {
                crate::migration::MigrationAction::Replace
            })
        }
        Ok(_) => Ok(crate::migration::MigrationAction::Replace),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(crate::migration::MigrationAction::Create)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

async fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = tokio::fs::metadata(path).await?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(MimirError::Session {
            path: path.to_path_buf(),
            message: "expected a directory".into(),
        })
    }
}

async fn reject_symlink(path: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Session {
            path: path.to_path_buf(),
            message: "symlinks are not allowed".into(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn create_dir_tree(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() {
            continue;
        }
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(MimirError::Session {
                    path: current,
                    message: "state path component must not be a symlink".into(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(MimirError::Session {
                    path: current,
                    message: "state path component is not a directory".into(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current).await?;
                set_private_directory(&current).await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn enforce_size_limit(path: &Path, actual: u64, limit: u64) -> Result<()> {
    if actual > limit {
        return Err(MimirError::Session {
            path: path.to_path_buf(),
            message: format!("migration input exceeds size limit of {limit} bytes"),
        });
    }
    Ok(())
}
