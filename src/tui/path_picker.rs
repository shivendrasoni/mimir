use std::{collections::BTreeMap, path::Path};

use serde::Serialize;
use walkdir::{DirEntry, WalkDir};

use crate::tools::WorkspacePathPolicy;

const EXCLUDED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".mimir",
    ".next",
    ".nuxt",
    ".svn",
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
const MAX_PICKER_PATHS: usize = 10_000;

#[derive(Serialize)]
struct WorkspaceReference<'a> {
    path: &'a str,
    kind: &'static str,
}

pub(super) fn discover_workspace_paths(workspace: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in WalkDir::new(workspace)
        .max_depth(20)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
        .filter_map(Result::ok)
    {
        if entry.path() == workspace || entry.file_type().is_symlink() {
            continue;
        }
        if !entry.file_type().is_dir() && !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let mut display = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if entry.file_type().is_dir() {
            display.push('/');
        }
        paths.push(display);
        if paths.len() == MAX_PICKER_PATHS {
            break;
        }
    }
    paths
}

pub(super) fn workspace_reference_context(prompt: &str, workspace: &Path) -> Option<String> {
    let policy = WorkspacePathPolicy::new(workspace).ok()?;
    let mut references = BTreeMap::new();
    for candidate in extract_mentions(prompt) {
        let Some((path, resolved)) = resolve_candidate(&policy, candidate) else {
            continue;
        };
        let kind = if resolved.is_dir() { "folder" } else { "file" };
        references.insert(path, kind);
    }
    if references.is_empty() {
        return None;
    }
    let payload = references
        .iter()
        .map(|(path, kind)| WorkspaceReference { path, kind })
        .collect::<Vec<_>>();
    let json = serde_json::to_string(&payload).ok()?;
    Some(format!(
        "<workspace_references>{json}</workspace_references>\n\
         The user explicitly selected these workspace paths. Inspect the referenced files or folders with workspace tools as needed; do not assume uninspected contents."
    ))
}

fn include_entry(entry: &DirEntry) -> bool {
    !entry.file_type().is_dir()
        || !EXCLUDED_DIRECTORIES
            .iter()
            .any(|excluded| entry.file_name() == *excluded)
}

fn extract_mentions(prompt: &str) -> Vec<&str> {
    let mut mentions = Vec::new();
    let bytes = prompt.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' || !is_mention_boundary(prompt, index) {
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let content_start = index + 2;
            let Some(relative_end) = prompt[content_start..].find('}') else {
                break;
            };
            let end = content_start + relative_end;
            if end > content_start {
                mentions.push(&prompt[content_start..end]);
            }
            index = end + 1;
            continue;
        }
        let start = index + 1;
        let end = prompt[start..]
            .find(char::is_whitespace)
            .map_or(prompt.len(), |relative| start + relative);
        if end > start {
            mentions.push(&prompt[start..end]);
        }
        index = end;
    }
    mentions
}

fn is_mention_boundary(prompt: &str, index: usize) -> bool {
    index == 0
        || prompt[..index]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_whitespace() || "([{'\"".contains(character))
}

fn resolve_candidate(
    policy: &WorkspacePathPolicy,
    candidate: &str,
) -> Option<(String, std::path::PathBuf)> {
    let candidate = candidate.trim_end_matches('/');
    let (requested, resolved) = policy
        .resolve_existing(candidate)
        .ok()
        .map(|resolved| (candidate, resolved))
        .or_else(|| {
            let without_punctuation =
                candidate.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']']);
            (without_punctuation != candidate)
                .then(|| {
                    policy
                        .resolve_existing(without_punctuation)
                        .ok()
                        .map(|resolved| (without_punctuation, resolved))
                })
                .flatten()
        })?;
    let path = Path::new(requested)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some((path, resolved))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{discover_workspace_paths, workspace_reference_context};

    #[test]
    fn discovery_lists_files_and_folders_but_skips_generated_trees() {
        let workspace = TempDir::new().expect("workspace");
        fs::create_dir_all(workspace.path().join("src/tui")).expect("source tree");
        fs::write(workspace.path().join("src/tui/app.rs"), "fn main() {}").expect("source");
        fs::create_dir_all(workspace.path().join("target/debug")).expect("target tree");
        fs::write(workspace.path().join("target/debug/app"), "binary").expect("target file");

        let paths = discover_workspace_paths(workspace.path());

        assert!(paths.contains(&"src/".into()));
        assert!(paths.contains(&"src/tui/".into()));
        assert!(paths.contains(&"src/tui/app.rs".into()));
        assert!(!paths.iter().any(|path| path.starts_with("target/")));
    }

    #[test]
    fn reference_context_keeps_only_existing_workspace_paths() {
        let workspace = TempDir::new().expect("workspace");
        fs::create_dir(workspace.path().join("docs")).expect("docs");
        fs::write(workspace.path().join("docs/design notes.md"), "design").expect("design");
        fs::write(workspace.path().join("README.md"), "readme").expect("readme");

        let context = workspace_reference_context(
            "Review @{docs/design notes.md}, @README.md, @../secret and me@example.com",
            workspace.path(),
        )
        .expect("references");

        assert!(context.contains(r#""path":"docs/design notes.md","kind":"file""#));
        assert!(context.contains(r#""path":"README.md","kind":"file""#));
        assert!(!context.contains("secret"));
        assert!(!context.contains("example.com"));
    }
}
