use std::path::Path;

use crate::{
    error::{MimirError, Result},
    migration::{ArtifactKind, SessionReport},
    session_compat::{SessionFormat, import_jsonl},
};

use super::safe_fs::{list_regular_files, read_optional_bytes};
use super::types::PreparedArtifact;

pub(crate) async fn import_sessions(
    legacy_root: &Path,
) -> Result<Vec<(PreparedArtifact, SessionReport)>> {
    let mut imported = Vec::new();
    for path in list_regular_files(legacy_root, "sessions", "jsonl").await? {
        let session_id = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| MimirError::Session {
                path: path.clone(),
                message: "session filename must be valid UTF-8".into(),
            })?;
        if !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(MimirError::Configuration(format!(
                "invalid legacy session id {session_id}"
            )));
        }
        let relative = format!("sessions/{session_id}.jsonl");
        let bytes = read_optional_bytes(legacy_root, &relative)
            .await?
            .ok_or_else(|| MimirError::Session {
                path: path.clone(),
                message: "session file disappeared during migration planning".into(),
            })?;
        let translated = import_jsonl(&path, &bytes)?;
        let source_version = match translated.format {
            SessionFormat::Reference(version) | SessionFormat::Rust(version) => version,
        };
        let mut encoded = Vec::new();
        for record in &translated.records {
            encoded.extend(serde_json::to_vec(record)?);
            encoded.push(b'\n');
        }
        imported.push((
            PreparedArtifact {
                kind: ArtifactKind::Session,
                source: relative.clone(),
                target: relative,
                summary: format!("migrate session {session_id}"),
                bytes: encoded,
            },
            SessionReport {
                session_id: session_id.to_owned(),
                records: translated.records.len(),
                source_version,
            },
        ));
    }
    Ok(imported)
}
