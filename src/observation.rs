use std::{
    collections::HashSet,
    fs::Metadata,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::Message,
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
};

pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(10);
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Creates cancellable, bounded watchers over durable session transcripts.
#[derive(Debug, Clone)]
pub struct SessionObserver {
    state_root: PathBuf,
    poll_interval: Duration,
}

impl SessionObserver {
    #[must_use]
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Applies a polling interval clamped to the supported non-busy range.
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval.clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL);
        self
    }

    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Validates and starts observation of an existing durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session id is unsafe, the target does not exist as a regular
    /// file, the path crosses a symlink, or the transcript cannot be loaded safely.
    pub async fn start(&self, session_id: &str) -> Result<SessionObservation> {
        let store = FileSessionStore::create(&self.state_root, session_id).await?;
        require_regular_session_file(store.path()).await?;
        let loaded = store.load().await?;
        let fingerprint = fingerprint(&require_regular_session_file(store.path()).await?);
        let initial_messages = compaction_aware_messages(&loaded.records);
        let known_records = loaded
            .records
            .iter()
            .map(|record| record.record_id)
            .collect();
        let (events, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(watch_session(
            store,
            session_id.to_owned(),
            known_records,
            fingerprint,
            self.poll_interval,
            cancellation.clone(),
            events,
        ));
        Ok(SessionObservation {
            initial_messages,
            receiver,
            cancellation,
            task: Some(task),
        })
    }
}

/// One active session observation and its bounded event stream.
pub struct SessionObservation {
    initial_messages: Vec<Message>,
    receiver: mpsc::Receiver<ObservedSessionOutput>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl SessionObservation {
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.initial_messages
    }

    pub async fn next_event(&mut self) -> Option<ObservedSessionOutput> {
        self.receiver.recv().await
    }

    /// Requests cancellation without waiting for the polling task to exit.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Stops the watcher, waits for it to exit, and discards any queued events.
    ///
    /// Calling this method more than once is safe.
    ///
    /// # Errors
    ///
    /// Returns a protocol error only when the internal polling task panicked or was aborted.
    pub async fn stop(&mut self) -> Result<()> {
        self.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                MimirError::Protocol(format!("session observation task failed: {error}"))
            })?;
        }
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
        Ok(())
    }
}

impl Drop for SessionObservation {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Reference-compatible event emitted for a watched session.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ObservedSessionOutput {
    #[serde(rename = "observed_session_event")]
    Event {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        event: ObservedSessionEvent,
    },
    #[serde(rename = "observed_session_closed")]
    Closed {
        #[serde(rename = "activeSessionId")]
        active_session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Durable session event nested inside [`ObservedSessionOutput::Event`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ObservedSessionEvent {
    #[serde(rename = "message_end")]
    MessageEnd { message: Message },
    #[serde(rename = "compaction_end")]
    CompactionEnd {
        reason: String,
        result: ObservedCompactionResult,
        aborted: bool,
        #[serde(rename = "willRetry")]
        will_retry: bool,
        #[serde(rename = "customInstructions", skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedCompactionResult {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId", skip_serializing_if = "Option::is_none")]
    pub first_kept_entry_id: Option<String>,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

async fn watch_session(
    store: FileSessionStore,
    session_id: String,
    mut known_records: HashSet<Uuid>,
    mut fingerprint: FileFingerprint,
    poll_interval: Duration,
    cancellation: CancellationToken,
    events: mpsc::Sender<ObservedSessionOutput>,
) {
    let start = Instant::now() + poll_interval;
    let mut ticker = tokio::time::interval_at(start, poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return,
            _ = ticker.tick() => {}
        }
        match poll_once(&store, &mut known_records, &mut fingerprint).await {
            Ok(PollResult::Events(session_events)) => {
                for event in session_events {
                    let output = ObservedSessionOutput::Event {
                        active_session_id: session_id.clone(),
                        event,
                    };
                    if !send_or_cancel(&events, output, &cancellation).await {
                        return;
                    }
                }
            }
            Ok(PollResult::Closed) => {
                let _ = send_or_cancel(
                    &events,
                    ObservedSessionOutput::Closed {
                        active_session_id: session_id,
                        error: None,
                    },
                    &cancellation,
                )
                .await;
                return;
            }
            Err(_) => {
                let _ = send_or_cancel(
                    &events,
                    ObservedSessionOutput::Closed {
                        active_session_id: session_id,
                        error: Some("session observation failed".into()),
                    },
                    &cancellation,
                )
                .await;
                return;
            }
        }
    }
}

enum PollResult {
    Events(Vec<ObservedSessionEvent>),
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    length: u64,
    modified: Option<SystemTime>,
}

async fn poll_once(
    store: &FileSessionStore,
    known_records: &mut HashSet<Uuid>,
    previous_fingerprint: &mut FileFingerprint,
) -> Result<PollResult> {
    let metadata = match tokio::fs::symlink_metadata(store.path()).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(unavailable_session_error(store.path()));
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PollResult::Closed);
        }
        Err(error) => return Err(error.into()),
    };
    let current_fingerprint = fingerprint(&metadata);
    if current_fingerprint == *previous_fingerprint {
        return Ok(PollResult::Events(Vec::new()));
    }
    let loaded = store.load().await?;
    let current_records = loaded
        .records
        .iter()
        .map(|record| record.record_id)
        .collect::<HashSet<_>>();
    let session_events = loaded
        .records
        .into_iter()
        .filter(|record| !known_records.contains(&record.record_id))
        .filter_map(|record| match record.payload {
            SessionPayload::Message(message) => Some(ObservedSessionEvent::MessageEnd { message }),
            SessionPayload::Compaction {
                summary,
                reason,
                first_kept_entry_id,
                tokens_before,
                custom_instructions,
                details,
                ..
            } => Some(ObservedSessionEvent::CompactionEnd {
                reason: reason.unwrap_or_else(|| {
                    if tokens_before == 0 {
                        "threshold".into()
                    } else {
                        "manual".into()
                    }
                }),
                result: ObservedCompactionResult {
                    summary,
                    first_kept_entry_id,
                    tokens_before,
                    details,
                },
                aborted: false,
                will_retry: false,
                custom_instructions,
            }),
            SessionPayload::RuntimeEvent { .. } => None,
        })
        .collect();
    *known_records = current_records;
    *previous_fingerprint = current_fingerprint;
    Ok(PollResult::Events(session_events))
}

async fn send_or_cancel(
    events: &mpsc::Sender<ObservedSessionOutput>,
    output: ObservedSessionOutput,
    cancellation: &CancellationToken,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => false,
        result = events.send(output) => result.is_ok(),
    }
}

async fn require_regular_session_file(path: &Path) -> Result<Metadata> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MimirError::Session {
            path: path.to_owned(),
            message: "session path must not be a symlink".into(),
        }),
        Ok(metadata) if metadata.is_file() => Ok(metadata),
        Ok(_) => Err(MimirError::Session {
            path: path.to_owned(),
            message: "session path must be a regular file".into(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(MimirError::Session {
            path: path.to_owned(),
            message: "unknown active session".into(),
        }),
        Err(error) => Err(error.into()),
    }
}

fn unavailable_session_error(path: &Path) -> MimirError {
    MimirError::Session {
        path: path.to_owned(),
        message: "observed session is no longer a regular file".into(),
    }
}

fn fingerprint(metadata: &Metadata) -> FileFingerprint {
    FileFingerprint {
        length: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

fn compaction_aware_messages(records: &[SessionRecord]) -> Vec<Message> {
    let mut messages = Vec::new();
    for record in records {
        match &record.payload {
            SessionPayload::Message(message) => messages.push(message.clone()),
            SessionPayload::Compaction {
                summary,
                retained_message_count,
                ..
            } => {
                let retained =
                    messages.split_off(messages.len().saturating_sub(*retained_message_count));
                messages.clear();
                messages.push(Message::system(summary));
                messages.extend(retained);
            }
            SessionPayload::RuntimeEvent { .. } => {}
        }
    }
    messages
}
