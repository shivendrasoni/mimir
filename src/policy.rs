//! Optional enrolled Betterloop execution. No runner is loaded on standalone paths.
use crate::error::{MimirError, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConstraints {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowed_tools: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowed_shell_programs: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub approved_models: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub maximum_reasoning_level: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyReference {
    pub id: String,
    pub version: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyInstance {
    pub id: String,
    pub definition: PolicyReference,
    pub parameters: Value,
    pub target_agents: Vec<String>,
    pub requirement: String,
    pub mode: String,
    pub on_error: String,
    pub order: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyArtifact {
    pub manifest: Value,
    pub program: String,
    pub sha256: String,
    pub definition_sha256: String,
}
const fn invalid() -> MimirError {
    MimirError::Configuration(String::new())
}
fn error(message: &str) -> MimirError {
    MimirError::Configuration(message.into())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn validate_release(profile: &crate::enterprise::CompiledHarnessProfile) -> Result<()> {
    let id = regex::Regex::new(r"^[a-z][a-z0-9_.-]{0,127}$").map_err(|_| invalid())?;
    let digest = regex::Regex::new(r"^[a-f0-9]{64}$").map_err(|_| invalid())?;
    if profile.instances.len() > 256 || profile.policies.len() > 256 {
        return Err(error("Policy count exceeds limit"));
    }
    let mut identities = BTreeSet::new();
    for artifact in &profile.policies {
        if artifact.program.len() > 1024 * 1024
            || artifact.program.is_empty()
            || hash(artifact.program.as_bytes()) != artifact.sha256
            || !digest.is_match(&artifact.definition_sha256)
            || artifact.manifest["schema"] != 1
            || artifact.manifest["sdk_version"] != "1.0.0"
            || !artifact.manifest["id"]
                .as_str()
                .is_some_and(|value| id.is_match(value))
            || artifact.manifest["match"]["events"]
                .as_array()
                .is_none_or(|events| events.is_empty() || events.len() > 30)
        {
            return Err(error("Invalid compiled policy artifact"));
        }
    }
    for instance in &profile.instances {
        if !id.is_match(&instance.id)
            || !identities.insert(&instance.id)
            || instance.order > 10_000
            || !matches!(instance.requirement.as_str(), "required" | "optional")
            || !matches!(instance.mode.as_str(), "observe" | "enforce")
            || !matches!(instance.on_error.as_str(), "deny" | "continue")
            || instance.target_agents.is_empty()
            || instance.target_agents.len() > 4
            || !digest.is_match(&instance.definition.sha256)
            || !profile.policies.iter().any(|artifact| {
                artifact.manifest["id"] == instance.definition.id
                    && artifact.manifest["version"] == instance.definition.version
                    && artifact.definition_sha256 == instance.definition.sha256
            })
        {
            return Err(error("Invalid or unpinned policy instance"));
        }
    }
    for (agent, values) in profile
        .native_constraints
        .allowed_tools
        .iter()
        .chain(&profile.native_constraints.allowed_shell_programs)
        .chain(&profile.native_constraints.approved_models)
    {
        if !matches!(agent.as_str(), "mimir" | "claude-code" | "cursor" | "codex")
            || values.len() > 256
            || values
                .iter()
                .any(|value| value.is_empty() || value.len() > 256)
        {
            return Err(error("Invalid native policy constraint"));
        }
    }
    for (agent, level) in &profile.native_constraints.maximum_reasoning_level {
        if !matches!(agent.as_str(), "mimir" | "claude-code" | "cursor" | "codex")
            || !matches!(
                level.as_str(),
                "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        {
            return Err(error("Invalid native reasoning constraint"));
        }
    }
    Ok(())
}

fn bounded_file(file: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut current = PathBuf::new();
    for component in file.components() {
        current.push(component);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(error("Policy runtime paths must not contain symlinks"));
        }
    }
    let info = fs::symlink_metadata(file)?;
    if !info.is_file() || info.len() > limit {
        return Err(error("Invalid policy runtime file"));
    }
    Ok(fs::read(file)?)
}
fn verify_installation(descriptor: &Value) -> Result<(PathBuf, PathBuf)> {
    let executable = PathBuf::from(descriptor["executable"].as_str().ok_or_else(invalid)?);
    let runner = PathBuf::from(descriptor["runner"].as_str().ok_or_else(invalid)?);
    if !executable.is_absolute()
        || !runner.is_absolute()
        || descriptor["schema"] != 1
        || descriptor["node_version"]
            .as_str()
            .and_then(|version| version.split('.').next()?.parse::<u32>().ok())
            .is_none_or(|version| version < 22)
        || hash(&bounded_file(&executable, 512 * 1024 * 1024)?) != descriptor["executable_sha256"]
    {
        return Err(error("Policy runner identity changed"));
    }
    let root = runner.parent().ok_or_else(invalid)?;
    let manifest_bytes = bounded_file(&root.join("runtime-manifest.json"), 64 * 1024)?;
    if hash(&manifest_bytes) != descriptor["manifest_sha256"] {
        return Err(error("Policy runtime manifest changed"));
    }
    let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
    let files = manifest["files"].as_array().ok_or_else(invalid)?;
    if !files.iter().any(|file| file["path"] == "policy-child.mjs") {
        return Err(error("Policy child runner is unavailable"));
    }
    for file in files {
        let name = file["path"].as_str().ok_or_else(invalid)?;
        if name.contains('\\')
            || name.split('/').any(|part| matches!(part, "" | "." | ".."))
            || hash(&bounded_file(&root.join(name), 32 * 1024 * 1024)?) != file["sha256"]
        {
            return Err(error("Policy runtime asset changed"));
        }
    }
    Ok((executable, runner))
}
pub(crate) fn verify_broker(broker: &Value) -> Result<()> {
    verify_installation(&broker["policy_runtime"])?;
    for key in ["policy_activation", "policy_activation_root"] {
        if !broker[key]
            .as_str()
            .is_some_and(|value| Path::new(value).is_absolute())
        {
            return Err(error("Policy activation paths must be absolute"));
        }
    }
    Ok(())
}
fn authenticated_activation(snapshot: &Value, token: &str) -> Result<Value> {
    let bytes = BASE64
        .decode(
            snapshot["authentication_input"]
                .as_str()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|_| invalid())?;
    mac.update(&bytes);
    let signature = snapshot["authentication"].as_str().ok_or_else(invalid)?;
    let expected = format!("{:x}", mac.finalize().into_bytes());
    if signature != expected {
        return Err(error("Policy activation authentication failed"));
    }
    let authenticated: Value = serde_json::from_slice(&bytes)?;
    let mut received = snapshot.clone();
    let object = received.as_object_mut().ok_or_else(invalid)?;
    object.remove("authentication");
    object.remove("authentication_input");
    if received != authenticated {
        return Err(error("Policy activation snapshot changed"));
    }
    Ok(authenticated)
}
/// Recovers only signed match descriptors from an authenticated, scoped
/// activation. It never activates a policy or authorizes executable evaluation.
pub(crate) fn captured_envelope(
    workspace: &Path,
    enrollment: &crate::enterprise::Enrollment,
) -> Result<Option<crate::enterprise::SignedHarnessProfileEnvelope>> {
    let broker_file = crate::enterprise::enterprise_state_dir()?.join("device-broker.json");
    let Ok(broker_bytes) = bounded_file(&broker_file, 64 * 1024) else {
        return Ok(None);
    };
    let Ok(broker) = serde_json::from_slice::<Value>(&broker_bytes) else {
        return Ok(None);
    };
    let Some(root) = broker["policy_activation_root"]
        .as_str()
        .filter(|root| Path::new(root).is_absolute())
    else {
        return Ok(None);
    };
    let project = fs::canonicalize(workspace)?;
    let alias = hash(project.to_string_lossy().as_bytes());
    let file = Path::new(root)
        .join(&alias)
        .join("mimir/policy-activation.json");
    let Ok(bytes) = bounded_file(&file, 32 * 1024 * 1024) else {
        return Ok(None);
    };
    let Ok(snapshot) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    let Ok(snapshot) = authenticated_activation(&snapshot, &enrollment.installation_token) else {
        return Ok(None);
    };
    if snapshot["schema"] != 1
        || snapshot["installation_id"] != enrollment.installation_id.to_string()
        || snapshot["organization_id"] != enrollment.organization_id.to_string()
        || snapshot["team_id"] != enrollment.team_id.to_string()
        || snapshot["project_alias"] != alias
        || snapshot["activation"]["runtime"]["harness"] != "mimir"
        || snapshot["activation"]["runtime"]["harness_version"] != env!("CARGO_PKG_VERSION")
        || snapshot["release_digest"] != snapshot["envelope"]["sha256"]
        || snapshot["activation"]["release_digest"] != snapshot["release_digest"]
    {
        return Ok(None);
    }
    let Ok(envelope) = serde_json::from_value::<crate::enterprise::SignedHarnessProfileEnvelope>(
        snapshot["envelope"].clone(),
    ) else {
        return Ok(None);
    };
    let Ok(payload) = BASE64.decode(&envelope.payload_base64) else {
        return Ok(None);
    };
    if payload.len() > 16 * 1024 * 1024 || hash(&payload) != envelope.sha256 {
        return Ok(None);
    }
    let Ok(profile) = serde_json::from_slice::<Value>(&payload) else {
        return Ok(None);
    };
    if snapshot["activation"]["release_id"] != profile["release_id"] {
        return Ok(None);
    }
    Ok(Some(envelope))
}
fn matched(artifact: &PolicyArtifact, event: &Value) -> bool {
    artifact.manifest["match"]["events"]
        .as_array()
        .is_some_and(|events| events.contains(&event["event"]))
        && artifact.manifest["match"]["tools"]
            .as_array()
            .is_none_or(|tools| tools.contains(&event["tool"]))
}
fn failure_denies(
    profile: &crate::enterprise::CompiledHarnessProfile,
    activation: &Value,
    event: &Value,
) -> bool {
    profile.instances.iter().any(|instance| {
        instance.mode == "enforce"
            && activation["policies"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["instance_id"] == instance.id && item["state"] == "active")
            })
            && profile.policies.iter().any(|artifact| {
                artifact.manifest["id"] == instance.definition.id
                    && artifact.manifest["version"] == instance.definition.version
                    && artifact.definition_sha256 == instance.definition.sha256
                    && matched(artifact, event)
                    && artifact.manifest["effects"]
                        .as_array()
                        .is_some_and(|effects| effects.contains(&json!("deny")))
            })
    })
}
fn failed_policy_result(
    profile: &crate::enterprise::CompiledHarnessProfile,
    activation: &Value,
    event: &Value,
) -> Value {
    let results = profile.instances.iter().filter_map(|instance| {
        let state = activation["policies"].as_array()?.iter().find(|state| state["instance_id"] == instance.id)?;
        if !matches!(state["state"].as_str(), Some("active" | "observed")) { return None; }
        let artifact = profile.policies.iter().find(|artifact| artifact.manifest["id"] == instance.definition.id &&
            artifact.manifest["version"] == instance.definition.version && artifact.definition_sha256 == instance.definition.sha256)?;
        if !matched(artifact, event) { return None; }
        let deny = state["state"] == "active" && instance.mode == "enforce" && artifact.manifest["effects"].as_array().is_some_and(|effects| effects.contains(&json!("deny")));
        Some(json!({"instance_id":instance.id,"status":"failed","outcome":if deny {"deny"} else {"allow"},"reason_code":"policy_runtime_failed"}))
    }).collect::<Vec<_>>();
    json!({"results":results})
}

/// Evaluates a bounded native event only after explicit enrollment and activation.
/// Unsupported/unavailable-before-activation policies do not affect the agent.
pub async fn evaluate(workspace: &Path, event: Value) -> Result<String> {
    let Some(profile) = crate::enterprise::policy_release()? else {
        return Ok(String::new());
    };
    let Some(enrollment) = crate::enterprise::policy_enrollment()? else {
        return Ok(String::new());
    };
    let broker_file = crate::enterprise::enterprise_state_dir()?.join("device-broker.json");
    if !broker_file.is_file() {
        return Ok(String::new());
    }
    let broker: Value = match bounded_file(&broker_file, 64 * 1024)
        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
    {
        Ok(value) => value,
        Err(_) => return Ok(String::new()),
    };
    let project = fs::canonicalize(workspace)?;
    let alias = hash(project.to_string_lossy().as_bytes());
    let activation_path = if let Some(root) = broker["policy_activation_root"].as_str() {
        Path::new(root)
            .join(&alias)
            .join("mimir/policy-activation.json")
    } else if let Some(file) = broker["policy_activation"].as_str() {
        PathBuf::from(file)
    } else {
        return Ok(String::new());
    };
    let snapshot: Value = match bounded_file(&activation_path, 32 * 1024 * 1024)
        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
    {
        Ok(value) => value,
        Err(_) => return Ok(String::new()),
    };
    let snapshot = match authenticated_activation(&snapshot, &enrollment.installation_token) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(String::new()),
    };
    let envelope = crate::enterprise::policy_envelope()?;
    if snapshot["installation_id"] != enrollment.installation_id.to_string()
        || snapshot["organization_id"] != enrollment.organization_id.to_string()
        || snapshot["team_id"] != enrollment.team_id.to_string()
        || snapshot["project_alias"] != alias
        || snapshot["release_digest"] != envelope.sha256
        || snapshot["activation"]["release_id"] != profile.release_id.to_string()
        || snapshot["activation"]["runtime"]["harness"] != "mimir"
        || snapshot["activation"]["runtime"]["harness_version"] != env!("CARGO_PKG_VERSION")
    {
        return Ok(String::new());
    }
    let activation = &snapshot["activation"];
    if !activation["policies"].as_array().is_some_and(|policies| {
        policies
            .iter()
            .any(|policy| matches!(policy["state"].as_str(), Some("active" | "observed")))
    }) {
        return Ok(String::new());
    }
    let matching = profile.instances.iter().any(|instance| {
        activation["policies"].as_array().is_some_and(|policies| {
            policies.iter().any(|policy| {
                policy["instance_id"] == instance.id
                    && matches!(policy["state"].as_str(), Some("active" | "observed"))
            })
        }) && profile.policies.iter().any(|artifact| {
            artifact.definition_sha256 == instance.definition.sha256 && matched(artifact, &event)
        })
    });
    if !matching {
        return Ok(String::new());
    }
    let request_id = Uuid::new_v4().to_string();
    let identity = json!({"request_id":request_id,"installation_id":enrollment.installation_id,"organization_id":enrollment.organization_id,
        "team_id":enrollment.team_id,"project_alias":alias,"release_id":profile.release_id,"release_digest":envelope.sha256,
        "event":event["event"],"session_id":event.get("session_id").unwrap_or(&Value::Null)});
    let request = json!({"schema":1,"request_id":request_id,"release_id":profile.release_id,"release_digest":envelope.sha256,
        "runtime":activation["runtime"],"event":event,"policies":profile.policies,"instances":profile.instances,"activation":activation,"facts":{}});
    let expired = profile.expires_at <= chrono::Utc::now()
        || profile.created_at > chrono::Utc::now() + chrono::Duration::minutes(5);
    let result = if !crate::enterprise::policy_primary_cache_verified() {
        Err(error("Verified policy release unavailable"))
    } else if expired {
        Err(error("Verified policy release expired"))
    } else if snapshot["runtime_manifest_sha256"] != broker["policy_runtime"]["manifest_sha256"] {
        Err(error("Activated policy runtime changed"))
    } else {
        invoke(&broker["policy_runtime"], &project, &identity, request).await
    };
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            crate::enterprise::report_policy_results(
                &profile,
                &enrollment,
                &event,
                &failed_policy_result(&profile, activation, &event),
            );
            return if failure_denies(&profile, activation, &event) {
                Err(error(
                    "The enabled policy could not evaluate this action. Repair Betterloop and retry.",
                ))
            } else {
                Ok(String::new())
            };
        }
    };
    crate::enterprise::report_policy_results(&profile, &enrollment, &event, &result);
    if result["outcome"] == "deny" {
        let message = result["results"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["outcome"] == "deny")
            .and_then(|item| item["message"].as_str())
            .unwrap_or("Blocked by the reviewed policy");
        return Err(error(message));
    }
    Ok(result["guidance"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n"))
}
pub(crate) fn readiness_issues(workspace: &Path) -> Result<Vec<Value>> {
    if crate::enterprise::policy_enrollment()?.is_none() {
        return Ok(Vec::new());
    }
    let file = crate::enterprise::enterprise_state_dir()?.join("device-broker.json");
    if !file.is_file() {
        return Ok(vec![
            json!({"key":"policy_runtime","required":false,"reason_code":"runtime_unavailable"}),
        ]);
    }
    let broker: Value = serde_json::from_slice(&bounded_file(&file, 64 * 1024)?)?;
    let alias = hash(fs::canonicalize(workspace)?.to_string_lossy().as_bytes());
    let root = broker["policy_activation_root"]
        .as_str()
        .ok_or_else(invalid)?;
    let snapshot: Value = match bounded_file(
        &Path::new(root)
            .join(alias)
            .join("mimir/policy-activation.json"),
        32 * 1024 * 1024,
    ) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(_) => {
            return Ok(vec![
                json!({"key":"policy_activation","required":false,"reason_code":"activation_pending"}),
            ]);
        }
    };
    Ok(snapshot["activation"]["policies"].as_array().into_iter().flatten()
        .filter(|policy| matches!(policy["state"].as_str(), Some("inactive" | "error")))
        .map(|policy| json!({"key":policy["instance_id"],"required":false,"reason_code":policy["reason_code"]})).collect())
}
async fn invoke(
    descriptor: &Value,
    project: &Path,
    identity: &Value,
    request: Value,
) -> Result<Value> {
    let (executable, runner) = verify_installation(descriptor)?;
    let input = serde_json::to_vec(&json!({"identity":identity,"request":request}))?;
    if input.len() > 2 * 1024 * 1024
        || serde_json::to_vec(&json!({"event": request["event"], "facts": request["facts"]}))?.len()
            > 1024 * 1024
    {
        return Err(error("Policy IPC input exceeds limit"));
    }
    let mut child = tokio::process::Command::new(executable)
        .arg(runner)
        .current_dir(project)
        .env_clear()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = tokio::time::timeout(Duration::from_millis(2500), async {
        let mut stdin = child.stdin.take().ok_or_else(invalid)?;
        stdin.write_all(&input).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(invalid)?
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 256 * 1024 || !child.wait().await?.success() {
            return Err(error("Policy runner failed or exceeded output limit"));
        }
        let response: Value = serde_json::from_slice(&bytes)?;
        let result = &response["result"];
        if response["identity"] != *identity
            || result["schema"] != 1
            || result["request_id"] != identity["request_id"]
            || result["release_id"] != identity["release_id"]
            || result["release_digest"] != identity["release_digest"]
            || !matches!(
                result["outcome"].as_str(),
                Some("allow" | "deny" | "guidance")
            )
        {
            return Err(error("Policy response identity mismatch"));
        }
        Ok(result.clone())
    })
    .await;
    let _ = child.kill().await;
    result.map_err(|_| error("Policy runner timed out"))?
}
