#![allow(
    clippy::missing_errors_doc,
    reason = "catalog operations return typed configuration and persistence errors"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    atomic,
    error::{MimirError, Result},
};

use super::manifest::{ExtensionManifest, load_manifest};
use super::{Capability, ExtensionEntrypoint};

const MAX_MIGRATED_EXTENSIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestSource {
    Workspace,
    State,
    MigrationArchive,
}

#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub manifest: ExtensionManifest,
    pub root_dir: PathBuf,
    pub source: ManifestSource,
    pub enabled: bool,
}

#[derive(Debug)]
pub struct ExtensionCatalog {
    workspace_root: PathBuf,
    state_root: PathBuf,
    state_path: PathBuf,
    state_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    cache: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogState {
    schema_version: u16,
    #[serde(default)]
    disabled: BTreeSet<String>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            disabled: BTreeSet::new(),
        }
    }
}

impl ExtensionCatalog {
    pub fn new(workspace_root: &Path, state_root: &Path) -> Result<Self> {
        let state_root = atomic::canonical_state_root(state_root);
        let state_path = state_root.join("extensions/catalog.json");
        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            state_lock: atomic::path_lock(&state_path),
            state_path,
            state_root,
            cache: Vec::new(),
        })
    }

    pub async fn reload(&mut self) -> Result<Vec<CatalogEntry>> {
        let state = self.read_state().await?;
        let entries = discover(&self.workspace_root, &self.state_root, &state.disabled)?;
        self.cache.clone_from(&entries);
        Ok(entries)
    }

    pub async fn snapshot(&mut self) -> Result<Vec<CatalogEntry>> {
        self.reload().await
    }

    pub async fn disable(&mut self, name: &str) -> Result<()> {
        let mut state = self.read_state().await?;
        state.disabled.insert(name.to_owned());
        self.write_state(&state).await?;
        let _ = self.reload().await?;
        Ok(())
    }

    pub async fn enable(&mut self, name: &str) -> Result<()> {
        let mut state = self.read_state().await?;
        state.disabled.remove(name);
        self.write_state(&state).await?;
        let _ = self.reload().await?;
        Ok(())
    }

    async fn read_state(&self) -> Result<CatalogState> {
        let _guard = self.state_lock.lock().await;
        atomic::prepare_state_path(&self.state_root, &self.state_path).await?;
        let state: CatalogState = atomic::read_json(&self.state_path)
            .await?
            .unwrap_or_default();
        if state.schema_version != 1 {
            return Err(MimirError::Configuration(format!(
                "unsupported extension catalog schema version {}",
                state.schema_version
            )));
        }
        Ok(state)
    }

    async fn write_state(&self, state: &CatalogState) -> Result<()> {
        let _guard = self.state_lock.lock().await;
        atomic::prepare_state_path(&self.state_root, &self.state_path).await?;
        atomic::write_json(&self.state_path, state).await
    }
}

fn discover(
    workspace_root: &Path,
    state_root: &Path,
    disabled: &BTreeSet<String>,
) -> Result<Vec<CatalogEntry>> {
    let workspace_dir = workspace_root.join(".mimir/extensions");
    let state_dir = state_root.join("extensions");
    let mut discovered = BTreeMap::new();
    for entry in discover_migrated_extensions(state_root, disabled)? {
        discovered.insert(entry.manifest.name.clone(), entry);
    }
    for (root, source) in [
        (&state_dir, ManifestSource::State),
        (&workspace_dir, ManifestSource::Workspace),
    ] {
        for manifest_path in manifest_paths(root)? {
            let manifest = load_manifest(&manifest_path)?;
            let entry = CatalogEntry {
                enabled: !disabled.contains(&manifest.name),
                root_dir: manifest_path
                    .parent()
                    .ok_or_else(|| MimirError::Configuration("manifest path has no parent".into()))?
                    .to_path_buf(),
                source,
                manifest,
            };
            discovered.insert(entry.manifest.name.clone(), entry);
        }
    }
    Ok(discovered.into_values().collect())
}

fn discover_migrated_extensions(
    state_root: &Path,
    disabled: &BTreeSet<String>,
) -> Result<Vec<CatalogEntry>> {
    let root = state_root.join("migration/compatibility/v1/resources/extensions");
    if !root.exists() {
        return Ok(Vec::new());
    }
    if std::fs::symlink_metadata(&root)?.file_type().is_symlink() {
        return Err(MimirError::Configuration(
            "migrated extension archive cannot be a symlink".into(),
        ));
    }
    let mut candidates = Vec::new();
    let mut children = std::fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(std::fs::DirEntry::file_name);
    for child in children {
        let file_type = child.file_type()?;
        if file_type.is_symlink() {
            return Err(MimirError::Configuration(format!(
                "migrated extension archive cannot contain symlinks: {}",
                child.path().display()
            )));
        }
        if file_type.is_file() && is_extension_module(&child.path()) {
            candidates.push((child.path(), None, None));
        } else if file_type.is_dir() {
            candidates.extend(resolve_migrated_directory(&root, &child.path())?);
        }
        if candidates.len() > MAX_MIGRATED_EXTENSIONS {
            return Err(MimirError::Configuration(format!(
                "migrated extension archive exceeds the entrypoint limit of {MAX_MIGRATED_EXTENSIONS}"
            )));
        }
    }
    let mut names = BTreeSet::new();
    let mut entries = Vec::with_capacity(candidates.len());
    for (entrypoint, package_name, package_version) in candidates {
        ensure_archive_file(&root, &entrypoint)?;
        let base_name = package_name.unwrap_or_else(|| {
            entrypoint
                .parent()
                .filter(|parent| *parent != root)
                .and_then(Path::file_name)
                .or_else(|| entrypoint.file_stem())
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("extension")
                .to_owned()
        });
        let mut name = format!("migrated-{}", normalize_extension_name(&base_name));
        if !names.insert(name.clone()) {
            let stem = entrypoint
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("entry");
            name = format!("{name}-{}", normalize_extension_name(stem));
            let mut suffix = 2usize;
            while !names.insert(name.clone()) {
                name = format!("migrated-{}-{suffix}", normalize_extension_name(&base_name));
                suffix += 1;
            }
        }
        if name.len() > 64 {
            name.truncate(64);
            while !names.insert(name.clone()) {
                name.pop();
            }
        }
        let manifest = ExtensionManifest {
            schema_version: 1,
            name: name.clone(),
            version: package_version.unwrap_or_else(|| "0.0.0-migrated".into()),
            entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
                module: entrypoint.display().to_string(),
            },
            capabilities: BTreeSet::from([
                Capability::Tools,
                Capability::Commands,
                Capability::Ui,
                Capability::Lifecycle,
            ]),
        };
        manifest.validate()?;
        entries.push(CatalogEntry {
            enabled: !disabled.contains(&name),
            root_dir: entrypoint
                .parent()
                .ok_or_else(|| {
                    MimirError::Configuration("migrated extension entrypoint has no parent".into())
                })?
                .to_path_buf(),
            source: ManifestSource::MigrationArchive,
            manifest,
        });
    }
    Ok(entries)
}

type MigratedEntrypoint = (PathBuf, Option<String>, Option<String>);

fn resolve_migrated_directory(root: &Path, directory: &Path) -> Result<Vec<MigratedEntrypoint>> {
    let package_path = directory.join("package.json");
    if package_path.is_file() {
        let metadata = std::fs::symlink_metadata(&package_path)?;
        if metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
            return Err(MimirError::Configuration(format!(
                "migrated extension package manifest is unsafe: {}",
                package_path.display()
            )));
        }
        let package: serde_json::Value = serde_json::from_slice(&std::fs::read(&package_path)?)?;
        let package_name = package.get("name").and_then(serde_json::Value::as_str);
        let package_version = package.get("version").and_then(serde_json::Value::as_str);
        if let Some(paths) = package
            .get("pi")
            .and_then(|value| value.get("extensions"))
            .and_then(serde_json::Value::as_array)
        {
            let mut entries = Vec::new();
            for value in paths {
                let relative = value.as_str().ok_or_else(|| {
                    MimirError::Configuration(
                        "migrated package pi.extensions entries must be strings".into(),
                    )
                })?;
                let relative_path = Path::new(relative);
                if relative_path.is_absolute()
                    || relative_path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    return Err(MimirError::Configuration(format!(
                        "migrated package extension path is unsafe: {relative}"
                    )));
                }
                let path = directory.join(relative_path);
                ensure_archive_file(root, &path)?;
                if !is_extension_module(&path) {
                    return Err(MimirError::Configuration(format!(
                        "migrated package extension has an unsupported type: {}",
                        path.display()
                    )));
                }
                entries.push((
                    path,
                    package_name.map(str::to_owned),
                    package_version.map(str::to_owned),
                ));
            }
            if !entries.is_empty() {
                return Ok(entries);
            }
        }
    }
    for index in ["index.ts", "index.js", "index.mjs"] {
        let path = directory.join(index);
        if path.is_file() {
            ensure_archive_file(root, &path)?;
            return Ok(vec![(path, None, None)]);
        }
    }
    Ok(Vec::new())
}

fn ensure_archive_file(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        MimirError::Configuration(format!(
            "migrated extension entrypoint '{}' is unavailable: {error}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(MimirError::Configuration(
            "migrated extension entrypoint escaped its archive".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(MimirError::Configuration(
            "migrated extension entrypoint must be a regular file".into(),
        ));
    }
    Ok(())
}

fn is_extension_module(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "ts" | "js" | "mjs"))
}

fn normalize_extension_name(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-');
    let normalized = if normalized.is_empty() {
        "extension"
    } else {
        normalized
    };
    normalized.chars().take(48).collect()
}

fn manifest_paths(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    collect_manifests(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_manifests(root: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| matches!(name, ".staging" | ".trash"))
            {
                continue;
            }
            collect_manifests(&path, paths)?;
        } else if entry.file_name() == "manifest.json" {
            paths.push(path);
        }
    }
    Ok(())
}
