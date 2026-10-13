//! Reviewed registry artifacts consumed from the signed profile, never arbitrary repository files.
use crate::error::{MimirError, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    pub repository: String,
    pub commit_sha: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryArtifact {
    pub id: String,
    pub version: String,
    pub kind: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    pub path: String,
    pub sha256: String,
    pub enforcement: String,
    pub content: String,
    pub source: RegistrySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<RegistryFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RegistryOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<RegistryDependency>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_clients: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryFile {
    pub path: String,
    pub sha256: String,
    pub encoding: String,
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryOrigin {
    pub catalog_id: String,
    pub version: String,
    pub parameters: Option<std::collections::BTreeMap<String, String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryDependency {
    pub id: String,
    pub version: String,
}
fn safe_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value.len() <= 1024
        && value.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        })
}
fn validate_bundle(item: &RegistryArtifact, prefix: &str) -> Result<usize> {
    let Some(files) = &item.files else {
        return Ok(0);
    };
    if files.is_empty() || files.len() > 512 {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    let mut total = 0;
    for file in files {
        if !safe_path(&file.path) || !seen.insert(file.path.clone()) {
            return Err(invalid());
        }
        let bytes = match file.encoding.as_str() {
            "utf8" => file.content.as_bytes().to_vec(),
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(&file.content)
                .map_err(|_| invalid())?,
            _ => return Err(invalid()),
        };
        total += bytes.len();
        if bytes.len() > 96 * 1024
            || total > 16 * 1024 * 1024
            || file.sha256 != format!("{:x}", Sha256::digest(&bytes))
        {
            return Err(invalid());
        }
    }
    if files.iter().any(|file| {
        seen.iter()
            .any(|other| other.starts_with(&format!("{}/", file.path)))
    }) {
        return Err(invalid());
    }
    let entry = files
        .iter()
        .find(|file| format!("{prefix}{}", file.path) == item.path)
        .ok_or_else(invalid)?;
    if entry.encoding != "utf8" || entry.content != item.content || entry.sha256 != item.sha256 {
        return Err(invalid());
    }
    Ok(total)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRole {
    pub schema: u8,
    pub instructions: String,
    pub role: String,
    pub allowed_tools: Vec<String>,
}
fn invalid() -> MimirError {
    MimirError::Configuration("Invalid signed registry artifact".into())
}
/// Validates bounded reviewed artifact identity, source and content before any execution.
///
/// # Errors
/// Returns an error for unrecognized, duplicate or inconsistent artifacts.
#[allow(clippy::too_many_lines)]
pub fn validate(artifacts: &[RegistryArtifact]) -> Result<()> {
    if artifacts.len() > 128 {
        return Err(invalid());
    }
    let ids = regex::Regex::new(r"^[a-z][a-z0-9_.-]{0,127}$").map_err(|_| invalid())?;
    let repositories =
        regex::Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").map_err(|_| invalid())?;
    let revisions = regex::Regex::new(r"^[a-f0-9]{40}$").map_err(|_| invalid())?;
    let file_pattern =
        regex::Regex::new(r"^[A-Za-z0-9_-]+\.(md|json|mjs)$").map_err(|_| invalid())?;
    let mut seen = BTreeSet::new();
    let mut total = 0;
    for item in artifacts {
        total += item.content.len();
        let directory = match item.kind.as_str() {
            "skill" => "skills",
            "agent" => "agents",
            "advisory_rule" => "rules",
            "policy" => "policies",
            "workflow" => "workflows",
            "reference" => "references",
            _ => return Err(invalid()),
        };
        let prefix = format!("registry/{directory}/{}/{}/", item.id, item.version);
        total += validate_bundle(item, &prefix)?;
        if item.supported_clients.as_ref().is_some_and(|clients| {
            !clients.iter().any(|client| client == "mimir") || clients.len() > 4
        }) {
            return Err(invalid());
        }
        if let Some(origin) = &item.origin
            && (!ids.is_match(&origin.catalog_id)
                || crate::enterprise::compare_versions(&origin.version, "0.0.0").is_err()
                || origin.parameters.as_ref().is_some_and(|values| {
                    values.len() > 128
                        || values
                            .iter()
                            .any(|(key, value)| key.len() > 128 || value.len() > 1024)
                }))
        {
            return Err(invalid());
        }
        if let Some(dependencies) = &item.dependencies
            && (dependencies.len() > 128
                || dependencies.iter().any(|dependency| {
                    !artifacts.iter().any(|candidate| {
                        candidate.id == dependency.id && candidate.version == dependency.version
                    })
                }))
        {
            return Err(invalid());
        }
        if !ids.is_match(&item.id)
            || !seen.insert(&item.id)
            || !item.path.starts_with(&prefix)
            || !safe_path(&item.path)
            || (item.files.is_none()
                && (item.path[prefix.len()..].contains('/')
                    || !file_pattern.is_match(&item.path[prefix.len()..])))
            || !repositories.is_match(&item.source.repository)
            || !revisions.is_match(&item.source.commit_sha)
            || item.content.is_empty()
            || item.content.len() > 96 * 1024
            || total > 16 * 1024 * 1024
            || item.sha256 != format!("{:x}", Sha256::digest(item.content.as_bytes()))
            || !matches!(item.enforcement.as_str(), "default" | "mandatory")
            || item.name.is_empty()
            || item.name.len() > 64
            || item.description.is_empty()
            || item.description.len() > 1024
        {
            return Err(invalid());
        }
        crate::enterprise::compare_versions(&item.version, "0.0.0")?;
        if let Some(template) = &item.template_id {
            let templates: Value =
                serde_json::from_str(include_str!("shared-working-environment.json"))?;
            if item.kind != "skill"
                || !templates["templates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|entry| entry["id"] == *template)
            {
                return Err(invalid());
            }
        }
        match item.kind.as_str() {
            "agent" => {
                let agent: AgentRole = serde_json::from_str(&item.content)?;
                if agent.schema != 1
                    || agent.instructions.is_empty()
                    || agent.instructions.len() > 64 * 1024
                    || agent.role.is_empty()
                    || agent.role.len() > 128
                    || agent.allowed_tools.len() > 128
                    || agent.allowed_tools.iter().any(|tool| !ids.is_match(tool))
                {
                    return Err(invalid());
                }
            }
            _ => {}
        }
    }
    Ok(())
}
/// Reads only artifacts from the currently verified enrollment.
///
/// # Errors
/// Returns an error for unavailable or expired signed policy.
pub fn active() -> Result<Vec<RegistryArtifact>> {
    crate::enterprise::registry_artifacts()
}
/// Adds bounded reviewed role metadata to progressive startup context.
pub fn agent_catalog(artifacts: &[RegistryArtifact]) -> Value {
    json!(artifacts.iter().filter(|item|item.kind=="agent").map(|item|json!({"id":item.id,"version":item.version,"name":item.name,"description":item.description,"enforcement":item.enforcement})).collect::<Vec<_>>())
}

/// Reports per-policy activation gaps without blocking unsupported agents.
pub fn readiness_issues(workspace: &Path) -> Result<Vec<Value>> {
    crate::policy::readiness_issues(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn artifact(content: String, kind: &str) -> RegistryArtifact {
        let folder = match kind {
            "agent" => "agents",
            _ => "skills",
        };
        RegistryArtifact {
            id: "org.test".into(),
            version: "1.0.0".into(),
            kind: kind.into(),
            name: "Test".into(),
            description: "Reviewed test".into(),
            template_id: None,
            path: format!("registry/{folder}/org.test/1.0.0/entry.json"),
            sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            content,
            enforcement: "mandatory".into(),
            files: None,
            origin: None,
            dependencies: None,
            supported_clients: None,
            source: RegistrySource {
                repository: "test/rules".into(),
                commit_sha: "a".repeat(40),
            },
        }
    }
    #[test]
    fn four_client_registry_metadata_remains_compatible_with_mimir() {
        let mut item = artifact("Reviewed guidance".into(), "skill");
        item.supported_clients = Some(vec![
            "mimir".into(),
            "claude-code".into(),
            "codex".into(),
            "cursor".into(),
        ]);
        validate(std::slice::from_ref(&item)).unwrap();
        item.supported_clients
            .as_mut()
            .unwrap()
            .push("unknown".into());
        assert!(validate(&[item]).is_err());
    }
    #[test]
    fn rejects_tampering_in_guidance_artifacts() {
        let mut item = artifact("# Reviewed guidance".into(), "skill");
        validate(std::slice::from_ref(&item)).unwrap();
        item.content.push(' ');
        assert!(validate(&[item]).is_err());
    }
    #[test]
    fn rejects_retired_customer_hook_executors() {
        let item = artifact(json!({"schema":1,"runtime":"node","levels":["completion"],"timeout_ms":1000,"script":"process.stdout.write('{}')"}).to_string(), "hook_script");
        assert!(validate(&[item]).is_err());
    }
    #[test]
    fn rejects_noncanonical_paths_duplicate_resources_and_undeclared_dependencies() {
        let mut item = artifact("Reviewed guide".into(), "skill");
        item.path = "registry/skills/org.test/1.0.0/../entry.json".into();
        assert!(validate(&[item]).is_err());
        let item = artifact("Reviewed guide".into(), "skill");
        assert!(validate(&[item.clone(), item]).is_err());
        let mut item = artifact("Reviewed guide".into(), "skill");
        item.dependencies = Some(vec![RegistryDependency {
            id: "unapproved.dependency".into(),
            version: "1.0.0".into(),
        }]);
        assert!(validate(&[item]).is_err());
    }
}
