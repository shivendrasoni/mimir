use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
};

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::{MimirError, Result};

type PathLock = Arc<tokio::sync::Mutex<()>>;

static PATH_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

/// Returns a process-wide lock shared by every store that targets the same state file.
pub fn path_lock(path: &Path) -> PathLock {
    let key = std::fs::canonicalize(path)
        .or_else(|_| {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::canonicalize(parent).map(|parent| {
                parent.join(
                    path.file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("state")),
                )
            })
        })
        .unwrap_or_else(|_| path.to_path_buf());
    let locks = PATH_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// Canonicalizes an existing state root so `/var`-style platform aliases are resolved once.
pub fn canonical_state_root(state_root: &Path) -> PathBuf {
    if let Ok(metadata) = std::fs::symlink_metadata(state_root)
        && metadata.file_type().is_symlink()
    {
        return state_root.to_path_buf();
    }
    std::fs::canonicalize(state_root).unwrap_or_else(|_| state_root.to_path_buf())
}

/// Creates parent directories one component at a time and rejects symlink traversal.
pub async fn prepare_state_path(root: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(root) {
        return Err(MimirError::Session {
            path: path.to_owned(),
            message: "state path escapes its configured root".into(),
        });
    }
    let relative = path.strip_prefix(root).map_err(|_| MimirError::Session {
        path: path.to_owned(),
        message: "state path escapes its configured root".into(),
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(MimirError::Session {
            path: path.to_owned(),
            message: "invalid state path component".into(),
        });
    }
    let mut current = root.to_path_buf();
    reject_symlink(&current).await?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        if let Component::Normal(segment) = component {
            current.push(segment);
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(MimirError::Session {
                        path: current,
                        message: "state directory must not be a symlink".into(),
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
                    match tokio::fs::create_dir(&current).await {
                        Ok(()) => {}
                        // Another process may create the exact component after
                        // our metadata read. Re-inspect it rather than treating
                        // the benign race as a state failure.
                        Err(create_error)
                            if create_error.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            let metadata = tokio::fs::symlink_metadata(&current).await?;
                            if metadata.file_type().is_symlink() {
                                return Err(MimirError::Session {
                                    path: current,
                                    message: "state directory must not be a symlink".into(),
                                });
                            }
                            if !metadata.is_dir() {
                                return Err(MimirError::Session {
                                    path: current,
                                    message: "state path component is not a directory".into(),
                                });
                            }
                        }
                        Err(create_error) => return Err(create_error.into()),
                    }
                    reject_symlink(&current).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    reject_symlink(path).await
}

async fn reject_symlink(path: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Session {
            path: path.to_owned(),
            message: "state path must not be a symlink".into(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub async fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| MimirError::Session {
        path: path.to_owned(),
        message: "state path has no parent".into(),
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(&bytes).await?;
    file.write_all(b"\n").await?;
    file.sync_all().await?;
    drop(file);
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

pub async fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn concurrent_preparation_accepts_only_the_real_directory_winner() {
        let root = TempDir::new().expect("root");
        let root_path = root.path().to_owned();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(32));
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let root_path = root_path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            workers.spawn(async move {
                barrier.wait().await;
                let path = root_path.join("projects/example/state.json");
                prepare_state_path(&root_path, &path).await
            });
        }
        while let Some(result) = workers.join_next().await {
            result.expect("worker").expect("concurrent preparation");
        }
        assert!(root.path().join("projects/example").is_dir());
    }
}
