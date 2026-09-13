use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::model::ToolDefinition;

use super::{
    Tool, ToolError, ToolObservation, WorkspacePathPolicy, file::atomic_replace, object_schema,
    parse_input,
};

const MAX_PLAN_BYTES: usize = 2 * 1024 * 1024;
const MAX_QUESTION_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClarifyingOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClarifyingQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<ClarifyingOption>,
}

impl ClarifyingQuestion {
    #[must_use]
    pub fn formatted(&self) -> String {
        let mut output = format!("{}\n", self.question);
        for (index, option) in self.options.iter().enumerate() {
            let recommended = if index == 0 { " (Recommended)" } else { "" };
            writeln!(
                output,
                "{}. {}{} — {}",
                index + 1,
                option.label,
                recommended,
                option.description
            )
            .expect("writing to a String cannot fail");
        }
        output.push_str("Or provide another answer.");
        output
    }

    fn validate(&self) -> Result<(), ToolError> {
        if self.id.trim().is_empty()
            || self.id.len() > 128
            || self.id.chars().any(char::is_control)
            || self.header.trim().is_empty()
            || self.header.len() > 32
            || self.question.trim().is_empty()
            || self.question.len() > MAX_QUESTION_BYTES
            || self.options.len() < 2
            || self.options.len() > 3
        {
            return Err(ToolError::InvalidArguments {
                tool: "ask_user".into(),
                message: "question requires a bounded id, short header, prompt, and 2 to 3 options"
                    .into(),
            });
        }
        if self.options.iter().any(|option| {
            option.label.trim().is_empty()
                || option.label.len() > 64
                || option.description.trim().is_empty()
                || option.description.len() > 512
                || option.label.chars().any(char::is_control)
                || option.description.chars().any(char::is_control)
        }) {
            return Err(ToolError::InvalidArguments {
                tool: "ask_user".into(),
                message: "option labels and descriptions must be non-empty and bounded".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanState {
    artifact: Option<String>,
    pending_question: Option<ClarifyingQuestion>,
    handed_off: bool,
}

#[derive(Debug)]
pub struct PlanContextStore {
    workspace: PathBuf,
    state_root: PathBuf,
    state_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl PlanContextStore {
    /// Creates a workspace/session-scoped plan context without mutating either root.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the workspace cannot be canonicalized.
    pub fn new(workspace: &Path, state_root: &Path, session: &str) -> Result<Self, ToolError> {
        let workspace = workspace.canonicalize()?;
        let state_root = crate::atomic::canonical_state_root(state_root);
        let mut hasher = Sha256::new();
        hasher.update(workspace.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(session.as_bytes());
        let digest = hasher.finalize();
        let mut key = String::with_capacity(32);
        for byte in &digest[..16] {
            write!(key, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let state_path = state_root.join("plan-mode").join(format!("{key}.json"));
        Ok(Self {
            workspace,
            state_root,
            state_path: state_path.clone(),
            lock: crate::atomic::path_lock(&state_path),
        })
    }

    /// Clears a completed handoff while retaining unfinished plan state across restart.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be read or written.
    pub async fn prepare(&self) -> Result<(), ToolError> {
        let _guard = self.lock.lock().await;
        let mut state = self.read_unlocked().await?;
        if state.handed_off {
            state = PlanState::default();
            self.write_unlocked(&state).await?;
        }
        Ok(())
    }

    /// Returns the unresolved structured clarification for this workspace and session.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be read.
    pub async fn pending_question(&self) -> Result<Option<ClarifyingQuestion>, ToolError> {
        let _guard = self.lock.lock().await;
        Ok(self.read_unlocked().await?.pending_question)
    }

    /// Persists one unresolved structured clarification.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be read or written.
    pub async fn set_pending_question(&self, request: ClarifyingQuestion) -> Result<(), ToolError> {
        let _guard = self.lock.lock().await;
        let mut state = self.read_unlocked().await?;
        state.pending_question = Some(request);
        self.write_unlocked(&state).await
    }

    /// Clears the pending clarification after the next user answer is submitted.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when private plan state cannot be read or written.
    pub async fn clear_pending_question(&self) -> Result<(), ToolError> {
        let _guard = self.lock.lock().await;
        let mut state = self.read_unlocked().await?;
        if state.pending_question.take().is_some() {
            self.write_unlocked(&state).await?;
        }
        Ok(())
    }

    /// Returns the safe, non-empty regular plan artifact currently bound to this session.
    ///
    /// # Errors
    ///
    /// Returns a policy or I/O error when the bound artifact is unsafe or unreadable.
    pub async fn validated_artifact(&self) -> Result<Option<PathBuf>, ToolError> {
        let _guard = self.lock.lock().await;
        let state = self.read_unlocked().await?;
        let Some(relative) = state.artifact else {
            return Ok(None);
        };
        let paths = WorkspacePathPolicy::new(&self.workspace)?;
        let requested = self.workspace.join(&relative);
        let requested_metadata = std::fs::symlink_metadata(&requested)?;
        if requested_metadata.file_type().is_symlink() {
            return Err(ToolError::Execution {
                tool: "write_plan".into(),
                message: "bound plan artifact must not be a symlink".into(),
            });
        }
        let path = paths.resolve_existing(&relative)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
            return Err(ToolError::Execution {
                tool: "write_plan".into(),
                message: "bound plan artifact must be a non-empty regular file".into(),
            });
        }
        Ok(Some(path))
    }

    /// Records that a validated plan was handed to a rebuilt auto-mode runtime.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when no artifact is bound or state cannot be written.
    pub async fn mark_handed_off(&self) -> Result<(), ToolError> {
        let _guard = self.lock.lock().await;
        let mut state = self.read_unlocked().await?;
        if state.artifact.is_none() {
            return Err(ToolError::Execution {
                tool: "write_plan".into(),
                message: "no plan artifact is ready for implementation".into(),
            });
        }
        state.handed_off = true;
        state.pending_question = None;
        self.write_unlocked(&state).await
    }

    async fn write_plan(&self, title: &str, markdown: &str) -> Result<PathBuf, ToolError> {
        validate_plan(title, markdown)?;
        let _guard = self.lock.lock().await;
        let mut state = self.read_unlocked().await?;
        let paths = WorkspacePathPolicy::new(&self.workspace)?;
        let (relative, path) = if let Some(relative) = state.artifact.clone() {
            reject_non_regular_plan_target(&self.workspace.join(&relative))?;
            let mut path = paths.resolve_for_write(&relative)?;
            let parent = path.parent().ok_or_else(|| ToolError::Execution {
                tool: "write_plan".into(),
                message: "plan path has no parent directory".into(),
            })?;
            tokio::fs::create_dir_all(parent).await?;
            path = paths.resolve_for_write(&relative)?;
            atomic_replace(&path, markdown.as_bytes()).await?;
            (relative, path)
        } else {
            create_new_plan(&paths, title, markdown).await?
        };
        state.artifact = Some(relative);
        state.pending_question = None;
        state.handed_off = false;
        self.write_unlocked(&state).await?;
        Ok(path)
    }

    async fn read_unlocked(&self) -> Result<PlanState, ToolError> {
        crate::atomic::read_json(&self.state_path)
            .await
            .map(Option::unwrap_or_default)
            .map_err(|error| state_error(&error))
    }

    async fn write_unlocked(&self, state: &PlanState) -> Result<(), ToolError> {
        crate::atomic::prepare_state_path(&self.state_root, &self.state_path)
            .await
            .map_err(|error| state_error(&error))?;
        crate::atomic::write_json(&self.state_path, state)
            .await
            .map_err(|error| state_error(&error))?;
        set_private_state_permissions(&self.state_path).await
    }
}

fn state_error(error: &crate::error::MimirError) -> ToolError {
    ToolError::Execution {
        tool: "plan_mode".into(),
        message: error.to_string(),
    }
}

async fn create_new_plan(
    paths: &WorkspacePathPolicy,
    title: &str,
    markdown: &str,
) -> Result<(String, PathBuf), ToolError> {
    let slug = slugify(title);
    for suffix in 1..=10_000_u32 {
        let name = if suffix == 1 {
            format!("plans/{slug}.md")
        } else {
            format!("plans/{slug}-{suffix}.md")
        };
        let mut candidate = paths.resolve_for_write(&name)?;
        let parent = candidate.parent().ok_or_else(|| ToolError::Execution {
            tool: "write_plan".into(),
            message: "plan path has no parent directory".into(),
        })?;
        tokio::fs::create_dir_all(parent).await?;
        candidate = paths.resolve_for_write(&name)?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut file) => {
                if let Err(error) = async {
                    file.write_all(markdown.as_bytes()).await?;
                    file.sync_all().await
                }
                .await
                {
                    drop(file);
                    let _ = tokio::fs::remove_file(&candidate).await;
                    return Err(error.into());
                }
                return Ok((name, candidate));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(ToolError::Execution {
        tool: "write_plan".into(),
        message: "could not allocate a unique plan filename".into(),
    })
}

fn reject_non_regular_plan_target(path: &Path) -> Result<(), ToolError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ToolError::Execution {
                tool: "write_plan".into(),
                message: "plan artifact must be a regular file and not a symlink".into(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
async fn set_private_state_permissions(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_state_permissions(_path: &Path) -> std::future::Ready<Result<(), ToolError>> {
    std::future::ready(Ok(()))
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            if slug.len() < 64 {
                slug.push(character);
            }
        } else {
            separator = true;
        }
        if slug.len() >= 64 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "plan".into() } else { slug }
}

fn validate_plan(title: &str, markdown: &str) -> Result<(), ToolError> {
    if title.trim().is_empty() || title.len() > 256 || title.chars().any(char::is_control) {
        return Err(ToolError::InvalidArguments {
            tool: "write_plan".into(),
            message: "title must contain 1 to 256 printable bytes".into(),
        });
    }
    if markdown.trim().is_empty() || markdown.len() > MAX_PLAN_BYTES {
        return Err(ToolError::InvalidArguments {
            tool: "write_plan".into(),
            message: format!("markdown must contain 1 to {MAX_PLAN_BYTES} bytes"),
        });
    }
    let lower = markdown.to_ascii_lowercase();
    for heading in [
        "summary",
        "implementation changes",
        "public interfaces",
        "tests",
        "assumptions",
    ] {
        if !lower.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with('#')
                && line
                    .trim_start_matches('#')
                    .trim()
                    .eq_ignore_ascii_case(heading)
        }) {
            return Err(ToolError::InvalidArguments {
                tool: "write_plan".into(),
                message: format!("plan is missing the '{heading}' heading"),
            });
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserInput {
    id: String,
    header: String,
    question: String,
    options: Vec<ClarifyingOption>,
}

pub(super) struct AskUserTool {
    context: Arc<PlanContextStore>,
}

impl AskUserTool {
    pub(super) fn new(context: Arc<PlanContextStore>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".into(),
            description: "Pause planning for one material clarification. Put the recommended option first; the TUI also permits a free-form answer.".into(),
            parameters: object_schema(
                &json!({
                    "id": {"type": "string"},
                    "header": {"type": "string"},
                    "question": {"type": "string"},
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string"}
                            },
                            "required": ["label", "description"]
                        }
                    }
                }),
                &["id", "header", "question", "options"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: AskUserInput = parse_input("ask_user", input)?;
        let request = ClarifyingQuestion {
            id: input.id,
            header: input.header,
            question: input.question,
            options: input.options,
        };
        request.validate()?;
        self.context.set_pending_question(request.clone()).await?;
        Err(ToolError::UserInputRequired { request })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WritePlanInput {
    title: String,
    markdown: String,
}

pub(super) struct WritePlanTool {
    paths: WorkspacePathPolicy,
    context: Arc<PlanContextStore>,
    max_write_bytes: usize,
}

impl WritePlanTool {
    pub(super) fn new(
        paths: WorkspacePathPolicy,
        context: Arc<PlanContextStore>,
        max_write_bytes: usize,
    ) -> Self {
        Self {
            paths,
            context,
            max_write_bytes,
        }
    }
}

#[async_trait]
impl Tool for WritePlanTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_plan".into(),
            description: format!(
                "Create or update this session's sole committable plan artifact under plans/. {}",
                self.paths.path_guidance()
            ),
            parameters: object_schema(
                &json!({
                    "title": {"type": "string"},
                    "markdown": {"type": "string"}
                }),
                &["title", "markdown"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: WritePlanInput = parse_input("write_plan", input)?;
        if input.markdown.len() > self.max_write_bytes.min(MAX_PLAN_BYTES) {
            return Err(ToolError::Execution {
                tool: "write_plan".into(),
                message: "plan exceeds the configured write limit".into(),
            });
        }
        let path = self
            .context
            .write_plan(&input.title, &input.markdown)
            .await?;
        Ok(ToolObservation {
            status: super::ObservationStatus::Success,
            summary: "plan artifact written".into(),
            next_actions: vec!["Review the plan, revise it if needed, then say implement".into()],
            artifacts: vec![path],
            content: "The decision-complete plan is ready for review.".into(),
        })
    }
}
