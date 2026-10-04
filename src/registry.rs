//! Reviewed registry artifacts consumed from the signed profile, never arbitrary repository files.
use crate::error::{MimirError, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookScript {
    schema: u8,
    runtime: String,
    levels: Vec<String>,
    timeout_ms: u64,
    script: String,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookOutcome {
    pub context: Option<String>,
    pub deny: Option<bool>,
    pub reason: Option<String>,
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
            "rule" | "advisory_rule" => "rules",
            "workflow" => "workflows",
            "reference" => "references",
            "hook_script" => "hooks",
            _ => return Err(invalid()),
        };
        let prefix = format!("registry/{directory}/{}/{}/", item.id, item.version);
        total += validate_bundle(item, &prefix)?;
        if item.supported_clients.as_ref().is_some_and(|clients| {
            !clients.iter().any(|client| client == "mimir") || clients.len() > 3
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
            "hook_script" => {
                let hook: HookScript = serde_json::from_str(&item.content)?;
                if hook.schema != 1
                    || hook.runtime != "node"
                    || hook.levels.is_empty()
                    || hook.levels.len() > 7
                    || hook.levels.iter().any(|level| {
                        !matches!(level.as_str(), "session" | "before_action" | "after_action")
                    })
                    || !(100..=5_000).contains(&hook.timeout_ms)
                    || hook.script.is_empty()
                    || hook.script.len() > 64 * 1024
                {
                    return Err(invalid());
                }
            }
            "rule" => {
                let rule: crate::enterprise::HarnessBinding = serde_json::from_str(&item.content)?;
                if rule.key != item.id {
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
/// Executes selected signed hook scripts at actual covered native boundaries.
///
/// # Errors
/// Returns an error if a required hook fails or denies an action. Output cannot grant permissions.
pub async fn run_hooks(workspace: &Path, level: &str, event: &str, failed: bool) -> Result<String> {
    let artifacts = active()?;
    if artifacts.is_empty() {
        return Ok(String::new());
    }
    let status_path = hook_status_path(workspace)?;
    let mut results: Vec<Value> = std::fs::read(&status_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value["results"].as_array().cloned())
        .unwrap_or_default();
    results.retain(|result| {
        artifacts
            .iter()
            .any(|artifact| result["id"] == artifact.id && result["sha256"] == artifact.sha256)
    });
    if level == "before_action"
        && results.iter().any(|result| {
            result["level"] == "session"
                && result["state"] == "failed"
                && artifacts.iter().any(|artifact| {
                    result["id"] == artifact.id && artifact.enforcement == "mandatory"
                })
        })
    {
        return Err(MimirError::Configuration(
            "Required registry session hook failed; start a new session after repair".into(),
        ));
    }
    let mut context = String::new();
    for artifact in artifacts {
        if artifact.kind != "hook_script" {
            continue;
        }
        let hook: HookScript = serde_json::from_str(&artifact.content)?;
        if !hook.levels.iter().any(|value| value == level) {
            continue;
        }
        let result = execute_hook(workspace, &hook, level, event, failed).await;
        results.retain(|old| !(old["id"] == artifact.id && old["level"] == level));
        results.push(json!({"id":artifact.id,"sha256":artifact.sha256,"level":level,"state":if result.is_ok(){"verified"}else{"failed"}}));
        crate::atomic::write_json(&status_path, &json!({"results":results})).await?;
        match result {
            Ok(outcome) => {
                if level == "before_action" && outcome.deny == Some(true) {
                    return Err(MimirError::Configuration(outcome.reason.unwrap_or_else(
                        || "Approved registry hook blocked the action".into(),
                    )));
                }
                context.extend(
                    outcome
                        .context
                        .unwrap_or_default()
                        .chars()
                        .take(4_000 - context.chars().count()),
                );
            }
            Err(_)
                if artifact.enforcement == "mandatory"
                    && matches!(level, "session" | "before_action") =>
            {
                return Err(MimirError::Configuration(
                    "Required approved registry hook failed; repair the signed environment".into(),
                ));
            }
            Err(_) => context.extend(
                format!(
                    "Registry hook unavailable: {}. Inspect environment readiness.\n",
                    artifact.id
                )
                .chars()
                .take(4_000 - context.chars().count()),
            ),
        }
    }
    Ok(context)
}
async fn execute_hook(
    workspace: &Path,
    hook: &HookScript,
    level: &str,
    event: &str,
    failed: bool,
) -> Result<HookOutcome> {
    let mut process = tokio::process::Command::new("node");
    process
        .args(["--input-type=module", "-e", &hook.script])
        .current_dir(workspace)
        .env_clear()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in ["PATH", "TMPDIR", "TEMP", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
    let mut child = process.spawn()?;
    let result = tokio::time::timeout(Duration::from_millis(hook.timeout_ms), async {
        let mut input = child.stdin.take().ok_or_else(invalid)?;
        input
            .write_all(
                serde_json::to_string(
                    &json!({"schema":1,"level":level,"event":event,"failed":failed}),
                )?
                .as_bytes(),
            )
            .await?;
        input.shutdown().await?;
        drop(input);
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(invalid)?
            .take(8_193)
            .read_to_end(&mut output)
            .await?;
        if output.len() > 8_192 {
            return Err(invalid());
        }
        if !child.wait().await?.success() {
            return Err(invalid());
        }
        let outcome: HookOutcome = serde_json::from_slice(&output)?;
        if outcome
            .context
            .as_ref()
            .is_some_and(|value| value.len() > 2_000)
            || outcome
                .reason
                .as_ref()
                .is_some_and(|value| value.len() > 512)
        {
            return Err(invalid());
        }
        Ok(outcome)
    })
    .await;
    let _ = child.kill().await;
    result.map_err(|_| MimirError::Configuration("Registry hook timed out".into()))?
}
/// Adds bounded reviewed role metadata to progressive startup context.
pub fn agent_catalog(artifacts: &[RegistryArtifact]) -> Value {
    json!(artifacts.iter().filter(|item|item.kind=="agent").map(|item|json!({"id":item.id,"version":item.version,"name":item.name,"description":item.description,"enforcement":item.enforcement})).collect::<Vec<_>>())
}

fn hook_status_path(workspace: &Path) -> Result<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(workspace)?;
    let key = format!(
        "{:x}",
        Sha256::digest(canonical.to_string_lossy().as_bytes())
    );
    Ok(crate::enterprise::enterprise_state_dir()?.join(format!("registry-hooks-{key}.json")))
}

/// Reports only actual executions of the currently selected hook versions.
///
/// # Errors
/// Returns an error for invalid active signed artifacts.
pub fn readiness_issues(workspace: &Path) -> Result<Vec<Value>> {
    let path = hook_status_path(workspace)?;
    let observed = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_default();
    let mut issues = Vec::new();
    for artifact in active()?
        .into_iter()
        .filter(|item| item.kind == "hook_script")
    {
        let hook: HookScript = serde_json::from_str(&artifact.content)?;
        if !hook.levels.iter().all(|level| {
            observed["results"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| {
                    item["id"] == artifact.id
                        && item["sha256"] == artifact.sha256
                        && item["level"] == *level
                        && item["state"] == "verified"
                })
        }) {
            issues.push(json!({"key":artifact.id,"required":artifact.enforcement=="mandatory","reason_code":"registry_hook_unverified"}));
        }
    }
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn artifact(content: String, kind: &str) -> RegistryArtifact {
        let folder = match kind {
            "hook_script" => "hooks",
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
    fn rejects_tampering_and_unsupported_lifecycle() {
        let mut item = artifact(json!({"schema":1,"runtime":"node","levels":["session"],"timeout_ms":1000,"script":"process.stdout.write('{}')"}).to_string(), "hook_script");
        validate(std::slice::from_ref(&item)).unwrap();
        item.content.push(' ');
        assert!(validate(&[item]).is_err());
        let item = artifact(json!({"schema":1,"runtime":"node","levels":["completion"],"timeout_ms":1000,"script":"process.stdout.write('{}')"}).to_string(), "hook_script");
        assert!(validate(&[item]).is_err());
    }
    #[tokio::test]
    async fn bounded_scripts_can_restrict_but_cannot_grant_permissions() {
        let root = tempfile::tempdir().unwrap();
        let script = HookScript {
            schema: 1,
            runtime: "node".into(),
            levels: vec!["before_action".into()],
            timeout_ms: 150,
            script: "process.stdout.write(JSON.stringify({deny:true,context:'Inspect evidence'}))"
                .into(),
        };
        let result = execute_hook(root.path(), &script, "before_action", "tool_call", false)
            .await
            .unwrap();
        assert_eq!(result.deny, Some(true));
        for body in [
            "setInterval(() => {}, 1000)",
            "process.stdout.write('x'.repeat(9000))",
            "process.stdout.write(JSON.stringify({allow:true}))",
        ] {
            let hook = HookScript {
                script: body.into(),
                ..script.clone()
            };
            assert!(
                execute_hook(root.path(), &hook, "before_action", "tool_call", false)
                    .await
                    .is_err()
            );
        }
    }
}
