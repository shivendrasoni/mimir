//! Project-scoped continual learning and signed fleet learning packs.
//!
//! The learning store deliberately contains structured evidence and proposed
//! harness edits, never raw prompts, source code, tool arguments, tool output,
//! or session trajectories. The coordinator is the sole writer of learning
//! state.

#![allow(
    clippy::missing_errors_doc,
    reason = "learning operations return typed validation, provider, transport, and durable-state errors"
)]

pub mod jobs;
pub mod policy;
pub mod projection;

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration as StdDuration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    atomic::{canonical_state_root, path_lock, prepare_state_path, read_json, write_json},
    diagnostics::{DiagnosticOutcome, DiagnosticSummary},
    error::{MimirError, Result},
    refinement::{RefinementEdit, RefinementKind},
    runtime::AgentRuntime,
    session::{FileSessionStore, SessionStore},
};

pub const LEARNING_SCHEMA_VERSION: u16 = 3;
pub const PROJECT_CANARY_RUNS: u8 = 3;
const MAX_EVIDENCE: usize = 1_000;
const MAX_CANDIDATES: usize = 256;
const MAX_PACK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningMode {
    Off,
    #[default]
    Observe,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    VerifiedSuccess,
    VerifiedFailure,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSignal {
    UserFeedback,
    ValidationGate,
    Correction,
    Rollback,
    Diagnostic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    #[default]
    Proposed,
    Validated,
    Canary,
    Active,
    Quarantined,
    RolledBack,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applicability {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub path_prefixes: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMarker {
    pub schema: u16,
    pub project_id: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningEvidence {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub session_alias: String,
    pub outcome: EvidenceOutcome,
    pub signal: EvidenceSignal,
    pub task_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_run_id: Option<Uuid>,
    #[serde(default)]
    pub metrics: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Only candidates actually exposed in this logical task may consume this
    /// outcome. Absence means the evidence is diagnostic only.
    #[serde(default)]
    pub exposed_candidate_ids: Vec<Uuid>,
}

/// Durable typed Jev evaluation metadata and its deterministic disposition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningEvaluation {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub session_alias: String,
    pub first_record_id: Uuid,
    pub last_record_id: Uuid,
    pub rubric_hash: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub lesson_kind: Option<String>,
    pub resolution: Option<String>,
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overfit_risk: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_correction_probability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categorical_confidence: Option<f64>,
    pub cluster_id: Option<Uuid>,
    #[serde(default)]
    pub exposed_candidate_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCluster {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub summary: String,
    pub lesson_kind: String,
    #[serde(default)]
    pub evaluation_ids: Vec<Uuid>,
    #[serde(default)]
    pub task_aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidate {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub summary: String,
    pub rationale: String,
    pub status: CandidateStatus,
    pub applicability: Applicability,
    pub edits: Vec<RefinementEdit>,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
    /// The one qualified evaluation cluster this candidate was synthesized from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cluster_id: Option<Uuid>,
    #[serde(default)]
    pub canary_outcomes: Vec<EvidenceOutcome>,
    pub canary_runs_required: u8,
    #[serde(default)]
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningState {
    pub schema: u16,
    pub generation: u64,
    pub mode: LearningMode,
    pub contribution_enabled: bool,
    #[serde(default = "Uuid::new_v4")]
    pub contributor_id: Uuid,
    #[serde(default)]
    pub evidence: Vec<LearningEvidence>,
    #[serde(default)]
    pub evaluations: Vec<LearningEvaluation>,
    #[serde(default)]
    pub clusters: Vec<LearningCluster>,
    #[serde(default)]
    pub candidates: Vec<LearningCandidate>,
    #[serde(default)]
    pub feedback_requested_for: Option<Uuid>,
    #[serde(default)]
    pub pinned_fleet_version: Option<String>,
}

impl Default for LearningState {
    fn default() -> Self {
        Self {
            schema: LEARNING_SCHEMA_VERSION,
            generation: 0,
            // The lifecycle is automatic when Jev is available. With
            // TypeSafe off, its evaluation is disabled and no candidate can
            // advance, so this remains a safe no-flag default.
            mode: LearningMode::Auto,
            contribution_enabled: false,
            contributor_id: Uuid::new_v4(),
            evidence: Vec::new(),
            evaluations: Vec::new(),
            clusters: Vec::new(),
            candidates: Vec::new(),
            feedback_requested_for: None,
            pinned_fleet_version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetLearningEntry {
    pub id: String,
    pub kind: RefinementKind,
    pub title: String,
    pub content: String,
    pub path: String,
    #[serde(default)]
    pub applicability: Applicability,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub reference: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub arguments: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetLearningPack {
    pub schema: u16,
    pub version: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub minimum_mimir_version: String,
    #[serde(default)]
    pub entries: Vec<FleetLearningEntry>,
    #[serde(default)]
    pub revoked_entry_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedLearningPack {
    pub pack: FleetLearningPack,
    pub sha256: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetContribution {
    pub schema: u16,
    pub contributor_alias: String,
    pub candidate_id: Uuid,
    pub summary: String,
    pub rationale: String,
    pub applicability: Applicability,
    pub edit_kinds: Vec<RefinementKind>,
    pub verified_successes: usize,
    pub verified_failures: usize,
    pub evidence_hashes: Vec<String>,
    pub mimir_version: String,
}

#[derive(Debug, Deserialize)]
struct CandidateDraft {
    summary: String,
    rationale: String,
    #[serde(default)]
    applicability: Applicability,
    edits: Vec<RefinementEdit>,
}

#[derive(Debug, Deserialize)]
struct CriticDecision {
    approved: bool,
    #[serde(default)]
    reason: String,
}

const LEARNING_PROPOSER_PROMPT: &str = r#"You are Mimir's project learning proposer.
Return JSON only: {"summary":"...","rationale":"...","applicability":{"tags":[],"path_prefixes":[],"languages":[]},"edits":[...]}. The edit shape is the same as /refine. Propose at most 4 small evidence-backed edits. Never include source code, secrets, absolute paths, tool permissions, dependency installation, or base-system-prompt changes. A completed run alone is not proof of success."#;

const LEARNING_CRITIC_PROMPT: &str = r#"You are Mimir's independent project learning critic. Review the candidate for overfitting, unsupported claims, secret or project-identity leakage, executable payloads, permission expansion, and conflicts with its evidence. Return JSON only: {"approved":true|false,"reason":"..."}. Reject uncertain candidates."#;

#[allow(
    clippy::too_many_lines,
    reason = "one explicit bounded-cluster synthesis transaction keeps provider inputs auditable"
)]
pub async fn propose_project_candidate(
    runtime: &AgentRuntime,
    workspace: &Path,
) -> Result<LearningCandidate> {
    let project_root = discover_project_root(workspace)?;
    ensure_project_marker(&project_root).await?;
    let state = load_learning_state(&project_root).await?;
    if state.mode == LearningMode::Off {
        return Err(MimirError::Configuration(
            "project learning is disabled".into(),
        ));
    }
    let cluster = qualified_cluster(&state).ok_or_else(|| {
        MimirError::Configuration(
            "no qualified learning cluster; retain two distinct safe task evaluations, or one strong validated correction".into(),
        )
    })?;
    if state
        .candidates
        .iter()
        .any(|candidate| candidate.source_cluster_id == Some(cluster.id))
    {
        return Err(MimirError::Configuration(
            "a candidate already exists for the qualified learning cluster".into(),
        ));
    }
    let cluster_evaluations = state
        .evaluations
        .iter()
        .filter(|evaluation| evaluation.cluster_id == Some(cluster.id))
        .map(|evaluation| {
            serde_json::json!({
                "evaluation_id": evaluation.id,
                "lesson_kind": evaluation.lesson_kind,
                "resolution": evaluation.resolution,
                "scope": evaluation.scope,
                "reuse_value": evaluation.reuse_value,
                "overfit_risk": evaluation.overfit_risk,
                "categorical_confidence": evaluation.categorical_confidence,
            })
        })
        .collect::<Vec<_>>();
    let evidence = state
        .evidence
        .iter()
        .filter(|item| cluster.task_aliases.contains(&item.session_alias))
        .cloned()
        .collect::<Vec<_>>();
    let evidence_summary = evidence
        .iter()
        .map(|item| serde_json::json!({
            "id": item.id, "outcome": item.outcome, "signal": item.signal, "metrics": item.metrics,
        }))
        .collect::<Vec<_>>();
    let prompt = format!(
        "<qualified_cluster>{}</qualified_cluster>\n<bounded_evaluations>{}</bounded_evaluations>\n<verified_evidence>{}</verified_evidence>\nReturn JSON only.",
        serde_json::to_string(cluster)?,
        serde_json::to_string(&cluster_evaluations)?,
        serde_json::to_string(&evidence_summary)?,
    );
    let proposal = runtime
        .complete_control_request(LEARNING_PROPOSER_PROMPT, &prompt, 8_000)
        .await?;
    let draft: CandidateDraft = parse_json_response(&proposal, "learning proposer")?;
    let mut candidate = LearningCandidate {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        summary: draft.summary,
        rationale: draft.rationale,
        status: CandidateStatus::Proposed,
        applicability: draft.applicability,
        edits: draft.edits,
        evidence_ids: evidence.iter().map(|item| item.id).collect(),
        source_cluster_id: Some(cluster.id),
        canary_outcomes: Vec::new(),
        canary_runs_required: PROJECT_CANARY_RUNS,
        rejection_reason: None,
    };
    validate_candidate(&candidate)?;
    let critic_prompt = format!(
        "<candidate>{}</candidate>\n<qualified_cluster>{}</qualified_cluster>\n<bounded_evaluations>{}</bounded_evaluations>\n<verified_evidence>{}</verified_evidence>\nReturn JSON only.",
        serde_json::to_string(&candidate)?,
        serde_json::to_string(cluster)?,
        serde_json::to_string(&cluster_evaluations)?,
        serde_json::to_string(&evidence_summary)?
    );
    let review = runtime
        .complete_control_request(LEARNING_CRITIC_PROMPT, &critic_prompt, 2_000)
        .await?;
    let decision: CriticDecision = parse_json_response(&review, "learning critic")?;
    if decision.approved {
        // The provider may judge content but never lifecycle. Rust validates the
        // bounded Memory-only edit and deterministically advances it to canary.
        candidate.status = CandidateStatus::Validated;
        candidate.status = CandidateStatus::Canary;
    } else {
        candidate.status = CandidateStatus::Quarantined;
        candidate.rejection_reason = Some(if decision.reason.is_empty() {
            "Independent critic rejected the candidate".into()
        } else {
            decision.reason
        });
    }
    save_candidate(&project_root, candidate.clone()).await?;
    Ok(candidate)
}

/// Returns a single cluster that is sufficiently repeatable for provider
/// synthesis. This is deterministic policy: a provider cannot select the data
/// it is allowed to generalize from.
fn qualified_cluster(state: &LearningState) -> Option<&LearningCluster> {
    state.clusters.iter().find(|cluster| {
        let evaluations = state
            .evaluations
            .iter()
            .filter(|evaluation| evaluation.cluster_id == Some(cluster.id))
            .collect::<Vec<_>>();
        let strong_correction = evaluations.iter().any(|evaluation| {
            evaluation.lesson_kind.as_deref() == Some("correction")
                && evaluation.resolution.as_deref() == Some("validated")
                && evaluation
                    .human_correction_probability
                    .is_some_and(|value| value >= 0.85)
                && evaluation
                    .categorical_confidence
                    .is_some_and(|value| value >= 0.75)
        });
        strong_correction || cluster.task_aliases.len() >= 2
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningStatus {
    pub schema: u16,
    pub project_root: PathBuf,
    pub project_id: Option<Uuid>,
    pub mode: LearningMode,
    pub contribution_enabled: bool,
    pub evidence_count: usize,
    pub candidate_counts: BTreeMap<String, usize>,
    pub feedback_requested: bool,
    pub active_fleet_version: Option<String>,
    pub pinned_fleet_version: Option<String>,
}

pub fn discover_project_root(workspace: &Path) -> Result<PathBuf> {
    let workspace = std::fs::canonicalize(workspace).map_err(|error| {
        MimirError::Configuration(format!("workspace is inaccessible: {error}"))
    })?;
    // A marker outside the nearest repository cannot describe that repository.
    // This prevents an umbrella checkout from claiming nested, independent repos.
    let git_root = workspace
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf);
    for ancestor in workspace.ancestors() {
        if let Some(git_root) = git_root.as_deref()
            && !ancestor.starts_with(git_root)
        {
            break;
        }
        let marker = ancestor.join(".mimir/project.json");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(MimirError::Configuration(format!(
                    "project marker must not be a symlink: {}",
                    marker.display()
                )));
            }
            Ok(metadata) if metadata.is_file() => return Ok(ancestor.to_path_buf()),
            Ok(_) => {
                return Err(MimirError::Configuration(format!(
                    "project marker must be a regular file: {}",
                    marker.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Some(ancestor) == git_root.as_deref() {
            break;
        }
    }
    Ok(git_root.unwrap_or(workspace))
}

/// Returns a stable, non-identifying namespace for project-owned session data.
///
/// The canonical project path is hashed rather than copied into global state.
/// Moving a checkout intentionally creates a new isolation boundary; explicit
/// session export/import remains the portability mechanism.
pub fn project_storage_key(project_root: &Path) -> Result<String> {
    let root = discover_project_root(project_root)?;
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    Ok(digest[..16]
        .iter()
        .fold(String::with_capacity(32), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        }))
}

/// Resolves the global-state namespace used for one project's transcripts.
pub fn project_session_root(state_root: &Path, workspace: &Path) -> Result<PathBuf> {
    let state_root = canonical_state_root(state_root);
    Ok(state_root
        .join("projects")
        .join(project_storage_key(workspace)?))
}

pub async fn ensure_project_marker(project_root: &Path) -> Result<ProjectMarker> {
    let project_root = std::fs::canonicalize(project_root)?;
    let marker_path = project_marker_path(&project_root);
    prepare_state_path(&project_root, &marker_path).await?;
    let lock = path_lock(&marker_path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&marker_path).await?;
    if let Some(marker) = read_json::<ProjectMarker>(&marker_path).await? {
        if marker.schema != LEARNING_SCHEMA_VERSION {
            return Err(MimirError::Configuration(format!(
                "unsupported project learning schema {}",
                marker.schema
            )));
        }
        ensure_git_exclude(&project_root)?;
        return Ok(marker);
    }
    let marker = ProjectMarker {
        schema: LEARNING_SCHEMA_VERSION,
        project_id: Uuid::new_v4(),
        created_at: Utc::now(),
    };
    write_json(&marker_path, &marker).await?;
    set_private_permissions(&marker_path).await?;
    ensure_git_exclude(&project_root)?;
    Ok(marker)
}

#[must_use]
pub fn project_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".mimir/project.json")
}

#[must_use]
pub fn project_learning_dir(project_root: &Path) -> PathBuf {
    project_root.join(".mimir/learning")
}

#[must_use]
pub fn project_harness_path(project_root: &Path) -> PathBuf {
    project_learning_dir(project_root).join("harness/harness_state.json")
}

#[must_use]
pub fn fleet_root(state_root: &Path) -> PathBuf {
    canonical_state_root(state_root).join("learning/fleet")
}

#[must_use]
pub fn active_fleet_pack_path(state_root: &Path) -> PathBuf {
    fleet_root(state_root).join("active.json")
}

fn fleet_public_key_path(state_root: &Path) -> PathBuf {
    fleet_root(state_root).join("trusted_ed25519_key.json")
}

pub async fn load_learning_state(project_root: &Path) -> Result<LearningState> {
    let path = project_learning_dir(project_root).join("state.json");
    if !path.exists() {
        return Ok(LearningState::default());
    }
    prepare_state_path(project_root, &path).await?;
    let mut state: LearningState = read_json(&path).await?.unwrap_or_default();
    // Mimir is unlaunched: schema 2 has a compact, lossless in-place upgrade.
    // Earlier/unknown schemas fail closed rather than guessing at local state.
    if state.schema == 2 {
        state.schema = LEARNING_SCHEMA_VERSION;
    }
    validate_learning_state(&state)?;
    Ok(state)
}

pub async fn initialize_project(workspace: &Path) -> Result<ProjectMarker> {
    let project_root = discover_project_root(workspace)?;
    let marker = ensure_project_marker(&project_root).await?;
    let state_path = project_learning_dir(&project_root).join("state.json");
    prepare_state_path(&project_root, &state_path).await?;
    if !state_path.exists() {
        write_learning_state(&project_root, &LearningState::default()).await?;
    }
    Ok(marker)
}

pub async fn learning_status(workspace: &Path, state_root: &Path) -> Result<LearningStatus> {
    let project_root = discover_project_root(workspace)?;
    let marker = read_json::<ProjectMarker>(&project_marker_path(&project_root)).await?;
    let state = load_learning_state(&project_root).await?;
    let mut candidate_counts = BTreeMap::new();
    for candidate in &state.candidates {
        *candidate_counts
            .entry(format!("{:?}", candidate.status).to_ascii_lowercase())
            .or_default() += 1;
    }
    let active_fleet_version = load_active_fleet_pack(state_root)
        .await?
        .map(|pack| pack.pack.version);
    Ok(LearningStatus {
        schema: LEARNING_SCHEMA_VERSION,
        project_root,
        project_id: marker.map(|value| value.project_id),
        mode: state.mode,
        contribution_enabled: state.contribution_enabled,
        evidence_count: state.evidence.len(),
        candidate_counts,
        feedback_requested: state.feedback_requested_for.is_some(),
        active_fleet_version,
        pinned_fleet_version: state.pinned_fleet_version,
    })
}

pub async fn set_contribution(workspace: &Path, enabled: bool) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    mutate_learning_state(&root, |state| state.contribution_enabled = enabled).await
}

pub async fn set_mode(workspace: &Path, mode: LearningMode) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    mutate_learning_state(&root, |state| state.mode = mode).await
}

pub async fn record_evidence(
    workspace: &Path,
    evidence: LearningEvidence,
) -> Result<LearningState> {
    validate_evidence(&evidence)?;
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    mutate_learning_state(&root, move |state| {
        if state.evidence.iter().all(|item| item.id != evidence.id) {
            state.evidence.push(evidence);
            trim_front(&mut state.evidence, MAX_EVIDENCE);
        }
    })
    .await
}

/// Records a verified result for candidates that were actually assembled into
/// one logical task's harness context. Callers must never infer exposure from a
/// candidate's current status; the supplied IDs are immutable task provenance.
pub async fn record_attributed_outcome(
    workspace: &Path,
    session_id: &str,
    exposed_candidate_ids: &[Uuid],
    outcome: EvidenceOutcome,
    signal: EvidenceSignal,
) -> Result<LearningState> {
    if exposed_candidate_ids.is_empty()
        || !matches!(
            outcome,
            EvidenceOutcome::VerifiedSuccess | EvidenceOutcome::VerifiedFailure
        )
        || !matches!(
            signal,
            EvidenceSignal::ValidationGate | EvidenceSignal::Correction
        )
    {
        return load_learning_state(&discover_project_root(workspace)?).await;
    }
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    let ids = exposed_candidate_ids.to_vec();
    let session_alias = stable_alias(session_id);
    mutate_learning_state(&root, move |state| {
        let mut applied = Vec::new();
        for candidate in state.candidates.iter_mut().filter(|candidate| {
            ids.contains(&candidate.id)
                && matches!(
                    candidate.status,
                    CandidateStatus::Canary | CandidateStatus::Active
                )
        }) {
            candidate.canary_outcomes.push(outcome);
            candidate.updated_at = Utc::now();
            update_candidate_status(candidate);
            applied.push(candidate.id);
        }
        if !applied.is_empty() {
            state.evidence.push(LearningEvidence {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                session_alias,
                outcome,
                signal,
                task_fingerprint: stable_alias("attributed-outcome"),
                diagnostic_run_id: None,
                metrics: BTreeMap::new(),
                note: None,
                exposed_candidate_ids: applied,
            });
            trim_front(&mut state.evidence, MAX_EVIDENCE);
        }
    })
    .await
}

/// Enqueues a completed logical task by record references only. Queue failures
/// are returned to the caller so foreground dispatch can intentionally ignore
/// them; no transcript is copied into learning storage.
pub async fn enqueue_completed_task(
    workspace: &Path,
    session_id: &str,
    first_record_id: Uuid,
    last_record_id: Uuid,
    exposed_candidate_ids: Vec<Uuid>,
) -> Result<jobs::LearningJob> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    jobs::enqueue(
        &root,
        session_id,
        first_record_id,
        last_record_id,
        exposed_candidate_ids,
    )
    .await
}

/// Opportunistically processes one queued evaluation. Service, projection, and
/// persistence failures are isolated to the job and never alter task success.
#[allow(
    clippy::too_many_lines,
    reason = "one queue claim, projection, typed evaluation, and durable commit transaction"
)]
pub async fn process_one_learning_job(
    runtime: &AgentRuntime,
    workspace: &Path,
    state_root: &Path,
) -> Result<bool> {
    let root = discover_project_root(workspace)?;
    let Some(job) = jobs::claim(&root).await? else {
        return Ok(false);
    };
    let result = async {
        let session_root = project_session_root(state_root, &root)?;
        let store = FileSessionStore::create(&session_root, &job.session_id).await?;
        let records = store.load().await?.records;
        let projection = projection::project_records(
            &job.session_id,
            &records,
            job.first_record_id,
            job.last_record_id,
            root.to_str(),
            job.exposed_candidate_ids.clone(),
        )
        .ok_or_else(|| {
            MimirError::Configuration("queued learning record span is unavailable".into())
        })?;
        let state = load_learning_state(&root).await?;
        if state.mode == LearningMode::Off {
            return Ok(());
        }
        let options = state
            .clusters
            .iter()
            .take(32)
            .map(|cluster| cluster.summary.clone())
            .collect::<Vec<_>>();
        let evaluation = runtime
            .evaluate_learning(&serde_json::to_value(&projection)?, &options)
            .await;
        let attributed_outcome = automatic_attributed_outcome(&evaluation, &projection);
        let outcome_session_id = job.session_id.clone();
        let outcome_candidate_ids = job.exposed_candidate_ids.clone();
        let disposition = policy::disposition(&evaluation);
        let updated = mutate_learning_state(&root, move |state| {
            let evaluation_id = Uuid::new_v4();
            let task_alias = stable_alias(&job.session_id);
            let cluster_id = match disposition {
                policy::EvaluationDisposition::Discard => None,
                policy::EvaluationDisposition::MatchCluster => state
                    .clusters
                    .iter_mut()
                    .find(|cluster| {
                        Some(cluster.summary.as_str()) == evaluation.cluster_match.as_deref()
                    })
                    .map(|cluster| {
                        cluster.evaluation_ids.push(evaluation_id);
                        if !cluster.task_aliases.contains(&task_alias) {
                            cluster.task_aliases.push(task_alias.clone());
                        }
                        cluster.id
                    }),
                policy::EvaluationDisposition::NewCluster => {
                    let id = Uuid::new_v4();
                    state.clusters.push(LearningCluster {
                        id,
                        created_at: Utc::now(),
                        summary: format!(
                            "{}: {}",
                            evaluation.lesson_kind.clone().unwrap_or_default(),
                            evaluation.scope.clone().unwrap_or_default()
                        ),
                        lesson_kind: evaluation
                            .lesson_kind
                            .clone()
                            .unwrap_or_else(|| "none".into()),
                        evaluation_ids: vec![evaluation_id],
                        task_aliases: vec![task_alias.clone()],
                    });
                    Some(id)
                }
            };
            state.evaluations.push(LearningEvaluation {
                id: evaluation_id,
                created_at: Utc::now(),
                session_alias: stable_alias(&job.session_id),
                first_record_id: job.first_record_id,
                last_record_id: job.last_record_id,
                rubric_hash: evaluation.rubric_hash,
                model: evaluation.model,
                input_tokens: evaluation.input_tokens,
                output_tokens: evaluation.output_tokens,
                latency_ms: evaluation.latency_ms,
                lesson_kind: evaluation.lesson_kind,
                resolution: evaluation.resolution,
                scope: evaluation.scope,
                reuse_value: evaluation.reuse_value,
                overfit_risk: evaluation.overfit_risk,
                human_correction_probability: evaluation.human_correction_probability,
                categorical_confidence: evaluation.categorical_confidence,
                cluster_id,
                exposed_candidate_ids: job.exposed_candidate_ids,
            });
            trim_front(&mut state.evaluations, MAX_EVIDENCE);
        })
        .await?;
        // Synthesis is an optional second background step. Evaluation has
        // already been durably committed, so a provider outage cannot cause a
        // task to be retried or lose its bounded observation.
        if updated.mode == LearningMode::Auto && qualified_cluster(&updated).is_some() {
            let _ = propose_project_candidate(runtime, &root).await;
        }
        if let Some((outcome, signal)) = attributed_outcome {
            let _ = record_attributed_outcome(
                &root,
                &outcome_session_id,
                &outcome_candidate_ids,
                outcome,
                signal,
            )
            .await?;
        }
        Ok(())
    }
    .await;
    jobs::complete(&root, job.id, result.is_ok()).await?;
    result.map(|()| true)
}

fn automatic_attributed_outcome(
    evaluation: &crate::typesafe::TypeSafeLearningEvaluation,
    projection: &projection::LogicalTaskProjection,
) -> Option<(EvidenceOutcome, EvidenceSignal)> {
    if projection.exposed_candidate_ids.is_empty() {
        return None;
    }
    if policy::correction_is_strong(evaluation) {
        return Some((EvidenceOutcome::VerifiedFailure, EvidenceSignal::Correction));
    }
    (evaluation.resolution.as_deref() == Some("validated")
        && !projection.validation_evidence.is_empty())
    .then_some((
        EvidenceOutcome::VerifiedSuccess,
        EvidenceSignal::ValidationGate,
    ))
}

pub async fn record_diagnostic_evidence(
    workspace: &Path,
    session_id: &str,
    summary: &DiagnosticSummary,
) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    let session_alias = stable_alias(session_id);
    let outcome = match summary.outcome {
        DiagnosticOutcome::Failed | DiagnosticOutcome::Crashed => EvidenceOutcome::VerifiedFailure,
        DiagnosticOutcome::Completed
        | DiagnosticOutcome::Cancelled
        | DiagnosticOutcome::BudgetPaused
        | DiagnosticOutcome::Incomplete => EvidenceOutcome::Ambiguous,
    };
    let metrics = BTreeMap::from([
        ("duration_ms".into(), summary.duration_ms),
        ("provider_requests".into(), summary.provider_requests),
        ("retries".into(), summary.retries),
        ("tool_calls".into(), summary.tool_calls),
        ("tool_failures".into(), summary.tool_failures),
        ("operational_tokens".into(), summary.operational_tokens),
    ]);
    let evidence = LearningEvidence {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        session_alias,
        outcome,
        signal: EvidenceSignal::Diagnostic,
        task_fingerprint: stable_alias(&summary.run_id.to_string()),
        diagnostic_run_id: Some(summary.run_id),
        metrics,
        note: None,
        exposed_candidate_ids: Vec::new(),
    };
    mutate_learning_state(&root, move |state| {
        if state.mode == LearningMode::Off
            || state
                .evidence
                .iter()
                .any(|item| item.diagnostic_run_id == Some(summary.run_id))
        {
            return;
        }
        state.evidence.push(evidence.clone());
        trim_front(&mut state.evidence, MAX_EVIDENCE);
    })
    .await
}

pub async fn record_runtime_failure(workspace: &Path, session_id: &str) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    let evidence = LearningEvidence {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        session_alias: stable_alias(session_id),
        outcome: EvidenceOutcome::VerifiedFailure,
        signal: EvidenceSignal::Diagnostic,
        task_fingerprint: stable_alias(&format!("failure-{session_id}")),
        diagnostic_run_id: None,
        metrics: BTreeMap::new(),
        note: None,
        exposed_candidate_ids: Vec::new(),
    };
    mutate_learning_state(&root, move |state| {
        if state.mode == LearningMode::Off {
            return;
        }
        state.evidence.push(evidence.clone());
        trim_front(&mut state.evidence, MAX_EVIDENCE);
    })
    .await
}

pub async fn record_feedback(
    workspace: &Path,
    session_id: &str,
    achieved: bool,
    note: Option<&str>,
) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    let note = sanitize_note(note)?;
    let session_alias = stable_alias(session_id);
    let outcome = if achieved {
        EvidenceOutcome::VerifiedSuccess
    } else {
        EvidenceOutcome::VerifiedFailure
    };
    mutate_learning_state(&root, move |state| {
        let target = state.feedback_requested_for.take();
        let evidence = LearningEvidence {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            session_alias,
            outcome,
            signal: if achieved {
                EvidenceSignal::UserFeedback
            } else {
                EvidenceSignal::Correction
            },
            task_fingerprint: target.map_or_else(|| "unspecified".into(), |id| id.to_string()),
            diagnostic_run_id: None,
            metrics: BTreeMap::new(),
            note,
            exposed_candidate_ids: target.into_iter().collect(),
        };
        state.evidence.push(evidence.clone());
        trim_front(&mut state.evidence, MAX_EVIDENCE);
        if let Some(candidate_id) = target
            && let Some(candidate) = state
                .candidates
                .iter_mut()
                .find(|candidate| candidate.id == candidate_id)
        {
            candidate.canary_outcomes.push(outcome);
            candidate.updated_at = Utc::now();
            update_candidate_status(candidate);
            candidate.evidence_ids.push(evidence.id);
        }
    })
    .await
}

pub async fn save_candidate(
    workspace: &Path,
    mut candidate: LearningCandidate,
) -> Result<LearningState> {
    validate_candidate(&candidate)?;
    let root = discover_project_root(workspace)?;
    ensure_project_marker(&root).await?;
    candidate.updated_at = Utc::now();
    mutate_learning_state(&root, move |state| {
        if let Some(existing) = state
            .candidates
            .iter_mut()
            .find(|item| item.id == candidate.id)
        {
            *existing = candidate;
        } else {
            state.candidates.push(candidate);
            trim_front(&mut state.candidates, MAX_CANDIDATES);
        }
    })
    .await
}

pub async fn request_candidate_feedback(
    workspace: &Path,
    candidate_id: Uuid,
) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    mutate_learning_state(&root, |state| {
        if state.candidates.iter().any(|item| {
            item.id == candidate_id
                && matches!(
                    item.status,
                    CandidateStatus::Validated | CandidateStatus::Canary
                )
        }) {
            state.feedback_requested_for = Some(candidate_id);
        }
    })
    .await
}

/// Automatic feedback prompts are intentionally absent. `/learn feedback` is
/// retained as an explicit manual override for users who choose to provide it.
#[allow(
    clippy::unused_async,
    reason = "keeps the existing TUI command contract stable"
)]
pub async fn request_feedback_if_informative(_workspace: &Path) -> Result<bool> {
    Ok(false)
}

pub async fn rollback_candidate(workspace: &Path, candidate_id: Uuid) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    mutate_learning_state(&root, |state| {
        let mut rolled_back = false;
        if let Some(candidate) = state
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == candidate_id)
        {
            candidate.status = CandidateStatus::RolledBack;
            candidate.updated_at = Utc::now();
            candidate.rejection_reason = Some("Explicit rollback".into());
            rolled_back = true;
        }
        if state.feedback_requested_for == Some(candidate_id) {
            state.feedback_requested_for = None;
        }
        if rolled_back {
            state.evidence.push(LearningEvidence {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                session_alias: stable_alias("coordinator"),
                outcome: EvidenceOutcome::VerifiedFailure,
                signal: EvidenceSignal::Rollback,
                task_fingerprint: candidate_id.to_string(),
                diagnostic_run_id: None,
                metrics: BTreeMap::new(),
                note: None,
                exposed_candidate_ids: vec![candidate_id],
            });
            trim_front(&mut state.evidence, MAX_EVIDENCE);
        }
    })
    .await
}

pub fn build_fleet_contribution(
    candidate: &LearningCandidate,
    evidence: &[LearningEvidence],
    contributor_id: Uuid,
) -> Result<FleetContribution> {
    validate_candidate(candidate)?;
    validate_public_text(&candidate.summary)?;
    let linked = evidence
        .iter()
        .filter(|item| candidate.evidence_ids.contains(&item.id))
        .collect::<Vec<_>>();
    let verified_successes = linked
        .iter()
        .filter(|item| item.outcome == EvidenceOutcome::VerifiedSuccess)
        .count();
    let verified_failures = linked
        .iter()
        .filter(|item| item.outcome == EvidenceOutcome::VerifiedFailure)
        .count();
    let evidence_hashes = linked
        .iter()
        .map(|item| {
            let encoded = serde_json::to_vec(item).unwrap_or_default();
            hex_digest(&encoded)
        })
        .collect();
    Ok(FleetContribution {
        schema: LEARNING_SCHEMA_VERSION,
        contributor_alias: stable_alias(&format!(
            "{}-{}",
            contributor_id,
            Utc::now().format("%Y-%m")
        )),
        candidate_id: candidate.id,
        summary: candidate.summary.clone(),
        rationale: format!(
            "Supported by {verified_successes} verified successes and {verified_failures} verified failures."
        ),
        applicability: Applicability {
            tags: candidate
                .applicability
                .tags
                .iter()
                .filter(|tag| validate_public_text(tag).is_ok())
                .take(16)
                .cloned()
                .collect(),
            path_prefixes: Vec::new(),
            languages: candidate
                .applicability
                .languages
                .iter()
                .filter(|language| validate_public_text(language).is_ok())
                .take(16)
                .cloned()
                .collect(),
        },
        edit_kinds: candidate.edits.iter().map(|edit| edit.kind).collect(),
        verified_successes,
        verified_failures,
        evidence_hashes,
        mimir_version: env!("CARGO_PKG_VERSION").into(),
    })
}

pub async fn submit_fleet_contribution(
    workspace: &Path,
    endpoint: &str,
    candidate_id: Uuid,
) -> Result<()> {
    if !endpoint.starts_with("https://") {
        return Err(MimirError::Configuration(
            "fleet contribution endpoint must use HTTPS".into(),
        ));
    }
    let project_root = discover_project_root(workspace)?;
    let state = load_learning_state(&project_root).await?;
    if !state.contribution_enabled {
        return Err(MimirError::Configuration(
            "fleet contribution is disabled; enable it explicitly first".into(),
        ));
    }
    let candidate = state
        .candidates
        .iter()
        .find(|candidate| candidate.id == candidate_id)
        .ok_or_else(|| MimirError::Configuration("learning candidate was not found".into()))?;
    if candidate.status != CandidateStatus::Active {
        return Err(MimirError::Configuration(
            "only an active, verified project candidate may be contributed".into(),
        ));
    }
    let contribution = build_fleet_contribution(candidate, &state.evidence, state.contributor_id)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| {
            MimirError::Configuration(format!("fleet contribution client failed: {error}"))
        })?;
    let response = client
        .post(endpoint)
        .json(&contribution)
        .send()
        .await
        .map_err(|error| {
            MimirError::Configuration(format!("fleet contribution failed: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(MimirError::Configuration(format!(
            "fleet contribution returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

pub fn verify_signed_pack(
    envelope: &SignedLearningPack,
    public_key: &[u8],
    now: DateTime<Utc>,
) -> Result<()> {
    validate_pack(&envelope.pack, now)?;
    let encoded = serde_json::to_vec(&envelope.pack)?;
    let digest = hex_digest(&encoded);
    if !constant_time_eq(digest.as_bytes(), envelope.sha256.as_bytes()) {
        return Err(MimirError::Configuration(
            "fleet learning pack digest does not match its contents".into(),
        ));
    }
    let signature = STANDARD
        .decode(envelope.signature.as_bytes())
        .map_err(|_| {
            MimirError::Configuration("fleet learning pack signature is not valid base64".into())
        })?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&encoded, &signature)
        .map_err(|_| MimirError::Configuration("fleet learning pack signature is invalid".into()))
}

pub async fn install_signed_pack(
    state_root: &Path,
    envelope: &SignedLearningPack,
    public_key: &[u8],
) -> Result<PathBuf> {
    verify_signed_pack(envelope, public_key, Utc::now())?;
    let root = canonical_state_root(state_root);
    let fleet = fleet_root(&root);
    let version_path = fleet
        .join("versions")
        .join(format!("{}.json", safe_segment(&envelope.pack.version)?));
    let active_path = active_fleet_pack_path(&root);
    let key_path = fleet_public_key_path(&root);
    prepare_state_path(&root, &version_path).await?;
    prepare_state_path(&root, &active_path).await?;
    prepare_state_path(&root, &key_path).await?;
    let lock = path_lock(&active_path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&active_path).await?;
    if let Some(existing_key) = read_json::<Vec<u8>>(&key_path).await? {
        if !constant_time_eq(&existing_key, public_key) {
            return Err(MimirError::Configuration(
                "fleet learning signing key differs from the trusted cached key".into(),
            ));
        }
    } else {
        write_json(&key_path, public_key).await?;
        set_private_permissions(&key_path).await?;
    }
    write_json(&version_path, envelope).await?;
    write_json(&active_path, envelope).await?;
    set_private_permissions(&version_path).await?;
    set_private_permissions(&active_path).await?;
    Ok(active_path)
}

pub async fn fetch_and_install_signed_pack(
    state_root: &Path,
    endpoint: &str,
    public_key_base64: &str,
) -> Result<PathBuf> {
    if !endpoint.starts_with("https://") {
        return Err(MimirError::Configuration(
            "fleet learning endpoint must use HTTPS".into(),
        ));
    }
    let public_key = STANDARD.decode(public_key_base64.as_bytes()).map_err(|_| {
        MimirError::Configuration("fleet learning public key is not valid base64".into())
    })?;
    if public_key.len() != 32 {
        return Err(MimirError::Configuration(
            "fleet learning Ed25519 public key must be 32 bytes".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| MimirError::Configuration(format!("fleet pack client failed: {error}")))?;
    let response = client.get(endpoint).send().await.map_err(|error| {
        MimirError::Configuration(format!("fleet pack download failed: {error}"))
    })?;
    if !response.status().is_success() {
        return Err(MimirError::Configuration(format!(
            "fleet pack download returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACK_BYTES as u64)
    {
        return Err(MimirError::Configuration(
            "fleet learning pack exceeds the 4 MiB limit".into(),
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| MimirError::Configuration(format!("fleet pack read failed: {error}")))?;
    if bytes.len() > MAX_PACK_BYTES {
        return Err(MimirError::Configuration(
            "fleet learning pack exceeds the 4 MiB limit".into(),
        ));
    }
    let envelope: SignedLearningPack = serde_json::from_slice(&bytes)?;
    install_signed_pack(state_root, &envelope, &public_key).await
}

pub async fn load_active_fleet_pack(state_root: &Path) -> Result<Option<SignedLearningPack>> {
    let path = active_fleet_pack_path(state_root);
    load_verified_cached_pack(state_root, &path, false).await
}

async fn load_verified_cached_pack(
    state_root: &Path,
    path: &Path,
    required: bool,
) -> Result<Option<SignedLearningPack>> {
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MimirError::Configuration(format!(
                "cached fleet learning pack is not installed: {}",
                path.display()
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > MAX_PACK_BYTES as u64 {
        return Err(MimirError::Configuration(
            "fleet learning pack exceeds the 4 MiB limit".into(),
        ));
    }
    let envelope = read_json::<SignedLearningPack>(path)
        .await?
        .ok_or_else(|| MimirError::Configuration("cached fleet pack disappeared".into()))?;
    let key_path = fleet_public_key_path(state_root);
    let public_key = read_json::<Vec<u8>>(&key_path).await?.ok_or_else(|| {
        MimirError::Configuration("cached fleet learning pack has no trusted signing key".into())
    })?;
    verify_signed_pack(&envelope, &public_key, Utc::now())?;
    Ok(Some(envelope))
}

pub async fn load_fleet_pack(
    state_root: &Path,
    pinned_version: Option<&str>,
) -> Result<Option<SignedLearningPack>> {
    let Some(version) = pinned_version else {
        return load_active_fleet_pack(state_root).await;
    };
    let version = safe_segment(version)?;
    let path = fleet_root(state_root)
        .join("versions")
        .join(format!("{version}.json"));
    load_verified_cached_pack(state_root, &path, true)
        .await
        .map_err(|error| {
            MimirError::Configuration(format!(
                "pinned fleet learning pack {version} is unavailable: {error}"
            ))
        })
}

pub async fn pin_fleet_version(workspace: &Path, version: Option<&str>) -> Result<LearningState> {
    let root = discover_project_root(workspace)?;
    let version = version.map(safe_segment).transpose()?;
    mutate_learning_state(&root, move |state| state.pinned_fleet_version = version).await
}

fn validate_learning_state(state: &LearningState) -> Result<()> {
    if state.schema != LEARNING_SCHEMA_VERSION {
        return Err(MimirError::Configuration(format!(
            "unsupported learning state schema {}",
            state.schema
        )));
    }
    if state.evidence.len() > MAX_EVIDENCE || state.candidates.len() > MAX_CANDIDATES {
        return Err(MimirError::Configuration(
            "learning state exceeds its bounded collection limits".into(),
        ));
    }
    for evidence in &state.evidence {
        validate_evidence(evidence)?;
    }
    for candidate in &state.candidates {
        validate_candidate(candidate)?;
    }
    Ok(())
}

fn validate_evidence(evidence: &LearningEvidence) -> Result<()> {
    if evidence.session_alias.len() > 64
        || evidence.task_fingerprint.len() > 128
        || evidence.note.as_ref().is_some_and(|note| note.len() > 512)
    {
        return Err(MimirError::Configuration(
            "learning evidence exceeds its bounded text limits".into(),
        ));
    }
    if evidence.session_alias.contains('/') || evidence.session_alias.contains('\\') {
        return Err(MimirError::Configuration(
            "learning evidence must use a redacted session alias".into(),
        ));
    }
    Ok(())
}

fn validate_candidate(candidate: &LearningCandidate) -> Result<()> {
    if candidate.summary.is_empty()
        || candidate.summary.len() > 512
        || candidate.rationale.len() > 2_048
        || candidate.edits.is_empty()
        || candidate.edits.len() > 8
    {
        return Err(MimirError::Configuration(
            "learning candidate violates proposal size limits".into(),
        ));
    }
    for edit in &candidate.edits {
        if edit.kind != RefinementKind::Memory {
            return Err(MimirError::Configuration(
                "generated learning candidates may only contain bounded Memory edits".into(),
            ));
        }
        if edit.id.as_deref() == Some("base_system_prompt") {
            return Err(MimirError::Configuration(
                "learning candidates cannot edit the base system prompt".into(),
            ));
        }
        let serialized = serde_json::to_string(edit)?;
        let forbidden = [
            "allow_process",
            "allow_shell",
            "tool_policy",
            "install_dependency",
            "base_system_prompt",
        ];
        if forbidden.iter().any(|value| serialized.contains(value)) {
            return Err(MimirError::Configuration(
                "learning candidates cannot widen permissions or install code".into(),
            ));
        }
    }
    Ok(())
}

fn validate_pack(pack: &FleetLearningPack, now: DateTime<Utc>) -> Result<()> {
    if pack.schema != LEARNING_SCHEMA_VERSION || pack.entries.len() > 256 {
        return Err(MimirError::Configuration(
            "fleet learning pack has an unsupported schema or too many entries".into(),
        ));
    }
    safe_segment(&pack.version)?;
    if pack.expires_at <= now {
        return Err(MimirError::Configuration(
            "fleet learning pack has expired".into(),
        ));
    }
    if version_is_newer(&pack.minimum_mimir_version, env!("CARGO_PKG_VERSION"))? {
        return Err(MimirError::Configuration(format!(
            "fleet learning pack requires Mimir {} or newer",
            pack.minimum_mimir_version
        )));
    }
    for entry in &pack.entries {
        if entry.id.is_empty()
            || entry.id.len() > 128
            || entry.title.is_empty()
            || entry.content.is_empty()
            || entry.content.len() > 16 * 1024
            || entry.kind == RefinementKind::Unknown
        {
            return Err(MimirError::Configuration(
                "fleet learning pack contains an invalid entry".into(),
            ));
        }
        if entry.kind == RefinementKind::Skill
            && (entry
                .reference
                .get("type")
                .and_then(serde_json::Value::as_str)
                != Some("python")
                || entry
                    .reference
                    .get("import")
                    .or_else(|| entry.reference.get("python_import"))
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(str::is_empty)
                || entry
                    .reference
                    .get("callable")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(str::is_empty))
        {
            return Err(MimirError::Configuration(
                "fleet skills must reference an existing Python callable".into(),
            ));
        }
    }
    Ok(())
}

fn version_is_newer(required: &str, current: &str) -> Result<bool> {
    fn parse(value: &str) -> Result<(u64, u64, u64)> {
        let core = value
            .trim_start_matches('v')
            .split('-')
            .next()
            .unwrap_or(value);
        let mut parts = core.split('.');
        let major = parts
            .next()
            .and_then(|item| item.parse().ok())
            .ok_or_else(|| {
                MimirError::Configuration(format!("invalid semantic version: {value}"))
            })?;
        let minor = parts.next().and_then(|item| item.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|item| item.parse().ok()).unwrap_or(0);
        Ok((major, minor, patch))
    }
    Ok(parse(required)? > parse(current)?)
}

fn update_candidate_status(candidate: &mut LearningCandidate) {
    let failures = candidate
        .canary_outcomes
        .iter()
        .filter(|outcome| **outcome == EvidenceOutcome::VerifiedFailure)
        .count();
    let successes = candidate
        .canary_outcomes
        .iter()
        .filter(|outcome| **outcome == EvidenceOutcome::VerifiedSuccess)
        .count();
    if failures > 0 {
        candidate.status = CandidateStatus::Quarantined;
        candidate.rejection_reason = Some("Verified canary regression".into());
    } else if candidate.canary_outcomes.len() >= usize::from(candidate.canary_runs_required)
        && successes >= usize::from(candidate.canary_runs_required)
    {
        candidate.status = CandidateStatus::Active;
    } else {
        candidate.status = CandidateStatus::Canary;
    }
}

async fn mutate_learning_state(
    project_root: &Path,
    mutate: impl FnOnce(&mut LearningState),
) -> Result<LearningState> {
    let path = project_learning_dir(project_root).join("state.json");
    prepare_state_path(project_root, &path).await?;
    let lock = path_lock(&path);
    let _guard = lock.lock().await;
    let _process_guard = CrossProcessLock::acquire(&path).await?;
    let mut state: LearningState = read_json(&path).await?.unwrap_or_default();
    if state.schema == 2 {
        state.schema = LEARNING_SCHEMA_VERSION;
    }
    validate_learning_state(&state)?;
    mutate(&mut state);
    state.schema = LEARNING_SCHEMA_VERSION;
    state.generation = state.generation.saturating_add(1);
    validate_learning_state(&state)?;
    write_learning_state(project_root, &state).await?;
    Ok(state)
}

struct CrossProcessLock {
    path: PathBuf,
}

impl CrossProcessLock {
    async fn acquire(state_path: &Path) -> Result<Self> {
        let lock_path = state_path.with_extension("lock");
        for _ in 0..250 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(Self { path: lock_path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = std::fs::symlink_metadata(&lock_path)?;
                    if metadata.file_type().is_symlink() {
                        return Err(MimirError::Configuration(format!(
                            "learning lock must not be a symlink: {}",
                            lock_path.display()
                        )));
                    }
                    let owner_gone = std::fs::read_to_string(&lock_path)
                        .ok()
                        .and_then(|value| value.trim().parse::<u32>().ok())
                        .is_some_and(lock_owner_is_gone);
                    let stale = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                    if owner_gone || stale.is_some_and(|age| age > StdDuration::from_secs(30)) {
                        match std::fs::remove_file(&lock_path) {
                            Ok(()) => continue,
                            Err(remove_error)
                                if remove_error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                continue;
                            }
                            Err(remove_error) => return Err(remove_error.into()),
                        }
                    }
                    tokio::time::sleep(StdDuration::from_millis(20)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(MimirError::Configuration(format!(
            "timed out waiting for learning state lock: {}",
            lock_path.display()
        )))
    }
}

#[cfg(unix)]
fn lock_owner_is_gone(pid: u32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

    i32::try_from(pid)
        .ok()
        .is_some_and(|pid| kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH))
}

#[cfg(not(unix))]
const fn lock_owner_is_gone(_pid: u32) -> bool {
    false
}

impl Drop for CrossProcessLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn ensure_git_exclude(project_root: &Path) -> Result<()> {
    let dot_git = project_root.join(".git");
    let git_dir = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MimirError::Configuration(format!(
                "Git metadata must not be a symlink: {}",
                dot_git.display()
            )));
        }
        Ok(metadata) if metadata.is_dir() => dot_git,
        Ok(metadata) if metadata.is_file() => {
            let pointer = std::fs::read_to_string(&dot_git)?;
            let value = pointer
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .ok_or_else(|| {
                    MimirError::Configuration(format!(
                        "invalid linked-worktree metadata: {}",
                        dot_git.display()
                    ))
                })?;
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            }
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let common_dir_path = git_dir.join("commondir");
    let common_dir = if common_dir_path.is_file() {
        let value = std::fs::read_to_string(&common_dir_path)?;
        let path = PathBuf::from(value.trim());
        if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        }
    } else {
        git_dir
    };
    let info_dir = common_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let exclude = info_dir.join("exclude");
    if std::fs::symlink_metadata(&exclude).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(MimirError::Configuration(format!(
            "Git exclude file must not be a symlink: {}",
            exclude.display()
        )));
    }
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let required = [".mimir/project.json", ".mimir/learning/"];
    if required
        .iter()
        .all(|line| existing.lines().any(|item| item == *line))
    {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "# Mimir project-local learning (untracked)")?;
    for line in required {
        if !existing.lines().any(|item| item == line) {
            writeln!(file, "{line}")?;
        }
    }
    file.sync_all()?;
    Ok(())
}

async fn write_learning_state(project_root: &Path, state: &LearningState) -> Result<()> {
    let path = project_learning_dir(project_root).join("state.json");
    prepare_state_path(project_root, &path).await?;
    write_json(&path, state).await?;
    set_private_permissions(&path).await
}

fn sanitize_note(note: Option<&str>) -> Result<Option<String>> {
    let note = note.map(str::trim).filter(|value| !value.is_empty());
    if note.is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control)) {
        return Err(MimirError::Configuration(
            "learning feedback note must be control-free and at most 512 bytes".into(),
        ));
    }
    Ok(note.map(str::to_owned))
}

fn validate_public_text(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    let forbidden = [
        "/users/",
        "/home/",
        "c:\\users\\",
        "api_key",
        "apikey",
        "authorization:",
        "bearer ",
        "private key",
        "password=",
        "token=",
        "sk-",
    ];
    if value.len() > 512
        || value.chars().any(char::is_control)
        || forbidden.iter().any(|needle| lower.contains(needle))
    {
        return Err(MimirError::Configuration(
            "fleet contribution contains non-public or sensitive text".into(),
        ));
    }
    Ok(())
}

fn parse_json_response<T: for<'de> Deserialize<'de>>(text: &str, actor: &str) -> Result<T> {
    let trimmed = text.trim();
    let candidate = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed
    } else if let Some(fence) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        fence.strip_suffix("```").map(str::trim).ok_or_else(|| {
            MimirError::Protocol(format!("{actor} returned an unterminated JSON fence"))
        })?
    } else {
        let start = trimmed
            .find('{')
            .ok_or_else(|| MimirError::Protocol(format!("{actor} returned no JSON object")))?;
        let end = trimmed
            .rfind('}')
            .ok_or_else(|| MimirError::Protocol(format!("{actor} returned incomplete JSON")))?;
        &trimmed[start..=end]
    };
    serde_json::from_str(candidate)
        .map_err(|error| MimirError::Protocol(format!("{actor} returned invalid JSON: {error}")))
}

fn stable_alias(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("session-{}", &hex_digest(&digest)[..16])
}

fn safe_segment(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(MimirError::Configuration(
            "learning version must be a safe path segment".into(),
        ));
    }
    Ok(value.into())
}

fn trim_front<T>(items: &mut Vec<T>, limit: usize) {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> std::future::Ready<Result<()>> {
    std::future::ready(Ok(()))
}
