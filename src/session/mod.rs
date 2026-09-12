use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::Message,
};

pub const SESSION_SCHEMA_VERSION: u16 = 1;
const MAX_SESSION_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u16,
    pub record_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub payload: SessionPayload,
}

impl SessionRecord {
    pub fn new(payload: SessionPayload) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            record_id: Uuid::new_v4(),
            parent_id: None,
            created_at: Utc::now(),
            payload,
        }
    }

    #[must_use]
    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.parent_id = Some(parent_id);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionPayload {
    Message(Message),
    Compaction {
        summary: String,
        retained_message_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        #[serde(default)]
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    RuntimeEvent {
        name: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSession {
    pub records: Vec<SessionRecord>,
    pub recovered_incomplete_tail: bool,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn append(&self, record: SessionRecord) -> Result<()>;
    async fn load(&self) -> Result<LoadedSession>;
}

pub struct FileSessionStore {
    root: PathBuf,
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl FileSessionStore {
    /// Creates or opens one append-only session transcript.
    ///
    /// # Errors
    ///
    /// Returns a configuration or I/O error for invalid ids and inaccessible storage.
    pub async fn create(state_root: &std::path::Path, session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        tokio::fs::create_dir_all(state_root).await?;
        let root = crate::atomic::canonical_state_root(state_root);
        let directory = root.join("sessions");
        let path = directory.join(format!("{session_id}.jsonl"));
        crate::atomic::prepare_state_path(&root, &path).await?;
        Ok(Self {
            root,
            write_lock: crate::atomic::path_lock(&path),
            path,
        })
    }

    /// Creates or opens a transcript directly inside an explicit session directory.
    ///
    /// # Errors
    ///
    /// Returns a configuration or I/O error for invalid ids and inaccessible storage.
    pub async fn create_in_directory(
        directory: &std::path::Path,
        session_id: &str,
    ) -> Result<Self> {
        validate_session_id(session_id)?;
        tokio::fs::create_dir_all(directory).await?;
        let root = crate::atomic::canonical_state_root(directory);
        let path = root.join(format!("{session_id}.jsonl"));
        crate::atomic::prepare_state_path(&root, &path).await?;
        Ok(Self {
            root,
            write_lock: crate::atomic::path_lock(&path),
            path,
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Lists durable session ids under the configured state root.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the sessions directory cannot be inspected safely.
    pub async fn list_ids(state_root: &std::path::Path) -> Result<Vec<String>> {
        tokio::fs::create_dir_all(state_root).await?;
        let root = crate::atomic::canonical_state_root(state_root);
        let directory = root.join("sessions");
        crate::atomic::prepare_state_path(&root, &directory.join("placeholder.jsonl")).await?;
        let mut sessions = Vec::new();
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            if stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                sessions.push(stem.to_owned());
            }
        }
        sessions.sort_unstable();
        Ok(sessions)
    }

    /// Lists durable session ids directly inside an explicit session directory.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the directory cannot be inspected safely.
    pub async fn list_ids_in_directory(directory: &std::path::Path) -> Result<Vec<String>> {
        tokio::fs::create_dir_all(directory).await?;
        let root = crate::atomic::canonical_state_root(directory);
        crate::atomic::prepare_state_path(&root, &root.join("placeholder.jsonl")).await?;
        list_session_ids(&root).await
    }

    /// Atomically replaces this transcript while holding its path-scoped lock.
    ///
    /// # Errors
    ///
    /// Returns a persistence or serialization error, or rejects any record
    /// exceeding the same 4 MiB bound used by append operations.
    pub async fn replace_records(&self, records: Vec<SessionRecord>) -> Result<()> {
        for record in &records {
            if serde_json::to_vec(record)?.len() > MAX_SESSION_RECORD_BYTES {
                return Err(MimirError::Session {
                    path: self.path.clone(),
                    message: "session record exceeds the 4 MiB limit".into(),
                });
            }
        }
        let _guard = self.write_lock.lock().await;
        crate::atomic::prepare_state_path(&self.root, &self.path).await?;
        write_records_atomic(&self.path, &records).await
    }
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(MimirError::Configuration(
            "session id must contain only letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(())
}

async fn list_session_ids(directory: &std::path::Path) -> Result<Vec<String>> {
    let mut sessions = Vec::new();
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = entry.metadata().await?;
        if !metadata.is_file()
            || path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl")
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            sessions.push(stem.to_owned());
        }
    }
    sessions.sort_unstable();
    Ok(sessions)
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn append(&self, record: SessionRecord) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        crate::atomic::prepare_state_path(&self.root, &self.path).await?;
        if matches!(record.payload, SessionPayload::Compaction { .. }) {
            let mut loaded = load_records_streaming(&self.path).await?;
            fold_record(&mut loaded.records, record);
            return write_records_atomic(&self.path, &loaded.records).await;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        set_private_permissions(&self.path).await?;
        let mut encoded = serde_json::to_vec(&record)?;
        if encoded.len() > MAX_SESSION_RECORD_BYTES {
            return Err(MimirError::Session {
                path: self.path.clone(),
                message: "session record exceeds the 4 MiB limit".into(),
            });
        }
        encoded.push(b'\n');
        file.write_all(&encoded).await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn load(&self) -> Result<LoadedSession> {
        crate::atomic::prepare_state_path(&self.root, &self.path).await?;
        load_records_streaming(&self.path).await
    }
}

#[derive(Default)]
pub struct InMemorySessionStore {
    records: Arc<Mutex<Vec<SessionRecord>>>,
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn append(&self, record: SessionRecord) -> Result<()> {
        self.records.lock().await.push(record);
        Ok(())
    }

    async fn load(&self) -> Result<LoadedSession> {
        Ok(LoadedSession {
            records: self.records.lock().await.clone(),
            recovered_incomplete_tail: false,
        })
    }
}

async fn load_records_streaming(path: &std::path::Path) -> Result<LoadedSession> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedSession {
                records: Vec::new(),
                recovered_incomplete_tail: false,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata().await?.len();
    let ends_with_newline = if length == 0 {
        true
    } else {
        read_byte_at(path, length - 1)?
    };
    let mut lines = FramedRead::new(
        file,
        LinesCodec::new_with_max_length(MAX_SESSION_RECORD_BYTES),
    );
    let mut records = Vec::new();
    let mut recovered_incomplete_tail = false;
    let mut previous: Option<(usize, String)> = None;
    let mut line_index = 0_usize;
    while let Some(line) = lines.next().await {
        let line = line.map_err(|error| MimirError::Session {
            path: path.to_owned(),
            message: format!("session record exceeds the 4 MiB limit or is invalid UTF-8: {error}"),
        })?;
        if let Some((index, value)) = previous.replace((line_index, line)) {
            parse_loaded_line(path, index, &value, false, &mut records)?;
        }
        line_index = line_index.saturating_add(1);
    }
    if let Some((index, value)) = previous {
        recovered_incomplete_tail =
            parse_loaded_line(path, index, &value, !ends_with_newline, &mut records)?;
    }
    Ok(LoadedSession {
        records,
        recovered_incomplete_tail,
    })
}

fn parse_loaded_line(
    path: &std::path::Path,
    line_index: usize,
    line: &str,
    incomplete_tail: bool,
    records: &mut Vec<SessionRecord>,
) -> Result<bool> {
    if line.trim().is_empty() {
        return Ok(false);
    }
    match serde_json::from_str::<SessionRecord>(line) {
        Ok(record) if record.schema_version == SESSION_SCHEMA_VERSION => {
            fold_record(records, record);
            Ok(false)
        }
        Ok(record) => Err(MimirError::Session {
            path: path.to_owned(),
            message: format!(
                "unsupported schema version {} at line {}",
                record.schema_version,
                line_index + 1
            ),
        }),
        Err(_) if incomplete_tail => Ok(true),
        Err(error) => Err(MimirError::Session {
            path: path.to_owned(),
            message: format!("invalid JSON at line {}: {error}", line_index + 1),
        }),
    }
}

fn fold_record(records: &mut Vec<SessionRecord>, record: SessionRecord) {
    if let SessionPayload::Compaction {
        retained_message_count,
        ..
    } = &record.payload
    {
        let mut retained: Vec<_> = records
            .iter()
            .rev()
            .filter(|entry| matches!(entry.payload, SessionPayload::Message(_)))
            .take(*retained_message_count)
            .cloned()
            .collect();
        retained.reverse();
        records.clear();
        records.push(record);
        records.extend(retained);
    } else {
        records.push(record);
    }
}

async fn write_records_atomic(path: &std::path::Path, records: &[SessionRecord]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| MimirError::Session {
        path: path.to_owned(),
        message: "session path has no parent".into(),
    })?;
    let temporary = parent.join(format!(".session-{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    set_private_permissions(&temporary).await?;
    for record in records {
        let encoded = serde_json::to_vec(record)?;
        if encoded.len() > MAX_SESSION_RECORD_BYTES {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(MimirError::Session {
                path: path.to_owned(),
                message: "session record exceeds the 4 MiB limit".into(),
            });
        }
        file.write_all(&encoded).await?;
        file.write_all(b"\n").await?;
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, path).await?;
    Ok(())
}

fn read_byte_at(path: &std::path::Path, offset: u64) -> Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

#[cfg(unix)]
async fn set_private_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) -> std::future::Ready<Result<()>> {
    std::future::ready(Ok(()))
}
