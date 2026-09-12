#![allow(
    clippy::missing_errors_doc,
    reason = "daemon persistence errors are exhaustively represented by DaemonError"
)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::protocol::LeaseView;
use super::server::DaemonError;

const MAX_SESSION_HISTORY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonMetadataSnapshot {
    pub schema_version: u16,
    pub server_name: String,
    pub socket_path: String,
    pub launch_count: u64,
    pub last_started_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub last_stopped_at_ms: Option<u64>,
    pub sessions: BTreeMap<String, SessionCatalogEntry>,
}

impl Default for DaemonMetadataSnapshot {
    fn default() -> Self {
        Self {
            schema_version: 1,
            server_name: String::new(),
            socket_path: String::new(),
            launch_count: 0,
            last_started_at_ms: 0,
            last_seen_at_ms: 0,
            last_stopped_at_ms: None,
            sessions: BTreeMap::new(),
        }
    }
}

impl DaemonMetadataSnapshot {
    #[must_use]
    pub fn active_sessions(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| !session.active_leases.is_empty())
            .count()
    }

    #[must_use]
    pub fn active_leases(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.active_leases.len())
            .sum()
    }

    pub fn expire_leases(&mut self, now_ms: u64) {
        for session in self.sessions.values_mut() {
            let before = session.active_leases.len();
            session
                .active_leases
                .retain(|lease| lease.expires_at_ms > now_ms);
            if before != session.active_leases.len() {
                session.last_detached_at_ms = Some(now_ms);
            }
        }
        self.prune_inactive_history();
    }

    pub fn session_mut(&mut self, session_id: &str, now_ms: u64) -> &mut SessionCatalogEntry {
        self.prune_inactive_history();
        self.sessions
            .entry(session_id.into())
            .or_insert_with(|| SessionCatalogEntry {
                session_id: session_id.into(),
                name: None,
                session_path: None,
                lifecycle: "resident".into(),
                owner_client_id: None,
                message_count: 0,
                created_at_ms: now_ms,
                last_attached_at_ms: None,
                last_detached_at_ms: None,
                last_prompt_at_ms: None,
                active_leases: Vec::new(),
            })
    }

    fn prune_inactive_history(&mut self) {
        let excess = self.sessions.len().saturating_sub(MAX_SESSION_HISTORY);
        if excess == 0 {
            return;
        }
        let mut inactive: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.active_leases.is_empty())
            .map(|(id, session)| {
                let last_activity = session
                    .last_prompt_at_ms
                    .or(session.last_detached_at_ms)
                    .or(session.last_attached_at_ms)
                    .unwrap_or(session.created_at_ms);
                (last_activity, id.clone())
            })
            .collect();
        inactive.sort_unstable();
        for (_, id) in inactive.into_iter().take(excess) {
            self.sessions.remove(&id);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogEntry {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    #[serde(default = "default_session_lifecycle")]
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_client_id: Option<String>,
    #[serde(default)]
    pub message_count: usize,
    pub created_at_ms: u64,
    pub last_attached_at_ms: Option<u64>,
    pub last_detached_at_ms: Option<u64>,
    pub last_prompt_at_ms: Option<u64>,
    pub active_leases: Vec<LeaseView>,
}

fn default_session_lifecycle() -> String {
    "resident".into()
}

#[derive(Debug, Clone)]
pub struct DaemonStateStore {
    root: PathBuf,
    metadata_path: PathBuf,
}

impl DaemonStateStore {
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        let root = state_root.as_ref().join("daemon");
        let metadata_path = root.join("metadata.json");
        Self {
            root,
            metadata_path,
        }
    }

    #[must_use]
    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    pub async fn load(&self) -> Result<DaemonMetadataSnapshot, DaemonError> {
        match tokio::fs::read(&self.metadata_path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(DaemonMetadataSnapshot::default())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn save(&self, snapshot: &DaemonMetadataSnapshot) -> Result<(), DaemonError> {
        tokio::fs::create_dir_all(&self.root).await?;
        let temporary = self.root.join(format!(".metadata-{}.tmp", Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(snapshot)?;
        tokio::fs::write(&temporary, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&temporary, &self.metadata_path).await?;
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
