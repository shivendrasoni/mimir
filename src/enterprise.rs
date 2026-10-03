#![allow(
    clippy::missing_errors_doc,
    reason = "enterprise command and native-gate errors are surfaced through the shared MimirError contract"
)]

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
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

const MAX_PROFILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_INVENTORY_ITEMS: usize = 2_048;
const MAX_INVENTORY_ALLOWED_PROGRAMS: usize = 256;
const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CONFIG_FILE: &str = "enrollment.json";
const PROFILE_FILE: &str = "profile.json";
const PREVIOUS_PROFILE_FILE: &str = "profile.previous.json";
const OVERRIDES_FILE: &str = "overrides.json";

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
    pub catalog_version: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub minimum_mimir_version: String,
    pub bindings: Vec<HarnessBinding>,
    pub skills: Vec<ManagedSkill>,
    pub telemetry: Vec<String>,
    #[serde(default)]
    pub revoked_catalog_items: Vec<String>,
    #[serde(flatten)]
    pub source_metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessBinding {
    pub key: String,
    pub hook: String,
    pub action: String,
    pub catalog_version: String,
    pub enforcement: Enforcement,
    pub order: u32,
    #[serde(default)]
    pub critical: bool,
    #[serde(default)]
    pub parameters: Value,
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
    binding_key: Option<String>,
    hook: Option<String>,
    action: Option<String>,
    event_type: String,
    occurred_at: DateTime<Utc>,
    duration_bucket: Option<String>,
    decision: Option<String>,
    error_code: Option<String>,
    mimir_version: String,
}

#[derive(Debug)]
struct PolicyEvaluation {
    allowed: bool,
    events: Vec<ComplianceEvent>,
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
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| {
                    MimirError::Configuration(format!("enterprise HTTP client: {error}"))
                })?,
        })
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
        let profile = self.load_profile()?;
        Ok(status_json(&enrollment, &profile, false))
    }

    pub fn explain(&self) -> Result<Value> {
        let enrollment = self.load_enrollment()?;
        let profile = self.load_profile()?;
        let overrides = self.load_overrides()?;
        Ok(json!({
            "schema": 1,
            "organization_id": enrollment.organization_id,
            "team_id": enrollment.team_id,
            "profile_id": enrollment.profile_id,
            "release_id": profile.release_id,
            "profile_name": profile.profile_name,
            "minimum_mimir_version": profile.minimum_mimir_version,
            "expires_at": profile.expires_at,
            "bindings": profile.bindings.iter().map(|binding| json!({
                "key": binding.key,
                "hook": binding.hook,
                "action": binding.action,
                "enforcement": binding.enforcement,
                "critical": binding.critical,
                "disabled": override_disabled(&overrides, binding),
                "order": binding.order,
                "parameters": binding.parameters,
            })).collect::<Vec<_>>(),
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
            .bindings
            .iter()
            .find(|binding| binding.key == binding_key)
            .ok_or_else(|| {
                MimirError::Configuration(format!("unknown managed binding: {binding_key}"))
            })?;
        if binding.enforcement == Enforcement::Mandatory {
            return Err(MimirError::Configuration(
                "mandatory enterprise bindings cannot be overridden".into(),
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
                "binding_key": binding_key,
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
        Ok(json!({"binding_key": binding_key, "disabled": disabled}))
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
            if !response.status().is_success() {
                return Err(server_error("unenrollment", response).await);
            }
        }
        for name in [
            CONFIG_FILE,
            PROFILE_FILE,
            PREVIOUS_PROFILE_FILE,
            OVERRIDES_FILE,
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

    pub fn load_profile(&self) -> Result<CompiledHarnessProfile> {
        let bytes = fs::read(self.root.join(PROFILE_FILE)).map_err(|error| {
            MimirError::Configuration(format!(
                "Mimir is not enrolled or has no signed profile: {error}"
            ))
        })?;
        let envelope: SignedHarnessProfileEnvelope = serde_json::from_slice(&bytes)?;
        let enrollment = self.load_enrollment()?;
        let profile = verify_envelope(&envelope, &enrollment.signing_public_key_base64)?;
        validate_profile(&profile)?;
        validate_enrollment_scope(&profile, &enrollment)?;
        Ok(profile)
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
        write_secure_json(&current, envelope)
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
            binding_key: None,
            hook: None,
            action: None,
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
    if !is_enrolled()? {
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
    let profile = manager.load_profile()?;
    enforce_expiry(&profile)?;
    Ok(Some(profile))
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

/// Returns enabled bindings from the currently verified profile and local default overrides.
pub fn active_bindings() -> Result<Vec<HarnessBinding>> {
    if !is_enrolled()? {
        return Ok(Vec::new());
    }
    let manager = EnterpriseManager::global()?;
    let profile = manager.load_profile()?;
    enforce_expiry(&profile)?;
    let overrides = manager.load_overrides()?;
    Ok(profile
        .bindings
        .into_iter()
        .filter(|binding| !override_disabled(&overrides, binding))
        .collect())
}

/// Final native per-tool gate. It is independent of extensions and CLI disable switches.
pub fn tool_allowed(tool: &str) -> Result<bool> {
    if tool == "finish_task" {
        return Ok(true);
    }
    let normalized = local_bridge_policy_alias(tool);
    evaluate_allowlist_policy("restrict_tools", "allowed", &normalized, "tool_not_allowed")
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
    evaluate_allowlist_policy(
        "restrict_shell_programs",
        "allowed",
        program,
        "shell_program_not_allowed",
    )
}

/// Reads bounded reviewed artifacts from the active signed profile.
///
/// # Errors
/// Returns an error for invalid or expired enrollment state.
pub fn registry_artifacts() -> Result<Vec<crate::registry::RegistryArtifact>> {
    if !is_enrolled()? {
        return Ok(Vec::new());
    }
    let profile = EnterpriseManager::global()?.load_profile()?;
    enforce_expiry(&profile)?;
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
    let profile = manager.load_profile()?;
    enforce_expiry(&profile)?;
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

pub fn allowed_values(action: &str, parameter: &str) -> Result<Option<BTreeSet<String>>> {
    let mut restrictions = active_bindings()?
        .into_iter()
        .filter(|binding| binding.action == action)
        .filter_map(|binding| binding.parameters.get(parameter)?.as_array().cloned())
        .map(|values| {
            values
                .into_iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect::<BTreeSet<_>>()
        });
    let Some(mut intersection) = restrictions.next() else {
        return Ok(None);
    };
    for restriction in restrictions {
        intersection = intersection.intersection(&restriction).cloned().collect();
    }
    Ok(Some(intersection))
}

fn evaluate_allowlist_policy(
    action: &str,
    parameter: &str,
    candidate: &str,
    blocked_error_code: &str,
) -> Result<bool> {
    if !is_enrolled()? {
        return Ok(true);
    }
    let manager = EnterpriseManager::global()?;
    let enrollment = manager.load_enrollment()?;
    let profile = manager.load_profile()?;
    enforce_expiry(&profile)?;
    let overrides = manager.load_overrides()?;
    let bindings = profile
        .bindings
        .iter()
        .filter(|binding| !override_disabled(&overrides, binding))
        .collect::<Vec<_>>();
    let evaluation = evaluate_allowlist(
        &profile,
        &bindings,
        action,
        parameter,
        candidate,
        blocked_error_code,
    );
    let allowed = evaluation.allowed;
    manager.report_events_best_effort(enrollment, evaluation.events);
    Ok(allowed)
}

fn evaluate_allowlist(
    profile: &CompiledHarnessProfile,
    bindings: &[&HarnessBinding],
    action: &str,
    parameter: &str,
    candidate: &str,
    blocked_error_code: &str,
) -> PolicyEvaluation {
    let policy_decision = telemetry_enabled(profile, "policy_decision");
    let failure_code = telemetry_enabled(profile, "failure_code");
    let mut matched = false;
    let mut allowed = true;
    let mut events = Vec::new();

    for binding in bindings
        .iter()
        .copied()
        .filter(|binding| binding.action == action)
    {
        let Some(values) = binding.parameters.get(parameter).and_then(Value::as_array) else {
            continue;
        };
        matched = true;
        let binding_allowed = values
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == candidate);
        allowed &= binding_allowed;
        if policy_decision || (!binding_allowed && failure_code) {
            events.push(ComplianceEvent {
                schema: 1,
                event_id: Uuid::new_v4(),
                release_id: profile.release_id,
                binding_key: Some(binding.key.clone()),
                hook: Some(binding.hook.clone()),
                action: Some(binding.action.clone()),
                event_type: if binding_allowed {
                    "policy_allowed".into()
                } else {
                    "policy_blocked".into()
                },
                occurred_at: Utc::now(),
                duration_bucket: None,
                decision: policy_decision.then(|| {
                    if binding_allowed {
                        "allow".into()
                    } else {
                        "block".into()
                    }
                }),
                error_code: (!binding_allowed && failure_code)
                    .then(|| blocked_error_code.to_owned()),
                mimir_version: env!("CARGO_PKG_VERSION").to_owned(),
            });
        }
    }

    PolicyEvaluation {
        allowed: !matched || allowed,
        events,
    }
}

fn telemetry_enabled(profile: &CompiledHarnessProfile, category: &str) -> bool {
    profile.telemetry.iter().any(|value| value == category)
}

pub fn maximum_thinking_level() -> Result<Option<ThinkingLevel>> {
    active_bindings()?
        .iter()
        .filter(|binding| binding.action == "limit_thinking_level")
        .filter_map(|binding| binding.parameters.get("maximum")?.as_str())
        .map(parse_thinking_level)
        .try_fold(None, |current, next| {
            let next = next?;
            Ok(Some(
                current.map_or(next, |value: ThinkingLevel| value.min(next)),
            ))
        })
}

/// Applies content-free managed observability bindings for one lifecycle event.
/// Delivery is best effort and cannot change the runtime outcome.
pub fn observe_lifecycle(
    hook: &str,
    failed: bool,
    duration_bucket: Option<&str>,
) -> Result<ManagedLifecycleEffect> {
    if !is_enrolled()? {
        return Ok(ManagedLifecycleEffect::default());
    }
    let manager = EnterpriseManager::global()?;
    let enrollment = manager.load_enrollment()?;
    let profile = manager.load_profile()?;
    enforce_expiry(&profile)?;
    let overrides = manager.load_overrides()?;
    let (effect, events) = evaluate_lifecycle(&profile, &overrides, hook, failed, duration_bucket);
    manager.report_events_best_effort(enrollment, events);
    Ok(effect)
}

fn evaluate_lifecycle(
    profile: &CompiledHarnessProfile,
    overrides: &OverrideStore,
    hook: &str,
    failed: bool,
    duration_bucket: Option<&str>,
) -> (ManagedLifecycleEffect, Vec<ComplianceEvent>) {
    let mut events = Vec::new();
    let mut effect = ManagedLifecycleEffect::default();
    for binding in profile
        .bindings
        .iter()
        .filter(|binding| binding.hook == hook && !override_disabled(overrides, binding))
    {
        let (event_type, reported_duration, error_code) = match binding.action.as_str() {
            "record_metadata" => (Some("lifecycle_observed"), None, None),
            "measure_duration"
                if duration_bucket.is_some() && telemetry_enabled(profile, "duration_bucket") =>
            {
                (Some("duration_observed"), duration_bucket, None)
            }
            "notify_failure" if failed => {
                effect.notify_failure = true;
                if telemetry_enabled(profile, "failure_code") {
                    (
                        Some("failure_observed"),
                        None,
                        Some("managed_lifecycle_failure"),
                    )
                } else {
                    (None, None, None)
                }
            }
            _ => (None, None, None),
        };
        if let Some(event_type) = event_type {
            events.push(ComplianceEvent {
                schema: 1,
                event_id: Uuid::new_v4(),
                release_id: profile.release_id,
                binding_key: Some(binding.key.clone()),
                hook: Some(binding.hook.clone()),
                action: Some(binding.action.clone()),
                event_type: event_type.into(),
                occurred_at: Utc::now(),
                duration_bucket: reported_duration.map(str::to_owned),
                decision: None,
                error_code: error_code.map(str::to_owned),
                mimir_version: env!("CARGO_PKG_VERSION").into(),
            });
        }
    }
    (effect, events)
}

fn verify_envelope(
    envelope: &SignedHarnessProfileEnvelope,
    public_key_base64: &str,
) -> Result<CompiledHarnessProfile> {
    if envelope.schema != 1 {
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

fn validate_profile(profile: &CompiledHarnessProfile) -> Result<()> {
    if ![1, 3].contains(&profile.schema) || (profile.schema == 3) != profile.environment.is_some() {
        return Err(MimirError::Configuration(
            "unsupported compiled profile schema".into(),
        ));
    }
    if compare_versions(env!("CARGO_PKG_VERSION"), &profile.minimum_mimir_version)? < 0 {
        return Err(MimirError::Configuration(format!(
            "profile requires Mimir {} or newer",
            profile.minimum_mimir_version
        )));
    }
    crate::registry::validate(&profile.registry_artifacts)?;
    if let Some(environment) = &profile.environment {
        crate::working_environment::validate(environment)?;
    }
    let mut keys = BTreeSet::new();
    for binding in &profile.bindings {
        if !keys.insert(binding.key.as_str()) {
            return Err(MimirError::Configuration(format!(
                "compiled profile repeats binding key {}",
                binding.key
            )));
        }
        if binding.key.len() > 128 || binding.order > 10_000 {
            return Err(MimirError::Configuration(
                "compiled binding exceeds bounds".into(),
            ));
        }
        validate_binding(binding)?;
        if profile.revoked_catalog_items.contains(&format!(
            "{}@{}",
            binding.action.replace('_', "-"),
            binding.catalog_version
        )) {
            return Err(MimirError::Configuration(format!(
                "binding {} references a revoked catalog item",
                binding.key
            )));
        }
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
        if profile
            .revoked_catalog_items
            .contains(&format!("{}@{}", skill.id, skill.version))
        {
            return Err(MimirError::Configuration(format!(
                "skill {} references a revoked catalog item",
                skill.id
            )));
        }
    }
    Ok(())
}

fn validate_binding(binding: &HarnessBinding) -> Result<()> {
    let version_supported = binding.catalog_version == "1.0.0";
    if !version_supported {
        return Err(MimirError::Configuration(format!(
            "compiled binding {} uses unsupported {} catalog version {}",
            binding.key, binding.action, binding.catalog_version
        )));
    }
    let compatible = match binding.action.as_str() {
        "record_metadata" => &[
            "session_start",
            "session_shutdown",
            "agent_start",
            "agent_end",
            "turn_start",
            "turn_end",
            "tool_execution_start",
            "tool_execution_end",
            "after_provider_response",
            "refine_complete",
        ][..],
        "measure_duration" => &[
            "agent_end",
            "turn_end",
            "tool_execution_end",
            "after_provider_response",
        ][..],
        "notify_failure" => &["agent_end", "tool_execution_end", "after_provider_response"][..],
        "restrict_tools" => &["tool_call"][..],
        "restrict_shell_programs" => &["user_bash"][..],
        "restrict_models" => &["model_select"][..],
        "limit_thinking_level" => &["thinking_level_select"][..],
        unsupported => {
            return Err(MimirError::Configuration(format!(
                "compiled binding {} uses unsupported action {unsupported}",
                binding.key
            )));
        }
    };
    if !compatible.contains(&binding.hook.as_str()) {
        return Err(MimirError::Configuration(format!(
            "compiled binding {} uses action {} on incompatible hook {}",
            binding.key, binding.action, binding.hook
        )));
    }
    validate_binding_parameters(binding)
}

fn validate_binding_parameters(binding: &HarnessBinding) -> Result<()> {
    match binding.action.as_str() {
        "record_metadata" | "measure_duration" | "notify_failure" => {
            if binding
                .parameters
                .as_object()
                .is_none_or(|value| !value.is_empty())
            {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} must use empty parameters",
                    binding.key
                )));
            }
        }
        "restrict_tools" | "restrict_shell_programs" | "restrict_models" => {
            let Some(values) = binding.parameters.get("allowed").and_then(Value::as_array) else {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} requires an allowed array",
                    binding.key
                )));
            };
            if values.len() > 1_024
                || values.iter().any(|value| {
                    value
                        .as_str()
                        .is_none_or(|value| value.is_empty() || value.len() > 256)
                })
            {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} has an invalid allowed array",
                    binding.key
                )));
            }
            let unique = values
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>();
            if unique.len() != values.len()
                || binding.parameters.as_object().is_none_or(|o| o.len() != 1)
            {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} has duplicate or unknown allowlist parameters",
                    binding.key
                )));
            }
        }
        "limit_thinking_level" => {
            let Some(maximum) = binding.parameters.get("maximum").and_then(Value::as_str) else {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} requires maximum",
                    binding.key
                )));
            };
            parse_thinking_level(maximum)?;
            if binding.parameters.as_object().is_none_or(|o| o.len() != 1) {
                return Err(MimirError::Configuration(format!(
                    "compiled binding {} has unknown thinking parameters",
                    binding.key
                )));
            }
        }
        _ => unreachable!("supported action match is exhaustive"),
    }
    Ok(())
}

fn enforce_expiry(profile: &CompiledHarnessProfile) -> Result<()> {
    if profile.schema == 3
        && (profile.expires_at <= Utc::now()
            || profile.created_at > Utc::now() + chrono::Duration::minutes(5))
    {
        return Err(MimirError::Configuration(
            "Signed working environment is expired or not yet valid; sync before continuing".into(),
        ));
    }
    if profile.expires_at >= Utc::now() {
        return Ok(());
    }
    if profile
        .bindings
        .iter()
        .any(|binding| binding.enforcement == Enforcement::Mandatory && binding.critical)
    {
        return Err(MimirError::Configuration(
            "enterprise profile expired and contains critical mandatory controls; refusing to run"
                .into(),
        ));
    }
    eprintln!(
        "warning: enterprise profile is expired; continuing only noncritical cached defaults"
    );
    Ok(())
}

fn override_disabled(store: &OverrideStore, binding: &HarnessBinding) -> bool {
    binding.enforcement == Enforcement::Default
        && store
            .overrides
            .iter()
            .find(|entry| entry.binding_key == binding.key)
            .is_some_and(|entry| entry.disabled)
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
    Ok(enterprise_state_dir()?.join(CONFIG_FILE).is_file())
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
    let profile = EnterpriseManager::global()?.load_profile()?;
    enforce_expiry(&profile)?;
    Ok(Some(
        profile
            .environment
            .unwrap_or_else(crate::working_environment::defaults),
    ))
}

fn local_bridge_policy_alias(tool: &str) -> String {
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
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };

    fn profile_with(bindings: Vec<HarnessBinding>, telemetry: &[&str]) -> CompiledHarnessProfile {
        CompiledHarnessProfile {
            schema: 1,
            environment: None,
            registry_artifacts: Vec::new(),
            release_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            team_id: Some(Uuid::new_v4()),
            project_id: None,
            profile_name: "Test".into(),
            catalog_version: "1".into(),
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            minimum_mimir_version: env!("CARGO_PKG_VERSION").into(),
            bindings,
            skills: Vec::new(),
            telemetry: telemetry.iter().map(ToString::to_string).collect(),
            revoked_catalog_items: Vec::new(),
            source_metadata: json!({}),
        }
    }

    #[test]
    fn enrollment_scope_rejects_other_team_or_organization_before_activation() {
        let mut profile = profile_with(Vec::new(), &[]);
        let enrollment = enrollment("http://127.0.0.1".into(), &profile);
        assert!(validate_enrollment_scope(&profile, &enrollment).is_ok());
        profile.team_id = Some(Uuid::new_v4());
        assert!(validate_enrollment_scope(&profile, &enrollment).is_err());
        profile.team_id = Some(enrollment.team_id);
        profile.organization_id = Uuid::new_v4();
        assert!(validate_enrollment_scope(&profile, &enrollment).is_err());
    }

    fn restriction(key: &str, hook: &str, action: &str, allowed: &[&str]) -> HarnessBinding {
        HarnessBinding {
            key: key.into(),
            hook: hook.into(),
            action: action.into(),
            catalog_version: "1.0.0".into(),
            enforcement: Enforcement::Mandatory,
            order: 100,
            critical: true,
            parameters: json!({"allowed": allowed}),
        }
    }

    fn observation(key: &str, hook: &str, action: &str) -> HarnessBinding {
        HarnessBinding {
            key: key.into(),
            hook: hook.into(),
            action: action.into(),
            catalog_version: "1.0.0".into(),
            enforcement: Enforcement::Default,
            order: 100,
            critical: false,
            parameters: json!({}),
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

    #[test]
    fn verifies_exact_signed_payload_bytes_and_rejects_tampering() {
        let release_id = Uuid::new_v4();
        let payload = serde_json::to_vec(&json!({
            "schema": 1,
            "release_id": release_id,
            "organization_id": Uuid::new_v4(),
            "team_id": Uuid::new_v4(),
            "project_id": null,
            "profile_name": "Payments",
            "source": {},
            "source_revisions": [],
            "catalog_version": "1",
            "created_at": Utc::now(),
            "expires_at": Utc::now() + chrono::Duration::hours(1),
            "minimum_mimir_version": env!("CARGO_PKG_VERSION"),
            "bindings": [],
            "skills": [],
            "telemetry": ["profile_application"],
            "revoked_catalog_items": [],
        }))
        .expect("payload");
        let random = SystemRandom::new();
        let document = Ed25519KeyPair::generate_pkcs8(&random).expect("key document");
        let key = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("key");
        let envelope = SignedHarnessProfileEnvelope {
            schema: 1,
            payload_base64: BASE64.encode(&payload),
            sha256: format!("{:x}", Sha256::digest(&payload)),
            key_id: Uuid::new_v4(),
            signature: BASE64.encode(key.sign(&payload).as_ref()),
        };
        let verified = verify_envelope(&envelope, &BASE64.encode(key.public_key().as_ref()))
            .expect("verified payload");
        assert_eq!(verified.release_id, release_id);

        let mut tampered = envelope;
        tampered.sha256 = "0".repeat(64);
        assert!(verify_envelope(&tampered, &BASE64.encode(key.public_key().as_ref())).is_err());
    }

    #[test]
    fn local_overrides_cannot_disable_mandatory_bindings() {
        let store = OverrideStore {
            schema: 1,
            overrides: vec![LocalOverride {
                binding_key: "policy.tools".into(),
                disabled: true,
                reason: "test".into(),
                changed_at: Utc::now(),
            }],
        };
        let binding = HarnessBinding {
            key: "policy.tools".into(),
            hook: "tool_call".into(),
            action: "restrict_tools".into(),
            catalog_version: "1.0.0".into(),
            enforcement: Enforcement::Mandatory,
            order: 1,
            critical: true,
            parameters: json!({"allowed": []}),
        };
        assert!(!override_disabled(&store, &binding));
    }

    #[test]
    fn profile_validation_rejects_advertised_but_unsupported_actions() {
        let mut profile = profile_with(
            vec![HarnessBinding {
                key: "unsupported.block".into(),
                hook: "tool_call".into(),
                action: "block".into(),
                catalog_version: "1.0.0".into(),
                enforcement: Enforcement::Mandatory,
                order: 1,
                critical: true,
                parameters: json!({"reason_code": "denied"}),
            }],
            &[],
        );
        assert!(validate_profile(&profile).is_err());
        profile.bindings[0] = restriction(
            "policy.tools",
            "tool_call",
            "restrict_tools",
            &["read_file", "read_file"],
        );
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn profile_validation_accepts_dormant_future_tool_selections() {
        let profile = profile_with(
            vec![restriction(
                "policy.tools",
                "tool_call",
                "restrict_tools",
                &["read_file", "future_unavailable_tool"],
            )],
            &[],
        );
        assert!(validate_profile(&profile).is_ok());
    }

    #[test]
    fn expired_critical_profiles_fail_closed_but_noncritical_defaults_do_not() {
        let mut critical = profile_with(
            vec![restriction(
                "policy.tools",
                "tool_call",
                "restrict_tools",
                &["read_file"],
            )],
            &[],
        );
        critical.expires_at = Utc::now() - chrono::Duration::seconds(1);
        assert!(enforce_expiry(&critical).is_err());

        let mut noncritical = profile_with(
            vec![observation(
                "observe.lifecycle",
                "agent_end",
                "record_metadata",
            )],
            &[],
        );
        noncritical.expires_at = Utc::now() - chrono::Duration::seconds(1);
        assert!(enforce_expiry(&noncritical).is_ok());
    }

    #[test]
    fn observability_events_match_wire_contract_and_privacy_gates() {
        let bindings = vec![
            observation("observe.lifecycle", "agent_end", "record_metadata"),
            observation("observe.duration", "agent_end", "measure_duration"),
            observation("observe.failure", "agent_end", "notify_failure"),
        ];
        let profile = profile_with(bindings.clone(), &["duration_bucket", "failure_code"]);
        let (effect, events) = evaluate_lifecycle(
            &profile,
            &OverrideStore::default(),
            "agent_end",
            true,
            Some("lt_1s"),
        );
        assert!(effect.notify_failure);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "lifecycle_observed");
        assert_eq!(events[0].duration_bucket, None);
        assert_eq!(events[1].event_type, "duration_observed");
        assert_eq!(events[1].duration_bucket.as_deref(), Some("lt_1s"));
        assert_eq!(events[2].event_type, "failure_observed");
        assert_eq!(
            events[2].error_code.as_deref(),
            Some("managed_lifecycle_failure")
        );

        let private_profile = profile_with(bindings, &[]);
        let (effect, events) = evaluate_lifecycle(
            &private_profile,
            &OverrideStore::default(),
            "agent_end",
            true,
            Some("lt_1s"),
        );
        assert!(effect.notify_failure);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "lifecycle_observed");
        assert!(events[0].duration_bucket.is_none());
        assert!(events[0].error_code.is_none());
    }

    #[test]
    fn observability_catalog_version_is_exact() {
        let mut binding = observation("observe.lifecycle", "agent_end", "record_metadata");
        assert!(validate_binding(&binding).is_ok());
        binding.catalog_version = "1.1.0".into();
        assert!(validate_binding(&binding).is_err());
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
            let mode = fs::metadata(manager.root.join(OVERRIDES_FILE))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
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
                MAX_INVENTORY_ALLOWED_PROGRAMS,
            )
            .is_ok()
        );
        assert!(
            validate_tool_inventory_bounds(
                &fingerprint,
                MAX_TOOL_INVENTORY_ITEMS + 1,
                MAX_INVENTORY_ALLOWED_PROGRAMS,
            )
            .is_err()
        );
        assert!(
            validate_tool_inventory_bounds(
                &fingerprint,
                MAX_TOOL_INVENTORY_ITEMS,
                MAX_INVENTORY_ALLOWED_PROGRAMS + 1,
            )
            .is_err()
        );
        assert!(validate_tool_inventory_bounds(&"z".repeat(64), 0, 0).is_err());
    }

    #[test]
    fn tool_inventory_json_matches_the_frozen_wire_shape() {
        let tool = ToolInventoryItem {
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
        };
        let fingerprint = "a".repeat(64);
        let tools = vec![tool];
        let report = ToolInventoryReport {
            schema: 1,
            release_id: Uuid::nil(),
            mimir_version: env!("CARGO_PKG_VERSION"),
            fingerprint: &fingerprint,
            tools: &tools,
            local_process: LocalProcessInventory {
                enabled: true,
                allowed_programs: vec!["git".into()],
            },
        };
        let value = serde_json::to_value(report).expect("inventory json");
        let top = value.as_object().expect("top-level object");
        assert_eq!(
            top.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "fingerprint",
                "local_process",
                "mimir_version",
                "release_id",
                "schema",
                "tools",
            ])
        );
        let item = value["tools"][0].as_object().expect("tool object");
        assert_eq!(
            item.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "availability",
                "capabilities",
                "id",
                "label",
                "required_by_runtime",
                "risk",
                "source",
            ])
        );
        assert_eq!(value["tools"][0]["risk"], "read_only");
        assert_eq!(value["tools"][0]["source"]["kind"], "builtin");
        assert_eq!(value["tools"][0]["availability"], "available");
        assert_eq!(value["local_process"]["allowed_programs"][0], "git");
    }

    #[test]
    fn policy_payloads_are_content_free_for_allow_and_block() {
        let binding = restriction(
            "policy.tools",
            "tool_call",
            "restrict_tools",
            &["read_file"],
        );
        let profile = profile_with(vec![binding], &["policy_decision", "failure_code"]);
        let bindings = profile.bindings.iter().collect::<Vec<_>>();

        let allowed = evaluate_allowlist(
            &profile,
            &bindings,
            "restrict_tools",
            "allowed",
            "read_file",
            "tool_not_allowed",
        );
        assert!(allowed.allowed);
        assert_eq!(allowed.events.len(), 1);
        assert_eq!(allowed.events[0].event_type, "policy_allowed");
        assert_eq!(allowed.events[0].decision.as_deref(), Some("allow"));
        assert_eq!(allowed.events[0].error_code, None);

        let private_attempt = "write_file:/private/customer/secret.txt";
        let blocked = evaluate_allowlist(
            &profile,
            &bindings,
            "restrict_tools",
            "allowed",
            private_attempt,
            "tool_not_allowed",
        );
        assert!(!blocked.allowed);
        assert_eq!(blocked.events.len(), 1);
        let event = &blocked.events[0];
        assert_eq!(event.binding_key.as_deref(), Some("policy.tools"));
        assert_eq!(event.hook.as_deref(), Some("tool_call"));
        assert_eq!(event.action.as_deref(), Some("restrict_tools"));
        assert_eq!(event.event_type, "policy_blocked");
        assert_eq!(event.decision.as_deref(), Some("block"));
        assert_eq!(event.error_code.as_deref(), Some("tool_not_allowed"));
        let payload = serde_json::to_string(&blocked.events).expect("payload");
        assert!(!payload.contains(private_attempt));
        assert!(!payload.contains("customer"));
        assert!(!payload.contains("secret.txt"));
    }

    #[test]
    fn decision_and_failure_telemetry_are_independent() {
        let binding = restriction(
            "policy.shell",
            "user_bash",
            "restrict_shell_programs",
            &["git"],
        );
        let decision_profile = profile_with(vec![binding.clone()], &["policy_decision"]);
        let decision_bindings = decision_profile.bindings.iter().collect::<Vec<_>>();
        let blocked = evaluate_allowlist(
            &decision_profile,
            &decision_bindings,
            "restrict_shell_programs",
            "allowed",
            "printf",
            "shell_program_not_allowed",
        );
        assert_eq!(blocked.events[0].decision.as_deref(), Some("block"));
        assert_eq!(blocked.events[0].error_code, None);

        let failure_profile = profile_with(vec![binding], &["failure_code"]);
        let failure_bindings = failure_profile.bindings.iter().collect::<Vec<_>>();
        let blocked = evaluate_allowlist(
            &failure_profile,
            &failure_bindings,
            "restrict_shell_programs",
            "allowed",
            "printf",
            "shell_program_not_allowed",
        );
        assert_eq!(blocked.events[0].decision, None);
        assert_eq!(
            blocked.events[0].error_code.as_deref(),
            Some("shell_program_not_allowed")
        );
        let allowed = evaluate_allowlist(
            &failure_profile,
            &failure_bindings,
            "restrict_shell_programs",
            "allowed",
            "git",
            "shell_program_not_allowed",
        );
        assert!(allowed.allowed);
        assert!(allowed.events.is_empty());

        let silent_profile = profile_with(
            vec![restriction(
                "policy.tools",
                "tool_call",
                "restrict_tools",
                &[],
            )],
            &[],
        );
        let silent_bindings = silent_profile.bindings.iter().collect::<Vec<_>>();
        let blocked = evaluate_allowlist(
            &silent_profile,
            &silent_bindings,
            "restrict_tools",
            "allowed",
            "write_file",
            "tool_not_allowed",
        );
        assert!(!blocked.allowed);
        assert!(blocked.events.is_empty());
    }

    #[tokio::test]
    async fn failed_delivery_cannot_change_the_policy_result() {
        let profile = profile_with(
            vec![restriction(
                "policy.tools",
                "tool_call",
                "restrict_tools",
                &[],
            )],
            &["policy_decision", "failure_code"],
        );
        let bindings = profile.bindings.iter().collect::<Vec<_>>();
        let evaluation = evaluate_allowlist(
            &profile,
            &bindings,
            "restrict_tools",
            "allowed",
            "write_file",
            "tool_not_allowed",
        );
        assert!(!evaluation.allowed);
        let manager =
            EnterpriseManager::at(tempfile::tempdir().expect("tempdir").keep()).expect("manager");
        let delivery = manager
            .send_events(
                &enrollment("not a control-plane URL".into(), &profile),
                &evaluation.events,
            )
            .await;
        assert!(delivery.is_err());
        assert!(!evaluation.allowed);
    }
}
