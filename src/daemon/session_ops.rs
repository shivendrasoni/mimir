//! Bounded durable operations for public daemon session commands.

use std::{
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::{
    model::{Message, Role},
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    session_compat::{ReferenceSessionMetadata, export_jsonl, prepare_switch_session},
    session_tree::{MAX_SESSION_NAME_CHARS, SessionBranchCatalog},
};

use super::{DaemonError, PublicDaemonCommand};

const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPORT_BYTES: usize = 128 * 1024 * 1024;
const MAX_CATALOG_TEXT_CHARS: usize = 1_048_576;
const MAX_FIRST_MESSAGE_CHARS: usize = 4_096;
const MAX_BRANCH_SUMMARY_BYTES: usize = 2 * 1024 * 1024;
const MAX_CUSTOM_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// Executes durable public session operations below the daemon's ownership layer.
#[derive(Debug, Clone)]
pub(crate) struct SessionOps {
    state_root: PathBuf,
}

impl SessionOps {
    pub(crate) fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
        }
    }

    /// Returns `None` for commands owned by another daemon subsystem.
    pub(crate) async fn execute(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Option<Value>, DaemonError> {
        let result = match command.command_type() {
            "list_saved_sessions" => self.list_saved_sessions(command).await?,
            "new_session" => self.new_session(command).await?,
            "switch_session" => self.switch_session(command, "sessionPath").await?,
            "import_jsonl" => self.switch_session(command, "inputPath").await?,
            "fork" => self.fork(command).await?,
            "navigate_tree" => self.execute_navigation(command, None).await?,
            "export_html" => self.export_html(command).await?,
            "export_jsonl" => self.export_jsonl(command).await?,
            "rename_saved_session" => self.rename_saved_session(command).await?,
            "delete_saved_session" => self.delete_saved_session(command).await?,
            "set_session_entry_label" => self.set_session_entry_label(command).await?,
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    async fn list_saved_sessions(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        match optional_string(command, "scope")?.as_deref() {
            None | Some("current" | "all") => {}
            Some(_) => return Err(protocol("scope must be current or all")),
        }
        if let Some(session_dir) = optional_string(command, "sessionDir")?
            && self.canonical_sessions_dir().await?
                != canonical_existing_dir(Path::new(&session_dir)).await?
        {
            return Err(protocol(
                "sessionDir must identify the configured session directory",
            ));
        }
        let ids = FileSessionStore::list_ids(&self.state_root)
            .await
            .map_err(|error| core_error(&error))?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            sessions.push(self.saved_session_info(&id).await?);
        }
        sessions.sort_by(|left, right| {
            right["modified"]
                .as_str()
                .cmp(&left["modified"].as_str())
                .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
        });
        Ok(json!({"sessions": sessions}))
    }

    async fn saved_session_info(&self, session_id: &str) -> Result<Value, DaemonError> {
        let store = self.store(session_id).await?;
        let metadata = bounded_regular_metadata(store.path()).await?;
        let loaded = store.load().await.map_err(|error| core_error(&error))?;
        let catalog = SessionBranchCatalog::from_records(loaded.records.clone())
            .map_err(|error| core_error(&error))?;
        let messages = loaded
            .records
            .iter()
            .filter_map(|record| match &record.payload {
                SessionPayload::Message(message) => Some(message),
                _ => None,
            });
        let mut all_text = String::new();
        let mut first_message = None;
        let mut message_count = 0_usize;
        for message in messages {
            message_count = message_count.saturating_add(1);
            let text = message.text();
            if first_message.is_none() && !text.is_empty() {
                first_message = Some(truncate_chars(&text, MAX_FIRST_MESSAGE_CHARS));
            }
            append_catalog_text(&mut all_text, &text);
        }
        let created = loaded.records.first().map_or_else(
            || system_time(metadata.created().unwrap_or(SystemTime::UNIX_EPOCH)),
            |record| record.created_at,
        );
        let modified = system_time(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let mut value = json!({
            "path": store.path(),
            "id": session_id,
            "cwd": cwd,
            "created": created.to_rfc3339(),
            "modified": modified.to_rfc3339(),
            "messageCount": message_count,
            "firstMessage": first_message.unwrap_or_default(),
            "allMessagesText": all_text,
        });
        if let Some(name) = catalog.session_name() {
            value["name"] = Value::String(name.to_owned());
        }
        Ok(value)
    }

    async fn new_session(&self, command: &PublicDaemonCommand) -> Result<Value, DaemonError> {
        let active = required_string(command, "activeSessionId")?;
        validate_session_id(active)?;
        let parent = optional_string(command, "parentSession")?;
        let session_id = format!("session-{}", Uuid::new_v4().simple());
        let store = self.store(&session_id).await?;
        let detail = parent.unwrap_or_default();
        store
            .append(SessionRecord::new(SessionPayload::RuntimeEvent {
                name: "session_created".into(),
                detail,
            }))
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({"sessionId": session_id, "sessionFile": store.path()}))
    }

    async fn switch_session(
        &self,
        command: &PublicDaemonCommand,
        path_field: &str,
    ) -> Result<Value, DaemonError> {
        validate_session_id(required_string(command, "activeSessionId")?)?;
        let source = PathBuf::from(required_string(command, path_field)?);
        let bytes = read_bounded_regular_file(&source).await?;
        let cwd = optional_string(command, "cwdOverride")?.map(PathBuf::from);
        let plan = prepare_switch_session(&source, &bytes, cwd.as_deref())
            .map_err(|error| core_error(&error))?;
        let store = self.store(&plan.target_session_id).await?;
        let record_count = plan.records.len();
        store
            .replace_records(plan.records)
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({
            "sessionId": plan.target_session_id,
            "sessionFile": store.path(),
            "cwd": plan.cwd,
            "records": record_count,
        }))
    }

    async fn fork(&self, command: &PublicDaemonCommand) -> Result<Value, DaemonError> {
        let source_id = required_string(command, "activeSessionId")?;
        let entry_id = parse_uuid(required_string(command, "entryId")?, "entryId")?;
        let position = optional_string(command, "position")?.unwrap_or_else(|| "before".into());
        let source = self.store(source_id).await?;
        bounded_regular_metadata(source.path()).await?;
        let catalog = SessionBranchCatalog::from_records(
            source
                .load()
                .await
                .map_err(|error| core_error(&error))?
                .records,
        )
        .map_err(|error| core_error(&error))?;
        let derivation = match position.as_str() {
            "before" => catalog.fork_before_user_message(entry_id, source_id),
            "at" => catalog.clone_at(entry_id, source_id),
            _ => return Err(protocol("position must be before or at")),
        }
        .map_err(|error| core_error(&error))?;
        let session_id = format!("session-{}", Uuid::new_v4().simple());
        let store = self.store(&session_id).await?;
        let record_count = derivation.records.len();
        store
            .replace_records(derivation.records)
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({
            "sessionId": session_id,
            "sessionFile": store.path(),
            "records": record_count,
            "selectedText": derivation.selected_text,
        }))
    }

    /// Loads the exact bounded message slice abandoned by a summarized tree
    /// navigation, without mutating the session.
    pub(crate) async fn navigation_summary_messages(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Vec<Message>, DaemonError> {
        if command.field("summarize") != Some(&Value::Bool(true)) {
            return Err(protocol(
                "navigation summary messages require summarize=true",
            ));
        }
        let session_id = required_string(command, "activeSessionId")?;
        let target_id = parse_uuid(required_string(command, "targetId")?, "targetId")?;
        let store = self.store(session_id).await?;
        bounded_regular_metadata(store.path()).await?;
        let catalog = SessionBranchCatalog::from_records(
            store
                .load()
                .await
                .map_err(|error| core_error(&error))?
                .records,
        )
        .map_err(|error| core_error(&error))?;
        catalog
            .abandoned_branch_messages(target_id)
            .map_err(|error| core_error(&error))
    }

    /// Switches the active transcript branch and optionally commits a summary
    /// generated outside the session store as a native compaction checkpoint.
    pub(crate) async fn execute_navigation(
        &self,
        command: &PublicDaemonCommand,
        summary: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let session_id = required_string(command, "activeSessionId")?;
        let target_id = parse_uuid(required_string(command, "targetId")?, "targetId")?;
        let summary_options = validate_navigation_features(command, summary)?;
        let store = self.store(session_id).await?;
        bounded_regular_metadata(store.path()).await?;
        let catalog = SessionBranchCatalog::from_records(
            store
                .load()
                .await
                .map_err(|error| core_error(&error))?
                .records,
        )
        .map_err(|error| core_error(&error))?;
        let mut records = catalog
            .branch_records(target_id)
            .map_err(|error| core_error(&error))?;
        let mut summary_id = None;
        if let Some((summary, options)) = summary.zip(summary_options) {
            let retained_message_count = records
                .iter()
                .filter(|record| matches!(record.payload, SessionPayload::Message(_)))
                .count();
            let checkpoint = SessionRecord::new(SessionPayload::Compaction {
                summary: summary.into(),
                retained_message_count,
                reason: Some("branch_navigation".into()),
                first_kept_entry_id: Some(target_id.to_string()),
                tokens_before: 0,
                custom_instructions: options.custom_instructions,
                details: Some(json!({"replaceInstructions": options.replace_instructions})),
            })
            .with_parent(target_id);
            let checkpoint_id = checkpoint.record_id;
            records.push(checkpoint);
            if let Some(label) = options.label {
                let detail =
                    serde_json::to_string(&json!({"targetId": checkpoint_id, "label": label}))?;
                let label_record = SessionRecord::new(SessionPayload::RuntimeEvent {
                    name: "session_label".into(),
                    detail,
                })
                .with_parent(checkpoint_id);
                records.push(label_record);
            }
            summary_id = Some(checkpoint_id);
        }
        let record_count = records.len();
        store
            .replace_records(records)
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({
            "sessionId": session_id,
            "sessionFile": store.path(),
            "targetId": target_id,
            "records": record_count,
            "cancelled": false,
            "summaryEntryId": summary_id,
        }))
    }

    async fn rename_saved_session(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        let name = required_string(command, "name")?;
        let (session_id, store) = self
            .saved_store(required_string(command, "sessionPath")?)
            .await?;
        let loaded = store.load().await.map_err(|error| core_error(&error))?;
        let catalog = SessionBranchCatalog::from_records(loaded.records)
            .map_err(|error| core_error(&error))?;
        store
            .append(
                catalog
                    .rename_record(name)
                    .map_err(|error| core_error(&error))?,
            )
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({"id": session_id, "path": store.path(), "name": name.trim()}))
    }

    async fn delete_saved_session(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        let (_, store) = self
            .saved_store(required_string(command, "sessionPath")?)
            .await?;
        let path = store.path().to_path_buf();
        let lock = crate::atomic::path_lock(&path);
        let _guard = lock.lock().await;
        bounded_regular_metadata(&path).await?;
        let staged = path.with_file_name(format!(".deleted-{}.jsonl", Uuid::new_v4().simple()));
        tokio::fs::rename(&path, &staged).await?;
        if let Err(error) = tokio::fs::remove_file(&staged).await {
            let _ = tokio::fs::rename(&staged, &path).await;
            return Err(error.into());
        }
        Ok(json!({"deleted": true, "path": path}))
    }

    async fn set_session_entry_label(
        &self,
        command: &PublicDaemonCommand,
    ) -> Result<Value, DaemonError> {
        let session_id = required_string(command, "activeSessionId")?;
        let entry_id = parse_uuid(required_string(command, "entryId")?, "entryId")?;
        let label = optional_string(command, "label")?;
        if let Some(label) = &label {
            validate_label(label)?;
        }
        let store = self.store(session_id).await?;
        bounded_regular_metadata(store.path()).await?;
        let catalog = SessionBranchCatalog::from_records(
            store
                .load()
                .await
                .map_err(|error| core_error(&error))?
                .records,
        )
        .map_err(|error| core_error(&error))?;
        if catalog.node(entry_id).is_none() {
            return Err(protocol(format!("entry {entry_id} was not found")));
        }
        let detail = serde_json::to_string(&json!({"targetId": entry_id, "label": label}))?;
        let mut record = SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_label".into(),
            detail,
        });
        record.parent_id = catalog.leaf_id();
        store
            .append(record)
            .await
            .map_err(|error| core_error(&error))?;
        Ok(json!({"entryId": entry_id, "label": label}))
    }

    async fn export_jsonl(&self, command: &PublicDaemonCommand) -> Result<Value, DaemonError> {
        let session_id = required_string(command, "activeSessionId")?;
        let store = self.store(session_id).await?;
        bounded_regular_metadata(store.path()).await?;
        let records = store
            .load()
            .await
            .map_err(|error| core_error(&error))?
            .records;
        let timestamp = records
            .first()
            .map_or_else(Utc::now, |record| record.created_at);
        let cwd = std::env::current_dir()?;
        let output = self.export_path(command, session_id, "jsonl")?;
        let text = export_jsonl(
            &records,
            &ReferenceSessionMetadata {
                session_id: session_id.into(),
                timestamp,
                cwd,
                parent_session: None,
            },
        )
        .map_err(|error| core_error(&error))?;
        write_atomic_bounded(&output, text.as_bytes()).await?;
        Ok(json!({"path": output}))
    }

    async fn export_html(&self, command: &PublicDaemonCommand) -> Result<Value, DaemonError> {
        let session_id = required_string(command, "activeSessionId")?;
        let store = self.store(session_id).await?;
        bounded_regular_metadata(store.path()).await?;
        let records = store
            .load()
            .await
            .map_err(|error| core_error(&error))?
            .records;
        let html = render_html(records.iter().filter_map(|record| match &record.payload {
            SessionPayload::Message(message) => Some(message),
            _ => None,
        }))?;
        let output = self.export_path(command, session_id, "html")?;
        write_atomic_bounded(&output, html.as_bytes()).await?;
        Ok(json!({"path": output}))
    }

    fn export_path(
        &self,
        command: &PublicDaemonCommand,
        session_id: &str,
        extension: &str,
    ) -> Result<PathBuf, DaemonError> {
        let requested = optional_string(command, "outputPath")?.map(PathBuf::from);
        let path = if let Some(path) = requested {
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            }
        } else {
            self.state_root
                .join("exports")
                .join(format!("{session_id}.{extension}"))
        };
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some(extension) {
            return Err(protocol(format!("export path must end in .{extension}")));
        }
        Ok(path)
    }

    async fn saved_store(
        &self,
        requested: &str,
    ) -> Result<(String, FileSessionStore), DaemonError> {
        let root = self.canonical_sessions_dir().await?;
        let requested = PathBuf::from(requested);
        if requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(protocol("sessionPath must not contain parent traversal"));
        }
        let candidate = if requested.is_absolute() {
            requested
        } else {
            root.join(requested)
        };
        let metadata = bounded_regular_metadata(&candidate).await?;
        if metadata.file_type().is_symlink() {
            return Err(protocol("sessionPath must not be a symlink"));
        }
        let canonical = tokio::fs::canonicalize(&candidate).await?;
        if canonical.parent() != Some(root.as_path())
            || canonical.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl")
        {
            return Err(protocol(
                "sessionPath must be a JSONL file in the configured session directory",
            ));
        }
        let session_id = canonical
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| protocol("sessionPath has no valid session id"))?
            .to_owned();
        validate_session_id(&session_id)?;
        let store = self.store(&session_id).await?;
        if tokio::fs::canonicalize(store.path()).await? != canonical {
            return Err(protocol("sessionPath does not match its session id"));
        }
        Ok((session_id, store))
    }

    async fn canonical_sessions_dir(&self) -> Result<PathBuf, DaemonError> {
        tokio::fs::create_dir_all(&self.state_root).await?;
        let root = crate::atomic::canonical_state_root(&self.state_root);
        let sessions = root.join("sessions");
        crate::atomic::prepare_state_path(&root, &sessions.join("placeholder.jsonl"))
            .await
            .map_err(|error| core_error(&error))?;
        canonical_existing_dir(&sessions).await
    }

    async fn store(&self, session_id: &str) -> Result<FileSessionStore, DaemonError> {
        validate_session_id(session_id)?;
        FileSessionStore::create(&self.state_root, session_id)
            .await
            .map_err(|error| core_error(&error))
    }
}

fn required_string<'a>(
    command: &'a PublicDaemonCommand,
    field: &str,
) -> Result<&'a str, DaemonError> {
    command
        .field(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| protocol(format!("{field} must be a non-empty string")))
}

fn optional_string(
    command: &PublicDaemonCommand,
    field: &str,
) -> Result<Option<String>, DaemonError> {
    match command.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(protocol(format!("{field} must be a string"))),
    }
}

fn validate_session_id(value: &str) -> Result<(), DaemonError> {
    if value.is_empty()
        || value.len() > 1024
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(protocol("activeSessionId contains invalid characters"));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), DaemonError> {
    if label.chars().count() > MAX_SESSION_NAME_CHARS || label.chars().any(char::is_control) {
        return Err(protocol(
            "label is invalid or exceeds the 256 character limit",
        ));
    }
    Ok(())
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, DaemonError> {
    Uuid::parse_str(value).map_err(|_| protocol(format!("{field} must be a UUID")))
}

struct NavigationSummaryOptions {
    custom_instructions: Option<String>,
    replace_instructions: bool,
    label: Option<String>,
}

fn validate_navigation_features(
    command: &PublicDaemonCommand,
    summary: Option<&str>,
) -> Result<Option<NavigationSummaryOptions>, DaemonError> {
    if summary.is_none() {
        if command
            .field("summarize")
            .is_some_and(|value| value != &Value::Bool(false))
            || command.field("customInstructions").is_some()
            || command.field("replaceInstructions").is_some()
            || command.field("label").is_some()
        {
            return Err(protocol(
                "summarized or labelled navigation requires a generated branch summary",
            ));
        }
        return Ok(None);
    }
    match command.field("summarize") {
        Some(Value::Bool(true)) => {}
        _ => return Err(protocol("summarize must be true when committing a summary")),
    }
    let summary = summary.expect("checked above");
    if summary.trim().is_empty() || summary.len() > MAX_BRANCH_SUMMARY_BYTES {
        return Err(protocol(
            "branch summary must be non-empty and at most 2 MiB",
        ));
    }
    let custom_instructions = optional_string(command, "customInstructions")?;
    if custom_instructions
        .as_ref()
        .is_some_and(|instructions| instructions.len() > MAX_CUSTOM_INSTRUCTIONS_BYTES)
    {
        return Err(protocol("customInstructions exceeds the 16 KiB limit"));
    }
    let replace_instructions = match command.field("replaceInstructions") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err(protocol("replaceInstructions must be a boolean")),
    };
    let label = optional_string(command, "label")?;
    if let Some(label) = &label {
        validate_label(label)?;
    }
    Ok(Some(NavigationSummaryOptions {
        custom_instructions,
        replace_instructions,
        label,
    }))
}

async fn read_bounded_regular_file(path: &Path) -> Result<Vec<u8>, DaemonError> {
    bounded_regular_metadata(path).await?;
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() > usize::try_from(MAX_SESSION_BYTES).unwrap_or(usize::MAX) {
        return Err(protocol("session exceeds the 64 MiB limit"));
    }
    Ok(bytes)
}

async fn bounded_regular_metadata(path: &Path) -> Result<std::fs::Metadata, DaemonError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(protocol(format!(
            "{} must be a regular non-symlink file",
            path.display()
        )));
    }
    if metadata.len() > MAX_SESSION_BYTES {
        return Err(protocol("session exceeds the 64 MiB limit"));
    }
    Ok(metadata)
}

async fn canonical_existing_dir(path: &Path) -> Result<PathBuf, DaemonError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(protocol(format!(
            "{} must be a regular directory",
            path.display()
        )));
    }
    Ok(tokio::fs::canonicalize(path).await?)
}

async fn write_atomic_bounded(path: &Path, bytes: &[u8]) -> Result<(), DaemonError> {
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(protocol("session export exceeds the 128 MiB limit"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| protocol("export path has no parent"))?;
    tokio::fs::create_dir_all(parent).await?;
    let parent = canonical_existing_dir(parent).await?;
    let file_name = path
        .file_name()
        .ok_or_else(|| protocol("export path has no file name"))?;
    let target = parent.join(file_name);
    if let Ok(metadata) = tokio::fs::symlink_metadata(&target).await
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(protocol("export target must be a regular non-symlink file"));
    }
    let temporary = parent.join(format!(".export-{}.tmp", Uuid::new_v4().simple()));
    let mut file = tokio::fs::File::create(&temporary).await?;
    if let Err(error) = async {
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, &target).await
    }
    .await
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

fn render_html<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Result<String, DaemonError> {
    let mut output = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Mimir Session</title><style>body{font:15px system-ui;max-width:900px;margin:2rem auto;padding:0 1rem}article{border:1px solid #d4d4d8;border-radius:8px;padding:1rem;margin:1rem 0}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style></head><body><h1>Mimir Session</h1>",
    );
    for message in messages {
        output.push_str("<article><h2>");
        push_html_escaped(
            &mut output,
            match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            },
        );
        output.push_str("</h2><pre>");
        push_html_escaped(&mut output, &message.text());
        output.push_str("</pre></article>");
        if output.len() > MAX_EXPORT_BYTES {
            return Err(protocol("session HTML export exceeds the 128 MiB limit"));
        }
    }
    output.push_str("</body></html>\n");
    Ok(output)
}

fn push_html_escaped(output: &mut String, input: &str) {
    for character in input.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
}

fn append_catalog_text(output: &mut String, text: &str) {
    if !output.is_empty() && output.chars().count() < MAX_CATALOG_TEXT_CHARS {
        output.push('\n');
    }
    let remaining = MAX_CATALOG_TEXT_CHARS.saturating_sub(output.chars().count());
    output.extend(text.chars().take(remaining));
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn system_time(value: SystemTime) -> DateTime<Utc> {
    value.into()
}

fn core_error(error: &crate::error::MimirError) -> DaemonError {
    DaemonError::Protocol(error.to_string())
}

fn protocol(message: impl Into<String>) -> DaemonError {
    DaemonError::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn command(kind: &str, fields: &[(&str, Value)]) -> PublicDaemonCommand {
        PublicDaemonCommand::new(
            kind,
            fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone())),
        )
        .expect("command")
    }

    async fn seeded_session(state: &Path, id: &str) -> (FileSessionStore, Uuid) {
        let store = FileSessionStore::create(state, id).await.expect("store");
        let user = SessionRecord::new(SessionPayload::Message(Message::user("hello <world>")));
        let record_id = user.record_id;
        store.append(user).await.expect("user");
        store
            .append(SessionRecord::new(SessionPayload::Message(
                Message::assistant(
                    vec![crate::model::Content::Text {
                        text: "answer & more".into(),
                    }],
                    crate::model::StopReason::Stop,
                ),
            )))
            .await
            .expect("assistant");
        (store, record_id)
    }

    #[tokio::test]
    async fn saved_session_catalog_rename_exports_and_delete_are_real_and_bounded() {
        let state = TempDir::new().expect("state");
        let (store, _) = seeded_session(state.path(), "alpha").await;
        let ops = SessionOps::new(state.path());
        let listed = ops
            .execute(&command("list_saved_sessions", &[("scope", json!("all"))]))
            .await
            .expect("list")
            .expect("handled");
        assert_eq!(listed["sessions"][0]["id"], "alpha");
        assert_eq!(listed["sessions"][0]["messageCount"], 2);

        ops.execute(&command(
            "rename_saved_session",
            &[
                ("sessionPath", json!(store.path())),
                ("name", json!("Named session")),
            ],
        ))
        .await
        .expect("rename")
        .expect("handled");
        let renamed = ops
            .execute(&command("list_saved_sessions", &[]))
            .await
            .expect("list")
            .expect("handled");
        assert_eq!(renamed["sessions"][0]["name"], "Named session");

        let html = state.path().join("transcript.html");
        let jsonl = state.path().join("transcript.jsonl");
        ops.execute(&command(
            "export_html",
            &[
                ("activeSessionId", json!("alpha")),
                ("outputPath", json!(html)),
            ],
        ))
        .await
        .expect("html");
        ops.execute(&command(
            "export_jsonl",
            &[
                ("activeSessionId", json!("alpha")),
                ("outputPath", json!(jsonl)),
            ],
        ))
        .await
        .expect("jsonl");
        let rendered = tokio::fs::read_to_string(&html).await.expect("rendered");
        assert!(rendered.contains("hello &lt;world&gt;"));
        assert!(rendered.contains("answer &amp; more"));
        assert!(
            tokio::fs::read_to_string(&jsonl)
                .await
                .expect("export")
                .starts_with("{\"cwd\":")
        );

        ops.execute(&command(
            "delete_saved_session",
            &[("sessionPath", json!(store.path()))],
        ))
        .await
        .expect("delete")
        .expect("handled");
        assert!(!tokio::fs::try_exists(store.path()).await.expect("exists"));
    }

    #[tokio::test]
    async fn fork_navigation_labels_new_and_import_use_native_session_primitives() {
        let state = TempDir::new().expect("state");
        let (source, user_id) = seeded_session(state.path(), "source").await;
        let ops = SessionOps::new(state.path());

        let forked = ops
            .execute(&command(
                "fork",
                &[
                    ("activeSessionId", json!("source")),
                    ("entryId", json!(user_id)),
                ],
            ))
            .await
            .expect("fork")
            .expect("handled");
        let fork_id = forked["sessionId"].as_str().expect("fork id");
        assert_eq!(forked["selectedText"], "hello <world>");
        assert!(
            tokio::fs::try_exists(ops.store(fork_id).await.expect("fork store").path())
                .await
                .expect("exists")
        );

        ops.execute(&command(
            "set_session_entry_label",
            &[
                ("activeSessionId", json!("source")),
                ("entryId", json!(user_id)),
                ("label", json!("bookmark")),
            ],
        ))
        .await
        .expect("label");
        let labeled = source.load().await.expect("load").records;
        assert!(labeled.iter().any(|record| matches!(&record.payload,
            SessionPayload::RuntimeEvent { name, detail } if name == "session_label" && detail.contains("bookmark"))));

        ops.execute(&command(
            "navigate_tree",
            &[
                ("activeSessionId", json!("source")),
                ("targetId", json!(user_id)),
            ],
        ))
        .await
        .expect("navigate");
        assert_eq!(source.load().await.expect("load").records.len(), 1);

        let fresh = ops
            .execute(&command(
                "new_session",
                &[("activeSessionId", json!("source"))],
            ))
            .await
            .expect("new")
            .expect("handled");
        assert!(
            fresh["sessionId"]
                .as_str()
                .is_some_and(|id| id.starts_with("session-"))
        );

        let imported = ops
            .execute(&command(
                "import_jsonl",
                &[
                    ("activeSessionId", json!("source")),
                    ("inputPath", json!(source.path())),
                    ("cwdOverride", json!(state.path())),
                ],
            ))
            .await
            .expect("import")
            .expect("handled");
        assert_eq!(imported["records"], 1);
    }

    #[tokio::test]
    async fn summarized_navigation_commits_native_checkpoint_and_label_atomically() {
        let state = TempDir::new().expect("state");
        let (store, user_id) = seeded_session(state.path(), "summarized").await;
        let ops = SessionOps::new(state.path());
        let navigate = command(
            "navigate_tree",
            &[
                ("activeSessionId", json!("summarized")),
                ("targetId", json!(user_id)),
                ("summarize", json!(true)),
                ("customInstructions", json!("focus on parity")),
                ("replaceInstructions", json!(true)),
                ("label", json!("previous branch")),
            ],
        );

        assert!(ops.execute(&navigate).await.is_err());
        let result = ops
            .execute_navigation(&navigate, Some("durable branch summary"))
            .await
            .expect("summarized navigation");
        let summary_id =
            Uuid::parse_str(result["summaryEntryId"].as_str().expect("summary entry id"))
                .expect("summary UUID");
        let records = store.load().await.expect("load").records;
        assert_eq!(records.len(), 3);
        let checkpoint = records
            .iter()
            .find(|record| record.record_id == summary_id)
            .expect("summary checkpoint");
        assert!(matches!(
            &checkpoint.payload,
            SessionPayload::Compaction {
                summary,
                reason: Some(reason),
                custom_instructions: Some(instructions),
                details: Some(details),
                ..
            } if summary == "durable branch summary"
                && reason == "branch_navigation"
                && instructions == "focus on parity"
                && details["replaceInstructions"] == true
        ));
        assert_eq!(checkpoint.parent_id, Some(user_id));
        let label_record = records
            .iter()
            .find(|record| record.parent_id == Some(summary_id))
            .expect("label record");
        assert!(matches!(
            &label_record.payload,
            SessionPayload::RuntimeEvent { name, detail }
                if name == "session_label" && detail.contains("previous branch")
        ));
    }

    #[tokio::test]
    async fn navigation_summary_messages_are_exactly_the_abandoned_branch_slice() {
        let state = TempDir::new().expect("state");
        let store = FileSessionStore::create(state.path(), "branched")
            .await
            .expect("store");
        let root = SessionRecord::new(SessionPayload::Message(Message::user("shared root")));
        let shared = SessionRecord::new(SessionPayload::Message(Message::assistant(
            vec![crate::model::Content::Text {
                text: "shared answer".into(),
            }],
            crate::model::StopReason::Stop,
        )))
        .with_parent(root.record_id);
        let target = SessionRecord::new(SessionPayload::Message(Message::user("target sibling")))
            .with_parent(shared.record_id);
        let abandoned_user =
            SessionRecord::new(SessionPayload::Message(Message::user("abandoned request")))
                .with_parent(shared.record_id);
        let abandoned_assistant = SessionRecord::new(SessionPayload::Message(Message::assistant(
            vec![crate::model::Content::Text {
                text: "abandoned answer".into(),
            }],
            crate::model::StopReason::Stop,
        )))
        .with_parent(abandoned_user.record_id);
        let target_id = target.record_id;
        store
            .replace_records(vec![
                root,
                shared,
                target,
                abandoned_user,
                abandoned_assistant,
            ])
            .await
            .expect("branched records");
        let ops = SessionOps::new(state.path());
        let navigate = command(
            "navigate_tree",
            &[
                ("activeSessionId", json!("branched")),
                ("targetId", json!(target_id)),
                ("summarize", json!(true)),
            ],
        );

        let messages = ops
            .navigation_summary_messages(&navigate)
            .await
            .expect("summary messages");
        assert_eq!(
            messages.iter().map(Message::text).collect::<Vec<_>>(),
            vec!["abandoned request", "abandoned answer"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_session_mutations_reject_symlinks_and_path_escape() {
        use std::os::unix::fs::symlink;

        let state = TempDir::new().expect("state");
        let outside = TempDir::new().expect("outside");
        let (store, _) = seeded_session(state.path(), "safe").await;
        let outside_file = outside.path().join("outside.jsonl");
        tokio::fs::copy(store.path(), &outside_file)
            .await
            .expect("copy");
        let link = state.path().join("sessions/link.jsonl");
        symlink(&outside_file, &link).expect("symlink");
        let ops = SessionOps::new(state.path());

        for path in [&outside_file, &link] {
            let error = ops
                .execute(&command(
                    "delete_saved_session",
                    &[("sessionPath", json!(path))],
                ))
                .await
                .expect_err("reject unsafe path");
            assert!(
                error.to_string().contains("sessionPath")
                    || error.to_string().contains("non-symlink")
            );
        }
        assert!(
            tokio::fs::try_exists(&outside_file)
                .await
                .expect("outside remains")
        );
    }
}
