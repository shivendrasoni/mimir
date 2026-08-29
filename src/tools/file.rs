use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

use crate::model::ToolDefinition;

use super::{
    Tool, ToolError, ToolObservation, ToolPolicy, WorkspacePathPolicy, object_schema, parse_input,
    truncate_utf8,
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

const DISCOVERY_EXCLUDED_DIRECTORIES: &[&str] = &[
    ".mimir",
    ".git",
    ".hg",
    ".svn",
    ".next",
    ".nuxt",
    ".turbo",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "vendor",
    "venv",
];
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_FILES: usize = 10_000;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_SEARCH_FILE_BYTES: usize = 2 * 1024 * 1024;

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
            &format!(
                "List files under a directory. Recursive discovery skips harness state, version-control metadata, dependencies, and generated outputs ({})",
                DISCOVERY_EXCLUDED_DIRECTORIES.join(", ")
            ),
            &[],
        )
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: ListInput = parse_input("list_files", input)?;
        let root = self.paths.resolve_existing(&input.path)?;
        ensure_discovery_root("list_files", self.paths.root(), &root)?;
        let mut entries = Vec::new();
        let mut bounded = false;
        for entry in WalkDir::new(&root)
            .max_depth(input.max_depth.min(20))
            .into_iter()
            .filter_entry(include_discovery_entry)
            .filter_map(Result::ok)
        {
            if entry.path() == root {
                continue;
            }
            if entries.len() == MAX_LIST_ENTRIES {
                bounded = true;
                break;
            }
            if let Ok(relative) = entry.path().strip_prefix(self.paths.root()) {
                entries.push(relative.display().to_string());
            }
        }
        entries.sort();
        let joined = entries.join("\n");
        let (content, truncated) = truncate_utf8(&joined, self.policy.max_output_bytes);
        Ok(ToolObservation::success(
            if bounded || truncated {
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
                "Search UTF-8 files with a regular expression. Recursive search skips harness state, version-control metadata, dependencies, and generated outputs ({}). Use read_file only when a specific internal file is deliberately needed. {}",
                DISCOVERY_EXCLUDED_DIRECTORIES.join(", "),
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
        ensure_discovery_root("search", self.paths.root(), &root)?;
        let output_limit = self.policy.max_output_bytes.min(MAX_SEARCH_OUTPUT_BYTES);
        let mut output = String::new();
        let mut matches = 0_usize;
        let mut bounded = false;
        'files: for (files_scanned, entry) in WalkDir::new(root)
            .into_iter()
            .filter_entry(include_discovery_entry)
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .enumerate()
        {
            if files_scanned == MAX_SEARCH_FILES {
                bounded = true;
                break;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let max_scan_bytes = self
                .policy
                .max_output_bytes
                .saturating_mul(16)
                .clamp(1024 * 1024, MAX_SEARCH_FILE_BYTES);
            if metadata.len() > u64::try_from(max_scan_bytes).unwrap_or(u64::MAX) {
                continue;
            }
            let Ok(file_content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for (index, line) in file_content.lines().enumerate() {
                if regex.is_match(line) {
                    if matches == MAX_SEARCH_MATCHES {
                        bounded = true;
                        break 'files;
                    }
                    let relative = entry
                        .path()
                        .strip_prefix(self.paths.root())
                        .unwrap_or(entry.path());
                    let rendered = format!("{}:{}:{line}", relative.display(), index + 1);
                    let separator_bytes = usize::from(!output.is_empty());
                    let remaining = output_limit.saturating_sub(output.len());
                    if separator_bytes + rendered.len() > remaining {
                        if separator_bytes < remaining && separator_bytes == 1 {
                            output.push('\n');
                        }
                        let remaining = output_limit.saturating_sub(output.len());
                        let (partial, _) = truncate_utf8(&rendered, remaining);
                        output.push_str(&partial);
                        bounded = true;
                        break 'files;
                    }
                    if separator_bytes == 1 {
                        output.push('\n');
                    }
                    output.push_str(&rendered);
                    matches += 1;
                }
            }
        }
        Ok(ToolObservation::success(
            if bounded {
                "search complete; output truncated"
            } else {
                "search complete"
            },
            output,
        ))
    }
}

fn include_discovery_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0
        || !entry.file_type().is_dir()
        || !DISCOVERY_EXCLUDED_DIRECTORIES
            .iter()
            .any(|excluded| entry.file_name().to_str() == Some(excluded))
}

fn ensure_discovery_root(
    tool: &str,
    workspace_root: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), ToolError> {
    let relative = root.strip_prefix(workspace_root).unwrap_or(root);
    let excluded = relative.components().find_map(|component| {
        let component = component.as_os_str().to_str()?;
        DISCOVERY_EXCLUDED_DIRECTORIES
            .contains(&component)
            .then_some(component)
    });
    if let Some(excluded) = excluded {
        return Err(ToolError::Execution {
            tool: tool.into(),
            message: format!(
                "recursive discovery excludes '{excluded}' to prevent harness state, dependencies, or generated output from entering model context; use read_file with a specific file path only when that internal file is deliberately needed"
            ),
        });
    }
    Ok(())
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
