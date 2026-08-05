use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    atomic::{canonical_state_root, path_lock, prepare_state_path, read_json, write_json},
    error::{MimirError, Result},
    runtime::AgentRuntime,
    session::{FileSessionStore, SessionPayload, SessionStore},
};

const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;
const MAX_HISTORY_BYTES: u64 = 16 * 1024 * 1024;
const REFINEMENT_SYSTEM_PROMPT: &str = r#"You are Mimir's continual harness refinement subsystem.
Return JSON only with this shape:
{"summary":"one sentence","rationale":"evidence","expectedOutcome":"outcome","edits":[{"action":"create|update|delete","kind":"prompt|memory|skill|subagent","id":"optional for create","title":"required except delete","content":"required except delete","path":"optional","reference":{},"arguments":{},"metadata":{},"reason":"why"}]}
Make only small evidence-backed edits. Never rewrite a base system prompt. Skill creates and updates require reference.type=python, a Python import, a callable or call_pattern, and an arguments object."#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessScope {
    Local,
    Global,
}

impl HarnessScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementKind {
    Prompt,
    Memory,
    Skill,
    Subagent,
    #[serde(other)]
    #[default]
    Unknown,
}

impl RefinementKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Memory => "memory",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementAction {
    Create,
    Update,
    Delete,
    #[serde(other)]
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessEntry {
    pub id: String,
    pub kind: RefinementKind,
    pub title: String,
    pub content: String,
    pub path: String,
    pub scope: HarnessScope,
    pub reference: BTreeMap<String, serde_json::Value>,
    pub arguments: BTreeMap<String, serde_json::Value>,
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    pub version: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HarnessEntries {
    #[serde(default)]
    pub prompt: BTreeMap<String, HarnessEntry>,
    #[serde(default)]
    pub memory: BTreeMap<String, HarnessEntry>,
    #[serde(default)]
    pub skill: BTreeMap<String, HarnessEntry>,
    #[serde(default)]
    pub subagent: BTreeMap<String, HarnessEntry>,
}

impl HarnessEntries {
    fn records(&self, kind: RefinementKind) -> &BTreeMap<String, HarnessEntry> {
        match kind {
            RefinementKind::Prompt | RefinementKind::Unknown => &self.prompt,
            RefinementKind::Memory => &self.memory,
            RefinementKind::Skill => &self.skill,
            RefinementKind::Subagent => &self.subagent,
        }
    }

    fn records_mut(&mut self, kind: RefinementKind) -> &mut BTreeMap<String, HarnessEntry> {
        match kind {
            RefinementKind::Prompt | RefinementKind::Unknown => &mut self.prompt,
            RefinementKind::Memory => &mut self.memory,
            RefinementKind::Skill => &mut self.skill,
            RefinementKind::Subagent => &mut self.subagent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessRefinementEvent {
    pub id: String,
    pub trigger: String,
    pub changes: Vec<String>,
    pub evidence: String,
    pub outcome: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RefinementResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessState {
    pub schema: u16,
    #[serde(default)]
    pub entries: HarnessEntries,
    #[serde(default)]
    pub refinements: Vec<HarnessRefinementEvent>,
}

impl Default for HarnessState {
    fn default() -> Self {
        Self {
            schema: 1,
            entries: HarnessEntries::default(),
            refinements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefinementEdit {
    #[serde(default)]
    pub action: RefinementAction,
    #[serde(default)]
    pub kind: RefinementKind,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub reference: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub arguments: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedRefinementEdit {
    pub action: RefinementAction,
    pub kind: RefinementKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<HarnessEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<HarnessEntry>,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementResult {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub applied_edits: Vec<AppliedRefinementEdit>,
    pub harness_state_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<String>,
    pub scope: HarnessScope,
}

#[derive(Debug, Clone, Default)]
pub struct RefineOptions<'a> {
    pub instructions: Option<&'a str>,
    pub rollback_id: Option<&'a str>,
    pub global: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct RefinementProposal {
    #[serde(default = "default_summary")]
    summary: String,
    #[serde(default)]
    rationale: String,
    #[serde(default, rename = "expectedOutcome")]
    expected_outcome: String,
    #[serde(default)]
    edits: Vec<RefinementEdit>,
}

fn default_summary() -> String {
    "Refined continual harness state".into()
}

/// Plans and applies one local or global continual-harness refinement.
///
/// # Errors
///
/// Returns a validation, provider, JSON protocol, safe-path, or durable I/O
/// error. Invalid edits are represented as unapplied result entries rather than
/// aborting otherwise valid edits in the same proposal.
pub async fn refine(
    runtime: &AgentRuntime,
    state_root: &Path,
    session_id: &str,
    options: RefineOptions<'_>,
) -> Result<RefinementResult> {
    validate_options(session_id, &options)?;
    let root = canonical_state_root(state_root);
    let (proposal, rollback_of, scope, baseline, state_path) =
        if let Some(rollback_id) = options.rollback_id {
            let target = find_history_result(&root, session_id, rollback_id).await?;
            let (scope, state_path) = rollback_target_path(&root, session_id, &target)?;
            if !state_path.exists() {
                return Err(MimirError::Protocol(format!(
                    "Refinement {} state file not found: {}",
                    target.id,
                    state_path.display()
                )));
            }
            let baseline = load_state(&root, &state_path).await?;
            (
                rollback_proposal(&target),
                Some(target.id),
                scope,
                baseline,
                state_path,
            )
        } else {
            let scope = if options.global {
                HarnessScope::Global
            } else {
                HarnessScope::Local
            };
            let state_path = harness_state_path(&root, session_id, scope);
            let baseline = load_state(&root, &state_path).await?;
            let global_context = if scope == HarnessScope::Local {
                let global_path = harness_state_path(&root, session_id, HarnessScope::Global);
                Some(load_state(&root, &global_path).await?)
            } else {
                None
            };
            let history = load_all_history(&root, session_id).await?;
            let prompt = refinement_prompt(
                runtime,
                &baseline,
                global_context.as_ref(),
                &history,
                options.instructions,
                scope,
            )
            .await?;
            let response = runtime
                .complete_control_request(REFINEMENT_SYSTEM_PROMPT, &prompt, 32_000)
                .await?;
            (
                parse_proposal(&response)?,
                None,
                scope,
                baseline,
                state_path,
            )
        };

    let history_path = history_path_for_state(&state_path)?;
    prepare_state_path(&root, &state_path).await?;
    prepare_state_path(&root, &history_path).await?;
    let lock = path_lock(&state_path);
    let _guard = lock.lock().await;
    let mut state = load_state(&root, &state_path).await?;
    let id = format!("refine_{}", Uuid::new_v4().simple());
    let applied_edits = apply_proposal(&mut state, &baseline, &proposal, scope);
    let created_at = Utc::now().to_rfc3339();
    let changes = applied_edits
        .iter()
        .filter(|edit| edit.applied)
        .map(|edit| format!("{:?} {:?}:{}", edit.action, edit.kind, edit.id).to_lowercase())
        .collect();
    let result = RefinementResult {
        id,
        summary: proposal.summary,
        rationale: proposal.rationale,
        expected_outcome: proposal.expected_outcome,
        applied_edits,
        harness_state_path: state_path.to_string_lossy().into_owned(),
        rollback_of,
        scope,
    };
    state.refinements.push(HarnessRefinementEvent {
        id: result.id.clone(),
        trigger: result.summary.clone(),
        changes,
        evidence: result.rationale.clone(),
        outcome: result.expected_outcome.clone(),
        created_at,
        result: Some(result.clone()),
    });
    write_json(&state_path, &state).await?;
    set_private_permissions(&state_path).await?;
    append_history(&root, &history_path, &result).await?;
    Ok(result)
}

fn history_path_for_state(state_path: &Path) -> Result<PathBuf> {
    state_path
        .parent()
        .map(|parent| parent.join("refinements.jsonl"))
        .ok_or_else(|| {
            MimirError::Protocol(format!(
                "Harness state path has no parent: {}",
                state_path.display()
            ))
        })
}

/// Loads global and session-local continual-harness entries as bounded system
/// context for subsequent agent turns.
///
/// # Errors
///
/// Returns a safe-path, JSON, or I/O error while loading either harness store.
pub async fn load_harness_context(state_root: &Path, session_id: &str) -> Result<String> {
    validate_options(session_id, &RefineOptions::default())?;
    let root = canonical_state_root(state_root);
    let global = load_state(
        &root,
        &harness_state_path(&root, session_id, HarnessScope::Global),
    )
    .await?;
    let local = load_state(
        &root,
        &harness_state_path(&root, session_id, HarnessScope::Local),
    )
    .await?;
    Ok(format_harness_context(&global, &local))
}

fn format_harness_context(global: &HarnessState, local: &HarnessState) -> String {
    let mut lines = vec![
        "## Continual Harness".to_owned(),
        "Use these persisted prompt notes, memories, skills, and subagent specifications as reusable context. Local entries take precedence for this session.".to_owned(),
    ];
    for (scope, state) in [(HarnessScope::Global, global), (HarnessScope::Local, local)] {
        for kind in [
            RefinementKind::Prompt,
            RefinementKind::Memory,
            RefinementKind::Skill,
            RefinementKind::Subagent,
        ] {
            for entry in state.entries.records(kind).values().take(6) {
                let content = compact_text(&entry.content, 180);
                lines.push(format!(
                    "- [{} {}:{}] {} ({} v{}): {content}",
                    scope.as_str(),
                    kind.as_str(),
                    entry.id,
                    entry.title,
                    entry.path,
                    entry.version
                ));
                if kind == RefinementKind::Skill {
                    if !entry.reference.is_empty() {
                        lines.push(format!(
                            "  reference: {}",
                            serde_json::Value::Object(
                                entry.reference.clone().into_iter().collect()
                            )
                        ));
                    }
                    if !entry.arguments.is_empty() {
                        lines.push(format!(
                            "  arguments: {}",
                            serde_json::Value::Object(
                                entry.arguments.clone().into_iter().collect()
                            )
                        ));
                    }
                }
            }
        }
    }
    if lines.len() == 2 {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut compact = normalized
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    compact.push_str("...");
    compact
}

fn validate_options(session_id: &str, options: &RefineOptions<'_>) -> Result<()> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(MimirError::Configuration(
            "session id must contain only letters, digits, '-' or '_'".into(),
        ));
    }
    if options
        .instructions
        .is_some_and(|instructions| instructions.len() > MAX_INSTRUCTIONS_BYTES)
    {
        return Err(MimirError::Protocol(
            "refinement instructions exceed the 16 KiB limit".into(),
        ));
    }
    if options
        .rollback_id
        .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
    {
        return Err(MimirError::Protocol(
            "rollbackId must be a non-empty identifier of at most 128 characters".into(),
        ));
    }
    Ok(())
}

async fn refinement_prompt(
    runtime: &AgentRuntime,
    state: &HarnessState,
    global_context: Option<&HarnessState>,
    history: &[RefinementResult],
    instructions: Option<&str>,
    scope: HarnessScope,
) -> Result<String> {
    let messages = runtime.messages_snapshot().await;
    let trajectory = suffix_chars(&serde_json::to_string(&messages)?, 80_000);
    let current_state = serde_json::to_string(state)?;
    let global_context = global_context
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_else(|| "null".into());
    let history = serde_json::to_string(&history[history.len().saturating_sub(20)..])?;
    let scope = match scope {
        HarnessScope::Local => "local session",
        HarnessScope::Global => "global cross-session",
    };
    Ok(format!(
        "<current_harness_state>\n{current_state}\n</current_harness_state>\n\n<global_read_only_context>\n{global_context}\n</global_read_only_context>\n\n<refinement_history>\n{history}\n</refinement_history>\n\n<conversation>\n{trajectory}\n</conversation>\n\n<scope>{scope}</scope>\n\n<user_refine_instructions>{}</user_refine_instructions>\n\nReturn JSON only.",
        instructions.unwrap_or_default()
    ))
}

fn suffix_chars(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    value.chars().skip(count.saturating_sub(limit)).collect()
}

fn parse_proposal(text: &str) -> Result<RefinementProposal> {
    let trimmed = text.trim();
    let candidate = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed
    } else if let Some(fence) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        fence.strip_suffix("```").map(str::trim).ok_or_else(|| {
            MimirError::Protocol("refiner returned an unterminated JSON fence".into())
        })?
    } else {
        let start = trimmed
            .find('{')
            .ok_or_else(|| MimirError::Protocol("refiner did not return a JSON object".into()))?;
        let end = trimmed
            .rfind('}')
            .ok_or_else(|| MimirError::Protocol("refiner returned incomplete JSON".into()))?;
        &trimmed[start..=end]
    };
    serde_json::from_str(candidate)
        .map_err(|error| MimirError::Protocol(format!("refiner returned invalid JSON: {error}")))
}

fn apply_proposal(
    state: &mut HarnessState,
    baseline: &HarnessState,
    proposal: &RefinementProposal,
    scope: HarnessScope,
) -> Vec<AppliedRefinementEdit> {
    proposal
        .edits
        .iter()
        .map(|edit| apply_edit(state, baseline, edit, scope))
        .collect()
}

fn apply_edit(
    state: &mut HarnessState,
    baseline: &HarnessState,
    edit: &RefinementEdit,
    scope: HarnessScope,
) -> AppliedRefinementEdit {
    let id = edit.id.as_deref().map(strip_scope_prefix).map_or_else(
        || slug(edit.title.as_deref().unwrap_or("entry")),
        str::to_owned,
    );
    let mut result = applied_shell(edit, id.clone());
    if let Some(error) = validate_edit(edit, &id) {
        result.error = Some(error);
        return result;
    }
    let baseline_entry = baseline.entries.records(edit.kind).get(&id).cloned();
    let records = state.entries.records_mut(edit.kind);
    let before = records.get(&id).cloned();
    if before != baseline_entry {
        result.before = before;
        result.error = Some("entry changed during refinement planning".into());
        return result;
    }
    result.before.clone_from(&before);
    match edit.action {
        RefinementAction::Delete => {
            if before.is_none() {
                result.error = Some("entry not found".into());
                return result;
            }
            records.remove(&id);
            result.applied = true;
        }
        RefinementAction::Create if before.is_some() => {
            result.error = Some("entry already exists".into());
        }
        RefinementAction::Update if before.is_none() => {
            result.error = Some("entry not found".into());
        }
        RefinementAction::Create | RefinementAction::Update => {
            let now = Utc::now().to_rfc3339();
            let after = HarnessEntry {
                id: id.clone(),
                kind: edit.kind,
                title: edit
                    .title
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.title.clone()))
                    .unwrap_or_else(|| id.clone()),
                content: edit
                    .content
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.content.clone()))
                    .unwrap_or_default(),
                path: edit
                    .path
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.path.clone()))
                    .unwrap_or_else(|| "general".into()),
                scope: before.as_ref().map_or(scope, |entry| entry.scope),
                reference: edit
                    .reference
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.reference.clone()))
                    .unwrap_or_default(),
                arguments: edit
                    .arguments
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.arguments.clone()))
                    .unwrap_or_default(),
                metadata: edit
                    .metadata
                    .clone()
                    .or_else(|| before.as_ref().map(|entry| entry.metadata.clone()))
                    .unwrap_or_default(),
                source: "refine".into(),
                created_at: before
                    .as_ref()
                    .map_or_else(|| now.clone(), |entry| entry.created_at.clone()),
                updated_at: now,
                version: before
                    .as_ref()
                    .map_or(1, |entry| entry.version.saturating_add(1)),
            };
            records.insert(id, after.clone());
            result.after = Some(after);
            result.applied = true;
        }
        RefinementAction::Unknown => {
            result.error = Some("unsupported action".into());
        }
    }
    result
}

fn applied_shell(edit: &RefinementEdit, id: String) -> AppliedRefinementEdit {
    AppliedRefinementEdit {
        action: edit.action,
        kind: edit.kind,
        id,
        title: edit.title.clone(),
        content: edit.content.clone(),
        path: edit.path.clone(),
        reference: edit.reference.clone(),
        arguments: edit.arguments.clone(),
        metadata: edit.metadata.clone(),
        reason: edit.reason.clone(),
        before: None,
        after: None,
        applied: false,
        error: None,
    }
}

fn validate_edit(edit: &RefinementEdit, id: &str) -> Option<String> {
    if edit.action == RefinementAction::Unknown {
        return Some("unsupported action".into());
    }
    if edit.kind == RefinementKind::Unknown {
        return Some("unsupported kind".into());
    }
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Some("entry id must be non-empty, control-free, and at most 128 characters".into());
    }
    if edit.kind == RefinementKind::Prompt && id == "base_system_prompt" {
        return Some("base system prompt is not editable".into());
    }
    if edit.action != RefinementAction::Create && edit.id.is_none() {
        return Some(format!("{:?} requires id", edit.action).to_lowercase());
    }
    if edit.action != RefinementAction::Delete
        && (edit.title.as_deref().is_none_or(str::is_empty)
            || edit.content.as_deref().is_none_or(str::is_empty))
    {
        return Some(format!("{:?} requires title and content", edit.action).to_lowercase());
    }
    if edit.action != RefinementAction::Delete && edit.kind == RefinementKind::Skill {
        let Some(reference) = &edit.reference else {
            return Some("skill requires python reference".into());
        };
        if reference.get("type").and_then(serde_json::Value::as_str) != Some("python") {
            return Some("skill reference.type must be python".into());
        }
        let has_import = reference
            .get("import")
            .or_else(|| reference.get("python_import"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if !has_import {
            return Some("skill requires python import".into());
        }
        let has_callable = reference
            .get("callable")
            .or_else(|| reference.get("call_pattern"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if !has_callable {
            return Some("skill requires callable or call_pattern".into());
        }
        if edit.arguments.is_none() {
            return Some("skill requires arguments".into());
        }
    }
    None
}

fn strip_scope_prefix(id: &str) -> &str {
    id.strip_prefix("local:")
        .or_else(|| id.strip_prefix("global:"))
        .unwrap_or(id)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('_');
            }
            separator = false;
            output.push(character);
        } else {
            separator = true;
        }
        if output.len() >= 80 {
            break;
        }
    }
    let output = output.trim_end_matches('_');
    if output.is_empty() {
        "entry".into()
    } else {
        output.into()
    }
}

fn rollback_proposal(target: &RefinementResult) -> RefinementProposal {
    let mut edits = Vec::new();
    for applied in target
        .applied_edits
        .iter()
        .rev()
        .filter(|edit| edit.applied)
    {
        let edit = if let Some(before) = &applied.before {
            RefinementEdit {
                action: if applied.after.is_some() {
                    RefinementAction::Update
                } else {
                    RefinementAction::Create
                },
                kind: applied.kind,
                id: Some(applied.id.clone()),
                title: Some(before.title.clone()),
                content: Some(before.content.clone()),
                path: Some(before.path.clone()),
                reference: Some(before.reference.clone()),
                arguments: Some(before.arguments.clone()),
                metadata: Some(before.metadata.clone()),
                reason: Some(format!("Rollback {}", target.id)),
            }
        } else {
            RefinementEdit {
                action: RefinementAction::Delete,
                kind: applied.kind,
                id: Some(applied.id.clone()),
                title: None,
                content: None,
                path: None,
                reference: None,
                arguments: None,
                metadata: None,
                reason: Some(format!("Rollback {}", target.id)),
            }
        };
        edits.push(edit);
    }
    RefinementProposal {
        summary: format!("Rollback refinement {}", target.id),
        rationale: format!(
            "Restores continual harness state snapshots from refinement {}.",
            target.id
        ),
        expected_outcome: "Faulty refinement edits are reverted.".into(),
        edits,
    }
}

fn rollback_target_path(
    root: &Path,
    session_id: &str,
    target: &RefinementResult,
) -> Result<(HarnessScope, PathBuf)> {
    let fallback = harness_state_path(root, session_id, target.scope);
    let path = if target.harness_state_path.trim().is_empty() {
        fallback
    } else {
        let recorded = PathBuf::from(&target.harness_state_path);
        if recorded.is_absolute() {
            recorded
        } else {
            root.join(recorded)
        }
    };
    if !path.starts_with(root.join("harness"))
        || path.file_name().and_then(std::ffi::OsStr::to_str) != Some("harness_state.json")
    {
        return Err(MimirError::Session {
            path,
            message: "recorded refinement path escapes the configured harness root".into(),
        });
    }
    let global_path = harness_state_path(root, session_id, HarnessScope::Global);
    let scope = if path == global_path {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    };
    Ok((scope, path))
}

async fn load_all_history(root: &Path, session_id: &str) -> Result<Vec<RefinementResult>> {
    let mut history = Vec::new();
    for scope in [HarnessScope::Global, HarnessScope::Local] {
        let state_path = harness_state_path(root, session_id, scope);
        let state = load_state(root, &state_path).await?;
        for result in state
            .refinements
            .into_iter()
            .filter_map(|event| event.result)
        {
            upsert_history(&mut history, result);
        }
        let history_path = refinement_history_path(root, session_id, scope);
        for result in load_history(root, &history_path).await? {
            upsert_history(&mut history, result);
        }
    }
    let session = FileSessionStore::create(root, session_id).await?;
    for record in session.load().await?.records {
        let SessionPayload::RuntimeEvent { name, detail } = record.payload else {
            continue;
        };
        if name != "refinement" {
            continue;
        }
        let result = serde_json::from_str(&detail).map_err(|error| MimirError::Session {
            path: session.path().to_owned(),
            message: format!("invalid persisted refinement event: {error}"),
        })?;
        upsert_history(&mut history, result);
    }
    Ok(history)
}

fn upsert_history(history: &mut Vec<RefinementResult>, result: RefinementResult) {
    if let Some(existing) = history.iter_mut().find(|item| item.id == result.id) {
        *existing = result;
    } else {
        history.push(result);
    }
}

async fn find_history_result(root: &Path, session_id: &str, id: &str) -> Result<RefinementResult> {
    if let Some(result) = load_all_history(root, session_id)
        .await?
        .into_iter()
        .find(|result| result.id == id)
    {
        return Ok(result);
    }
    Err(MimirError::Protocol(format!("Refinement {id} not found")))
}

async fn load_state(root: &Path, path: &Path) -> Result<HarnessState> {
    prepare_state_path(root, path).await?;
    Ok(read_json(path).await?.unwrap_or_default())
}

async fn load_history(root: &Path, path: &Path) -> Result<Vec<RefinementResult>> {
    prepare_state_path(root, path).await?;
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > MAX_HISTORY_BYTES {
        return Err(MimirError::Session {
            path: path.to_owned(),
            message: "refinement history exceeds the 16 MiB limit".into(),
        });
    }
    let text = tokio::fs::read_to_string(path).await?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|error| MimirError::Session {
                path: path.to_owned(),
                message: format!("invalid refinement history at line {}: {error}", index + 1),
            })
        })
        .collect()
}

async fn append_history(root: &Path, path: &Path, result: &RefinementResult) -> Result<()> {
    prepare_state_path(root, path).await?;
    let mut encoded = serde_json::to_vec(result)?;
    encoded.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    set_private_permissions(path).await?;
    file.write_all(&encoded).await?;
    file.sync_all().await?;
    Ok(())
}

fn harness_state_path(root: &Path, session_id: &str, scope: HarnessScope) -> PathBuf {
    harness_dir(root, session_id, scope).join("harness_state.json")
}

fn refinement_history_path(root: &Path, session_id: &str, scope: HarnessScope) -> PathBuf {
    harness_dir(root, session_id, scope).join("refinements.jsonl")
}

fn harness_dir(root: &Path, session_id: &str, scope: HarnessScope) -> PathBuf {
    match scope {
        HarnessScope::Local => root.join("harness/sessions").join(session_id),
        HarnessScope::Global => root.join("harness/global"),
    }
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
