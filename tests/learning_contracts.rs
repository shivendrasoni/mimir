use std::{collections::BTreeMap, fmt::Write as _};

use assert_cmd::Command;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Duration, Utc};
use mimir::{
    learning::{
        self, Applicability, CandidateStatus, EvidenceOutcome, EvidenceSignal, FleetLearningEntry,
        FleetLearningPack, LearningCandidate, LearningEvidence, LearningMode, SignedLearningPack,
    },
    refinement::{
        self, HarnessEntries, HarnessEntry, HarnessScope, HarnessState, RefinementAction,
        RefinementEdit, RefinementKind,
    },
    session::{FileSessionStore, SessionPayload, SessionRecord, SessionStore},
    typesafe::{TypeSafeLearningEvaluation, TypeSafeRecommendationStatus},
};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn continual_learning_benchmark_has_representative_inputs_and_policy_expectations() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../benchmarks/continual-learning/fixtures.json"
    ))
    .expect("benchmark fixture JSON");
    let cases = fixture["cases"].as_array().expect("benchmark cases");
    let categories = cases
        .iter()
        .filter_map(|case| case["category"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for required in [
        "human_correction",
        "debugging_pattern",
        "false_positive_rejection",
        "privacy_redaction",
        "incomplete_task",
        "attributed_failure",
        "promotion",
    ] {
        assert!(
            categories.contains(required),
            "missing benchmark case: {required}"
        );
    }
    for case in cases {
        assert!(case["projection"].is_object(), "missing projection: {case}");
        assert!(case["evaluation"].is_object(), "missing evaluation: {case}");
        let evaluation = &case["evaluation"];
        let typed = TypeSafeLearningEvaluation {
            status: TypeSafeRecommendationStatus::Success,
            lesson_kind: evaluation["lesson_kind"].as_str().map(str::to_owned),
            resolution: evaluation["resolution"].as_str().map(str::to_owned),
            scope: Some("project".into()),
            cluster_match: Some("none".into()),
            reuse_value: evaluation["reuse_value"].as_f64(),
            overfit_risk: evaluation["overfit_risk"].as_f64(),
            human_correction_probability: evaluation["human_correction_probability"].as_f64(),
            categorical_confidence: evaluation["categorical_confidence"].as_f64(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
            rubric_hash: "fixture".into(),
            error_kind: None,
        };
        let expected = case["expected"]["disposition"]
            .as_str()
            .expect("disposition");
        let actual = match learning::policy::disposition(&typed) {
            learning::policy::EvaluationDisposition::Discard => "discard",
            learning::policy::EvaluationDisposition::NewCluster => "new_cluster",
            learning::policy::EvaluationDisposition::MatchCluster => "match_cluster",
        };
        assert_eq!(actual, expected, "fixture policy: {}", case["id"]);
    }
}

fn memory_edit(id: &str, content: &str) -> RefinementEdit {
    RefinementEdit {
        action: RefinementAction::Create,
        kind: RefinementKind::Memory,
        id: Some(id.into()),
        title: Some(id.replace('_', " ")),
        content: Some(content.into()),
        path: Some("general".into()),
        reference: None,
        arguments: None,
        metadata: None,
        reason: Some("verified project evidence".into()),
    }
}

fn signed_pack(key: &Ed25519KeyPair, pack: FleetLearningPack) -> SignedLearningPack {
    let encoded = serde_json::to_vec(&pack).expect("pack json");
    SignedLearningPack {
        signature: STANDARD.encode(key.sign(&encoded).as_ref()),
        sha256: Sha256::digest(&encoded).iter().fold(
            String::with_capacity(64),
            |mut output, byte| {
                let _ = write!(output, "{byte:02x}");
                output
            },
        ),
        pack,
    }
}

fn candidate(id: Uuid) -> LearningCandidate {
    LearningCandidate {
        id,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        summary: "Prefer deterministic validation".into(),
        rationale: "The project gate provided a verified outcome".into(),
        status: CandidateStatus::Canary,
        applicability: Applicability {
            tags: vec!["rust".into()],
            path_prefixes: vec!["src".into()],
            languages: vec!["rust".into()],
        },
        edits: vec![memory_edit(
            "deterministic_validation",
            "Run the project gate.",
        )],
        evidence_ids: Vec::new(),
        source_cluster_id: None,
        canary_outcomes: Vec::new(),
        canary_runs_required: learning::PROJECT_CANARY_RUNS,
        rejection_reason: None,
    }
}

fn harness_with_entry(id: &str, content: &str, scope: HarnessScope) -> HarnessState {
    let now = Utc::now().to_rfc3339();
    let entry = HarnessEntry {
        id: id.into(),
        kind: RefinementKind::Memory,
        title: id.into(),
        content: content.into(),
        path: "src".into(),
        scope,
        reference: BTreeMap::new(),
        arguments: BTreeMap::new(),
        metadata: BTreeMap::new(),
        source: "test".into(),
        created_at: now.clone(),
        updated_at: now,
        version: 1,
    };
    let mut entries = HarnessEntries::default();
    entries.memory.insert(id.into(), entry);
    HarnessState {
        schema: 2,
        generation: 1,
        entries,
        refinements: Vec::new(),
    }
}

#[tokio::test]
async fn nearest_marker_owns_project_learning_and_projects_are_isolated() {
    let state = TempDir::new().expect("state");
    let project_a = TempDir::new().expect("project a");
    let project_b = TempDir::new().expect("project b");
    let child = project_a.path().join("crates/app");
    std::fs::create_dir_all(&child).expect("child");
    learning::initialize_project(project_a.path())
        .await
        .expect("initialize a");
    learning::initialize_project(project_b.path())
        .await
        .expect("initialize b");
    assert_eq!(
        learning::discover_project_root(&child).expect("discover"),
        project_a.path().canonicalize().expect("canonical a")
    );

    let harness = harness_with_entry("only_a", "Only project A sees this", HarnessScope::Project);
    let path = learning::project_harness_path(project_a.path());
    std::fs::create_dir_all(path.parent().expect("parent")).expect("harness dir");
    std::fs::write(&path, serde_json::to_vec_pretty(&harness).expect("json")).expect("write");

    let context_a = refinement::load_harness_context_for_workspace(
        state.path(),
        &child,
        "main",
        Some("project A"),
    )
    .await
    .expect("context a");
    let context_b = refinement::load_harness_context_for_workspace(
        state.path(),
        project_b.path(),
        "main",
        Some("project A"),
    )
    .await
    .expect("context b");
    assert!(context_a.contains("Only project A sees this"));
    assert!(!context_b.contains("Only project A sees this"));
}

#[test]
fn parent_marker_does_not_claim_a_nested_git_repository() {
    let umbrella = TempDir::new().expect("umbrella");
    std::fs::create_dir(umbrella.path().join(".git")).expect("umbrella git");
    let marker_dir = umbrella.path().join(".mimir");
    std::fs::create_dir(&marker_dir).expect("marker dir");
    std::fs::write(marker_dir.join("project.json"), r#"{"schema":2}"#).expect("marker");

    let nested = umbrella.path().join("nested-repository");
    let child = nested.join("crates/app");
    std::fs::create_dir_all(nested.join(".git")).expect("nested git");
    std::fs::create_dir_all(&child).expect("child");

    assert_eq!(
        learning::discover_project_root(&child).expect("nested root"),
        nested.canonicalize().expect("canonical nested")
    );
}

#[test]
fn session_storage_namespaces_are_stable_and_project_isolated() {
    let state = TempDir::new().expect("state");
    let project_a = TempDir::new().expect("project a");
    let project_b = TempDir::new().expect("project b");
    std::fs::create_dir(project_a.path().join(".git")).expect("git a");
    std::fs::create_dir(project_b.path().join(".git")).expect("git b");

    let first = learning::project_session_root(state.path(), project_a.path()).expect("first");
    let repeated =
        learning::project_session_root(state.path(), project_a.path()).expect("repeated");
    let second = learning::project_session_root(state.path(), project_b.path()).expect("second");

    assert_eq!(first, repeated);
    assert_ne!(first, second);
    let canonical_state = state.path().canonicalize().expect("canonical state");
    assert!(first.starts_with(canonical_state.join("projects")));
    let project_path = project_a.path().to_string_lossy();
    assert!(!first.to_string_lossy().contains(project_path.as_ref()));
}

#[tokio::test]
async fn identical_session_names_use_distinct_project_transcripts() {
    let state = TempDir::new().expect("state");
    let project_a = TempDir::new().expect("project a");
    let project_b = TempDir::new().expect("project b");
    std::fs::create_dir(project_a.path().join(".git")).expect("git a");
    std::fs::create_dir(project_b.path().join(".git")).expect("git b");
    let root_a = learning::project_session_root(state.path(), project_a.path()).expect("root a");
    let root_b = learning::project_session_root(state.path(), project_b.path()).expect("root b");
    let store_a = FileSessionStore::create(&root_a, "shared-name")
        .await
        .expect("store a");
    store_a
        .append(SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "project_a_only".into(),
            detail: "phase data must remain here".into(),
        }))
        .await
        .expect("record a");
    let store_b = FileSessionStore::create(&root_b, "shared-name")
        .await
        .expect("store b");

    assert_eq!(store_a.load().await.expect("load a").records.len(), 1);
    assert!(store_b.load().await.expect("load b").records.is_empty());
    assert_ne!(store_a.path(), store_b.path());
}

#[tokio::test]
async fn same_session_id_does_not_share_session_learning_between_projects() {
    let state = TempDir::new().expect("state");
    let project_a = TempDir::new().expect("project a");
    let project_b = TempDir::new().expect("project b");
    std::fs::create_dir(project_a.path().join(".git")).expect("git a");
    std::fs::create_dir(project_b.path().join(".git")).expect("git b");

    refinement::remember(
        state.path(),
        project_a.path(),
        "same-name",
        HarnessScope::Session,
        "Only project A may see this scratch memory.",
    )
    .await
    .expect("remember session memory");

    let context_a = refinement::load_harness_context_for_workspace(
        state.path(),
        project_a.path(),
        "same-name",
        None,
    )
    .await
    .expect("project a context");
    let context_b = refinement::load_harness_context_for_workspace(
        state.path(),
        project_b.path(),
        "same-name",
        None,
    )
    .await
    .expect("project b context");
    assert!(context_a.contains("Only project A"));
    assert!(!context_b.contains("Only project A"));
}

#[tokio::test]
async fn context_loading_is_read_only_and_precedence_is_token_bounded() {
    let state = TempDir::new().expect("state");
    let project = TempDir::new().expect("project");
    let untouched = TempDir::new().expect("untouched");
    refinement::load_harness_context_for_workspace(state.path(), untouched.path(), "main", None)
        .await
        .expect("read-only context");
    assert!(!untouched.path().join(".mimir").exists());

    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let mut project_harness = HarnessState::default();
    for index in 0..40 {
        let id = if index == 0 {
            "shared".into()
        } else {
            format!("project-{index}")
        };
        let content = if index == 0 {
            "project wins".into()
        } else {
            "bounded project context ".repeat(80)
        };
        project_harness.entries.memory.extend(
            harness_with_entry(&id, &content, HarnessScope::Project)
                .entries
                .memory,
        );
    }
    let project_path = learning::project_harness_path(project.path());
    std::fs::create_dir_all(project_path.parent().expect("project parent")).expect("project dir");
    std::fs::write(
        &project_path,
        serde_json::to_vec_pretty(&project_harness).expect("project json"),
    )
    .expect("project state");

    let user_path = state.path().join("harness/global/harness_state.json");
    std::fs::create_dir_all(user_path.parent().expect("user parent")).expect("user dir");
    std::fs::write(
        &user_path,
        serde_json::to_vec_pretty(&harness_with_entry(
            "shared",
            "user loses",
            HarnessScope::User,
        ))
        .expect("user json"),
    )
    .expect("user state");
    let context = refinement::load_harness_context_for_workspace(
        state.path(),
        project.path(),
        "main",
        Some("shared"),
    )
    .await
    .expect("context");
    assert!(context.contains("project wins"));
    assert!(!context.contains("user loses"));
    assert!(context.len() <= 12 * 1024);
    assert!(
        context
            .lines()
            .filter(|line| line.starts_with("- ["))
            .count()
            <= 24
    );
}

#[test]
fn legacy_scope_names_deserialize_without_changing_meaning() {
    assert_eq!(
        serde_json::from_str::<HarnessScope>(r#""local""#).expect("local"),
        HarnessScope::Session
    );
    assert_eq!(
        serde_json::from_str::<HarnessScope>(r#""global""#).expect("global"),
        HarnessScope::User
    );
    assert_eq!(
        serde_json::to_string(&HarnessScope::Project).expect("project"),
        r#""project""#
    );
}

#[tokio::test]
async fn ambiguous_legacy_global_harness_is_quarantined_from_context_without_rewrite() {
    let state = TempDir::new().expect("state");
    let project = TempDir::new().expect("project");
    let old_path = state.path().join("harness/global/harness_state.json");
    std::fs::create_dir_all(old_path.parent().expect("parent")).expect("directory");
    let old = serde_json::json!({
        "schema": 1,
        "entries": {
            "memory": {
                "legacy": {
                    "id": "legacy",
                    "kind": "memory",
                    "title": "Legacy global learning",
                    "content": "Preserve the old global lesson",
                    "path": "general",
                    "scope": "global",
                    "reference": {},
                    "arguments": {},
                    "metadata": {},
                    "source": "legacy",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z",
                    "version": 1
                }
            }
        },
        "refinements": []
    });
    let original = serde_json::to_vec_pretty(&old).expect("old json");
    std::fs::write(&old_path, &original).expect("old state");
    let context =
        refinement::load_harness_context_for_workspace(state.path(), project.path(), "main", None)
            .await
            .expect("legacy context");
    assert!(!context.contains("Preserve the old global lesson"));
    assert_eq!(std::fs::read(&old_path).expect("unchanged state"), original);
}

#[tokio::test]
async fn feedback_promotes_after_three_successes_and_quarantines_on_failure() {
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let id = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(id))
        .await
        .expect("candidate");
    for _ in 0..3 {
        learning::request_candidate_feedback(project.path(), id)
            .await
            .expect("request");
        learning::record_feedback(project.path(), "private/session", true, None)
            .await
            .expect("feedback");
    }
    let root = learning::discover_project_root(project.path()).expect("root");
    let state = learning::load_learning_state(&root).await.expect("state");
    assert_eq!(state.candidates[0].status, CandidateStatus::Active);
    assert!(
        state
            .evidence
            .iter()
            .all(|evidence| !evidence.session_alias.contains("private"))
    );

    let failed_id = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(failed_id))
        .await
        .expect("failed candidate");
    learning::request_candidate_feedback(project.path(), failed_id)
        .await
        .expect("request failure");
    learning::record_feedback(project.path(), "main", false, Some("did not pass"))
        .await
        .expect("negative feedback");
    let state = learning::load_learning_state(&root).await.expect("state");
    assert_eq!(
        state
            .candidates
            .iter()
            .find(|item| item.id == failed_id)
            .expect("failed candidate")
            .status,
        CandidateStatus::Quarantined
    );
}

#[tokio::test]
async fn attributed_validation_promotes_and_correction_quarantines_without_feedback() {
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let promoted = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(promoted))
        .await
        .expect("candidate");
    // No provenance means a genuine validation signal remains neutral.
    learning::record_attributed_outcome(
        project.path(),
        "main",
        &[],
        EvidenceOutcome::VerifiedSuccess,
        EvidenceSignal::ValidationGate,
    )
    .await
    .expect("neutral unexposed result");
    for _ in 0..3 {
        learning::record_attributed_outcome(
            project.path(),
            "main",
            &[promoted],
            EvidenceOutcome::VerifiedSuccess,
            EvidenceSignal::ValidationGate,
        )
        .await
        .expect("attributed validation");
    }
    let root = learning::discover_project_root(project.path()).expect("root");
    let state = learning::load_learning_state(&root).await.expect("state");
    assert_eq!(state.candidates[0].status, CandidateStatus::Active);

    let failed = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(failed))
        .await
        .expect("candidate");
    learning::record_attributed_outcome(
        project.path(),
        "main",
        &[failed],
        EvidenceOutcome::VerifiedFailure,
        EvidenceSignal::Correction,
    )
    .await
    .expect("attributed correction");
    let state = learning::load_learning_state(&root).await.expect("state");
    assert_eq!(
        state
            .candidates
            .iter()
            .find(|candidate| candidate.id == failed)
            .expect("failed candidate")
            .status,
        CandidateStatus::Quarantined
    );
}

#[tokio::test]
async fn harness_provenance_includes_only_candidate_entries_that_fit_context() {
    let project = TempDir::new().expect("project");
    let state = TempDir::new().expect("state");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let id = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(id))
        .await
        .expect("candidate");
    let assembled = refinement::load_harness_context_with_provenance(
        state.path(),
        project.path(),
        "main",
        None,
    )
    .await
    .expect("assembled context");
    assert!(assembled.text.contains("Run the project gate"));
    assert_eq!(assembled.exposed_candidate_ids, vec![id]);
}

#[tokio::test]
async fn unexposed_runtime_failure_is_diagnostic_only_and_recovers_orphan_lock() {
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let id = Uuid::new_v4();
    learning::save_candidate(project.path(), candidate(id))
        .await
        .expect("candidate");
    let lock = project.path().join(".mimir/learning/state.lock");
    std::fs::write(&lock, "2147483647\n").expect("orphan lock");
    let state = learning::record_runtime_failure(project.path(), "main")
        .await
        .expect("failure evidence");
    assert_eq!(state.candidates[0].status, CandidateStatus::Canary);
    assert!(!lock.exists());
}

#[tokio::test]
async fn generated_candidates_are_limited_to_memory_refinements() {
    let project = TempDir::new().expect("project");
    let state = TempDir::new().expect("state");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let mut proposal = candidate(Uuid::new_v4());
    let executable_or_broad_edits = vec![
        RefinementEdit {
            kind: RefinementKind::Prompt,
            ..memory_edit("prompt", "Prompt guidance")
        },
        memory_edit("memory", "Memory guidance"),
        RefinementEdit {
            kind: RefinementKind::Subagent,
            ..memory_edit("subagent", "Subagent specification")
        },
        RefinementEdit {
            kind: RefinementKind::Skill,
            reference: Some(BTreeMap::from([
                ("type".into(), serde_json::json!("python")),
                ("import".into(), serde_json::json!("existing.module")),
                ("callable".into(), serde_json::json!("validate")),
            ])),
            arguments: Some(BTreeMap::new()),
            ..memory_edit("skill", "Use the existing validator")
        },
    ];
    proposal.edits = executable_or_broad_edits;
    assert!(
        learning::save_candidate(project.path(), proposal.clone())
            .await
            .is_err()
    );
    proposal.edits = vec![memory_edit("memory", "Memory guidance")];
    learning::save_candidate(project.path(), proposal.clone())
        .await
        .expect("memory only");
    let context =
        refinement::load_harness_context_for_workspace(state.path(), project.path(), "main", None)
            .await
            .expect("context");
    assert!(context.contains("Memory guidance"));

    let mut deletion = candidate(Uuid::new_v4());
    deletion.edits = vec![RefinementEdit {
        action: RefinementAction::Delete,
        ..memory_edit("memory", "unused")
    }];
    learning::save_candidate(project.path(), deletion)
        .await
        .expect("delete candidate");
    let context =
        refinement::load_harness_context_for_workspace(state.path(), project.path(), "main", None)
            .await
            .expect("context after delete");
    assert!(!context.contains("Memory guidance"));
}

#[tokio::test]
async fn concurrent_evidence_writes_are_serialized_and_bounded() {
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let mut tasks = Vec::new();
    for index in 0..32 {
        let root = project.path().to_owned();
        tasks.push(tokio::spawn(async move {
            learning::record_evidence(
                &root,
                LearningEvidence {
                    id: Uuid::new_v4(),
                    created_at: Utc::now(),
                    session_alias: format!("session-{index}"),
                    outcome: EvidenceOutcome::Ambiguous,
                    signal: EvidenceSignal::Diagnostic,
                    task_fingerprint: format!("task-{index}"),
                    diagnostic_run_id: None,
                    metrics: BTreeMap::new(),
                    note: None,
                    exposed_candidate_ids: Vec::new(),
                },
            )
            .await
            .expect("record")
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }
    let root = learning::discover_project_root(project.path()).expect("root");
    let state = learning::load_learning_state(&root).await.expect("state");
    assert_eq!(state.evidence.len(), 32);
    assert_eq!(state.generation, 32);
}

#[test]
fn contribution_is_structurally_redacted_and_rejects_sensitive_summary() {
    let id = Uuid::new_v4();
    let mut proposal = candidate(id);
    let evidence = LearningEvidence {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        session_alias: "session-redacted".into(),
        outcome: EvidenceOutcome::VerifiedSuccess,
        signal: EvidenceSignal::ValidationGate,
        task_fingerprint: "task-redacted".into(),
        diagnostic_run_id: None,
        metrics: BTreeMap::new(),
        note: Some("raw private observation".into()),
        exposed_candidate_ids: vec![id],
    };
    proposal.evidence_ids.push(evidence.id);
    proposal.rationale = "raw user prompt with authorization: Bearer credential".into();
    proposal.edits[0].content = Some("fn generated() { tool_payload(); }".into());
    proposal.edits[0].path = Some("/Users/example/private/project/src/lib.rs".into());
    let contribution = learning::build_fleet_contribution(
        &proposal,
        std::slice::from_ref(&evidence),
        Uuid::new_v4(),
    )
    .expect("contribution");
    let encoded = serde_json::to_string(&contribution).expect("json");
    assert!(!encoded.contains("raw private observation"));
    assert!(!encoded.contains("raw user prompt"));
    assert!(!encoded.contains("generated"));
    assert!(!encoded.contains("tool_payload"));
    assert!(!encoded.contains("credential"));
    assert!(!encoded.contains("project/src"));
    assert!(!encoded.contains("src"));
    assert_eq!(contribution.verified_successes, 1);

    proposal.summary = "Read /Users/example/private and use token=secret".into();
    assert!(learning::build_fleet_contribution(&proposal, &[evidence], Uuid::new_v4()).is_err());
}

#[tokio::test]
async fn signed_pack_verifies_installs_pins_and_rejects_tampering() {
    let state = TempDir::new().expect("state");
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let seed = [7_u8; 32];
    let key = Ed25519KeyPair::from_seed_unchecked(&seed).expect("key");
    let pack = FleetLearningPack {
        schema: learning::LEARNING_SCHEMA_VERSION,
        version: "2026.09.1".into(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        minimum_mimir_version: "0.7.0".into(),
        entries: vec![FleetLearningEntry {
            id: "fleet_validation".into(),
            kind: RefinementKind::Memory,
            title: "Fleet validation".into(),
            content: "Prefer deterministic validation evidence.".into(),
            path: "general".into(),
            applicability: Applicability::default(),
            metadata: BTreeMap::new(),
            reference: BTreeMap::new(),
            arguments: BTreeMap::new(),
        }],
        revoked_entry_ids: Vec::new(),
    };
    let envelope = signed_pack(&key, pack);
    learning::verify_signed_pack(&envelope, key.public_key().as_ref(), Utc::now()).expect("verify");
    learning::install_signed_pack(state.path(), &envelope, key.public_key().as_ref())
        .await
        .expect("install");
    learning::pin_fleet_version(project.path(), Some("2026.09.1"))
        .await
        .expect("pin");
    let context = refinement::load_harness_context_for_workspace(
        state.path(),
        project.path(),
        "main",
        Some("validation"),
    )
    .await
    .expect("context");
    assert!(context.contains("Prefer deterministic validation evidence"));

    let mut tampered = envelope;
    tampered.pack.entries[0].content = "tampered".into();
    assert!(
        learning::verify_signed_pack(&tampered, key.public_key().as_ref(), Utc::now()).is_err()
    );
    std::fs::write(
        learning::active_fleet_pack_path(state.path()),
        serde_json::to_vec_pretty(&tampered).expect("tampered json"),
    )
    .expect("tamper cache");
    assert!(
        learning::load_active_fleet_pack(state.path())
            .await
            .is_err()
    );
}

#[test]
fn signed_pack_rejects_expiry_and_incompatible_versions() {
    let key = Ed25519KeyPair::from_seed_unchecked(&[9_u8; 32]).expect("key");
    let base = FleetLearningPack {
        schema: learning::LEARNING_SCHEMA_VERSION,
        version: "2026.09.2".into(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(1),
        minimum_mimir_version: "0.7.0".into(),
        entries: Vec::new(),
        revoked_entry_ids: Vec::new(),
    };
    let mut expired = base.clone();
    expired.expires_at = Utc::now() - Duration::seconds(1);
    assert!(
        learning::verify_signed_pack(
            &signed_pack(&key, expired),
            key.public_key().as_ref(),
            Utc::now(),
        )
        .is_err()
    );
    let mut incompatible = base;
    incompatible.minimum_mimir_version = "999.0.0".into();
    assert!(
        learning::verify_signed_pack(
            &signed_pack(&key, incompatible),
            key.public_key().as_ref(),
            Utc::now(),
        )
        .is_err()
    );
}

#[tokio::test]
async fn cached_pack_supports_revocation_offline_fallback_and_version_rollback() {
    let state = TempDir::new().expect("state");
    let project = TempDir::new().expect("project");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let key = Ed25519KeyPair::from_seed_unchecked(&[11_u8; 32]).expect("key");
    let entry = |id: &str, content: &str| FleetLearningEntry {
        id: id.into(),
        kind: RefinementKind::Memory,
        title: id.into(),
        content: content.into(),
        path: "general".into(),
        applicability: Applicability::default(),
        metadata: BTreeMap::new(),
        reference: BTreeMap::new(),
        arguments: BTreeMap::new(),
    };
    let base = FleetLearningPack {
        schema: learning::LEARNING_SCHEMA_VERSION,
        version: "1.0.0".into(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        minimum_mimir_version: "0.7.0".into(),
        entries: vec![entry("shared", "cached version one")],
        revoked_entry_ids: Vec::new(),
    };
    learning::install_signed_pack(
        state.path(),
        &signed_pack(&key, base.clone()),
        key.public_key().as_ref(),
    )
    .await
    .expect("install v1");
    let mut next = base;
    next.version = "2.0.0".into();
    next.entries = vec![
        entry("shared", "revoked version two"),
        entry("live", "cached offline entry"),
    ];
    next.revoked_entry_ids = vec!["shared".into()];
    learning::install_signed_pack(
        state.path(),
        &signed_pack(&key, next),
        key.public_key().as_ref(),
    )
    .await
    .expect("install v2");
    let current =
        refinement::load_harness_context_for_workspace(state.path(), project.path(), "main", None)
            .await
            .expect("offline cache");
    assert!(current.contains("cached offline entry"));
    assert!(!current.contains("revoked version two"));

    learning::pin_fleet_version(project.path(), Some("1.0.0"))
        .await
        .expect("pin rollback");
    let rolled_back =
        refinement::load_harness_context_for_workspace(state.path(), project.path(), "main", None)
            .await
            .expect("pinned context");
    assert!(rolled_back.contains("cached version one"));
    assert!(!rolled_back.contains("cached offline entry"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_project_marker_is_rejected() {
    use std::os::unix::fs::symlink;

    let project = TempDir::new().expect("project");
    let target = project.path().join("target.json");
    std::fs::write(&target, "{}").expect("target");
    std::fs::create_dir(project.path().join(".mimir")).expect("mimir dir");
    symlink(&target, project.path().join(".mimir/project.json")).expect("symlink");
    assert!(learning::discover_project_root(project.path()).is_err());
}

#[tokio::test]
async fn learning_is_automatic_when_available_and_contribution_is_opt_in() {
    let project = TempDir::new().expect("project");
    let state = TempDir::new().expect("state");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let status = learning::learning_status(project.path(), state.path())
        .await
        .expect("status");
    assert_eq!(status.mode, LearningMode::Auto);
    assert!(!status.contribution_enabled);
    assert_eq!(
        learning::set_mode(project.path(), LearningMode::Auto)
            .await
            .expect("auto remains explicit opt-in/out controllable")
            .mode,
        LearningMode::Auto
    );
}

#[tokio::test]
async fn git_projects_exclude_local_learning_by_default() {
    let project = TempDir::new().expect("project");
    std::fs::create_dir_all(project.path().join(".git/info")).expect("git metadata");
    learning::initialize_project(project.path())
        .await
        .expect("initialize");
    let exclude =
        std::fs::read_to_string(project.path().join(".git/info/exclude")).expect("exclude");
    assert!(exclude.contains(".mimir/project.json"));
    assert!(exclude.contains(".mimir/learning/"));
    assert!(
        learning::pin_fleet_version(project.path(), Some("../escape"))
            .await
            .is_err()
    );
}

#[test]
fn learning_cli_alias_initializes_and_offline_update_fails_closed() {
    let project = TempDir::new().expect("project");
    let state = TempDir::new().expect("state");
    let common = [
        "--workspace",
        project.path().to_str().expect("project path"),
        "--state-dir",
        state.path().to_str().expect("state path"),
    ];
    let initialized = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["learn", "init"])
        .output()
        .expect("init output");
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(project.path().join(".mimir/project.json").is_file());

    let offline = Command::cargo_bin("mimir")
        .expect("binary")
        .args(common)
        .args(["--offline", "learning", "update"])
        .output()
        .expect("offline output");
    assert!(!offline.status.success());
    assert!(String::from_utf8_lossy(&offline.stderr).contains("offline mode"));
}
