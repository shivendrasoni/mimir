use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::server::DaemonError;

#[derive(Debug, Clone)]
pub(super) enum JournalLookup {
    Pending,
    Complete(Value),
}

#[derive(Debug, Clone)]
struct JournalEntry {
    response: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JournalRecord {
    Received {
        version: u8,
        client_id: String,
        command_id: String,
        command_type: String,
        recorded_at: String,
    },
    Result {
        version: u8,
        client_id: String,
        command_id: String,
        response: Value,
        recorded_at: String,
    },
    Acknowledged {
        version: u8,
        client_id: String,
        command_id: String,
        recorded_at: String,
    },
}

pub(super) struct PublicCommandJournal {
    path: PathBuf,
    entries: BTreeMap<(String, String), JournalEntry>,
}

impl PublicCommandJournal {
    pub(super) async fn load(state_root: &std::path::Path) -> Result<Self, DaemonError> {
        let path = state_root.join("daemon/public-command-journal.jsonl");
        let mut entries = BTreeMap::new();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                for line in contents.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(record) = serde_json::from_str::<JournalRecord>(line) else {
                        // A process crash can leave only its final append truncated.
                        continue;
                    };
                    match record {
                        JournalRecord::Received {
                            version,
                            client_id,
                            command_id,
                            ..
                        } => {
                            if version != 1 {
                                continue;
                            }
                            entries
                                .insert((client_id, command_id), JournalEntry { response: None });
                        }
                        JournalRecord::Result {
                            version,
                            client_id,
                            command_id,
                            response,
                            ..
                        } => {
                            if version != 1 {
                                continue;
                            }
                            if let Some(entry) = entries.get_mut(&(client_id, command_id)) {
                                entry.response = Some(response);
                            }
                        }
                        JournalRecord::Acknowledged {
                            version,
                            client_id,
                            command_id,
                            ..
                        } => {
                            if version != 1 {
                                continue;
                            }
                            entries.remove(&(client_id, command_id));
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self { path, entries })
    }

    pub(super) fn lookup(&self, client_id: &str, command_id: &str) -> Option<JournalLookup> {
        self.entries
            .get(&(client_id.to_owned(), command_id.to_owned()))
            .map(|entry| {
                entry
                    .response
                    .clone()
                    .map_or(JournalLookup::Pending, JournalLookup::Complete)
            })
    }

    pub(super) async fn begin(
        &mut self,
        client_id: &str,
        command_id: &str,
        command_type: &str,
    ) -> Result<(), DaemonError> {
        let record = JournalRecord::Received {
            version: 1,
            client_id: client_id.to_owned(),
            command_id: command_id.to_owned(),
            command_type: command_type.to_owned(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        self.append(&record).await?;
        self.entries.insert(
            (client_id.to_owned(), command_id.to_owned()),
            JournalEntry { response: None },
        );
        Ok(())
    }

    pub(super) async fn record_result(
        &mut self,
        client_id: &str,
        command_id: &str,
        response: Value,
    ) -> Result<(), DaemonError> {
        let key = (client_id.to_owned(), command_id.to_owned());
        if !self.entries.contains_key(&key) {
            return Err(DaemonError::Protocol(
                "cannot record a public command result before receipt".into(),
            ));
        }
        let record = JournalRecord::Result {
            version: 1,
            client_id: client_id.to_owned(),
            command_id: command_id.to_owned(),
            response: response.clone(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        self.append(&record).await?;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.response = Some(response);
        }
        Ok(())
    }

    pub(super) async fn acknowledge(
        &mut self,
        client_id: &str,
        command_id: &str,
    ) -> Result<(), DaemonError> {
        let key = (client_id.to_owned(), command_id.to_owned());
        if !self.entries.contains_key(&key) {
            return Ok(());
        }
        let record = JournalRecord::Acknowledged {
            version: 1,
            client_id: client_id.to_owned(),
            command_id: command_id.to_owned(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        self.append(&record).await?;
        self.entries.remove(&key);
        Ok(())
    }

    async fn append(&self, record: &JournalRecord) -> Result<(), DaemonError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        let mut encoded = serde_json::to_vec(record)?;
        encoded.push(b'\n');
        file.write_all(&encoded).await?;
        file.sync_all().await?;
        Ok(())
    }
}
