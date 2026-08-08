#![allow(
    clippy::missing_errors_doc,
    reason = "package operations return typed validation, persistence, and filesystem errors"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    atomic,
    error::{MimirError, Result},
};

use super::{Capability, ExtensionEntrypoint, ExtensionManifest};

const PACKAGE_STATE_VERSION: u16 = 1;
const MAX_PACKAGE_FILES: usize = 256;
const MAX_PACKAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PACKAGE_ENTRIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledExtensionPackage {
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    pub installed_path: PathBuf,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedExtensionPackage {
    pub name: String,
    pub recovery_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageState {
    schema_version: u16,
    #[serde(default)]
    packages: BTreeMap<String, InstalledExtensionPackage>,
}

impl Default for PackageState {
    fn default() -> Self {
        Self {
            schema_version: PACKAGE_STATE_VERSION,
            packages: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    name: String,
    version: String,
    pi: PiManifest,
}

#[derive(Debug, Deserialize)]
struct PiManifest {
    extensions: Vec<String>,
}

#[derive(Debug)]
struct PreparedPackage {
    record: InstalledExtensionPackage,
    staging_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExtensionPackageManager {
    state_root: PathBuf,
    state_path: PathBuf,
    packages_root: PathBuf,
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl ExtensionPackageManager {
    pub fn new(state_root: &Path) -> Result<Self> {
        let state_root = atomic::canonical_state_root(state_root);
        let state_path = state_root.join("extensions/packages.json");
        Ok(Self {
            lock: atomic::path_lock(&state_path),
            packages_root: state_root.join("extensions/packages"),
            state_path,
            state_root,
        })
    }

    pub async fn list(&self) -> Result<Vec<InstalledExtensionPackage>> {
        let _guard = self.lock.lock().await;
        Ok(self.read_state().await?.packages.into_values().collect())
    }

    pub async fn install(&self, source: &str) -> Result<InstalledExtensionPackage> {
        let path = Path::new(source);
        if !path.exists() {
            return Err(MimirError::Configuration(
                "extension package installation currently supports local filesystem paths only"
                    .into(),
            ));
        }
        self.install_local(path).await
    }

    pub async fn install_local(&self, source: &Path) -> Result<InstalledExtensionPackage> {
        self.install_local_mode(source, false).await
    }

    pub async fn update(&self, name: &str) -> Result<InstalledExtensionPackage> {
        let source = {
            let _guard = self.lock.lock().await;
            self.read_state()
                .await?
                .packages
                .get(name)
                .ok_or_else(|| {
                    MimirError::Configuration(format!(
                        "extension package '{name}' is not installed"
                    ))
                })?
                .source
                .clone()
        };
        self.install_local_mode(&source, true).await
    }

    pub async fn remove(&self, name: &str) -> Result<Option<RemovedExtensionPackage>> {
        validate_package_name(name)?;
        let _guard = self.lock.lock().await;
        let mut state = self.read_state().await?;
        let Some(record) = state.packages.remove(name) else {
            return Ok(None);
        };
        let recovery_path = self.recovery_path(name);
        atomic::prepare_state_path(
            &self.state_root,
            &self.state_root.join("extensions/.trash/placeholder"),
        )
        .await?;
        if !record.installed_path.starts_with(&self.packages_root) {
            return Err(MimirError::Configuration(
                "installed package path escaped the managed package root".into(),
            ));
        }
        tokio::fs::rename(&record.installed_path, &recovery_path).await?;
        if let Err(error) = self.write_state(&state).await {
            let _ = tokio::fs::rename(&recovery_path, &record.installed_path).await;
            return Err(error);
        }
        Ok(Some(RemovedExtensionPackage {
            name: name.to_owned(),
            recovery_path,
        }))
    }

    async fn install_local_mode(
        &self,
        source: &Path,
        replacing: bool,
    ) -> Result<InstalledExtensionPackage> {
        let prepared = self.prepare_package(source).await?;
        let _guard = self.lock.lock().await;
        let mut state = self.read_state().await?;
        let existing = state.packages.get(&prepared.record.name).cloned();
        if existing.is_some() && !replacing {
            let _ = tokio::fs::remove_dir_all(&prepared.staging_path).await;
            return Err(MimirError::Configuration(format!(
                "extension package '{}' is already installed",
                prepared.record.name
            )));
        }
        if replacing && existing.is_none() {
            let _ = tokio::fs::remove_dir_all(&prepared.staging_path).await;
            return Err(MimirError::Configuration(format!(
                "extension package '{}' is not installed",
                prepared.record.name
            )));
        }
        atomic::prepare_state_path(&self.state_root, &self.packages_root.join("placeholder"))
            .await?;
        atomic::prepare_state_path(
            &self.state_root,
            &self.state_root.join("extensions/.trash/placeholder"),
        )
        .await?;
        let recovery = if let Some(existing) = &existing {
            if existing.source != prepared.record.source {
                let _ = tokio::fs::remove_dir_all(&prepared.staging_path).await;
                return Err(MimirError::Configuration(format!(
                    "updated package '{}' does not match its installed source",
                    prepared.record.name
                )));
            }
            let recovery = self.recovery_path(&prepared.record.name);
            tokio::fs::rename(&existing.installed_path, &recovery).await?;
            Some(recovery)
        } else {
            None
        };
        if let Err(error) =
            tokio::fs::rename(&prepared.staging_path, &prepared.record.installed_path).await
        {
            if let (Some(recovery), Some(existing)) = (&recovery, &existing) {
                let _ = tokio::fs::rename(recovery, &existing.installed_path).await;
            }
            return Err(error.into());
        }
        state
            .packages
            .insert(prepared.record.name.clone(), prepared.record.clone());
        if let Err(error) = self.write_state(&state).await {
            let _ = tokio::fs::rename(
                &prepared.record.installed_path,
                self.recovery_path(&prepared.record.name),
            )
            .await;
            if let (Some(recovery), Some(existing)) = (&recovery, &existing) {
                let _ = tokio::fs::rename(recovery, &existing.installed_path).await;
            }
            return Err(error);
        }
        Ok(prepared.record)
    }

    async fn prepare_package(&self, source: &Path) -> Result<PreparedPackage> {
        let source_metadata = std::fs::symlink_metadata(source).map_err(|error| {
            MimirError::Configuration(format!(
                "extension package source '{}' is unavailable: {error}",
                source.display()
            ))
        })?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            return Err(MimirError::Configuration(
                "extension package source must be a regular local directory, not a symlink".into(),
            ));
        }
        let source = std::fs::canonicalize(source)?;
        let package_json_path = source.join("package.json");
        let package_json = checked_source_file(&source, &package_json_path)?;
        if package_json.len() > 1024 * 1024 {
            return Err(MimirError::Configuration(
                "extension package manifest exceeds 1 MiB".into(),
            ));
        }
        let package: PackageJson = serde_json::from_slice(&package_json)?;
        validate_package_name(&package.name)?;
        if package.version.trim().is_empty() || package.version.len() > 128 {
            return Err(MimirError::Configuration(
                "extension package version is invalid".into(),
            ));
        }
        if package.pi.extensions.is_empty() || package.pi.extensions.len() > MAX_PACKAGE_ENTRIES {
            return Err(MimirError::Configuration(format!(
                "extension package must declare between 1 and {MAX_PACKAGE_ENTRIES} pi.extensions entries"
            )));
        }
        let destination = self.packages_root.join(&package.name);
        let staging_path = self
            .state_root
            .join("extensions/.staging")
            .join(Uuid::new_v4().to_string());
        atomic::prepare_state_path(&self.state_root, &staging_path.join("placeholder")).await?;
        copy_package_tree(&source, &staging_path)?;
        let extension_names =
            write_package_manifests(&package, &source, &destination, &staging_path)?;
        Ok(PreparedPackage {
            record: InstalledExtensionPackage {
                name: package.name,
                version: package.version,
                source,
                installed_path: destination,
                extensions: extension_names,
            },
            staging_path,
        })
    }

    async fn read_state(&self) -> Result<PackageState> {
        atomic::prepare_state_path(&self.state_root, &self.state_path).await?;
        let state: PackageState = atomic::read_json(&self.state_path)
            .await?
            .unwrap_or_default();
        if state.schema_version != PACKAGE_STATE_VERSION {
            return Err(MimirError::Configuration(format!(
                "unsupported extension package state version {}",
                state.schema_version
            )));
        }
        Ok(state)
    }

    async fn write_state(&self, state: &PackageState) -> Result<()> {
        atomic::prepare_state_path(&self.state_root, &self.state_path).await?;
        atomic::write_json(&self.state_path, state).await
    }

    fn recovery_path(&self, name: &str) -> PathBuf {
        self.state_root
            .join("extensions/.trash")
            .join(format!("{name}-{}", Uuid::new_v4()))
    }
}

fn write_package_manifests(
    package: &PackageJson,
    source: &Path,
    destination: &Path,
    staging_path: &Path,
) -> Result<Vec<String>> {
    let mut extension_names = Vec::new();
    let mut seen_names = BTreeSet::new();
    for (index, relative) in package.pi.extensions.iter().enumerate() {
        let relative = safe_relative_path(relative)?;
        let source_entry = source.join(&relative);
        let _ = checked_source_file(source, &source_entry)?;
        if !is_extension_module(&source_entry) {
            return Err(MimirError::Configuration(format!(
                "package extension entry '{}' must use .js, .mjs, or .ts",
                relative.display()
            )));
        }
        let name = package_extension_name(package, &relative);
        validate_package_name(&name)?;
        if !seen_names.insert(name.clone()) {
            return Err(MimirError::Configuration(format!(
                "package extensions resolve to duplicate name '{name}'"
            )));
        }
        let manifest = ExtensionManifest {
            schema_version: 1,
            name: name.clone(),
            version: package.version.clone(),
            entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
                module: destination.join(&relative).display().to_string(),
            },
            capabilities: BTreeSet::from([
                Capability::Tools,
                Capability::Commands,
                Capability::Ui,
                Capability::Lifecycle,
            ]),
        };
        manifest.validate()?;
        let manifest_path = staging_path
            .join(".mimir-manifests")
            .join(format!("{index:02}-{name}"))
            .join("manifest.json");
        std::fs::create_dir_all(manifest_path.parent().ok_or_else(|| {
            MimirError::Configuration("generated manifest has no parent".into())
        })?)?;
        std::fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        extension_names.push(name);
    }
    Ok(extension_names)
}

fn package_extension_name(package: &PackageJson, relative: &Path) -> String {
    if package.pi.extensions.len() == 1 {
        return package.name.clone();
    }
    let stem = relative
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("entry");
    format!("{}-{}", package.name, normalize_name_fragment(stem))
}

fn copy_package_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut files = 0usize;
    let mut bytes = 0usize;
    for item in WalkDir::new(source)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
        })
    {
        let item = item.map_err(|error| {
            MimirError::Configuration(format!("failed to inspect package source: {error}"))
        })?;
        if item.file_type().is_symlink() {
            return Err(MimirError::Configuration(format!(
                "extension packages cannot contain symlinks: {}",
                item.path().display()
            )));
        }
        let relative = item.path().strip_prefix(source).map_err(|_| {
            MimirError::Configuration("package file escaped its source root".into())
        })?;
        let target = destination.join(relative);
        if item.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if !item.file_type().is_file() {
            return Err(MimirError::Configuration(format!(
                "extension package contains a non-regular file: {}",
                item.path().display()
            )));
        }
        if item.file_name() == "manifest.json" {
            return Err(MimirError::Configuration(
                "extension package source cannot contain manifest.json because that name is reserved by the native catalog"
                    .into(),
            ));
        }
        files += 1;
        let item_metadata = item.metadata().map_err(|error| {
            MimirError::Configuration(format!(
                "failed to inspect package file '{}': {error}",
                item.path().display()
            ))
        })?;
        bytes = bytes
            .checked_add(usize::try_from(item_metadata.len()).unwrap_or(usize::MAX))
            .ok_or_else(|| MimirError::Configuration("extension package size overflow".into()))?;
        if files > MAX_PACKAGE_FILES || bytes > MAX_PACKAGE_BYTES {
            return Err(MimirError::Configuration(format!(
                "extension package exceeds {MAX_PACKAGE_FILES} files or {MAX_PACKAGE_BYTES} bytes"
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(item.path(), target)?;
    }
    Ok(())
}

fn checked_source_file(root: &Path, path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        MimirError::Configuration(format!(
            "package file '{}' is unavailable: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(MimirError::Configuration(format!(
            "package file '{}' must be a regular file and not a symlink",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(root) {
        return Err(MimirError::Configuration(
            "package file escaped its source root".into(),
        ));
    }
    Ok(std::fs::read(canonical)?)
}

fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(MimirError::Configuration(format!(
            "package extension path is unsafe: {value}"
        )));
    }
    Ok(path.to_path_buf())
}

fn validate_package_name(name: &str) -> Result<()> {
    let pattern = Regex::new(r"^[a-z][a-z0-9_-]{0,63}$").map_err(|error| {
        MimirError::Configuration(format!("invalid internal package name pattern: {error}"))
    })?;
    if !pattern.is_match(name) {
        return Err(MimirError::Configuration(format!(
            "extension package name '{name}' is invalid"
        )));
    }
    Ok(())
}

fn normalize_name_fragment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(24)
        .collect()
}

fn is_extension_module(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "js" | "mjs" | "ts"))
}
