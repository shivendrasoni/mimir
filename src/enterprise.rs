#![allow(
    clippy::missing_errors_doc,
    reason = "enterprise command and native-gate errors are surfaced through the shared MimirError contract"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::{Client, StatusCode, Url};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::ThinkingLevel,
    resources::Skill,
    tools::ToolInventoryItem,
};

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOOL_INVENTORY_ITEMS: usize = 2_048;
const MAX_INVENTORY_ALLOWED_PROGRAMS: usize = 256;
const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CONFIG_FILE: &str = "enrollment.json";
const PROFILE_FILE: &str = "profile.json";
const PREVIOUS_PROFILE_FILE: &str = "profile.previous.json";
const OVERRIDES_FILE: &str = "overrides.json";
const REVOKED_FILE: &str = "revoked.json";
#[derive(Clone, PartialEq, Eq)]
struct ConnectionIdentity {
    installation_id: Uuid,
    organization_id: Uuid,
    team_id: Uuid,
    profile_id: Uuid,
    endpoint: String,
    credential_digest: String,
}
impl From<&Enrollment> for ConnectionIdentity {
    fn from(enrollment: &Enrollment) -> Self {
        Self {
            installation_id: enrollment.installation_id,
            organization_id: enrollment.organization_id,
            team_id: enrollment.team_id,
            profile_id: enrollment.profile_id,
            endpoint: enrollment.endpoint.clone(),
            credential_digest: format!(
                "{:x}",
                Sha256::digest(enrollment.installation_token.as_bytes())
            ),
        }
    }
}
static SESSION_CONNECTION: OnceLock<Option<ConnectionIdentity>> = OnceLock::new();
static SESSION_WORKSPACE: OnceLock<PathBuf> = OnceLock::new();
fn runtime_workspace() -> Result<PathBuf> {
    SESSION_WORKSPACE
        .get()
        .cloned()
        .map_or_else(|| Ok(std::env::current_dir()?), Ok)
}
/// Capture repository identity before any native company policy is evaluated.
pub fn bind_repository(workspace: &Path) -> Result<()> {
    let canonical = fs::canonicalize(workspace)?;
    if let Some(existing) = SESSION_WORKSPACE.get() {
        if existing != &canonical {
            return Err(MimirError::Configuration(
                "Repository changed; start a fresh agent session".into(),
            ));
        }
    } else {
        let _ = SESSION_WORKSPACE.set(canonical);
    }
    Ok(())
}
pub(crate) fn current_repository_approved(profile: &CompiledHarnessProfile) -> bool {
    let path = SESSION_WORKSPACE
        .get()
        .cloned()
        .or_else(|| std::env::current_dir().ok());
    let Some(path) = path else {
        return false;
    };
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(path)
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    let identity = if remote.contains("://") {
        Url::parse(remote.trim()).ok().and_then(|url| {
            url.host_str().map(|host| {
                (
                    host.to_lowercase(),
                    url.path().trim_start_matches('/').to_owned(),
                )
            })
        })
    } else {
        remote.trim().split_once(':').map(|(host, repo)| {
            (
                host.rsplit('@').next().unwrap_or(host).to_lowercase(),
                repo.to_owned(),
            )
        })
    };
    let Some((host, repo)) = identity else {
        return false;
    };
    if !host.contains('.') {
        return false;
    }
    let repo = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    profile.source_metadata["knowledge_map"]["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|project| {
            project["repositories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|entry| {
                    let entry = entry.to_lowercase();
                    entry == format!("{host}/{repo}") || (host == "github.com" && entry == repo)
                })
        })
}
fn project_repository_policy(
    mut profile: CompiledHarnessProfile,
    approved: bool,
) -> CompiledHarnessProfile {
    if approved {
        return profile;
    }
    let mode = profile
        .environment
        .as_ref()
        .and_then(|env| env.unapproved_repository_behavior.as_deref());
    if mode == Some("report_only") {
        profile
            .instances
            .retain(|instance| instance.requirement == "required");
        profile.skills.clear();
        profile.registry_artifacts.clear();
        profile.telemetry.clear();
        if let Some(env) = &mut profile.environment {
            env.context.clear();
            env.skills.clear();
            env.mcp_servers.clear();
            env.workflows.clear();
        }
        if let Some(metadata) = profile.source_metadata.as_object_mut() {
            metadata.remove("knowledge_map");
        }
    } else if mode == Some("apply_team_defaults") {
        profile
            .instances
            .retain(|instance| instance.requirement == "required");
    }
    profile
}
pub fn report_only_repository() -> Result<bool> {
    if !is_enrolled()? {
        return Ok(false);
    }
    let profile = EnterpriseManager::global()?.load_runtime_profile()?;
    Ok(profile
        .environment
        .as_ref()
        .and_then(|e| e.unapproved_repository_behavior.as_deref())
        == Some("report_only")
        && !current_repository_approved(&profile))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub schema: u8,
    pub endpoint: String,
    pub installation_id: Uuid,
    pub installation_token: String,
    pub organization_id: Uuid,
    pub team_id: Uuid,
    pub profile_id: Uuid,
    pub channel: String,
    pub signing_public_key_base64: String,
    pub etag: Option<String>,
}

/// A project-scoped fleet-learning credential issued only after Harness enrollment.
///
/// The client token is deliberately kept in the project-local `.mimir` store and
/// is never included in command output or debug formatting.
#[derive(Clone, Serialize, Deserialize)]
pub struct FleetLearningConnection {
    pub schema: u8,
    pub project_id: Uuid,
    pub installation_id: Uuid,
    pub pack_url: String,
    pub contribution_url: String,
    pub public_key_base64: String,
    pub client_token: String,
    pub local_development: bool,
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "the hand-written Debug output intentionally exposes only bounded connection metadata"
)]
impl std::fmt::Debug for FleetLearningConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FleetLearningConnection")
            .field("schema", &self.schema)
            .field("project_id", &self.project_id)
            .field("installation_id", &self.installation_id)
            .field("pack_url", &self.pack_url)
            .field("contribution_url", &self.contribution_url)
            .field("local_development", &self.local_development)
            .field("client_token", &"[REDACTED]")
            .finish()
    }
}

impl std::fmt::Debug for Enrollment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Enrollment")
            .field("schema", &self.schema)
            .field("endpoint", &self.endpoint)
            .field("installation_id", &self.installation_id)
            .field("organization_id", &self.organization_id)
            .field("team_id", &self.team_id)
            .field("profile_id", &self.profile_id)
            .field("channel", &self.channel)
            .field("installation_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedHarnessProfileEnvelope {
    pub schema: u8,
    pub payload_base64: String,
    pub sha256: String,
    pub key_id: Uuid,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledHarnessProfile {
    pub schema: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<crate::working_environment::WorkingEnvironment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registry_artifacts: Vec<crate::registry::RegistryArtifact>,
    pub release_id: Uuid,
    pub organization_id: Uuid,
    pub team_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub profile_name: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub minimum_setup_version: String,
    pub minimum_runtime_versions: BTreeMap<String, String>,
    pub policies: Vec<crate::policy::PolicyArtifact>,
    pub instances: Vec<crate::policy::PolicyInstance>,
    pub native_constraints: crate::policy::NativeConstraints,
    #[serde(default)]
    pub revoked_catalog_items: Vec<String>,
    pub skills: Vec<ManagedSkill>,
    pub telemetry: Vec<String>,
    #[serde(flatten)]
    pub source_metadata: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforcement {
    Mandatory,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSkill {
    pub id: String,
    pub version: String,
    pub enforcement: Enforcement,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub content_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalOverride {
    pub binding_key: String,
    pub disabled: bool,
    pub reason: String,
    pub changed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OverrideStore {
    schema: u8,
    overrides: Vec<LocalOverride>,
}

#[derive(Debug, Clone, Serialize)]
struct ComplianceEvent {
    schema: u8,
    event_id: Uuid,
    release_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_context: Option<Value>,
    hook: Option<String>,
    event_type: String,
    occurred_at: DateTime<Utc>,
    duration_bucket: Option<String>,
    decision: Option<String>,
    error_code: Option<String>,
    mimir_version: String,
}

/// Produces an opaque key scoped to this Betterloop installation. The native
/// Mimir session name is never serialized or sent over the network.
fn session_alias(enrollment: &Enrollment, session_id: &str) -> Option<String> {
    (!session_id.trim().is_empty()).then(|| {
        let mut mac = Hmac::<Sha256>::new_from_slice(enrollment.installation_token.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(session_id.as_bytes());
        format!("ses_{:x}", mac.finalize().into_bytes())
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ManagedLifecycleEffect {
    pub notify_failure: bool,
}

#[derive(Debug, Serialize)]
struct LocalProcessInventory {
    enabled: bool,
    allowed_programs: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolInventoryReport<'a> {
    schema: u8,
    release_id: Uuid,
    mimir_version: &'static str,
    fingerprint: &'a str,
    tools: &'a [ToolInventoryItem],
    local_process: LocalProcessInventory,
}

#[derive(Debug, Deserialize)]
struct EnrollmentResponse {
    schema: u8,
    installation_id: Uuid,
    installation_token: String,
    organization_id: Uuid,
    team_id: Uuid,
    profile_id: Uuid,
    channel: String,
    profile: SignedHarnessProfileEnvelope,
    signing_public_key_base64: String,
}

#[derive(Debug, Clone)]
pub struct EnterpriseManager {
    root: PathBuf,
    client: Client,
}

impl EnterpriseManager {
    /// Opens the global, user-owned enterprise state store.
    pub fn global() -> Result<Self> {
        let root = enterprise_state_dir()?;
        Ok(Self {
            root,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| {
                    MimirError::Configuration(format!("enterprise HTTP client: {error}"))
                })?,
        })
    }

    /// Attach a device-issued restricted credential after verifying its signed profile.
    ///
    /// # Errors
    /// Rejects invalid credentials, profiles or replacement of an unrelated enrollment.
    pub fn attach_device(&self, input: &Value) -> Result<Value> {
        if input.as_object().is_none_or(|value| {
            value.len() != 3
                || value
                    .keys()
                    .any(|key| !matches!(key.as_str(), "endpoint" | "registration" | "broker"))
        }) {
            return Err(MimirError::Configuration(
                "Invalid device attachment fields".into(),
            ));
        }
        let endpoint = validate_endpoint(input["endpoint"].as_str().unwrap_or_default())?;
        let response: EnrollmentResponse = serde_json::from_value(input["registration"].clone())?;
        if response.schema != 1
            || !response.installation_token.starts_with("bli_")
            || response.installation_token.len() < 32
        {
            return Err(MimirError::Configuration(
                "Invalid device runtime credential".into(),
            ));
        }
        let profile = verify_envelope(&response.profile, &response.signing_public_key_base64)?;
        validate_profile(&profile)?;
        if self.root.join(CONFIG_FILE).exists() {
            let prior = self.load_enrollment()?;
            if prior.installation_id != response.installation_id
                || prior.organization_id != response.organization_id
                || prior.team_id != response.team_id
                || prior.profile_id != response.profile_id
                || prior.endpoint != endpoint.as_str().trim_end_matches('/')
            {
                return Err(MimirError::Configuration(
                    "Disconnect the existing native enrollment before attaching this device".into(),
                ));
            }
        }
        let enrollment = Enrollment {
            schema: 1,
            endpoint: endpoint.as_str().trim_end_matches('/').to_owned(),
            installation_id: response.installation_id,
            installation_token: response.installation_token,
            organization_id: response.organization_id,
            team_id: response.team_id,
            profile_id: response.profile_id,
            channel: response.channel,
            signing_public_key_base64: response.signing_public_key_base64,
            etag: None,
        };
        validate_enrollment_scope(&profile, &enrollment)?;
        let broker = input["broker"].clone();
        if broker.as_object().is_none_or(|value| {
            value.len() != 5
                || value.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "executable"
                            | "runner"
                            | "policy_runtime"
                            | "policy_activation"
                            | "policy_activation_root"
                    )
                })
        }) {
            return Err(MimirError::Configuration(
                "Invalid device broker fields".into(),
            ));
        }
        for key in ["executable", "runner"] {
            let value = broker[key]
                .as_str()
                .ok_or_else(|| MimirError::Configuration("Missing device broker".into()))?;
            if !Path::new(value).is_absolute() || !Path::new(value).is_file() {
                return Err(MimirError::Configuration(
                    "Device broker must use existing absolute files".into(),
                ));
            }
        }
        crate::policy::verify_broker(&broker)?;
        ensure_owner_only_dir(&self.root)?;
        write_secure_json(&self.root.join(CONFIG_FILE), &enrollment)?;
        self.activate_envelope(&response.profile)?;
        write_secure_json(&self.root.join("device-broker.json"), &broker)?;
        Ok(status_json(&enrollment, &profile, true))
    }

    #[cfg(test)]
    fn at(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
            client: Client::builder()
                .build()
                .map_err(|error| MimirError::Configuration(error.to_string()))?,
        })
    }

    /// Redeems a one-time code and atomically activates the returned signed profile.
    pub async fn enroll(&self, endpoint: &str, code: &str) -> Result<Value> {
        let endpoint = validate_endpoint(endpoint)?;
        let alias = format!("inst_{}", Uuid::new_v4().simple());
        let response = self
            .client
            .post(
                endpoint
                    .join("v1/harness/enroll")
                    .map_err(configuration_error)?,
            )
            .json(&json!({
                "code": code,
                "installation_alias": alias,
                "mimir_version": env!("CARGO_PKG_VERSION"),
                "platform": platform(),
            }))
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            return Err(server_error("enrollment", response).await);
        }
        let response: EnrollmentResponse = response.json().await.map_err(http_error)?;
        if response.schema != 1 {
            return Err(MimirError::Configuration(
                "unsupported enrollment response schema".into(),
            ));
        }
        let profile = verify_envelope(&response.profile, &response.signing_public_key_base64)?;
        validate_profile(&profile)?;
        let enrollment = Enrollment {
            schema: 1,
            endpoint: endpoint.as_str().trim_end_matches('/').to_owned(),
            installation_id: response.installation_id,
            installation_token: response.installation_token,
            organization_id: response.organization_id,
            team_id: response.team_id,
            profile_id: response.profile_id,
            channel: response.channel,
            signing_public_key_base64: response.signing_public_key_base64,
            etag: None,
        };
        validate_enrollment_scope(&profile, &enrollment)?;
        ensure_owner_only_dir(&self.root)?;
        write_secure_json(&self.root.join(CONFIG_FILE), &enrollment)?;
        self.activate_envelope(&response.profile)?;
        let _ = self
            .report_event(&enrollment, &profile, "profile_applied", None)
            .await;
        Ok(status_json(&enrollment, &profile, true))
    }

    /// Provisions the least-privilege fleet connection for one local project.
    /// Harness enrollment is the authentication proof; callers never paste an
    /// API URL or token.
    pub async fn connect_fleet_learning(
        &self,
        project_id: Uuid,
        project_label: &str,
    ) -> Result<FleetLearningConnection> {
        let enrollment = self.load_enrollment()?;
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let response = self
            .client
            .post(
                endpoint
                    .join("v1/harness/fleet-connection")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .json(&json!({
                "project_id": project_id,
                "project_label": project_label,
            }))
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            return Err(server_error("fleet learning connection", response).await);
        }
        let connection: FleetLearningConnection = response.json().await.map_err(http_error)?;
        if connection.schema != 1
            || connection.project_id.is_nil()
            || connection.client_token.is_empty()
        {
            return Err(MimirError::Configuration(
                "invalid fleet learning connection response".into(),
            ));
        }
        validate_endpoint(&connection.pack_url)?;
        validate_endpoint(&connection.contribution_url)?;
        Ok(connection)
    }

    /// Fetches and atomically activates the latest valid signed profile.
    pub async fn sync(&self) -> Result<Value> {
        let mut enrollment = self.load_enrollment()?;
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let mut request = self
            .client
            .get(
                endpoint
                    .join("v1/harness/profile")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .header("x-mimir-version", env!("CARGO_PKG_VERSION"));
        if let Some(etag) = &enrollment.etag {
            request = request.header("if-none-match", etag);
        }
        let response = request.send().await.map_err(http_error)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            let profile = self.load_profile()?;
            validate_profile(&profile)?;
            return Ok(status_json(&enrollment, &profile, false));
        }
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            write_secure_json(
                &self.root.join(REVOKED_FILE),
                &json!({"installation_id":enrollment.installation_id}),
            )?;
            return Err(MimirError::Configuration(
                "enterprise installation credential was revoked".into(),
            ));
        }
        if !response.status().is_success() {
            return Err(server_error("profile sync", response).await);
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes().await.map_err(http_error)?;
        if bytes.len() > MAX_PROFILE_BYTES * 2 {
            return Err(MimirError::Configuration(
                "signed profile envelope exceeds size limit".into(),
            ));
        }
        let envelope: SignedHarnessProfileEnvelope = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.report_profile_rejected(&enrollment, "invalid_envelope")
                    .await;
                return Err(error.into());
            }
        };
        let profile = match verify_envelope(&envelope, &enrollment.signing_public_key_base64) {
            Ok(profile) => profile,
            Err(error) => {
                self.report_profile_rejected(&enrollment, "invalid_signature_or_payload")
                    .await;
                return Err(error);
            }
        };
        if let Err(error) = validate_profile(&profile)
            .and_then(|()| validate_enrollment_scope(&profile, &enrollment))
        {
            self.report_profile_rejected(&enrollment, "unsupported_or_invalid_profile")
                .await;
            return Err(error);
        }
        self.activate_envelope(&envelope)?;
        enrollment.etag = etag;
        write_secure_json(&self.root.join(CONFIG_FILE), &enrollment)?;
        let _ = self
            .report_event(&enrollment, &profile, "profile_applied", None)
            .await;
        Ok(status_json(&enrollment, &profile, true))
    }

    pub async fn sync_with_backoff(&self) -> Result<Value> {
        let mut delay = Duration::from_millis(250);
        for attempt in 0..4 {
            match self.sync().await {
                Ok(value) => return Ok(value),
                Err(error)
                    if attempt == 3
                        || error
                            .to_string()
                            .contains("enterprise installation credential was revoked") =>
                {
                    return Err(error);
                }
                Err(_) => {
                    let jitter = Duration::from_millis(u64::from(Uuid::new_v4().as_bytes()[0]));
                    tokio::time::sleep(delay + jitter).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    pub fn status(&self) -> Result<Value> {
        if !self.root.join(CONFIG_FILE).exists() {
            return Ok(json!({"schema": 1, "enrolled": false}));
        }
        let enrollment = self.load_enrollment()?;
        if self.root.join(REVOKED_FILE).exists() {
            let marker: Value = serde_json::from_slice(&fs::read(self.root.join(REVOKED_FILE))?)?;
            if marker["installation_id"] == json!(enrollment.installation_id) {
                return Ok(
                    json!({"schema":1,"enrolled":true,"installation_id":enrollment.installation_id,"organization_id":enrollment.organization_id,"team_id":enrollment.team_id,"profile_id":enrollment.profile_id,"revoked":true,"expired":true}),
                );
            }
        }
        let profile = self.load_profile()?;
        Ok(status_json(&enrollment, &profile, false))
    }

    pub fn explain(&self) -> Result<Value> {
        let enrollment = self.load_enrollment()?;
        let profile = self.load_profile()?;
        Ok(json!({
            "schema": 1,
            "organization_id": enrollment.organization_id,
            "team_id": enrollment.team_id,
            "profile_id": enrollment.profile_id,
            "release_id": profile.release_id,
            "profile_name": profile.profile_name,
            "minimum_runtime_versions": profile.minimum_runtime_versions,
            "expires_at": profile.expires_at,
            "policies": profile.instances,
            "native_constraints": profile.native_constraints,
            "skills": profile.skills.iter().map(|skill| json!({
                "id": skill.id,
                "version": skill.version,
                "enforcement": skill.enforcement,
            })).collect::<Vec<_>>(),
            "environment": profile.environment,
            "environment_origins": profile.source_metadata.get("environment_origins"),
            "telemetry": profile.telemetry,
        }))
    }

    pub fn list_overrides(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.load_overrides()?.overrides)?)
    }

    pub async fn set_override(
        &self,
        binding_key: &str,
        disabled: bool,
        reason: &str,
    ) -> Result<Value> {
        let enrollment = self.load_enrollment()?;
        let profile = self.load_profile()?;
        let binding = profile
            .instances
            .iter()
            .find(|instance| instance.id == binding_key)
            .ok_or_else(|| {
                MimirError::Configuration(format!("unknown managed policy: {binding_key}"))
            })?;
        if binding.requirement == "required" {
            return Err(MimirError::Configuration(
                "required enterprise policies cannot be overridden".into(),
            ));
        }
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 1024 {
            return Err(MimirError::Configuration(
                "override reason must contain 1 to 1024 bytes".into(),
            ));
        }
        let mut store = self.load_overrides()?;
        store
            .overrides
            .retain(|entry| entry.binding_key != binding_key);
        store.overrides.push(LocalOverride {
            binding_key: binding_key.to_owned(),
            disabled,
            reason: reason.to_owned(),
            changed_at: Utc::now(),
        });
        write_secure_json(&self.root.join(OVERRIDES_FILE), &store)?;
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let response = self
            .client
            .post(
                endpoint
                    .join("v1/harness/overrides")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .json(&json!({
                "release_id": profile.release_id,
                "instance_id": binding_key,
                "disabled": disabled,
                "reason_code": "user_requested",
            }))
            .send()
            .await;
        if let Ok(response) = response
            && !response.status().is_success()
        {
            eprintln!(
                "warning: enterprise override is active locally but its metadata report was not accepted"
            );
        }
        Ok(json!({"instance_id": binding_key, "disabled": disabled,
            "native_constraints_authoritative": true,
            "notice": "This override changes the optional script policy. Signed native bounds remain active; changing them requires a reviewed company release."}))
    }

    /// Removes only the known enterprise credential and cache files.
    pub async fn unenroll(&self) -> Result<Value> {
        if self.root.join(CONFIG_FILE).exists() {
            let enrollment = self.load_enrollment()?;
            let endpoint = validate_endpoint(&enrollment.endpoint)?;
            let response = self
                .client
                .delete(
                    endpoint
                        .join("v1/harness/enrollment")
                        .map_err(configuration_error)?,
                )
                .bearer_auth(&enrollment.installation_token)
                .send()
                .await
                .map_err(http_error)?;
            if !response.status().is_success()
                && !matches!(response.status().as_u16(), 401 | 403 | 410)
            {
                return Err(server_error("unenrollment", response).await);
            }
        }
        let known_files = [
            CONFIG_FILE,
            PROFILE_FILE,
            PREVIOUS_PROFILE_FILE,
            OVERRIDES_FILE,
            "device-broker.json",
            REVOKED_FILE,
        ];
        let backup = self
            .root
            .parent()
            .unwrap_or(&self.root)
            .join(format!("removed-enterprise-{}", Uuid::new_v4()));
        for name in known_files {
            let file = self.root.join(name);
            if !file.exists() {
                continue;
            }
            let metadata = fs::symlink_metadata(&file)?;
            if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
                return Err(MimirError::Configuration(
                    "Enterprise removal encountered an unexpected file".into(),
                ));
            }
            ensure_owner_only_dir(&backup)?;
            fs::copy(&file, backup.join(name))?;
            set_owner_only(&backup.join(name))?;
        }
        for name in [
            CONFIG_FILE,
            PROFILE_FILE,
            PREVIOUS_PROFILE_FILE,
            OVERRIDES_FILE,
            "device-broker.json",
            REVOKED_FILE,
        ] {
            match fs::remove_file(self.root.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_dir(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(error.into()),
        }
        Ok(json!({"unenrolled": true}))
    }

    pub fn load_runtime_profile(&self) -> Result<CompiledHarnessProfile> {
        let profile = match self.load_profile() {
            Ok(profile) => profile,
            Err(original) => {
                let enrollment = self.load_enrollment()?;
                if self.root.join(REVOKED_FILE).exists() {
                    return Err(original);
                }
                let Some(envelope) =
                    crate::policy::captured_envelope(&runtime_workspace()?, &enrollment)?
                else {
                    return Err(original);
                };
                let profile = verify_envelope(&envelope, &enrollment.signing_public_key_base64)?;
                validate_profile(&profile)?;
                validate_enrollment_scope(&profile, &enrollment)?;
                // Recovered descriptors preserve native gates. Customer code is
                // never evaluated while the primary cache is unverifiable.
                profile
            }
        };
        let approved = current_repository_approved(&profile);
        Ok(project_repository_policy(profile, approved))
    }

    pub fn load_profile(&self) -> Result<CompiledHarnessProfile> {
        let bytes = fs::read(self.root.join(PROFILE_FILE)).map_err(|error| {
            MimirError::Configuration(format!(
                "Mimir is not enrolled or has no signed profile: {error}"
            ))
        })?;
        let envelope: SignedHarnessProfileEnvelope = serde_json::from_slice(&bytes)?;
        let enrollment = self.load_enrollment()?;
        let revoked = self.root.join(REVOKED_FILE);
        if revoked.exists() {
            let marker: Value = serde_json::from_slice(&fs::read(&revoked)?)?;
            if marker["installation_id"] == json!(enrollment.installation_id) {
                return Err(MimirError::Configuration(
                    "enterprise installation credential was revoked; reconnect this device harness"
                        .into(),
                ));
            }
        }
        let profile = verify_envelope(&envelope, &enrollment.signing_public_key_base64)?;
        validate_profile(&profile)?;
        validate_enrollment_scope(&profile, &enrollment)?;
        Ok(crate::optional_resources::load(
            profile,
            &enrollment.signing_public_key_base64,
            enrollment.installation_id,
        ))
    }

    fn load_enrollment(&self) -> Result<Enrollment> {
        let bytes = fs::read(self.root.join(CONFIG_FILE)).map_err(|error| {
            MimirError::Configuration(format!("Mimir is not enterprise-enrolled: {error}"))
        })?;
        let enrollment: Enrollment = serde_json::from_slice(&bytes)?;
        if enrollment.schema != 1 {
            return Err(MimirError::Configuration(
                "unsupported enrollment schema".into(),
            ));
        }
        if let Some(expected) = SESSION_CONNECTION.get()
            && expected.as_ref() != Some(&ConnectionIdentity::from(&enrollment))
        {
            return Err(MimirError::Configuration(
                "Enterprise connection changed; start a fresh session".into(),
            ));
        }
        Ok(enrollment)
    }

    fn load_overrides(&self) -> Result<OverrideStore> {
        match fs::read(self.root.join(OVERRIDES_FILE)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(OverrideStore {
                schema: 1,
                overrides: Vec::new(),
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn activate_envelope(&self, envelope: &SignedHarnessProfileEnvelope) -> Result<()> {
        ensure_owner_only_dir(&self.root)?;
        let current = self.root.join(PROFILE_FILE);
        if current.exists() {
            let previous = self.root.join(PREVIOUS_PROFILE_FILE);
            fs::copy(&current, &previous)?;
            set_owner_only(&previous)?;
        }
        write_secure_json(&current, envelope)?;
        match fs::remove_file(self.root.join(REVOKED_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn report_event(
        &self,
        enrollment: &Enrollment,
        profile: &CompiledHarnessProfile,
        event_type: &str,
        error_code: Option<&str>,
    ) -> Result<()> {
        let category = match event_type {
            "profile_applied" | "profile_rejected" => "profile_application",
            "override_changed" => "override_state",
            "sync_failed" => "failure_code",
            _ => return Ok(()),
        };
        if !telemetry_enabled(profile, category) {
            return Ok(());
        }
        let event = ComplianceEvent {
            schema: 1,
            event_id: Uuid::new_v4(),
            release_id: profile.release_id,
            session_id: None,
            policy_context: None,
            hook: None,

            event_type: event_type.to_owned(),
            occurred_at: Utc::now(),
            duration_bucket: None,
            decision: None,
            error_code: error_code.map(str::to_owned),
            mimir_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        self.send_events(enrollment, &[event]).await
    }

    async fn report_profile_rejected(&self, enrollment: &Enrollment, error_code: &str) {
        if let Ok(active) = self.load_profile() {
            let _ = self
                .report_event(enrollment, &active, "profile_rejected", Some(error_code))
                .await;
        }
    }

    /// Reports only bounded environment readiness; local repository content stays local.
    pub async fn report_environment(&self, report: Value) -> Result<()> {
        let enrollment = self.load_enrollment()?;
        let profile = self.load_profile()?;
        if report["release_id"] != json!(profile.release_id) {
            return Err(MimirError::Configuration(
                "Prepared environment release changed; retry prepare".into(),
            ));
        }
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let response = self
            .client
            .put(
                endpoint
                    .join("v1/harness/environment")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .json(&report)
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            return Err(server_error("environment report", response).await);
        }
        Ok(())
    }

    async fn send_events(&self, enrollment: &Enrollment, events: &[ComplianceEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let response = self
            .client
            .post(
                endpoint
                    .join("v1/harness/events")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .json(events)
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            return Err(server_error("event report", response).await);
        }
        Ok(())
    }

    async fn send_tool_inventory(
        &self,
        enrollment: &Enrollment,
        profile: &CompiledHarnessProfile,
        fingerprint: &str,
        tools: &[ToolInventoryItem],
        process_enabled: bool,
        allowed_programs: &[String],
    ) -> Result<()> {
        validate_tool_inventory_bounds(fingerprint, tools.len(), allowed_programs.len())?;
        let endpoint = validate_endpoint(&enrollment.endpoint)?;
        let response = self
            .client
            .put(
                endpoint
                    .join("v1/harness/tool-inventory")
                    .map_err(configuration_error)?,
            )
            .bearer_auth(&enrollment.installation_token)
            .json(&ToolInventoryReport {
                schema: 1,
                release_id: profile.release_id,
                mimir_version: env!("CARGO_PKG_VERSION"),
                fingerprint,
                tools,
                local_process: LocalProcessInventory {
                    enabled: process_enabled,
                    allowed_programs: allowed_programs.to_vec(),
                },
            })
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            return Err(server_error("tool inventory", response).await);
        }
        Ok(())
    }

    fn report_events_best_effort(&self, enrollment: Enrollment, events: Vec<ComplianceEvent>) {
        if events.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            let _ = manager.send_events(&enrollment, &events).await;
        });
    }
}

fn validate_tool_inventory_bounds(
    fingerprint: &str,
    tool_count: usize,
    allowed_program_count: usize,
) -> Result<()> {
    if fingerprint.len() != 64
        || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        || tool_count > MAX_TOOL_INVENTORY_ITEMS
        || allowed_program_count > MAX_INVENTORY_ALLOWED_PROGRAMS
    {
        return Err(MimirError::Configuration(
            "tool inventory exceeds its metadata bounds".into(),
        ));
    }
    Ok(())
}

/// Reports only bounded tool metadata. Delivery never changes local policy or
/// prevents the runtime from starting.
pub fn report_tool_inventory_best_effort(
    fingerprint: &str,
    tools: &[ToolInventoryItem],
    process_enabled: bool,
    allowed_programs: &[String],
) {
    if !is_enrolled().unwrap_or(false) {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let fingerprint = fingerprint.to_owned();
    let tools = tools.to_vec();
    let allowed_programs = allowed_programs.to_vec();
    runtime.spawn(async move {
        let result = async {
            let manager = EnterpriseManager::global()?;
            let enrollment = manager.load_enrollment()?;
            let profile = manager.load_profile()?;
            enforce_expiry(&profile)?;
            manager
                .send_tool_inventory(
                    &enrollment,
                    &profile,
                    &fingerprint,
                    &tools,
                    process_enabled,
                    &allowed_programs,
                )
                .await
        }
        .await;
        if let Err(error) = result {
            eprintln!("warning: enterprise tool inventory report failed: {error}");
        }
    });
}

/// Synchronizes an enrollment at startup, then verifies offline-expiry policy.
pub async fn prepare_runtime(offline: bool) -> Result<Option<CompiledHarnessProfile>> {
    let enrolled = is_enrolled()?;
    let identity = if enrolled {
        Some(ConnectionIdentity::from(
            &EnterpriseManager::global()?.load_enrollment()?,
        ))
    } else {
        None
    };
    let _ = SESSION_CONNECTION.set(identity);
    if !enrolled {
        return Ok(None);
    }
    let manager = EnterpriseManager::global()?;
    if !offline && let Err(error) = manager.sync_with_backoff().await {
        if error
            .to_string()
            .contains("enterprise installation credential was revoked")
        {
            return Err(error);
        }
        eprintln!(
            "warning: enterprise profile sync failed; using last known good profile: {error}"
        );
    }
    let profile = manager.load_runtime_profile()?;
    Ok(Some(profile))
}
pub(crate) fn session_installation_id() -> Result<Option<Uuid>> {
    if !is_enrolled()? {
        return Ok(None);
    }
    Ok(Some(
        EnterpriseManager::global()?
            .load_enrollment()?
            .installation_id,
    ))
}

/// Keeps the signed cache fresh for long-running sessions. Runtime tool gates read the
/// active cache on every invocation, while provider/model bounds apply on the next run.
pub fn spawn_periodic_sync() {
    if !is_enrolled().unwrap_or(false) {
        return;
    }
    let Ok(manager) = EnterpriseManager::global() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            let jitter = Duration::from_secs(u64::from(Uuid::new_v4().as_bytes()[0]) / 2);
            tokio::time::sleep(SYNC_INTERVAL + jitter).await;
            if let Err(error) = manager.sync_with_backoff().await {
                eprintln!("warning: periodic enterprise sync failed: {error}");
            }
        }
    });
}

/// Returns the current verified enrolled release. Standalone execution has no policy dependency.
pub(crate) fn policy_release() -> Result<Option<CompiledHarnessProfile>> {
    if !is_enrolled()? {
        return Ok(None);
    }
    let manager = EnterpriseManager::global()?;
    let mut profile = manager.load_runtime_profile()?;
    let overrides = manager.load_overrides()?;
    profile.instances.retain(|instance| {
        instance.requirement == "required"
            || !overrides
                .overrides
                .iter()
                .any(|item| item.binding_key == instance.id && item.disabled)
    });
    Ok(Some(profile))
}
pub(crate) fn policy_enrollment() -> Result<Option<Enrollment>> {
    if !is_enrolled()? {
        return Ok(None);
    }
    Ok(Some(EnterpriseManager::global()?.load_enrollment()?))
}
pub(crate) fn policy_envelope() -> Result<SignedHarnessProfileEnvelope> {
    let manager = EnterpriseManager::global()?;
    if manager.load_profile().is_ok() {
        return Ok(serde_json::from_slice(&fs::read(
            enterprise_state_dir()?.join(PROFILE_FILE),
        )?)?);
    }
    let enrollment = manager.load_enrollment()?;
    crate::policy::captured_envelope(&runtime_workspace()?, &enrollment)?
        .ok_or_else(|| MimirError::Configuration("Verified policy release unavailable".into()))
}
pub(crate) fn policy_primary_cache_verified() -> bool {
    EnterpriseManager::global()
        .and_then(|manager| manager.load_profile())
        .is_ok()
}

/// Final native per-tool gate. It is independent of extensions and CLI disable switches.
pub fn tool_allowed(tool: &str) -> Result<bool> {
    if tool == "finish_task" {
        return Ok(true);
    }
    let normalized = local_bridge_policy_alias(tool);
    Ok(allowed_values("restrict_tools", "allowed")?
        .is_none_or(|values| values.contains(&normalized)))
}

/// Content-free visibility check used while constructing provider tool lists.
/// Unlike the invocation gate this does not emit a policy-decision event.
pub fn tool_visible(tool: &str) -> Result<bool> {
    if tool == "finish_task" {
        return Ok(true);
    }
    Ok(allowed_values("restrict_tools", "allowed")?
        .is_none_or(|allowed| allowed.contains(&local_bridge_policy_alias(tool))))
}

pub fn model_allowed(provider: &str, model: &str) -> Result<bool> {
    let selector = format!("{provider}/{model}");
    Ok(allowed_values("restrict_models", "allowed")?
        .is_none_or(|allowed| allowed.contains(&selector) || allowed.contains(model)))
}

/// Final native shell-program gate. The attempted command and its arguments never leave Mimir.
pub fn shell_program_allowed(program: &str) -> Result<bool> {
    Ok(allowed_values("restrict_shell_programs", "allowed")?
        .is_none_or(|values| values.contains(program)))
}

/// Reads bounded reviewed artifacts from the active signed profile.
///
/// # Errors
/// Returns an error for invalid or expired enrollment state.
pub fn registry_artifacts() -> Result<Vec<crate::registry::RegistryArtifact>> {
    if !is_enrolled()? {
        return Ok(Vec::new());
    }
    let profile = EnterpriseManager::global()?.load_runtime_profile()?;
    if enforce_expiry(&profile).is_err() {
        return Ok(Vec::new());
    }
    Ok(profile.registry_artifacts)
}

pub fn managed_skills() -> Result<Vec<Skill>> {
    managed_skills_inner(None)
}

/// Renders only approved factual parameters from local repository evidence.
///
/// # Errors
/// Returns an error for invalid signed content or unavailable factual templates.
pub fn managed_skills_for(workspace: &Path) -> Result<Vec<Skill>> {
    managed_skills_inner(Some(workspace))
}
fn managed_skills_inner(workspace: Option<&Path>) -> Result<Vec<Skill>> {
    if !is_enrolled()? {
        return Ok(Vec::new());
    }
    let manager = EnterpriseManager::global()?;
    let profile = manager.load_runtime_profile()?;
    if enforce_expiry(&profile).is_err() {
        return Ok(Vec::new());
    }
    profile
        .skills
        .into_iter()
        .map(|skill| {
            let actual = format!("{:x}", Sha256::digest(skill.instructions.as_bytes()));
            if actual != skill.content_sha256 {
                return Err(MimirError::Configuration(format!(
                    "managed skill {} failed content digest validation",
                    skill.id
                )));
            }
            let instructions =
                if let (Some(template), Some(workspace)) = (&skill.template_id, workspace) {
                    crate::working_environment::render_reviewed_skill(
                        template,
                        &skill.instructions,
                        &crate::working_environment::repository_facts(workspace)?,
                    )?
                } else {
                    skill.instructions
                };
            Ok(Skill::in_memory(
                skill.id.clone(),
                skill.description,
                instructions,
                PathBuf::from(format!("enterprise://{}@{}", skill.id, skill.version)),
            ))
        })
        .collect()
}

pub fn allowed_values(action: &str, _parameter: &str) -> Result<Option<BTreeSet<String>>> {
    let Some(profile) = policy_release()? else {
        return Ok(None);
    };
    let values = match action {
        "restrict_tools" => profile
            .native_constraints
            .allowed_tools
            .get("mimir")
            .cloned(),
        "restrict_shell_programs" => profile
            .native_constraints
            .allowed_shell_programs
            .get("mimir")
            .cloned(),
        "restrict_models" => profile
            .native_constraints
            .approved_models
            .get("mimir")
            .cloned(),
        _ => None,
    };
    if values.is_some() {
        enforce_expiry(&profile)?;
    }
    Ok(values.map(|values| values.into_iter().collect()))
}

fn telemetry_enabled(profile: &CompiledHarnessProfile, category: &str) -> bool {
    profile.telemetry.iter().any(|value| value == category)
}

fn policy_result_events(
    profile: &CompiledHarnessProfile,
    enrollment: &Enrollment,
    event: &Value,
    result: &Value,
) -> Vec<ComplianceEvent> {
    let decisions = telemetry_enabled(profile, "policy_decision");
    let failures = telemetry_enabled(profile, "failure_code");
    result["results"].as_array().into_iter().flatten().filter_map(|result| {
        if !matches!(result["status"].as_str(), Some("evaluated" | "failed")) { return None; }
        let instance = profile.instances.iter().find(|instance| result["instance_id"] == instance.id)?;
        let failed = result["status"] == "failed";
        let denied = result["outcome"] == "deny";
        if !decisions && !(failures && (failed || denied)) { return None; }
        let reason = result["reason_code"].as_str()?;
        if reason.len() > 80 || !reason.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte,b'_'|b'.'|b'-')) { return None; }
        let context = decisions.then(|| {
            let mut context = json!({"instance_id":instance.id,"definition_id":instance.definition.id,
                "definition_version":instance.definition.version,"definition_digest":instance.definition.sha256,
                "mode":instance.mode,"reason_code":reason});
            if failed { context["error_code"] = json!(reason); }
            context
        });
        Some(ComplianceEvent { schema:1,event_id:Uuid::new_v4(),release_id:profile.release_id,
            session_id:session_alias(enrollment,event["session_id"].as_str().unwrap_or_default()),policy_context:context,
            hook:event["event"].as_str().map(str::to_owned),event_type:if failed {"policy_evaluation_failed"} else {
                match result["outcome"].as_str() {Some("deny")=>"policy_blocked",Some("guidance")=>"policy_guidance",_=>"policy_allowed"}
            }.into(),occurred_at:Utc::now(),duration_bucket:None,decision:decisions.then(||result["outcome"].as_str().unwrap_or("allow").to_owned()),
            error_code:(failures && (failed || denied)).then(||reason.to_owned()),mimir_version:env!("CARGO_PKG_VERSION").into() })
    }).collect()
}
pub(crate) fn report_policy_results(
    profile: &CompiledHarnessProfile,
    enrollment: &Enrollment,
    event: &Value,
    result: &Value,
) {
    let events = policy_result_events(profile, enrollment, event, result);
    if let Ok(manager) = EnterpriseManager::global() {
        manager.report_events_best_effort(enrollment.clone(), events);
    }
}

pub fn maximum_thinking_level() -> Result<Option<ThinkingLevel>> {
    let Some(profile) = policy_release()? else {
        return Ok(None);
    };
    if profile
        .native_constraints
        .maximum_reasoning_level
        .contains_key("mimir")
    {
        enforce_expiry(&profile)?;
    }
    profile
        .native_constraints
        .maximum_reasoning_level
        .get("mimir")
        .map(|value| parse_thinking_level(value))
        .transpose()
}

/// Applies content-free managed observability bindings for one lifecycle event.
/// Delivery is best effort and cannot change the runtime outcome.
pub fn observe_lifecycle(
    hook: &str,
    failed: bool,
    duration_bucket: Option<&str>,
    session_id: &str,
) -> Result<ManagedLifecycleEffect> {
    if !is_enrolled()? {
        return Ok(ManagedLifecycleEffect::default());
    }
    let manager = EnterpriseManager::global()?;
    let enrollment = manager.load_enrollment()?;
    let profile = manager.load_runtime_profile()?;
    enforce_expiry(&profile)?;
    let overrides = manager.load_overrides()?;
    let (effect, events) = evaluate_lifecycle(
        &profile,
        &overrides,
        hook,
        failed,
        duration_bucket,
        session_alias(&enrollment, session_id),
    );
    manager.report_events_best_effort(enrollment, events);
    Ok(effect)
}

fn evaluate_lifecycle(
    profile: &CompiledHarnessProfile,
    _overrides: &OverrideStore,
    hook: &str,
    failed: bool,
    duration_bucket: Option<&str>,
    session_id: Option<String>,
) -> (ManagedLifecycleEffect, Vec<ComplianceEvent>) {
    let mut events = Vec::new();
    let effect = ManagedLifecycleEffect {
        notify_failure: failed && telemetry_enabled(profile, "failure_code"),
    };
    let make_event =
        |event_type: &str, duration: Option<&str>, code: Option<&str>| ComplianceEvent {
            schema: 1,
            event_id: Uuid::new_v4(),
            release_id: profile.release_id,
            session_id: session_id.clone(),
            policy_context: None,
            hook: Some(hook.to_owned()),
            event_type: event_type.into(),
            occurred_at: Utc::now(),
            duration_bucket: duration.map(str::to_owned),
            decision: None,
            error_code: code.map(str::to_owned),
            mimir_version: env!("CARGO_PKG_VERSION").into(),
        };
    if matches!(hook, "session_start" | "session_shutdown")
        && telemetry_enabled(profile, "session_activity")
    {
        events.push(make_event("lifecycle_observed", None, None));
    }
    if duration_bucket.is_some() && telemetry_enabled(profile, "duration_bucket") {
        events.push(make_event("duration_observed", duration_bucket, None));
    }
    if effect.notify_failure {
        events.push(make_event(
            "failure_observed",
            None,
            Some("managed_lifecycle_failure"),
        ));
    }
    (effect, events)
}

fn verify_envelope(
    envelope: &SignedHarnessProfileEnvelope,
    public_key_base64: &str,
) -> Result<CompiledHarnessProfile> {
    if envelope.schema != 2 {
        return Err(MimirError::Configuration(
            "unsupported signed profile schema".into(),
        ));
    }
    let payload = BASE64.decode(&envelope.payload_base64).map_err(|_| {
        MimirError::Configuration("signed profile payload is not valid base64".into())
    })?;
    if payload.len() > MAX_PROFILE_BYTES {
        return Err(MimirError::Configuration(
            "compiled profile exceeds size limit".into(),
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&payload));
    if digest != envelope.sha256 {
        return Err(MimirError::Configuration(
            "signed profile digest mismatch".into(),
        ));
    }
    let public_key = BASE64
        .decode(public_key_base64)
        .map_err(|_| MimirError::Configuration("enterprise signing key is invalid".into()))?;
    let signature = BASE64
        .decode(&envelope.signature)
        .map_err(|_| MimirError::Configuration("profile signature is invalid base64".into()))?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&payload, &signature)
        .map_err(|_| MimirError::Configuration("profile signature verification failed".into()))?;
    Ok(serde_json::from_slice(&payload)?)
}

pub(crate) fn validate_profile(profile: &CompiledHarnessProfile) -> Result<()> {
    if profile.schema != 4 || profile.source_metadata.get("bindings").is_some() {
        return Err(MimirError::Configuration(
            "unsupported compiled rules release schema".into(),
        ));
    }
    if let Some(minimum) = profile.minimum_runtime_versions.get("mimir")
        && compare_versions(env!("CARGO_PKG_VERSION"), minimum)? < 0
    {
        return Err(MimirError::Configuration(format!(
            "release requires Mimir {minimum} or newer"
        )));
    }
    crate::policy::validate_release(profile)?;
    crate::registry::validate(&profile.registry_artifacts)?;
    if let Some(environment) = &profile.environment {
        crate::working_environment::validate(environment)?;
    }
    let mut skills = BTreeSet::new();
    for skill in &profile.skills {
        if !skills.insert(&skill.id)
            || skill.instructions.len() > 96 * 1024
            || skill.content_sha256
                != format!("{:x}", Sha256::digest(skill.instructions.as_bytes()))
        {
            return Err(MimirError::Configuration(
                "Invalid or duplicate managed skill".into(),
            ));
        }
    }
    Ok(())
}

fn enforce_expiry(profile: &CompiledHarnessProfile) -> Result<()> {
    if profile.expires_at <= Utc::now()
        || profile.created_at > Utc::now() + chrono::Duration::minutes(5)
    {
        return Err(MimirError::Configuration(
            "Signed rules release is expired or not yet valid; sync before continuing".into(),
        ));
    }
    Ok(())
}

fn validate_enrollment_scope(
    profile: &CompiledHarnessProfile,
    enrollment: &Enrollment,
) -> Result<()> {
    if profile.organization_id != enrollment.organization_id
        || profile.team_id != Some(enrollment.team_id)
    {
        return Err(MimirError::Configuration(
            "Signed profile differs from enrollment scope".into(),
        ));
    }
    Ok(())
}

fn status_json(enrollment: &Enrollment, profile: &CompiledHarnessProfile, updated: bool) -> Value {
    json!({
        "schema": 1,
        "enrolled": true,
        "installation_id": enrollment.installation_id,
        "organization_id": enrollment.organization_id,
        "team_id": enrollment.team_id,
        "profile_id": enrollment.profile_id,
        "profile_name": profile.profile_name,
        "release_id": profile.release_id,
        "channel": enrollment.channel,
        "expires_at": profile.expires_at,
        "expired": profile.expires_at < Utc::now(),
        "updated": updated,
    })
}

fn validate_endpoint(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).map_err(configuration_error)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MimirError::Configuration(
            "Enterprise endpoint must exclude credentials, query and fragment".into(),
        ));
    }
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err(MimirError::Configuration(
            "enterprise control-plane URL must use HTTPS (HTTP is allowed only for localhost)"
                .into(),
        ));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

pub(crate) fn enterprise_state_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MIMIR_ENTERPRISE_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Mimir").join("enterprise"));
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join("Library/Application Support/Mimir/enterprise"));
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|path| path.join("mimir/enterprise"));
    root.ok_or_else(|| {
        MimirError::Configuration("cannot resolve global enterprise state directory".into())
    })
}

fn is_enrolled() -> Result<bool> {
    #[cfg(test)]
    if std::env::var_os("MIMIR_ENTERPRISE_STATE_DIR").is_none() {
        return Ok(false);
    }
    let enrolled = enterprise_state_dir()?.join(CONFIG_FILE).is_file();
    if let Some(expected) = SESSION_CONNECTION.get()
        && expected.is_some() != enrolled
    {
        return Err(MimirError::Configuration(
            "Enterprise connection changed; start a fresh session".into(),
        ));
    }
    Ok(enrolled)
}

fn ensure_owner_only_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_secure_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| MimirError::Configuration("enterprise state path has no parent".into()))?;
    ensure_owner_only_dir(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        Uuid::new_v4().simple()
    ));
    let bytes = serde_json::to_vec(value)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    set_owner_only(path)
}

fn set_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) fn compare_versions(left: &str, right: &str) -> Result<i8> {
    fn parse(value: &str) -> Result<[u64; 3]> {
        let core = value.split_once('-').map_or(value, |(core, _)| core);
        let parts = core
            .split('.')
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| MimirError::Configuration(format!("invalid semantic version: {value}")))?;
        if parts.len() != 3 {
            return Err(MimirError::Configuration(format!(
                "invalid semantic version: {value}"
            )));
        }
        Ok([parts[0], parts[1], parts[2]])
    }
    Ok(parse(left)?.cmp(&parse(right)?) as i8)
}

fn parse_thinking_level(value: &str) -> Result<ThinkingLevel> {
    match value {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(MimirError::Configuration(format!(
            "unknown managed thinking level: {value}"
        ))),
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn configuration_error(error: impl std::fmt::Display) -> MimirError {
    MimirError::Configuration(format!("enterprise configuration: {error}"))
}

fn http_error(error: impl std::fmt::Display) -> MimirError {
    MimirError::Configuration(format!("enterprise control-plane request failed: {error}"))
}

async fn server_error(operation: &str, response: reqwest::Response) -> MimirError {
    let status = response.status();
    let message = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "request rejected".into());
    MimirError::Configuration(format!(
        "enterprise {operation} failed ({status}): {message}"
    ))
}

/// Returns only verified configuration; installation credentials never cross this boundary.
pub fn working_environment() -> Result<Option<crate::working_environment::WorkingEnvironment>> {
    if !is_enrolled()? {
        return Ok(None);
    }
    let profile = EnterpriseManager::global()?.load_runtime_profile()?;
    if enforce_expiry(&profile).is_err() {
        return Ok(None);
    }
    Ok(Some(
        profile
            .environment
            .unwrap_or_else(crate::working_environment::defaults),
    ))
}

pub(crate) fn local_bridge_policy_alias(tool: &str) -> String {
    for name in [
        "environment_status",
        "repository_context",
        "read_repository_guidance",
        "list_agents",
        "activate_agent",
        "list_skills",
        "activate_skill",
    ] {
        if tool == crate::tools::managed_mcp_tool_name("betterloop", name) {
            return "search_skills".into();
        }
    }
    tool.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{NativeConstraints, PolicyArtifact, PolicyInstance, PolicyReference};
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };

    fn instance(id: &str, requirement: &str) -> PolicyInstance {
        PolicyInstance {
            id: id.into(),
            definition: PolicyReference {
                id: "example.policy".into(),
                version: "1.0.0".into(),
                sha256: "a".repeat(64),
            },
            parameters: json!({}),
            target_agents: vec!["mimir".into()],
            requirement: requirement.into(),
            mode: "enforce".into(),
            on_error: "deny".into(),
            order: 100,
        }
    }
    fn profile_with(instances: Vec<PolicyInstance>, telemetry: &[&str]) -> CompiledHarnessProfile {
        let program = "var __betterloopPolicy = { evaluate() { return {outcome:'allow',reason_code:'allowed'}; } };";
        let policies = if instances.is_empty() {
            Vec::new()
        } else {
            vec![PolicyArtifact {
                manifest: json!({"schema":1,"id":"example.policy","version":"1.0.0","description":"Reviewed example","sdk_version":"1.0.0","match":{"events":["tool_call"]},"parameters":{"type":"object","additionalProperties":false},"requires":[],"effects":["allow","deny"],"reason_codes":["allowed","blocked"]}),
                program: program.into(),
                sha256: format!("{:x}", Sha256::digest(program.as_bytes())),
                definition_sha256: "a".repeat(64),
            }]
        };
        CompiledHarnessProfile {
            schema: 4,
            environment: None,
            registry_artifacts: Vec::new(),
            release_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            team_id: Some(Uuid::new_v4()),
            project_id: None,
            profile_name: "Test".into(),
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            minimum_setup_version: "1.0.0".into(),
            minimum_runtime_versions: BTreeMap::from([(
                "mimir".into(),
                env!("CARGO_PKG_VERSION").into(),
            )]),
            policies,
            instances,
            native_constraints: NativeConstraints::default(),
            skills: Vec::new(),
            telemetry: telemetry.iter().map(ToString::to_string).collect(),
            revoked_catalog_items: Vec::new(),
            source_metadata: json!({}),
        }
    }
    fn enrollment(endpoint: String, profile: &CompiledHarnessProfile) -> Enrollment {
        Enrollment {
            schema: 1,
            endpoint,
            installation_id: Uuid::new_v4(),
            installation_token: format!("bli_{}", Uuid::new_v4().simple()),
            organization_id: profile.organization_id,
            team_id: profile.team_id.expect("team"),
            profile_id: Uuid::new_v4(),
            channel: "canary".into(),
            signing_public_key_base64: String::new(),
            etag: None,
        }
    }
    fn signing_key() -> Ed25519KeyPair {
        Ed25519KeyPair::from_pkcs8(
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .expect("key document")
                .as_ref(),
        )
        .expect("key")
    }
    fn signed(
        profile: &CompiledHarnessProfile,
        key: &Ed25519KeyPair,
    ) -> SignedHarnessProfileEnvelope {
        let payload = serde_json::to_vec(profile).expect("profile");
        SignedHarnessProfileEnvelope {
            schema: 2,
            payload_base64: BASE64.encode(&payload),
            sha256: format!("{:x}", Sha256::digest(&payload)),
            key_id: Uuid::new_v4(),
            signature: BASE64.encode(key.sign(&payload).as_ref()),
        }
    }
    fn installed(manager: &EnterpriseManager, profile: &CompiledHarnessProfile) -> Enrollment {
        let key = signing_key();
        let mut connection = enrollment("http://127.0.0.1:1".into(), profile);
        connection.signing_public_key_base64 = BASE64.encode(key.public_key().as_ref());
        write_secure_json(&manager.root.join(CONFIG_FILE), &connection).expect("enrollment");
        write_secure_json(&manager.root.join(PROFILE_FILE), &signed(profile, &key))
            .expect("release");
        connection
    }
    fn verified_broker(root: &Path) -> Value {
        let root = fs::canonicalize(root).expect("root");
        let executable =
            fs::canonicalize(std::env::current_exe().expect("exe")).expect("absolute exe");
        let runner = root.join("policy-child.mjs");
        fs::write(&runner, "// verified test runner asset").expect("asset");
        let manifest = serde_json::to_vec(&json!({"schema":1,"files":[{"path":"policy-child.mjs","sha256":format!("{:x}",Sha256::digest(fs::read(&runner).expect("runner bytes")))}]})).expect("manifest");
        fs::write(root.join("runtime-manifest.json"), &manifest).expect("manifest file");
        json!({"executable":executable,"runner":runner,"policy_runtime":{"schema":1,"executable":executable,"executable_sha256":format!("{:x}",Sha256::digest(fs::read(&executable).expect("exe bytes"))),"node_version":"22.0.0","runner":runner,"manifest_sha256":format!("{:x}",Sha256::digest(&manifest))},"policy_activation":root.join("policy-activation.json"),"policy_activation_root":root.join("policy-activations")})
    }

    #[test]
    fn unapproved_repository_retains_required_prevention_and_removes_optional_resources() {
        let mut profile = profile_with(
            vec![
                instance("required.policy", "required"),
                instance("optional.policy", "optional"),
            ],
            &["session_activity"],
        );
        let mut environment = crate::working_environment::defaults();
        environment.repository_reporting = Some("allow_and_report".into());
        environment.unapproved_repository_behavior = Some("report_only".into());
        profile.environment = Some(environment);
        profile.source_metadata["knowledge_map"] =
            json!({"schema":1,"references":[],"projects":[]});
        let restricted = project_repository_policy(profile.clone(), false);
        assert_eq!(restricted.instances.len(), 1);
        assert_eq!(restricted.instances[0].id, "required.policy");
        assert!(restricted.skills.is_empty());
        assert!(restricted.telemetry.is_empty());
        assert!(
            restricted
                .environment
                .as_ref()
                .expect("environment")
                .skills
                .is_empty()
        );
        assert!(restricted.source_metadata.get("knowledge_map").is_none());
        assert_eq!(project_repository_policy(profile, true).instances.len(), 2);
    }
    #[test]
    fn enrollment_scope_rejects_other_team_or_organization_before_activation() {
        let mut profile = profile_with(Vec::new(), &[]);
        let connection = enrollment("http://127.0.0.1".into(), &profile);
        assert!(validate_enrollment_scope(&profile, &connection).is_ok());
        profile.team_id = Some(Uuid::new_v4());
        assert!(validate_enrollment_scope(&profile, &connection).is_err());
        profile.team_id = Some(connection.team_id);
        profile.organization_id = Uuid::new_v4();
        assert!(validate_enrollment_scope(&profile, &connection).is_err());
    }
    #[test]
    fn verifies_exact_new_format_signature_and_rejects_tampering_and_old_envelopes() {
        let profile = profile_with(Vec::new(), &["profile_application"]);
        let key = signing_key();
        let public = BASE64.encode(key.public_key().as_ref());
        let envelope = signed(&profile, &key);
        assert_eq!(
            verify_envelope(&envelope, &public)
                .expect("verified")
                .release_id,
            profile.release_id
        );
        let mut tampered = envelope.clone();
        tampered.sha256 = "0".repeat(64);
        assert!(verify_envelope(&tampered, &public).is_err());
        let mut altered = envelope.clone();
        altered.payload_base64 = BASE64.encode(b"{}");
        altered.sha256 = format!("{:x}", Sha256::digest(b"{}"));
        assert!(verify_envelope(&altered, &public).is_err());
        let mut old = envelope;
        old.schema = 1;
        assert!(verify_envelope(&old, &public).is_err());
    }
    #[tokio::test]
    async fn local_overrides_cannot_disable_required_policies_or_write_override_state() {
        let root = tempfile::tempdir().expect("root");
        let manager = EnterpriseManager::at(root.path().join("enterprise")).expect("manager");
        installed(
            &manager,
            &profile_with(vec![instance("policy.tools", "required")], &[]),
        );
        assert!(
            manager
                .set_override("policy.tools", true, "test")
                .await
                .is_err()
        );
        assert!(!manager.root.join(OVERRIDES_FILE).exists());
    }
    #[test]
    fn device_attachment_checks_scope_before_writing_and_preserves_enrollment_identity() {
        let root = tempfile::tempdir().expect("root");
        let manager = EnterpriseManager::at(root.path().join("enterprise")).expect("manager");
        let profile = profile_with(Vec::new(), &[]);
        let key = signing_key();
        let installation = Uuid::new_v4();
        let assets = tempfile::tempdir().expect("assets");
        let mut input = json!({"endpoint":"http://127.0.0.1:8088","registration":{"schema":1,"installation_id":installation,"installation_token":format!("bli_{}",Uuid::new_v4().simple()),"organization_id":profile.organization_id,"team_id":profile.team_id,"profile_id":Uuid::new_v4(),"channel":"stable","signing_public_key_base64":BASE64.encode(key.public_key().as_ref()),"profile":signed(&profile,&key)},"broker":verified_broker(assets.path())});
        input["registration"]["team_id"] = json!(Uuid::new_v4());
        assert!(manager.attach_device(&input).is_err());
        assert!(!manager.root.join(CONFIG_FILE).exists());
        input["registration"]["team_id"] = json!(profile.team_id);
        assert_eq!(
            manager.attach_device(&input).expect("attach")["installation_id"],
            json!(installation)
        );
        let original = manager.load_enrollment().expect("enrollment");
        let mut changed = original.clone();
        changed.team_id = Uuid::new_v4();
        assert!(ConnectionIdentity::from(&original) != ConnectionIdentity::from(&changed));
        changed = original.clone();
        changed.installation_token.push('x');
        assert!(ConnectionIdentity::from(&original) != ConnectionIdentity::from(&changed));
        write_secure_json(
            &manager.root.join(REVOKED_FILE),
            &json!({"installation_id":installation}),
        )
        .expect("marker");
        assert!(manager.load_profile().is_err());
        assert_eq!(manager.status().expect("status")["revoked"], json!(true));
        input["registration"]["installation_id"] = json!(Uuid::new_v4());
        assert!(manager.attach_device(&input).is_err());
        assert_eq!(
            manager.load_enrollment().expect("retained").installation_id,
            installation
        );
    }
    #[test]
    fn attachment_rejects_tampered_assets_before_enrollment() {
        let root = tempfile::tempdir().expect("root");
        let assets = tempfile::tempdir().expect("assets");
        let manager = EnterpriseManager::at(root.path().join("enterprise")).expect("manager");
        let profile = profile_with(Vec::new(), &[]);
        let key = signing_key();
        let broker = verified_broker(assets.path());
        fs::write(
            broker["policy_runtime"]["runner"].as_str().expect("runner"),
            "changed",
        )
        .expect("tamper");
        let input = json!({"endpoint":"http://127.0.0.1:8088","registration":{"schema":1,"installation_id":Uuid::new_v4(),"installation_token":format!("bli_{}",Uuid::new_v4().simple()),"organization_id":profile.organization_id,"team_id":profile.team_id,"profile_id":Uuid::new_v4(),"channel":"stable","signing_public_key_base64":BASE64.encode(key.public_key().as_ref()),"profile":signed(&profile,&key)},"broker":broker});
        assert!(manager.attach_device(&input).is_err());
        assert!(!manager.root.join(CONFIG_FILE).exists());
    }
    #[test]
    fn profile_validation_rejects_old_rules_unpinned_instances_and_program_tampering() {
        let mut profile = profile_with(vec![instance("policy.tools", "required")], &[]);
        assert!(validate_profile(&profile).is_ok());
        profile.schema = 3;
        assert!(validate_profile(&profile).is_err());
        profile.schema = 4;
        profile.source_metadata["bindings"] = json!([]);
        assert!(validate_profile(&profile).is_err());
        profile
            .source_metadata
            .as_object_mut()
            .expect("metadata")
            .remove("bindings");
        profile.instances[0].definition.sha256 = "b".repeat(64);
        assert!(validate_profile(&profile).is_err());
        profile.instances[0].definition.sha256 = "a".repeat(64);
        profile.policies[0].program.push(' ');
        assert!(validate_profile(&profile).is_err());
    }
    #[test]
    fn native_constraints_are_scoped_by_agent_and_accept_dormant_tool_identifiers() {
        let mut profile = profile_with(Vec::new(), &[]);
        profile.native_constraints.allowed_tools.insert(
            "mimir".into(),
            vec!["read_file".into(), "future_unavailable_tool".into()],
        );
        profile
            .native_constraints
            .allowed_tools
            .insert("claude-code".into(), vec!["different_tool".into()]);
        profile
            .native_constraints
            .allowed_shell_programs
            .insert("mimir".into(), vec!["git".into()]);
        profile
            .native_constraints
            .approved_models
            .insert("mimir".into(), vec!["provider/model".into()]);
        profile
            .native_constraints
            .maximum_reasoning_level
            .insert("mimir".into(), "medium".into());
        assert!(validate_profile(&profile).is_ok());
        assert_eq!(
            profile.native_constraints.allowed_tools["mimir"],
            vec!["read_file", "future_unavailable_tool"]
        );
        profile
            .native_constraints
            .maximum_reasoning_level
            .insert("mimir".into(), "unbounded".into());
        assert!(validate_profile(&profile).is_err());
    }
    #[test]
    fn minimum_runtime_version_and_expiry_are_verified() {
        let mut profile = profile_with(Vec::new(), &[]);
        assert!(validate_profile(&profile).is_ok());
        assert!(enforce_expiry(&profile).is_ok());
        profile
            .minimum_runtime_versions
            .insert("mimir".into(), "999.0.0".into());
        assert!(validate_profile(&profile).is_err());
        profile
            .minimum_runtime_versions
            .insert("mimir".into(), env!("CARGO_PKG_VERSION").into());
        profile.expires_at = Utc::now() - chrono::Duration::seconds(1);
        assert!(enforce_expiry(&profile).is_err());
        profile.expires_at = Utc::now() + chrono::Duration::hours(1);
        profile.created_at = Utc::now() + chrono::Duration::hours(1);
        assert!(enforce_expiry(&profile).is_err());
    }
    #[test]
    fn lifecycle_observations_keep_duration_and_failure_categories_independent() {
        let profile = profile_with(Vec::new(), &["duration_bucket", "failure_code"]);
        let (effect, events) = evaluate_lifecycle(
            &profile,
            &OverrideStore::default(),
            "agent_end",
            true,
            Some("lt_1s"),
            Some("ses_opaque".into()),
        );
        assert!(effect.notify_failure);
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "duration_observed"
                    && event.duration_bucket.as_deref() == Some("lt_1s"))
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "failure_observed"
                    && event.error_code.as_deref() == Some("managed_lifecycle_failure"))
        );
        let (_, events) = evaluate_lifecycle(
            &profile_with(Vec::new(), &["session_activity"]),
            &OverrideStore::default(),
            "session_start",
            false,
            None,
            Some("ses_opaque".into()),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].hook.as_deref(), Some("session_start"));
    }
    #[test]
    fn lifecycle_privacy_optout_does_not_export_duration_or_failure() {
        let (_, failure_only) = evaluate_lifecycle(
            &profile_with(Vec::new(), &["failure_code"]),
            &OverrideStore::default(),
            "agent_end",
            true,
            Some("lt_1s"),
            None,
        );
        assert_eq!(failure_only.len(), 1);
        assert!(failure_only[0].duration_bucket.is_none());
        let (effect, silent) = evaluate_lifecycle(
            &profile_with(Vec::new(), &[]),
            &OverrideStore::default(),
            "agent_end",
            true,
            Some("lt_1s"),
            None,
        );
        assert!(!effect.notify_failure);
        assert!(silent.is_empty());
    }
    #[test]
    fn session_aliases_are_opaque_installation_scoped_and_never_echo_private_names() {
        let profile = profile_with(Vec::new(), &[]);
        let mut connection = enrollment("http://localhost".into(), &profile);
        let private = "/private/customer/secrets/transcript";
        let alias = session_alias(&connection, private).expect("alias");
        assert!(alias.starts_with("ses_"));
        assert!(!alias.contains("customer"));
        assert_eq!(alias.len(), 68);
        assert_eq!(Some(alias.clone()), session_alias(&connection, private));
        connection.installation_token.push('x');
        assert_ne!(Some(alias), session_alias(&connection, private));
        assert!(session_alias(&connection, " ").is_none());
    }
    #[test]
    fn enterprise_state_files_are_owner_only() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let manager = EnterpriseManager::at(temporary.path().join("enterprise")).expect("manager");
        write_secure_json(
            &manager.root.join(OVERRIDES_FILE),
            &OverrideStore::default(),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(manager.root.join(OVERRIDES_FILE))
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn endpoint_rejects_unencrypted_remote_control_planes() {
        assert!(validate_endpoint("http://example.com").is_err());
        assert!(validate_endpoint("http://localhost:3000").is_ok());
    }
    #[test]
    fn tool_inventory_bounds_match_the_control_plane_contract() {
        let fingerprint = "a".repeat(64);
        assert!(
            validate_tool_inventory_bounds(
                &fingerprint,
                MAX_TOOL_INVENTORY_ITEMS,
                MAX_INVENTORY_ALLOWED_PROGRAMS
            )
            .is_ok()
        );
        assert!(
            validate_tool_inventory_bounds(
                &fingerprint,
                MAX_TOOL_INVENTORY_ITEMS + 1,
                MAX_INVENTORY_ALLOWED_PROGRAMS
            )
            .is_err()
        );
        assert!(
            validate_tool_inventory_bounds(
                &fingerprint,
                MAX_TOOL_INVENTORY_ITEMS,
                MAX_INVENTORY_ALLOWED_PROGRAMS + 1
            )
            .is_err()
        );
        assert!(validate_tool_inventory_bounds(&"z".repeat(64), 0, 0).is_err());
    }
    #[test]
    fn tool_inventory_json_matches_the_frozen_wire_shape() {
        let tools = vec![ToolInventoryItem {
            id: "read_file".into(),
            label: "Read file".into(),
            source: crate::tools::ToolInventorySource {
                kind: "builtin".into(),
                id: "mimir".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            },
            capabilities: vec!["workspace_read".into()],
            risk: "read_only".into(),
            availability: "available".into(),
            availability_reason_code: None,
            required_by_runtime: false,
        }];
        let fingerprint = "a".repeat(64);
        let value = serde_json::to_value(ToolInventoryReport {
            schema: 1,
            release_id: Uuid::nil(),
            mimir_version: env!("CARGO_PKG_VERSION"),
            fingerprint: &fingerprint,
            tools: &tools,
            local_process: LocalProcessInventory {
                enabled: true,
                allowed_programs: vec!["git".into()],
            },
        })
        .expect("inventory");
        assert_eq!(
            value
                .as_object()
                .expect("object")
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "fingerprint",
                "local_process",
                "mimir_version",
                "release_id",
                "schema",
                "tools"
            ])
        );
        assert_eq!(
            value["tools"][0]
                .as_object()
                .expect("tool")
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "availability",
                "capabilities",
                "id",
                "label",
                "required_by_runtime",
                "risk",
                "source"
            ])
        );
        assert_eq!(value["local_process"]["allowed_programs"][0], "git");
    }
    #[tokio::test]
    async fn failed_telemetry_delivery_cannot_change_a_recorded_denial() {
        let profile = profile_with(
            vec![instance("policy.tools", "required")],
            &["policy_decision"],
        );
        let manager =
            EnterpriseManager::at(tempfile::tempdir().expect("tempdir").keep()).expect("manager");
        let connection = enrollment("not a control-plane URL".into(), &profile);
        let result = json!({"outcome":"deny","results":[{"instance_id":"policy.tools","status":"evaluated","outcome":"deny","reason_code":"blocked","message":"Local recovery instructions"}]});
        let events = policy_result_events(
            &profile,
            &connection,
            &json!({"event":"tool_call"}),
            &result,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision.as_deref(), Some("deny"));
        assert!(manager.send_events(&connection, &events).await.is_err());
        assert_eq!(result["outcome"], "deny");
        assert_eq!(events[0].decision.as_deref(), Some("deny"));
    }
    #[test]
    fn policy_decision_records_are_content_free_for_allow_deny_and_guidance() {
        let profile = profile_with(
            vec![instance("policy.tools", "required")],
            &["policy_decision", "failure_code"],
        );
        let connection = enrollment("http://localhost".into(), &profile);
        let private = "/private/customer/secret-file";
        let event = json!({"event":"tool_call","tool":"bash","session_id":private,"tool_input":{"command":private},"tool_result":private});
        for outcome in ["allow", "deny", "guidance"] {
            let result = json!({"results":[{"instance_id":"policy.tools","status":"evaluated","outcome":outcome,"reason_code":"allowed","message":private}]});
            let events = policy_result_events(&profile, &connection, &event, &result);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].decision.as_deref(), Some(outcome));
            assert_eq!(
                events[0].policy_context.as_ref().expect("context")["definition_digest"],
                "a".repeat(64)
            );
            let payload = serde_json::to_string(&events).expect("wire payload");
            assert!(!payload.contains(private));
            assert!(!payload.contains("customer"));
            assert!(!payload.contains("secret-file"));
            assert!(!payload.contains("tool_input"));
            assert!(!payload.contains("tool_result"));
            assert!(!payload.contains("message"));
        }
    }
    #[test]
    fn decision_and_failure_telemetry_are_independent_and_ignore_inactive_results() {
        let event = json!({"event":"tool_call"});
        let denied = json!({"results":[{"instance_id":"policy.tools","status":"evaluated","outcome":"deny","reason_code":"blocked"}]});
        let mut profile = profile_with(
            vec![instance("policy.tools", "required")],
            &["policy_decision"],
        );
        let connection = enrollment("http://localhost".into(), &profile);
        let events = policy_result_events(&profile, &connection, &event, &denied);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision.as_deref(), Some("deny"));
        assert!(events[0].error_code.is_none());
        profile.telemetry = vec!["failure_code".into()];
        let events = policy_result_events(&profile, &connection, &event, &denied);
        assert_eq!(events.len(), 1);
        assert!(events[0].decision.is_none());
        assert!(events[0].policy_context.is_none());
        assert_eq!(events[0].error_code.as_deref(), Some("blocked"));
        let allowed = json!({"results":[{"instance_id":"policy.tools","status":"evaluated","outcome":"allow","reason_code":"allowed"}]});
        assert!(policy_result_events(&profile, &connection, &event, &allowed).is_empty());
        profile.telemetry.clear();
        assert!(policy_result_events(&profile, &connection, &event, &denied).is_empty());
        profile.telemetry = vec!["policy_decision".into()];
        let inactive = json!({"results":[{"instance_id":"policy.tools","status":"inactive","outcome":"allow","reason_code":"not_supported"}]});
        assert!(policy_result_events(&profile, &connection, &event, &inactive).is_empty());
        let unbounded = json!({"results":[{"instance_id":"policy.tools","status":"failed","outcome":"deny","reason_code":"/private/customer/secret"}]});
        assert!(policy_result_events(&profile, &connection, &event, &unbounded).is_empty());
    }
}
