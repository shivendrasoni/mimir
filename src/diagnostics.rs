//! Privacy-safe, append-only diagnostics for Mimir runs.
//!
//! Diagnostics are intentionally separate from the model transcript. The
//! recorder stores structural metadata, counters, hashes, and classifications;
//! it never stores prompt text, tool arguments/output, or environment values.

use std::{
    collections::BTreeMap,
    fmt::Write as FmtWrite,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    budget::BudgetPause,
    error::{MimirError, Result},
    model::{Content, Message, Role, Usage},
    runtime::RuntimeEvent,
    tools::{ObservationStatus, ToolObservation},
};

pub const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;
pub const DIAGNOSTIC_REDACTION_VERSION: u16 = 1;
const MAX_DIAGNOSTIC_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENTS_PER_RUN: u64 = 100_000;
const MAX_EVENT_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RETAINED_RUNS: usize = 100;
const MAX_DIAGNOSTIC_ROOT_BYTES: u64 = 512 * 1024 * 1024;
const FINALIZE_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticManifest {
    pub schema_version: u16,
    pub run_id: Uuid,
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub mimir_version: String,
    pub provider: String,
    pub model: String,
    /// A stable alias, never the host's absolute workspace path.
    pub workspace: String,
    pub configuration: DiagnosticConfiguration,
    pub privacy: DiagnosticPrivacy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticConfiguration {
    pub output_mode: String,
    pub offline: bool,
    pub autonomous: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the privacy manifest explicitly records each prohibited data category"
)]
pub struct DiagnosticPrivacy {
    pub raw_prompts: bool,
    pub raw_tool_arguments: bool,
    pub raw_tool_output: bool,
    pub environment_values: bool,
    pub absolute_paths: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub schema_version: u16,
    pub redaction_version: u16,
    pub event_id: Uuid,
    pub run_id: Uuid,
    pub sequence: u64,
    pub recorded_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(flatten)]
    pub kind: DiagnosticEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticEventKind {
    RunStarted,
    ProviderRequest {
        turn: u32,
        #[serde(default)]
        estimated_context_tokens: u64,
    },
    MessageStarted {
        metadata: MessageMetadata,
    },
    MessageCompleted {
        metadata: MessageMetadata,
    },
    TurnCompleted {
        metadata: MessageMetadata,
        tool_result_count: usize,
    },
    ToolStarted {
        name: String,
        arguments: PayloadMetadata,
    },
    PermissionRequested {
        action: String,
        command: PayloadMetadata,
    },
    ToolUpdated {
        name: String,
        observation: ObservationMetadata,
    },
    ToolFinished {
        name: String,
        observation: ObservationMetadata,
    },
    ProviderOutput {
        byte_count: usize,
    },
    AutoRetryStarted {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: ErrorMetadata,
    },
    AutoRetryFinished {
        success: bool,
        attempt: u32,
        error: Option<ErrorMetadata>,
    },
    Completed {
        output: PayloadMetadata,
    },
    Failed {
        error: ErrorMetadata,
    },
    BudgetPaused {
        error: ErrorMetadata,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pause: Option<BudgetPause>,
    },
    ExtensionEvent {
        event: String,
        extension_id: String,
        error: Option<ErrorMetadata>,
    },
    SessionEvent {
        event_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
}

impl DiagnosticEventKind {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::RunStarted => "run_started",
            Self::ProviderRequest { .. } => "provider_request",
            Self::MessageStarted { .. } => "message_started",
            Self::MessageCompleted { .. } => "message_completed",
            Self::TurnCompleted { .. } => "turn_completed",
            Self::ToolStarted { .. } => "tool_started",
            Self::PermissionRequested { .. } => "permission_requested",
            Self::ToolUpdated { .. } => "tool_updated",
            Self::ToolFinished { .. } => "tool_finished",
            Self::ProviderOutput { .. } => "provider_output",
            Self::AutoRetryStarted { .. } => "auto_retry_started",
            Self::AutoRetryFinished { .. } => "auto_retry_finished",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::BudgetPaused { .. } => "budget_paused",
            Self::ExtensionEvent { .. } => "extension_event",
            Self::SessionEvent { .. } => "session_event",
        }
    }

    fn status(&self) -> Option<&'static str> {
        match self {
            Self::ToolUpdated { observation, .. } | Self::ToolFinished { observation, .. } => {
                Some(observation.status.as_str())
            }
            Self::AutoRetryFinished { success: true, .. } | Self::Completed { .. } => {
                Some("success")
            }
            Self::AutoRetryFinished { success: false, .. }
            | Self::Failed { .. }
            | Self::BudgetPaused { .. } => Some("error"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub role: String,
    pub content_blocks: usize,
    pub text_bytes: usize,
    pub image_count: usize,
    pub tool_call_count: usize,
    pub tool_result_count: usize,
    pub usage: Usage,
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub fresh_input_tokens: u64,
    #[serde(default)]
    pub operational_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadMetadata {
    pub byte_count: usize,
    pub sha256: String,
}

impl PayloadMetadata {
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            byte_count: bytes.len(),
            sha256: hex_digest(bytes),
        }
    }

    fn from_json(value: &Value) -> Self {
        serde_json::to_vec(value).map_or_else(
            |_| Self::from_bytes(b"<unserializable>"),
            |bytes| Self::from_bytes(&bytes),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationMetadata {
    pub status: DiagnosticStatus,
    pub content: PayloadMetadata,
    pub summary: PayloadMetadata,
    pub artifact_count: usize,
    pub next_action_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ErrorMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    Success,
    Warning,
    Error,
}

impl DiagnosticStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorMetadata {
    pub code: FailureCode,
    pub component: FailureComponent,
    pub severity: FailureSeverity,
    pub byte_count: usize,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    BudgetExhausted,
    Timeout,
    PermissionDenied,
    ProviderError,
    ProviderAuthentication,
    ProviderRateLimited,
    ProviderUnavailable,
    ProviderProtocol,
    Cancelled,
    ExtensionError,
    ProcessSpawnFailed,
    ProcessExitNonzero,
    ProcessExecutionTimeout,
    ProcessPipeDrainTimeout,
    ProcessOutputLimit,
    ProcessCancelled,
    WorkspacePathInvalid,
    WorkspaceTargetOutside,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureComponent {
    Provider,
    Runtime,
    Tool,
    Extension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticSummary {
    pub schema_version: u16,
    pub run_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub outcome: DiagnosticOutcome,
    pub event_count: u64,
    pub event_counts: BTreeMap<String, u64>,
    pub provider_requests: u64,
    pub retries: u64,
    pub tool_calls: u64,
    pub tool_failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default)]
    pub fresh_input_tokens: u64,
    #[serde(default)]
    pub operational_tokens: u64,
    #[serde(default)]
    pub peak_context_tokens: u64,
    pub dropped_events: u64,
    /// True when the reader synthesized this summary because the writer never
    /// committed a terminal record (for example after a crash or hard kill).
    #[serde(default)]
    pub inferred_incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOutcome {
    Completed,
    Failed,
    Cancelled,
    BudgetPaused,
    Crashed,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticAnalysis {
    pub schema_version: u16,
    pub analysis_id: Uuid,
    pub run_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub author: String,
    pub finding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub evidence_event_ids: Vec<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticAnalysisInput {
    pub author: String,
    pub finding: String,
    pub confidence: Option<f32>,
    #[serde(default)]
    pub evidence_event_ids: Vec<Uuid>,
    pub proposed_fix: Option<String>,
    pub verification: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticIndexRecord {
    pub schema_version: u16,
    pub run_id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub phase: DiagnosticIndexPhase,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<DiagnosticOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticIndexPhase {
    Started,
    Finished,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticBundle {
    pub schema_version: u16,
    pub redacted: bool,
    pub manifest: DiagnosticManifest,
    pub events: Vec<DiagnosticEvent>,
    pub summary: Option<DiagnosticSummary>,
    pub analysis: Vec<DiagnosticAnalysis>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReplayReport {
    pub schema_version: u16,
    pub run_id: Uuid,
    pub mode: &'static str,
    pub valid: bool,
    pub event_count: usize,
    pub sequence_gaps: Vec<u64>,
    pub terminal_events: usize,
    pub executed_provider_requests: bool,
    pub executed_tools: bool,
}

enum WriterCommand {
    Event(Box<DiagnosticEvent>),
    Finalize {
        outcome: DiagnosticOutcome,
        duration_ms: u64,
        dropped_events: u64,
        acknowledgment: mpsc::Sender<()>,
    },
}

struct JournalInner {
    run_id: Uuid,
    started: Instant,
    sequence: AtomicU64,
    dropped_events: AtomicU64,
    sender: mpsc::Sender<WriterCommand>,
}

/// A cheap, cloneable producer for the best-effort background journal.
#[derive(Clone)]
pub struct DiagnosticJournal {
    inner: Arc<JournalInner>,
}

impl DiagnosticJournal {
    /// Starts a diagnostics writer. Initialization and later write failures are
    /// contained inside the writer thread and never fail the agent run.
    #[must_use]
    pub fn start(root: PathBuf, mut manifest: DiagnosticManifest) -> Self {
        manifest.provider = redact_free_text(&manifest.provider);
        manifest.model = redact_free_text(&manifest.model);
        manifest.workspace = "$WORKSPACE".into();
        let (sender, receiver) = mpsc::channel();
        let run_id = manifest.run_id;
        std::thread::Builder::new()
            .name(format!("mimir-diagnostics-{run_id}"))
            .spawn(move || writer_loop(&root, &manifest, receiver))
            .ok();
        Self {
            inner: Arc::new(JournalInner {
                run_id,
                started: Instant::now(),
                sequence: AtomicU64::new(0),
                dropped_events: AtomicU64::new(0),
                sender,
            }),
        }
    }

    #[must_use]
    pub fn run_id(&self) -> Uuid {
        self.inner.run_id
    }

    pub fn record(
        &self,
        kind: DiagnosticEventKind,
        turn_id: Option<String>,
        tool_call_id: Option<String>,
    ) {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        let elapsed_ms =
            u64::try_from(self.inner.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let event = DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            redaction_version: DIAGNOSTIC_REDACTION_VERSION,
            event_id: Uuid::new_v4(),
            run_id: self.inner.run_id,
            sequence,
            recorded_at: Utc::now(),
            elapsed_ms,
            turn_id,
            tool_call_id,
            kind,
        };
        if self
            .inner
            .sender
            .send(WriterCommand::Event(Box::new(event)))
            .is_err()
        {
            self.inner.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Accounts for events lost before they reached the journal, such as a
    /// bounded runtime broadcast receiver lagging behind the producer.
    pub fn note_dropped(&self, count: u64) {
        self.inner
            .dropped_events
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Requests a terminal summary and waits briefly for queued evidence to be
    /// flushed. Failure is deliberately ignored by callers.
    pub fn finish(&self, outcome: DiagnosticOutcome) {
        let (acknowledgment, receiver) = mpsc::channel();
        if self
            .inner
            .sender
            .send(WriterCommand::Finalize {
                outcome,
                duration_ms: u64::try_from(self.inner.started.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
                dropped_events: self.inner.dropped_events.load(Ordering::Relaxed),
                acknowledgment,
            })
            .is_ok()
        {
            let _ = receiver.recv_timeout(FINALIZE_WAIT);
        }
    }
}

/// Stateful converter from runtime events into privacy-safe diagnostic events.
#[derive(Default)]
pub struct RuntimeDiagnosticRecorder {
    current_turn: Option<String>,
    output_bytes: usize,
}

struct ActiveRuntimeDiagnosticRun {
    journal: DiagnosticJournal,
    recorder: RuntimeDiagnosticRecorder,
}

/// Splits a long-lived runtime event stream into one immutable diagnostic
/// bundle per prompt attempt.
///
/// Interactive frontends keep one runtime alive across many prompts. Runtime
/// `RunStarted` and terminal events, rather than frontend shutdown, therefore
/// define diagnostic run boundaries.
pub struct RuntimeDiagnosticRunCollector {
    root: PathBuf,
    manifest_template: DiagnosticManifest,
    active: Option<ActiveRuntimeDiagnosticRun>,
}

impl RuntimeDiagnosticRunCollector {
    #[must_use]
    pub fn new(root: PathBuf, manifest_template: DiagnosticManifest) -> Self {
        Self {
            root,
            manifest_template,
            active: None,
        }
    }

    pub fn record(&mut self, event: &RuntimeEvent) {
        if matches!(event, RuntimeEvent::RunStarted) {
            self.finish_unterminated();
            let mut manifest = self.manifest_template.clone();
            manifest.run_id = Uuid::new_v4();
            manifest.started_at = Utc::now();
            self.active = Some(ActiveRuntimeDiagnosticRun {
                journal: DiagnosticJournal::start(self.root.clone(), manifest),
                recorder: RuntimeDiagnosticRecorder::default(),
            });
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.recorder.record(&active.journal, event);
        let outcome = runtime_terminal_outcome(event);
        if let Some(outcome) = outcome
            && let Some(active) = self.active.take()
        {
            active.journal.finish(outcome);
        }
    }

    pub fn note_dropped(&self, count: u64) {
        if let Some(active) = &self.active {
            active.journal.note_dropped(count);
        }
    }

    /// Closes an in-flight prompt without allowing frontend shutdown to mark it
    /// completed. A normally terminated prompt has already been finalized.
    pub fn finish_open(&mut self) {
        self.finish_unterminated();
    }

    fn finish_unterminated(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        active.recorder.record(
            &active.journal,
            &RuntimeEvent::Failed {
                message: "run cancelled before a terminal runtime event was observed".into(),
            },
        );
        active.journal.finish(DiagnosticOutcome::Cancelled);
    }
}

fn runtime_terminal_outcome(event: &RuntimeEvent) -> Option<DiagnosticOutcome> {
    match event {
        RuntimeEvent::Completed { .. } => Some(DiagnosticOutcome::Completed),
        RuntimeEvent::BudgetPaused { .. } => Some(DiagnosticOutcome::BudgetPaused),
        RuntimeEvent::Failed { message }
            if message.to_ascii_lowercase().contains("cancel")
                || message.to_ascii_lowercase().contains("abort") =>
        {
            Some(DiagnosticOutcome::Cancelled)
        }
        RuntimeEvent::Failed { .. } => Some(DiagnosticOutcome::Failed),
        _ => None,
    }
}

impl RuntimeDiagnosticRecorder {
    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive runtime-event redaction map is kept in one auditable match"
    )]
    pub fn record(&mut self, journal: &DiagnosticJournal, event: &RuntimeEvent) {
        let (kind, tool_call_id) = match event {
            RuntimeEvent::RunStarted => (DiagnosticEventKind::RunStarted, None),
            RuntimeEvent::ProviderRequest {
                turn,
                estimated_context_tokens,
            } => {
                self.current_turn = Some(format!("turn-{turn}"));
                (
                    DiagnosticEventKind::ProviderRequest {
                        turn: *turn,
                        estimated_context_tokens: *estimated_context_tokens,
                    },
                    None,
                )
            }
            RuntimeEvent::MessageStarted { message } => (
                DiagnosticEventKind::MessageStarted {
                    metadata: message_metadata(message),
                },
                None,
            ),
            RuntimeEvent::MessageCompleted { message } => (
                DiagnosticEventKind::MessageCompleted {
                    metadata: message_metadata(message),
                },
                None,
            ),
            RuntimeEvent::TurnCompleted {
                message,
                tool_results,
            } => (
                DiagnosticEventKind::TurnCompleted {
                    metadata: message_metadata(message),
                    tool_result_count: tool_results.len(),
                },
                None,
            ),
            RuntimeEvent::ToolStarted {
                id,
                name,
                arguments,
            } => (
                DiagnosticEventKind::ToolStarted {
                    name: safe_identifier(name),
                    arguments: PayloadMetadata::from_json(arguments),
                },
                Some(safe_identifier(id)),
            ),
            RuntimeEvent::PermissionRequested { request } => (
                DiagnosticEventKind::PermissionRequested {
                    action: request.action.label().to_owned(),
                    command: PayloadMetadata::from_bytes(request.command.as_bytes()),
                },
                None,
            ),
            RuntimeEvent::UserInputRequested { .. } => (
                DiagnosticEventKind::SessionEvent {
                    event_type: "user_input_requested".into(),
                    status: Some("waiting".into()),
                    provider: None,
                },
                None,
            ),
            RuntimeEvent::ToolUpdated {
                id,
                name,
                observation,
                ..
            } => (
                DiagnosticEventKind::ToolUpdated {
                    name: safe_identifier(name),
                    observation: observation_metadata(observation),
                },
                Some(safe_identifier(id)),
            ),
            RuntimeEvent::ToolFinished {
                id,
                name,
                observation,
            } => (
                DiagnosticEventKind::ToolFinished {
                    name: safe_identifier(name),
                    observation: observation_metadata(observation),
                },
                Some(safe_identifier(id)),
            ),
            RuntimeEvent::TextDelta { text } => {
                self.output_bytes = self.output_bytes.saturating_add(text.len());
                return;
            }
            RuntimeEvent::AutoRetryStarted {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => (
                DiagnosticEventKind::AutoRetryStarted {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    delay_ms: *delay_ms,
                    error: error_metadata(
                        error_message,
                        FailureComponent::Provider,
                        FailureSeverity::Warning,
                    ),
                },
                None,
            ),
            RuntimeEvent::AutoRetryFinished {
                success,
                attempt,
                final_error,
            } => (
                DiagnosticEventKind::AutoRetryFinished {
                    success: *success,
                    attempt: *attempt,
                    error: final_error.as_deref().map(|error| {
                        error_metadata(error, FailureComponent::Provider, FailureSeverity::Error)
                    }),
                },
                None,
            ),
            RuntimeEvent::Completed { text } => {
                let byte_count = self.output_bytes.max(text.len());
                self.output_bytes = 0;
                (
                    DiagnosticEventKind::Completed {
                        output: PayloadMetadata {
                            byte_count,
                            sha256: hex_digest(text.as_bytes()),
                        },
                    },
                    None,
                )
            }
            RuntimeEvent::Failed { message } => (
                DiagnosticEventKind::Failed {
                    error: error_metadata(
                        message,
                        FailureComponent::Runtime,
                        FailureSeverity::Error,
                    ),
                },
                None,
            ),
            RuntimeEvent::BudgetPaused { pause } => (
                DiagnosticEventKind::BudgetPaused {
                    error: error_metadata(
                        &pause.to_string(),
                        FailureComponent::Runtime,
                        FailureSeverity::Error,
                    ),
                    pause: Some(*pause),
                },
                None,
            ),
            RuntimeEvent::ExtensionUi { extension, .. } => (
                DiagnosticEventKind::ExtensionEvent {
                    event: "ui".into(),
                    extension_id: safe_identifier(extension),
                    error: None,
                },
                None,
            ),
            RuntimeEvent::ExtensionRendered { custom_type, .. } => (
                DiagnosticEventKind::ExtensionEvent {
                    event: "rendered".into(),
                    extension_id: safe_identifier(custom_type),
                    error: None,
                },
                None,
            ),
            RuntimeEvent::ExtensionError {
                extension_path,
                event,
                error,
            } => (
                DiagnosticEventKind::ExtensionEvent {
                    event: safe_identifier(event),
                    extension_id: path_alias(extension_path),
                    error: Some(error_metadata(
                        error,
                        FailureComponent::Extension,
                        FailureSeverity::Error,
                    )),
                },
                None,
            ),
            RuntimeEvent::SessionEvent { event } => (
                DiagnosticEventKind::SessionEvent {
                    event_type: event
                        .get("type")
                        .and_then(Value::as_str)
                        .map_or_else(|| "unknown".into(), safe_identifier),
                    status: event
                        .get("status")
                        .and_then(Value::as_str)
                        .map(safe_identifier),
                    provider: event
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(safe_identifier),
                },
                None,
            ),
        };
        journal.record(kind, self.current_turn.clone(), tool_call_id);
    }
}

fn writer_loop(
    root: &Path,
    manifest: &DiagnosticManifest,
    receiver: mpsc::Receiver<WriterCommand>,
) {
    let run_directory = root.join("runs").join(manifest.run_id.to_string());
    let _ = enforce_retention(root);
    let initialized = initialize_bundle(root, &run_directory, manifest).is_ok();
    let mut accumulator = SummaryAccumulator::new(manifest);
    for command in receiver {
        match command {
            WriterCommand::Event(event) => {
                accumulator.observe(&event);
                let event_file = run_directory.join("events.jsonl");
                if initialized
                    && accumulator.event_count <= MAX_EVENTS_PER_RUN
                    && file_len(&event_file) < MAX_EVENT_FILE_BYTES
                {
                    let _ = append_json_line(&event_file, &event);
                } else if initialized {
                    accumulator.writer_dropped = accumulator.writer_dropped.saturating_add(1);
                }
            }
            WriterCommand::Finalize {
                outcome,
                duration_ms,
                dropped_events,
                acknowledgment,
            } => {
                if initialized {
                    let summary = accumulator.finish(outcome, duration_ms, dropped_events);
                    let _ = write_json_new(&run_directory.join("summary.json"), &summary);
                    let index = DiagnosticIndexRecord {
                        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
                        run_id: manifest.run_id,
                        recorded_at: summary.finished_at,
                        phase: DiagnosticIndexPhase::Finished,
                        session_id: manifest.session_id.clone(),
                        provider: manifest.provider.clone(),
                        model: manifest.model.clone(),
                        outcome: Some(summary.outcome),
                        duration_ms: Some(summary.duration_ms),
                    };
                    let _ = append_json_line(&root.join("index.jsonl"), &index);
                }
                let _ = acknowledgment.send(());
                break;
            }
        }
    }
}

fn initialize_bundle(
    root: &Path,
    run_directory: &Path,
    manifest: &DiagnosticManifest,
) -> Result<()> {
    let runs = root.join("runs");
    let artifacts = run_directory.join("artifacts");
    for directory in [root, runs.as_path(), run_directory, artifacts.as_path()] {
        fs::create_dir_all(directory)?;
        secure_directory(directory)?;
    }
    write_json_new(&run_directory.join("manifest.json"), manifest)?;
    for name in ["events.jsonl", "analysis.jsonl"] {
        let path = run_directory.join(name);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        secure_file_options(&mut options);
        options.open(&path)?;
        secure_file(&path)?;
    }
    let index = DiagnosticIndexRecord {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        run_id: manifest.run_id,
        recorded_at: manifest.started_at,
        phase: DiagnosticIndexPhase::Started,
        session_id: manifest.session_id.clone(),
        provider: manifest.provider.clone(),
        model: manifest.model.clone(),
        outcome: None,
        duration_ms: None,
    };
    append_json_line(&root.join("index.jsonl"), &index)
}

fn enforce_retention(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let runs = root.join("runs");
    fs::create_dir_all(&runs)?;
    secure_directory(&runs)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&runs)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            entries.push((
                entry.path(),
                metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                directory_size(&entry.path()),
            ));
        }
    }
    entries.sort_by_key(|(_, modified, _)| *modified);
    let mut total_bytes = entries
        .iter()
        .fold(0_u64, |total, (_, _, size)| total.saturating_add(*size));
    let mut retained = entries.len();
    for (path, _, size) in entries {
        if retained < MAX_RETAINED_RUNS && total_bytes <= MAX_DIAGNOSTIC_ROOT_BYTES {
            break;
        }
        fs::remove_dir_all(path)?;
        retained = retained.saturating_sub(1);
        total_bytes = total_bytes.saturating_sub(size);
    }
    Ok(())
}

fn directory_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .fold(0_u64, |total, metadata| {
            total.saturating_add(metadata.len())
        })
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[cfg(unix)]
fn secure_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn secure_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "callers share the fallible Unix permission contract"
)]
fn secure_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "callers share the fallible Unix permission contract"
)]
fn secure_file(_path: &Path) -> Result<()> {
    Ok(())
}

struct SummaryAccumulator {
    run_id: Uuid,
    started_at: DateTime<Utc>,
    event_count: u64,
    event_counts: BTreeMap<String, u64>,
    provider_requests: u64,
    retries: u64,
    tool_calls: u64,
    tool_failures: u64,
    usage: Usage,
    peak_context_tokens: u64,
    last_elapsed_ms: u64,
    writer_dropped: u64,
}

impl SummaryAccumulator {
    fn new(manifest: &DiagnosticManifest) -> Self {
        Self {
            run_id: manifest.run_id,
            started_at: manifest.started_at,
            event_count: 0,
            event_counts: BTreeMap::new(),
            provider_requests: 0,
            retries: 0,
            tool_calls: 0,
            tool_failures: 0,
            usage: Usage::default(),
            peak_context_tokens: 0,
            last_elapsed_ms: 0,
            writer_dropped: 0,
        }
    }

    fn observe(&mut self, event: &DiagnosticEvent) {
        self.event_count = self.event_count.saturating_add(1);
        self.last_elapsed_ms = self.last_elapsed_ms.max(event.elapsed_ms);
        *self
            .event_counts
            .entry(event.kind.name().to_owned())
            .or_default() += 1;
        match &event.kind {
            DiagnosticEventKind::ProviderRequest { .. } => {
                self.provider_requests = self.provider_requests.saturating_add(1);
            }
            DiagnosticEventKind::AutoRetryStarted { .. } => {
                self.retries = self.retries.saturating_add(1);
            }
            DiagnosticEventKind::ToolStarted { .. } => {
                self.tool_calls = self.tool_calls.saturating_add(1);
            }
            DiagnosticEventKind::ToolFinished { observation, .. }
                if observation.status == DiagnosticStatus::Error =>
            {
                self.tool_failures = self.tool_failures.saturating_add(1);
            }
            DiagnosticEventKind::MessageCompleted { metadata } => {
                // TurnCompleted repeats the same message, so usage is accumulated
                // only from MessageCompleted events.
                self.usage.input_tokens = self
                    .usage
                    .input_tokens
                    .saturating_add(metadata.usage.input_tokens);
                self.usage.output_tokens = self
                    .usage
                    .output_tokens
                    .saturating_add(metadata.usage.output_tokens);
                self.usage.cached_tokens = self
                    .usage
                    .cached_tokens
                    .saturating_add(metadata.usage.cached_tokens);
                self.peak_context_tokens =
                    self.peak_context_tokens.max(metadata.usage.input_tokens);
            }
            _ => {}
        }
    }

    fn finish(
        &self,
        outcome: DiagnosticOutcome,
        duration_ms: u64,
        dropped_events: u64,
    ) -> DiagnosticSummary {
        DiagnosticSummary {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            run_id: self.run_id,
            started_at: self.started_at,
            finished_at: Utc::now(),
            duration_ms: duration_ms.max(self.last_elapsed_ms),
            outcome,
            event_count: self.event_count,
            event_counts: self.event_counts.clone(),
            provider_requests: self.provider_requests,
            retries: self.retries,
            tool_calls: self.tool_calls,
            tool_failures: self.tool_failures,
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cached_tokens: self.usage.cached_tokens,
            fresh_input_tokens: self.usage.uncached_input_tokens(),
            operational_tokens: self.usage.budget_tokens(),
            peak_context_tokens: self.peak_context_tokens,
            dropped_events: dropped_events.saturating_add(self.writer_dropped),
            inferred_incomplete: false,
        }
    }
}

fn message_metadata(message: &Message) -> MessageMetadata {
    let mut text_bytes: usize = 0;
    let mut image_count = 0;
    let mut tool_call_count = 0;
    let mut tool_result_count = 0;
    for content in &message.content {
        match content {
            Content::Text { text } | Content::Thinking { text, .. } => {
                text_bytes = text_bytes.saturating_add(text.len());
            }
            Content::Image { .. } => image_count += 1,
            Content::ToolCall(_) => tool_call_count += 1,
            Content::ToolResult(_) => tool_result_count += 1,
        }
    }
    MessageMetadata {
        role: match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
        .into(),
        content_blocks: message.content.len(),
        text_bytes,
        image_count,
        tool_call_count,
        tool_result_count,
        usage: message.usage,
        context_tokens: message.usage.input_tokens,
        fresh_input_tokens: message.usage.uncached_input_tokens(),
        operational_tokens: message.usage.budget_tokens(),
        stop_reason: message
            .stop_reason
            .map(|reason| format!("{reason:?}").to_lowercase()),
    }
}

fn observation_metadata(observation: &ToolObservation) -> ObservationMetadata {
    let status = match observation.status {
        ObservationStatus::Success => DiagnosticStatus::Success,
        ObservationStatus::Warning => DiagnosticStatus::Warning,
        ObservationStatus::Error => DiagnosticStatus::Error,
    };
    let failure = match status {
        DiagnosticStatus::Success => None,
        DiagnosticStatus::Warning | DiagnosticStatus::Error => {
            let evidence = format!("{}\n{}", observation.summary, observation.content);
            Some(error_metadata(
                &evidence,
                FailureComponent::Tool,
                if status == DiagnosticStatus::Warning {
                    FailureSeverity::Warning
                } else {
                    FailureSeverity::Error
                },
            ))
        }
    };
    ObservationMetadata {
        status,
        content: PayloadMetadata::from_bytes(observation.content.as_bytes()),
        summary: PayloadMetadata::from_bytes(observation.summary.as_bytes()),
        artifact_count: observation.artifacts.len(),
        next_action_count: observation.next_actions.len(),
        failure,
    }
}

fn error_metadata(
    message: &str,
    component: FailureComponent,
    severity: FailureSeverity,
) -> ErrorMetadata {
    let lower = message.to_ascii_lowercase();
    let code = if lower.contains("output pipes remained open") || lower.contains("drain deadline") {
        FailureCode::ProcessPipeDrainTimeout
    } else if lower.contains("process execution timed out") {
        FailureCode::ProcessExecutionTimeout
    } else if lower.contains("process output exceeded") {
        FailureCode::ProcessOutputLimit
    } else if lower.contains("process cancelled") {
        FailureCode::ProcessCancelled
    } else if lower.contains("process exited with") {
        FailureCode::ProcessExitNonzero
    } else if lower.contains("run_process") && lower.contains("spawn") {
        FailureCode::ProcessSpawnFailed
    } else if lower.contains("outside $workspace") || lower.contains("outside the workspace") {
        FailureCode::WorkspaceTargetOutside
    } else if lower.contains("workspace-relative")
        || lower.contains("parent traversal")
        || lower.contains("path must be")
    {
        FailureCode::WorkspacePathInvalid
    } else if lower.contains("authentication")
        || lower.contains("unauthorized")
        || lower.contains("oauth")
        || lower.contains("http 401")
    {
        FailureCode::ProviderAuthentication
    } else if lower.contains("rate limit") || lower.contains("http 429") {
        FailureCode::ProviderRateLimited
    } else if lower.contains("provider unavailable")
        || lower.contains("connection reset")
        || lower.contains("service unavailable")
    {
        FailureCode::ProviderUnavailable
    } else if lower.contains("malformed")
        || lower.contains("truncated stream")
        || lower.contains("incomplete tool")
    {
        FailureCode::ProviderProtocol
    } else if lower.contains("budget") || lower.contains("token") {
        FailureCode::BudgetExhausted
    } else if component == FailureComponent::Provider
        && (lower.contains("timed out") || lower.contains("timeout"))
    {
        FailureCode::ProviderUnavailable
    } else if lower.contains("timed out") || lower.contains("timeout") {
        FailureCode::Timeout
    } else if lower.contains("permission") || lower.contains("denied") {
        FailureCode::PermissionDenied
    } else if lower.contains("provider") || lower.contains("api") {
        FailureCode::ProviderError
    } else if lower.contains("cancel") || lower.contains("abort") {
        FailureCode::Cancelled
    } else if component == FailureComponent::Extension {
        FailureCode::ExtensionError
    } else {
        FailureCode::Unknown
    };
    ErrorMetadata {
        code,
        component,
        severity,
        byte_count: message.len(),
        fingerprint: hex_digest(message.as_bytes()),
    }
}

fn safe_identifier(value: &str) -> String {
    let filtered: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(128)
        .collect();
    if filtered.is_empty() {
        "unknown".into()
    } else {
        filtered
    }
}

fn path_alias(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map_or_else(|| "extension".into(), safe_identifier)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn append_json_line(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_DIAGNOSTIC_LINE_BYTES {
        return Err(MimirError::Configuration(
            "diagnostic record exceeds the 1 MiB limit".into(),
        ));
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    secure_file_options(&mut options);
    let mut file = options.open(path)?;
    secure_file(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_file_options(&mut options);
    let mut file = options.open(path)?;
    secure_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn validate_run_id(run_id: &str) -> Result<Uuid> {
    Uuid::parse_str(run_id)
        .map_err(|_| MimirError::Configuration("diagnostic run id must be a UUID".into()))
}

#[must_use]
pub fn diagnostics_root(state_root: &Path) -> PathBuf {
    state_root.join("diagnostics")
}

/// Lists the latest index state for retained diagnostic runs.
///
/// # Errors
///
/// Returns an I/O or schema error when the append-only index cannot be read.
pub fn list_runs(root: &Path) -> Result<Vec<DiagnosticIndexRecord>> {
    let path = root.join("index.jsonl");
    let records: Vec<DiagnosticIndexRecord> = read_json_lines_optional(&path)?;
    let mut latest = BTreeMap::new();
    for record in records {
        latest.insert(record.run_id, record);
    }
    let mut runs: Vec<_> = latest
        .into_values()
        .filter(|record| {
            root.join("runs")
                .join(record.run_id.to_string())
                .join("manifest.json")
                .is_file()
        })
        .map(|mut record| {
            if record.phase == DiagnosticIndexPhase::Started {
                record.outcome = Some(DiagnosticOutcome::Incomplete);
            }
            record
        })
        .collect();
    runs.sort_by_key(|record| std::cmp::Reverse(record.recorded_at));
    Ok(runs)
}

/// Loads one portable, redacted diagnostic bundle.
///
/// # Errors
///
/// Returns an error for an invalid run id, missing bundle, or malformed record.
pub fn load_bundle(root: &Path, run_id: &str) -> Result<DiagnosticBundle> {
    let run_id = validate_run_id(run_id)?;
    let directory = root.join("runs").join(run_id.to_string());
    let manifest: DiagnosticManifest = read_json(&directory.join("manifest.json"))?;
    if manifest.run_id != run_id {
        return Err(MimirError::Protocol(
            "diagnostic manifest run id does not match its directory".into(),
        ));
    }
    let events: Vec<DiagnosticEvent> = read_json_lines_optional(&directory.join("events.jsonl"))?;
    let persisted_summary = read_json_optional(&directory.join("summary.json"))?;
    let has_terminal_event = events.iter().any(|event| {
        matches!(
            event.kind,
            DiagnosticEventKind::Completed { .. }
                | DiagnosticEventKind::Failed { .. }
                | DiagnosticEventKind::BudgetPaused { .. }
        )
    });
    let summary = persisted_summary
        .filter(|_| has_terminal_event)
        .or_else(|| {
            let finished_at = events
                .last()
                .map_or(manifest.started_at, |event| event.recorded_at);
            let duration_ms = events.last().map_or(0, |event| event.elapsed_ms);
            let mut accumulator = SummaryAccumulator::new(&manifest);
            for event in &events {
                accumulator.observe(event);
            }
            let mut summary = accumulator.finish(DiagnosticOutcome::Incomplete, duration_ms, 0);
            summary.finished_at = finished_at;
            summary.inferred_incomplete = true;
            Some(summary)
        });
    Ok(DiagnosticBundle {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        redacted: true,
        manifest,
        events,
        summary,
        analysis: read_json_lines_optional(&directory.join("analysis.jsonl"))?,
    })
}

/// Filters the typed events belonging to one diagnostic run.
///
/// # Errors
///
/// Returns an error for invalid filters or an unreadable bundle.
pub fn query_events(
    root: &Path,
    run_id: &str,
    kind: Option<&str>,
    status: Option<&str>,
) -> Result<Vec<DiagnosticEvent>> {
    if status.is_some_and(|value| !matches!(value, "success" | "warning" | "error")) {
        return Err(MimirError::Configuration(
            "diagnostic status must be success, warning, or error".into(),
        ));
    }
    let mut events = load_bundle(root, run_id)?.events;
    events.retain(|event| {
        kind.is_none_or(|expected| event.kind.name() == expected)
            && status.is_none_or(|expected| event.kind.status() == Some(expected))
    });
    Ok(events)
}

/// Appends a redacted external assessment without changing recorded evidence.
///
/// # Errors
///
/// Returns an error for invalid analysis data, unknown evidence, or a write failure.
pub fn append_analysis(
    root: &Path,
    run_id: &str,
    mut input: DiagnosticAnalysisInput,
) -> Result<DiagnosticAnalysis> {
    let run_id = validate_run_id(run_id)?;
    if input.author.trim().is_empty() || input.finding.trim().is_empty() {
        return Err(MimirError::Configuration(
            "diagnostic analysis requires non-empty author and finding fields".into(),
        ));
    }
    if input
        .confidence
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return Err(MimirError::Configuration(
            "diagnostic analysis confidence must be between 0 and 1".into(),
        ));
    }
    input.author = redact_free_text(&input.author);
    input.finding = redact_free_text(&input.finding);
    input.proposed_fix = input.proposed_fix.as_deref().map(redact_free_text);
    input.verification = input.verification.as_deref().map(redact_free_text);
    let bundle = load_bundle(root, &run_id.to_string())?;
    let known_events: std::collections::BTreeSet<_> =
        bundle.events.iter().map(|event| event.event_id).collect();
    if input
        .evidence_event_ids
        .iter()
        .any(|event_id| !known_events.contains(event_id))
    {
        return Err(MimirError::Configuration(
            "diagnostic analysis references an event outside this run".into(),
        ));
    }
    let analysis = DiagnosticAnalysis {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        analysis_id: Uuid::new_v4(),
        run_id,
        created_at: Utc::now(),
        author: input.author,
        finding: input.finding,
        confidence: input.confidence,
        evidence_event_ids: input.evidence_event_ids,
        proposed_fix: input.proposed_fix,
        verification: input.verification,
    };
    append_json_line(
        &root
            .join("runs")
            .join(run_id.to_string())
            .join("analysis.jsonl"),
        &analysis,
    )?;
    Ok(analysis)
}

fn redact_free_text(value: &str) -> String {
    static ABSOLUTE_PATH: OnceLock<regex::Regex> = OnceLock::new();
    static CREDENTIAL: OnceLock<regex::Regex> = OnceLock::new();
    let path = ABSOLUTE_PATH.get_or_init(|| {
        regex::Regex::new(r#"(?P<prefix>^|[\s\"'(:=])/(?:[^\s\"']+)"#)
            .expect("absolute path redaction regex")
    });
    let credential = CREDENTIAL.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:bearer\s+|(?:api[_-]?key|token|secret)\s*[:=]\s*)[^\s,;]+|sk-[A-Za-z0-9_-]{8,}",
        )
        .expect("credential redaction regex")
    });
    let normalized = path.replace_all(value, "${prefix}$$ABSOLUTE_PATH");
    credential
        .replace_all(&normalized, "$$REDACTED")
        .into_owned()
}

/// Verifies a recorded run without executing any provider or tool action.
///
/// # Errors
///
/// Returns an error when the diagnostic bundle cannot be parsed or validated.
pub fn replay_bundle(root: &Path, run_id: &str) -> Result<DiagnosticReplayReport> {
    let bundle = load_bundle(root, run_id)?;
    let mut expected = 0_u64;
    let mut gaps = Vec::new();
    let mut terminal_events = 0;
    for event in &bundle.events {
        if event.sequence != expected {
            gaps.push(expected);
            expected = event.sequence;
        }
        expected = expected.saturating_add(1);
        if matches!(
            event.kind,
            DiagnosticEventKind::Completed { .. }
                | DiagnosticEventKind::Failed { .. }
                | DiagnosticEventKind::BudgetPaused { .. }
        ) {
            terminal_events += 1;
        }
    }
    Ok(DiagnosticReplayReport {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        run_id: bundle.manifest.run_id,
        mode: "verification_only",
        valid: terminal_events > 0
            && gaps.is_empty()
            && bundle
                .events
                .iter()
                .all(|event| event.run_id == bundle.manifest.run_id),
        event_count: bundle.events.len(),
        sequence_gaps: gaps,
        terminal_events,
        executed_provider_requests: false,
        executed_tools: false,
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    if fs::metadata(path)?.len() > u64::try_from(MAX_DIAGNOSTIC_LINE_BYTES).unwrap_or(u64::MAX) {
        return Err(MimirError::Protocol(
            "diagnostic JSON file exceeds the 1 MiB limit".into(),
        ));
    }
    let file = fs::File::open(path)?;
    serde_json::from_reader(file).map_err(Into::into)
}

fn read_json_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::File::open(path) {
        Ok(file) => {
            if file.metadata()?.len() > u64::try_from(MAX_DIAGNOSTIC_LINE_BYTES).unwrap_or(u64::MAX)
            {
                return Err(MimirError::Protocol(
                    "diagnostic JSON file exceeds the 1 MiB limit".into(),
                ));
            }
            serde_json::from_reader(file).map(Some).map_err(Into::into)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_json_lines_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > MAX_EVENT_FILE_BYTES {
        return Err(MimirError::Protocol(
            "diagnostic JSONL file exceeds the 128 MiB limit".into(),
        ));
    }
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            return Err(MimirError::Protocol(
                "diagnostic record exceeds the 1 MiB limit".into(),
            ));
        }
        records.push(serde_json::from_str(&line)?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetKind, BudgetPause, BudgetSnapshot};
    use tempfile::TempDir;

    fn manifest(run_id: Uuid) -> DiagnosticManifest {
        DiagnosticManifest {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            run_id,
            session_id: "test-session".into(),
            started_at: Utc::now(),
            mimir_version: "test".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            workspace: "$WORKSPACE".into(),
            configuration: DiagnosticConfiguration {
                output_mode: "text".into(),
                offline: true,
                autonomous: false,
            },
            privacy: DiagnosticPrivacy::default(),
        }
    }

    #[test]
    fn journal_writes_portable_bundle_without_raw_payloads() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let journal = DiagnosticJournal::start(temporary.path().into(), manifest(run_id));
        let mut recorder = RuntimeDiagnosticRecorder::default();
        recorder.record(
            &journal,
            &RuntimeEvent::ToolStarted {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "/private/secret.txt", "token": "secret"}),
            },
        );
        recorder.record(
            &journal,
            &RuntimeEvent::Completed {
                text: "private model response".into(),
            },
        );
        journal.finish(DiagnosticOutcome::Completed);

        let bundle = load_bundle(temporary.path(), &run_id.to_string()).expect("bundle");
        assert_eq!(bundle.events.len(), 2);
        assert_eq!(
            bundle.summary.as_ref().expect("summary").outcome,
            DiagnosticOutcome::Completed
        );
        let serialized = serde_json::to_string(&bundle).expect("serialize bundle");
        assert!(!serialized.contains("/private/secret.txt"));
        assert!(!serialized.contains("private model response"));
        assert!(!serialized.contains("\"secret\""));
        assert!(serialized.contains("read_file"));
    }

    #[test]
    fn summary_separates_raw_cached_fresh_and_context_usage() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let journal = DiagnosticJournal::start(temporary.path().into(), manifest(run_id));
        let mut recorder = RuntimeDiagnosticRecorder::default();
        recorder.record(
            &journal,
            &RuntimeEvent::ProviderRequest {
                turn: 1,
                estimated_context_tokens: 139_500,
            },
        );
        let mut message = Message::assistant(
            vec![Content::Text { text: "ok".into() }],
            crate::model::StopReason::Stop,
        );
        message.usage = Usage {
            input_tokens: 140_000,
            cached_tokens: 133_000,
            output_tokens: 3_000,
        };
        recorder.record(&journal, &RuntimeEvent::MessageCompleted { message });
        recorder.record(&journal, &RuntimeEvent::Completed { text: "ok".into() });
        journal.finish(DiagnosticOutcome::Completed);

        let bundle = load_bundle(temporary.path(), &run_id.to_string()).expect("bundle");
        let summary = bundle.summary.expect("summary");
        assert_eq!(summary.input_tokens, 140_000);
        assert_eq!(summary.cached_tokens, 133_000);
        assert_eq!(summary.fresh_input_tokens, 7_000);
        assert_eq!(summary.output_tokens, 3_000);
        assert_eq!(summary.operational_tokens, 10_000);
        assert_eq!(summary.peak_context_tokens, 140_000);
        assert!(matches!(
            bundle.events[0].kind,
            DiagnosticEventKind::ProviderRequest {
                estimated_context_tokens: 139_500,
                ..
            }
        ));
    }

    #[test]
    fn prompt_run_collector_keeps_cancelled_and_budget_paused_outcomes_distinct() {
        let temporary = TempDir::new().expect("temporary directory");
        let mut collector =
            RuntimeDiagnosticRunCollector::new(temporary.path().into(), manifest(Uuid::nil()));

        collector.record(&RuntimeEvent::RunStarted);
        collector.record(&RuntimeEvent::Failed {
            message: "run cancelled by user".into(),
        });
        collector.record(&RuntimeEvent::RunStarted);
        collector.record(&RuntimeEvent::BudgetPaused {
            pause: BudgetPause {
                kind: BudgetKind::Tokens,
                limit: 1_000_000,
                usage: BudgetSnapshot {
                    turns: 7,
                    tool_calls: 8,
                    tokens: 1_006_975,
                    elapsed_ms: 191_951,
                    ..BudgetSnapshot::default()
                },
            },
        });
        collector.finish_open();

        let runs = list_runs(temporary.path()).expect("diagnostic runs");
        assert_eq!(runs.len(), 2);
        assert_ne!(runs[0].run_id, runs[1].run_id);
        let outcomes = runs
            .iter()
            .map(|run| run.outcome.expect("terminal outcome"))
            .collect::<Vec<_>>();
        assert!(outcomes.contains(&DiagnosticOutcome::BudgetPaused));
        assert!(outcomes.contains(&DiagnosticOutcome::Cancelled));
        for run in runs {
            let expected_outcome = run.outcome.expect("terminal outcome");
            let bundle = load_bundle(temporary.path(), &run.run_id.to_string()).expect("bundle");
            assert_eq!(
                bundle.summary.as_ref().expect("summary").outcome,
                expected_outcome
            );
            let replay = replay_bundle(temporary.path(), &run.run_id.to_string()).expect("replay");
            assert!(replay.valid);
            assert_eq!(replay.terminal_events, 1);
            assert_eq!(
                bundle
                    .events
                    .iter()
                    .filter(|event| matches!(event.kind, DiagnosticEventKind::RunStarted))
                    .count(),
                1
            );
            assert_eq!(
                bundle
                    .events
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        DiagnosticEventKind::Completed { .. }
                            | DiagnosticEventKind::Failed { .. }
                            | DiagnosticEventKind::BudgetPaused { .. }
                    ))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn analysis_is_append_only_and_evidence_is_scoped() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let journal = DiagnosticJournal::start(temporary.path().into(), manifest(run_id));
        journal.record(DiagnosticEventKind::RunStarted, None, None);
        journal.finish(DiagnosticOutcome::Completed);
        let event_id = load_bundle(temporary.path(), &run_id.to_string())
            .expect("bundle")
            .events[0]
            .event_id;
        append_analysis(
            temporary.path(),
            &run_id.to_string(),
            DiagnosticAnalysisInput {
                author: "codex".into(),
                finding: "process completion should be inspected".into(),
                confidence: Some(0.8),
                evidence_event_ids: vec![event_id],
                proposed_fix: None,
                verification: None,
            },
        )
        .expect("append analysis");
        assert_eq!(
            load_bundle(temporary.path(), &run_id.to_string())
                .expect("bundle")
                .analysis
                .len(),
            1
        );
    }

    #[test]
    fn replay_only_validates_and_never_executes() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let journal = DiagnosticJournal::start(temporary.path().into(), manifest(run_id));
        journal.record(DiagnosticEventKind::RunStarted, None, None);
        journal.record(
            DiagnosticEventKind::Completed {
                output: PayloadMetadata::from_bytes(b"done"),
            },
            None,
            None,
        );
        journal.finish(DiagnosticOutcome::Completed);
        let replay = replay_bundle(temporary.path(), &run_id.to_string()).expect("replay");
        assert!(replay.valid);
        assert_eq!(replay.mode, "verification_only");
        assert!(!replay.executed_provider_requests);
        assert!(!replay.executed_tools);
    }

    #[test]
    fn reader_synthesizes_an_incomplete_summary_without_terminal_write() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let manifest = manifest(run_id);
        let directory = temporary.path().join("runs").join(run_id.to_string());
        initialize_bundle(temporary.path(), &directory, &manifest).expect("initialize bundle");
        append_json_line(
            &directory.join("events.jsonl"),
            &DiagnosticEvent {
                schema_version: DIAGNOSTIC_SCHEMA_VERSION,
                redaction_version: DIAGNOSTIC_REDACTION_VERSION,
                event_id: Uuid::new_v4(),
                run_id,
                sequence: 0,
                recorded_at: Utc::now(),
                elapsed_ms: 10,
                turn_id: None,
                tool_call_id: None,
                kind: DiagnosticEventKind::RunStarted,
            },
        )
        .expect("append event");
        let bundle = load_bundle(temporary.path(), &run_id.to_string()).expect("bundle");
        let summary = bundle.summary.expect("synthetic summary");
        assert_eq!(summary.outcome, DiagnosticOutcome::Incomplete);
        assert!(summary.inferred_incomplete);
        assert_eq!(
            list_runs(temporary.path()).expect("runs")[0].outcome,
            Some(DiagnosticOutcome::Incomplete)
        );
    }

    #[test]
    fn reader_rejects_a_success_summary_without_a_terminal_event() {
        let temporary = TempDir::new().expect("temporary directory");
        let run_id = Uuid::new_v4();
        let journal = DiagnosticJournal::start(temporary.path().into(), manifest(run_id));
        journal.record(DiagnosticEventKind::RunStarted, None, None);
        journal.finish(DiagnosticOutcome::Completed);
        let summary = load_bundle(temporary.path(), &run_id.to_string())
            .expect("bundle")
            .summary
            .expect("summary");
        assert_eq!(summary.outcome, DiagnosticOutcome::Incomplete);
        assert!(summary.inferred_incomplete);
    }

    #[test]
    fn failure_metadata_uses_stable_typed_fields() {
        let metadata = error_metadata(
            "process timed out after 120000 ms at /Users/example/private",
            FailureComponent::Tool,
            FailureSeverity::Error,
        );
        assert_eq!(metadata.code, FailureCode::Timeout);
        assert_eq!(metadata.component, FailureComponent::Tool);
        assert_eq!(metadata.severity, FailureSeverity::Error);
        let event = DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            redaction_version: DIAGNOSTIC_REDACTION_VERSION,
            event_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            sequence: 0,
            recorded_at: Utc::now(),
            elapsed_ms: 0,
            turn_id: None,
            tool_call_id: None,
            kind: DiagnosticEventKind::Failed { error: metadata },
        };
        let json = serde_json::to_value(event).expect("event json");
        assert_eq!(json["redaction_version"], DIAGNOSTIC_REDACTION_VERSION);
        assert_eq!(json["error"]["code"], "timeout");
        assert_eq!(json["error"]["component"], "tool");
        assert_eq!(json["error"]["severity"], "error");
        assert!(!json.to_string().contains("/Users/example"));
    }

    #[test]
    fn reliability_failures_keep_domain_specific_codes() {
        let cases = [
            (
                "process execution timed out after 120000 ms",
                FailureCode::ProcessExecutionTimeout,
            ),
            (
                "output pipes remained open past the drain deadline",
                FailureCode::ProcessPipeDrainTimeout,
            ),
            (
                "tool run_process failed: spawn failed: executable missing",
                FailureCode::ProcessSpawnFailed,
            ),
            (
                "workspace policy denied path ../outside: parent traversal is not allowed",
                FailureCode::WorkspacePathInvalid,
            ),
            (
                "target is outside $WORKSPACE",
                FailureCode::WorkspaceTargetOutside,
            ),
            (
                "malformed SSE provider event",
                FailureCode::ProviderProtocol,
            ),
            (
                "provider authentication rejected after oauth refresh",
                FailureCode::ProviderAuthentication,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                error_metadata(message, FailureComponent::Runtime, FailureSeverity::Error).code,
                expected
            );
        }
    }
}
