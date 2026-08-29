use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::model::ToolDefinition;

use super::{
    DestructiveAction, Tool, ToolError, ToolObservation, ToolPolicy, WorkspacePathPolicy,
    object_schema, parse_input, truncate_utf8,
};

macro_rules! file_tool {
    ($name:ident) => {
        pub struct $name {
            paths: WorkspacePathPolicy,
            policy: ToolPolicy,
        }

        impl $name {
            pub fn new(paths: WorkspacePathPolicy, policy: ToolPolicy) -> Self {
                Self { paths, policy }
            }
        }
    };
}

file_tool!(ReadFileTool);
file_tool!(WriteFileTool);
file_tool!(EditFileTool);
file_tool!(ListFilesTool);
file_tool!(SearchTool);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    path: String,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        definition(&self.paths, "read_file", "Read a UTF-8 file", &["path"])
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: PathInput = parse_input("read_file", input)?;
        let path = self.paths.resolve_existing(&input.path)?;
        let file = tokio::fs::File::open(&path).await?;
        let read_limit = u64::try_from(self.policy.max_output_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(self.policy.max_output_bytes.min(64 * 1024));
        file.take(read_limit).read_to_end(&mut bytes).await?;
        let decoded = String::from_utf8_lossy(&bytes);
        let (content, truncated) = truncate_utf8(&decoded, self.policy.max_output_bytes);
        let mut observation = ToolObservation::success(
            if truncated {
                "file read; output truncated"
            } else {
                "file read"
            },
            content,
        );
        observation.artifacts.push(path);
        Ok(observation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
    #[serde(default)]
    provenance: Option<Value>,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: format!(
                "Write a UTF-8 file, creating missing parent directories. {}",
                self.paths.path_guidance()
            ),
            parameters: object_schema(
                &json!({
                    "path": workspace_path_schema(&self.paths),
                    "content": {"type": "string"},
                    "provenance": provenance_schema()
                }),
                &["path", "content"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        if !self.policy.allow_write {
            return Err(ToolError::Disabled {
                tool: "write_file".into(),
            });
        }
        let input: WriteInput = parse_input("write_file", input)?;
        let _provenance = input.provenance;
        if input.content.len() > self.policy.max_write_bytes {
            return Err(ToolError::Execution {
                tool: "write_file".into(),
                message: format!("content exceeds {} bytes", self.policy.max_write_bytes),
            });
        }
        let path = self.paths.resolve_for_write(&input.path)?;
        require_write_approval(&self.policy, "write_file", &input.path)?;
        let parent = path.parent().ok_or_else(|| ToolError::Execution {
            tool: "write_file".into(),
            message: "target path has no parent directory".into(),
        })?;
        tokio::fs::create_dir_all(parent).await?;
        // Re-resolve after creating directories so a newly introduced symlink
        // cannot redirect the subsequent atomic replacement outside the workspace.
        let path = self.paths.resolve_for_write(&input.path)?;
        atomic_replace(&path, input.content.as_bytes()).await?;
        let mut observation = ToolObservation::success("file written", "");
        observation.artifacts.push(path);
        Ok(observation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    provenance: Option<Value>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".into(),
            description: format!(
                "Replace one exact text occurrence in a file. {}",
                self.paths.path_guidance()
            ),
            parameters: object_schema(
                &json!({
                    "path": workspace_path_schema(&self.paths),
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "provenance": provenance_schema()
                }),
                &["path", "old_text", "new_text"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        if !self.policy.allow_write {
            return Err(ToolError::Disabled {
                tool: "edit_file".into(),
            });
        }
        let input: EditInput = parse_input("edit_file", input)?;
        let _provenance = input.provenance;
        if input.old_text.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "edit_file".into(),
                message: "old_text must not be empty".into(),
            });
        }
        let path = self.paths.resolve_existing(&input.path)?;
        let content = tokio::fs::read_to_string(&path).await?;
        if content.matches(&input.old_text).count() != 1 {
            return Err(ToolError::Execution {
                tool: "edit_file".into(),
                message: "old_text must occur exactly once".into(),
            });
        }
        let updated = content.replacen(&input.old_text, &input.new_text, 1);
        if updated.len() > self.policy.max_write_bytes {
            return Err(ToolError::Execution {
                tool: "edit_file".into(),
                message: format!("result exceeds {} bytes", self.policy.max_write_bytes),
            });
        }
        require_write_approval(&self.policy, "edit_file", &input.path)?;
        atomic_replace(&path, updated.as_bytes()).await?;
        let mut observation = ToolObservation::success("file edited", "");
        observation.artifacts.push(path);
        Ok(observation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    #[serde(default = "dot")]
    path: String,
    #[serde(default = "default_depth")]
    max_depth: usize,
}

#[async_trait]
impl Tool for ListFilesTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            &self.paths,
            "list_files",
            "List files under a directory",
            &[],
        )
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: ListInput = parse_input("list_files", input)?;
        let root = self.paths.resolve_existing(&input.path)?;
        let mut entries = Vec::new();
        for entry in WalkDir::new(&root)
            .max_depth(input.max_depth.min(20))
            .into_iter()
            .filter_map(Result::ok)
            .take(500)
        {
            if entry.path() == root {
                continue;
            }
            if let Ok(relative) = entry.path().strip_prefix(self.paths.root()) {
                entries.push(relative.display().to_string());
            }
        }
        entries.sort();
        let joined = entries.join("\n");
        let (content, truncated) = truncate_utf8(&joined, self.policy.max_output_bytes);
        Ok(ToolObservation::success(
            if truncated {
                "files listed; output truncated"
            } else {
                "files listed"
            },
            content,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    pattern: String,
    #[serde(default = "dot")]
    path: String,
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search".into(),
            description: format!(
                "Search UTF-8 files with a regular expression. {}",
                self.paths.path_guidance()
            ),
            parameters: object_schema(
                &json!({
                    "pattern": {"type": "string"},
                    "path": workspace_path_schema(&self.paths)
                }),
                &["pattern"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: SearchInput = parse_input("search", input)?;
        let regex = Regex::new(&input.pattern).map_err(|error| ToolError::InvalidArguments {
            tool: "search".into(),
            message: error.to_string(),
        })?;
        let root = self.paths.resolve_existing(&input.path)?;
        let mut matches = Vec::new();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .take(10_000)
        {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let max_scan_bytes = self
                .policy
                .max_output_bytes
                .saturating_mul(16)
                .max(1024 * 1024);
            if metadata.len() > u64::try_from(max_scan_bytes).unwrap_or(u64::MAX) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    let relative = entry
                        .path()
                        .strip_prefix(self.paths.root())
                        .unwrap_or(entry.path());
                    matches.push(format!("{}:{}:{line}", relative.display(), index + 1));
                    if matches.len() == 500 {
                        break;
                    }
                }
            }
            if matches.len() == 500 {
                break;
            }
        }
        let joined = matches.join("\n");
        let (content, truncated) = truncate_utf8(&joined, self.policy.max_output_bytes);
        Ok(ToolObservation::success(
            if truncated {
                "search complete; output truncated"
            } else {
                "search complete"
            },
            content,
        ))
    }
}

fn definition(
    paths: &WorkspacePathPolicy,
    name: &str,
    description: &str,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{description}. {}", paths.path_guidance()),
        parameters: object_schema(&json!({"path": workspace_path_schema(paths)}), required),
    }
}

fn workspace_path_schema(paths: &WorkspacePathPolicy) -> Value {
    json!({
        "type": "string",
        "description": paths.path_guidance()
    })
}

fn provenance_schema() -> Value {
    json!({
        "type": "object",
        "description": "For source-derived content, cite successful read_file calls. Set required=true when the mutation must be faithful to those sources; unavailable required evidence pauses the mutation instead of guessing.",
        "additionalProperties": false,
        "properties": {
            "required": {"type": "boolean"},
            "derivedFrom": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "toolCallId": {"type": "string"},
                        "path": {"type": "string"}
                    },
                    "required": ["toolCallId", "path"]
                }
            }
        }
    })
}

fn dot() -> String {
    ".".into()
}

fn default_depth() -> usize {
    2
}

fn require_write_approval(
    policy: &ToolPolicy,
    tool: &str,
    requested_path: &str,
) -> Result<(), ToolError> {
    if let Some(approvals) = &policy.approvals
        && let Some(request) = approvals.requires_action_approval(
            DestructiveAction::FilesystemWrite,
            &format!("{tool} $WORKSPACE/{requested_path}"),
        )?
    {
        return Err(ToolError::ApprovalRequired { request });
    }
    Ok(())
}

async fn atomic_replace(path: &std::path::Path, content: &[u8]) -> Result<(), ToolError> {
    let parent = path.parent().ok_or_else(|| ToolError::Execution {
        tool: "write_file".into(),
        message: "target has no parent".into(),
    })?;
    let temporary = parent.join(format!(".mimir-{}.tmp", Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, content).await?;
    file.sync_all().await?;
    drop(file);
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}
