#![allow(
    clippy::missing_errors_doc,
    reason = "manifest validation returns typed configuration and parse errors"
)]

use std::{collections::BTreeSet, path::Path};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{MimirError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Tools,
    Commands,
    Ui,
    Provider,
    WorkspaceRead,
    WorkspaceWrite,
    /// Authorizes process operations requested through a capability-aware host.
    Process,
    /// Explicitly authorizes an unsandboxed native extension process. Portable
    /// Rust cannot confine native code to the workspace capability set, so this
    /// grant means the extension is trusted with the caller's OS permissions.
    UnrestrictedNative,
    Lifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ExtensionEntrypoint {
    NativeProcess {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    EmbeddedJavaScript {
        module: String,
    },
}

impl ExtensionEntrypoint {
    pub fn native_process(&self) -> Option<(&str, &[String])> {
        match self {
            Self::NativeProcess { program, args } => Some((program, args)),
            Self::EmbeddedJavaScript { .. } => None,
        }
    }

    pub fn embedded_module(&self) -> Option<&str> {
        match self {
            Self::NativeProcess { .. } => None,
            Self::EmbeddedJavaScript { module } => Some(module),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub schema_version: u16,
    pub name: String,
    pub version: String,
    pub entrypoint: ExtensionEntrypoint,
    pub capabilities: BTreeSet<Capability>,
}

impl ExtensionManifest {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(MimirError::Configuration(format!(
                "unsupported extension manifest schema version {}",
                self.schema_version
            )));
        }
        let name_pattern = Regex::new(r"^[a-z][a-z0-9_-]{0,63}$").map_err(|error| {
            MimirError::Configuration(format!("invalid internal name pattern: {error}"))
        })?;
        if !name_pattern.is_match(&self.name) {
            return Err(MimirError::Configuration(format!(
                "extension name '{}' is invalid",
                self.name
            )));
        }
        if self.version.trim().is_empty() {
            return Err(MimirError::Configuration(
                "extension version must not be blank".into(),
            ));
        }
        match &self.entrypoint {
            ExtensionEntrypoint::NativeProcess { program, .. } => {
                validate_absolute_entrypoint(program, "program")?;
            }
            ExtensionEntrypoint::EmbeddedJavaScript { module } => {
                validate_absolute_entrypoint(module, "module")?;
                let extension = Path::new(module)
                    .extension()
                    .and_then(std::ffi::OsStr::to_str);
                if !extension.is_some_and(|value| matches!(value, "js" | "mjs" | "ts")) {
                    return Err(MimirError::Configuration(
                        "embedded extension module must use .js, .mjs, or .ts".into(),
                    ));
                }
                if self.capabilities.contains(&Capability::UnrestrictedNative) {
                    return Err(MimirError::Configuration(
                        "embedded extensions cannot request unrestricted_native capabilities"
                            .into(),
                    ));
                }
            }
        }
        if self.capabilities.is_empty() {
            return Err(MimirError::Configuration(
                "extension capabilities must not be empty".into(),
            ));
        }
        Ok(())
    }
}

fn validate_absolute_entrypoint(value: &str, kind: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MimirError::Configuration(format!(
            "extension entrypoint {kind} must not be blank"
        )));
    }
    if !Path::new(value).is_absolute() {
        return Err(MimirError::Configuration(format!(
            "extension entrypoint {kind} must be an absolute path"
        )));
    }
    Ok(())
}

pub fn load_manifest(path: &Path) -> Result<ExtensionManifest> {
    let manifest: ExtensionManifest = serde_json::from_slice(&std::fs::read(path)?)?;
    manifest.validate()?;
    Ok(manifest)
}
