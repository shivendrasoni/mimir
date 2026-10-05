//! Device-wide personal choices, authenticated separately from the shared profile.
use crate::{
    enterprise::{
        CompiledHarnessProfile, Enforcement, HarnessBinding, ManagedSkill,
        SignedHarnessProfileEnvelope,
    },
    error::{MimirError, Result},
    registry::RegistryArtifact,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u8,
    kind: String,
    release_id: uuid::Uuid,
    organization_id: uuid::Uuid,
    team_id: Option<uuid::Uuid>,
    organization_revision: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    registry_artifacts: Vec<RegistryArtifact>,
    mcp_servers: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    envelope: SignedHarnessProfileEnvelope,
    revocations: Vec<Revocation>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revocation {
    id: String,
    version: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Choices {
    schema: u8,
    selections: Vec<Choice>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Choice {
    id: String,
    version: String,
    kind: String,
    harnesses: Vec<String>,
}
fn invalid() -> MimirError {
    MimirError::Configuration("Optional device resource verification failed".into())
}
fn read<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err(invalid());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn verified(
    envelope: &SignedHarnessProfileEnvelope,
    key: &str,
    profile: &CompiledHarnessProfile,
) -> Result<Manifest> {
    let bytes = BASE64
        .decode(&envelope.payload_base64)
        .map_err(|_| invalid())?;
    if envelope.schema != 1
        || bytes.len() > 2 * 1024 * 1024
        || format!("{:x}", Sha256::digest(&bytes)) != envelope.sha256
    {
        return Err(invalid());
    }
    let key = BASE64.decode(key).map_err(|_| invalid())?;
    let signature = BASE64.decode(&envelope.signature).map_err(|_| invalid())?;
    if key.len() != 32 {
        return Err(invalid());
    }
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&bytes, &signature)
        .map_err(|_| invalid())?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.schema != 1
        || manifest.kind != "betterloop_optional_resources"
        || manifest.release_id != profile.release_id
        || manifest.organization_id != profile.organization_id
        || manifest.team_id != profile.team_id
        || Some(manifest.organization_revision.as_str())
            != profile.source_metadata["source_revisions"][0]["commit_sha"].as_str()
        || manifest.expires_at <= Utc::now()
        || manifest.expires_at > profile.expires_at
        || manifest.created_at > Utc::now() + chrono::Duration::minutes(5)
        || manifest.registry_artifacts.len() > 128
        || manifest.mcp_servers.len() > 64
    {
        return Err(invalid());
    }
    let supported: Vec<_> = manifest
        .registry_artifacts
        .iter()
        .cloned()
        .map(|mut item| {
            item.supported_clients = None;
            item
        })
        .collect();
    crate::registry::validate(&supported)?;
    let mut ids = BTreeSet::new();
    for item in &manifest.registry_artifacts {
        if item.enforcement != "default" || !ids.insert(item.id.clone()) {
            return Err(invalid());
        }
    }
    let mut keys = BTreeSet::new();
    for server in &manifest.mcp_servers {
        if server["enforcement"] != "default"
            || !keys.insert(server["key"].as_str().ok_or_else(invalid)?.to_owned())
        {
            return Err(invalid());
        }
    }
    // Validate every approved connection, including ones the developer has not selected.
    for server in &manifest.mcp_servers {
        let mut environment = crate::working_environment::defaults();
        environment.mcp_servers = vec![server.clone()];
        crate::working_environment::validate(&environment)?;
    }
    Ok(manifest)
}
fn apply(
    mut profile: CompiledHarnessProfile,
    manifest: Manifest,
    choices: Choices,
    revocations: &[Revocation],
) -> Result<CompiledHarnessProfile> {
    if choices.schema != 1 || choices.selections.len() > 192 || revocations.len() > 8192 {
        return Err(invalid());
    }
    let revoked: BTreeSet<String> = revocations
        .iter()
        .map(|item| format!("{}@{}", item.id, item.version))
        .chain(profile.revoked_catalog_items.clone())
        .collect();
    let selected: BTreeSet<(String, String, String)> = choices
        .selections
        .into_iter()
        .filter(|item| {
            item.harnesses.iter().any(|agent| agent == "mimir")
                && !revoked.contains(&format!("{}@{}", item.id, item.version))
        })
        .map(|item| (item.id, item.version, item.kind))
        .collect();
    let base: BTreeMap<String, String> = profile
        .registry_artifacts
        .iter()
        .map(|item| (item.id.clone(), item.version.clone()))
        .collect();
    let mut optional: BTreeMap<String, RegistryArtifact> = manifest
        .registry_artifacts
        .into_iter()
        .filter(|item| {
            selected.contains(&(item.id.clone(), item.version.clone(), "registry".into()))
                && !base.contains_key(&item.id)
                && !profile.skills.iter().any(|skill| skill.id == item.id)
                && !profile
                    .bindings
                    .iter()
                    .any(|binding| binding.key == item.id)
                && item
                    .supported_clients
                    .as_ref()
                    .is_none_or(|clients| clients.iter().any(|client| client == "mimir"))
        })
        .map(|item| (item.id.clone(), item))
        .collect();
    loop {
        let removed: Vec<String> = optional
            .values()
            .filter(|item| {
                item.dependencies.as_ref().is_some_and(|deps| {
                    deps.iter().any(|dep| {
                        optional
                            .get(&dep.id)
                            .is_none_or(|item| item.version != dep.version)
                            && base.get(&dep.id) != Some(&dep.version)
                    })
                })
            })
            .map(|item| item.id.clone())
            .collect();
        if removed.is_empty() {
            break;
        }
        for id in removed {
            optional.remove(&id);
        }
    }
    for item in optional.into_values() {
        if item.kind == "rule" {
            let mut binding: HarnessBinding = serde_json::from_str(&item.content)?;
            if binding.key != item.id
                || ![
                    "block",
                    "restrict_tools",
                    "restrict_shell_programs",
                    "restrict_models",
                ]
                .contains(&binding.action.as_str())
            {
                return Err(invalid());
            }
            binding.enforcement = Enforcement::Mandatory;
            binding.critical = true;
            profile.bindings.push(binding);
        }
        if item.kind == "skill" {
            profile.skills.push(ManagedSkill {
                id: item.id.clone(),
                version: item.version.clone(),
                enforcement: Enforcement::Default,
                name: item.name.clone(),
                description: item.description.clone(),
                instructions: item.content.clone(),
                content_sha256: item.sha256.clone(),
                template_id: item.template_id.clone(),
            });
        }
        profile.registry_artifacts.push(item);
    }
    if let Some(environment) = &mut profile.environment {
        for server in manifest.mcp_servers {
            let id = server["key"].as_str().ok_or_else(invalid)?.to_owned();
            let version = format!("{:x}", Sha256::digest(serde_json::to_vec(&server)?));
            if selected.contains(&(id.clone(), version, "mcp".into()))
                && !environment
                    .mcp_servers
                    .iter()
                    .any(|shared| shared["key"] == id)
            {
                environment.mcp_servers.push(server);
            }
        }
    }
    crate::enterprise::validate_profile(&profile)?;
    Ok(profile)
}
/// Invalid, expired or mismatched optional code is disabled while shared policy continues.
pub(crate) fn load(
    profile: CompiledHarnessProfile,
    key: &str,
    installation_id: uuid::Uuid,
) -> CompiledHarnessProfile {
    let root = std::env::var_os("BETTERLOOP_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".betterloop"))
        });
    let Some(root) = root else { return profile };
    let device = root.join("device");
    let result = (|| -> Result<CompiledHarnessProfile> {
        let registration: Value = read(
            &device.join("harnesses/mimir/registration.json"),
            1024 * 1024,
        )?;
        if registration["installationId"].as_str() != Some(installation_id.to_string().as_str()) {
            return Err(invalid());
        }
        let cache: Cache = read(
            &device.join("harnesses/mimir/optional-library.json"),
            4 * 1024 * 1024,
        )?;
        let choices: Choices = read(&device.join("personal-resources.json"), 1024 * 1024)?;
        let manifest = verified(&cache.envelope, key, &profile)?;
        apply(profile.clone(), manifest, choices, &cache.revocations)
    })();
    result.unwrap_or(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use serde_json::json;
    fn fixture() -> (CompiledHarnessProfile, Value, Ed25519KeyPair) {
        let now = Utc::now();
        let revision = "a".repeat(40);
        let profile:CompiledHarnessProfile=serde_json::from_value(json!({"schema":3,"environment":crate::working_environment::defaults(),"release_id":uuid::Uuid::new_v4(),"organization_id":uuid::Uuid::new_v4(),"team_id":uuid::Uuid::new_v4(),"project_id":null,"profile_name":"Team","catalog_version":"1","created_at":now,"expires_at":now+chrono::Duration::hours(1),"minimum_mimir_version":"0.0.0","bindings":[],"skills":[],"telemetry":[],"source_revisions":[{"commit_sha":revision}]})).unwrap();
        let content = "# Approved guide\nRead the repository before making changes.";
        let manifest = json!({"schema":1,"kind":"betterloop_optional_resources","release_id":profile.release_id,"organization_id":profile.organization_id,"team_id":profile.team_id,"organization_revision":revision,"created_at":now,"expires_at":profile.expires_at,"registry_artifacts":[{"id":"org.guide","version":"1.0.0","kind":"skill","name":"Guide","description":"Reviewed guidance","path":"registry/skills/org.guide/1.0.0/SKILL.md","sha256":format!("{:x}",Sha256::digest(content.as_bytes())),"content":content,"enforcement":"default","source":{"repository":"org/config","commit_sha":revision}}],"mcp_servers":[]});
        let key = Ed25519KeyPair::from_pkcs8(
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .unwrap()
                .as_ref(),
        )
        .unwrap();
        (profile, manifest, key)
    }
    fn signed(manifest: &Value, key: &Ed25519KeyPair) -> SignedHarnessProfileEnvelope {
        let bytes = serde_json::to_vec(manifest).unwrap();
        serde_json::from_value(json!({"schema":1,"payload_base64":BASE64.encode(&bytes),"sha256":format!("{:x}",Sha256::digest(&bytes)),"key_id":uuid::Uuid::new_v4(),"signature":BASE64.encode(key.sign(&bytes).as_ref())})).unwrap()
    }
    fn choices(id: &str) -> Choices {
        serde_json::from_value(json!({"schema":1,"selections":[{"id":id,"version":"1.0.0","kind":"registry","harnesses":["mimir"]}]})).unwrap()
    }
    #[test]
    fn verifies_separate_signature_and_scope() {
        let (profile, manifest, key) = fixture();
        let envelope = signed(&manifest, &key);
        let public = BASE64.encode(key.public_key().as_ref());
        assert!(verified(&envelope, &public, &profile).is_ok());
        let mut other = profile.clone();
        other.team_id = Some(uuid::Uuid::new_v4());
        assert!(verified(&envelope, &public, &other).is_err());
        let mut tampered = envelope;
        tampered.sha256 = "b".repeat(64);
        assert!(verified(&tampered, &public, &profile).is_err());
    }
    #[test]
    fn choices_preserve_shared_policy_and_revocations_disable_only_optional() {
        let (mut profile, manifest, key) = fixture();
        profile.bindings.push(HarnessBinding {
            key: "policy.tools".into(),
            hook: "tool_call".into(),
            action: "restrict_tools".into(),
            catalog_version: "1.0.0".into(),
            enforcement: Enforcement::Mandatory,
            order: 10,
            critical: true,
            parameters: json!({"allowed":["read_file"]}),
        });
        let public = BASE64.encode(key.public_key().as_ref());
        let parsed = verified(&signed(&manifest, &key), &public, &profile).unwrap();
        let result = apply(profile.clone(), parsed, choices("org.guide"), &[]).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(
            result.bindings[0].parameters,
            profile.bindings[0].parameters
        );
        let parsed = verified(&signed(&manifest, &key), &public, &profile).unwrap();
        let revoked = apply(
            profile,
            parsed,
            choices("org.guide"),
            &[Revocation {
                id: "org.guide".into(),
                version: "1.0.0".into(),
            }],
        )
        .unwrap();
        assert!(revoked.skills.is_empty());
        assert_eq!(revoked.bindings.len(), 1);
    }
    #[test]
    fn rejects_expiry_required_resources_and_missing_dependencies() {
        let (profile, mut manifest, key) = fixture();
        let public = BASE64.encode(key.public_key().as_ref());
        manifest["registry_artifacts"][0]["enforcement"] = json!("mandatory");
        assert!(verified(&signed(&manifest, &key), &public, &profile).is_err());
        manifest["registry_artifacts"][0]["enforcement"] = json!("default");
        manifest["registry_artifacts"][0]["dependencies"] =
            json!([{"id":"org.missing","version":"1.0.0"}]);
        assert!(verified(&signed(&manifest, &key), &public, &profile).is_err());
        manifest["registry_artifacts"][0]
            .as_object_mut()
            .unwrap()
            .remove("dependencies");
        manifest["expires_at"] = json!(Utc::now() - chrono::Duration::minutes(1));
        assert!(verified(&signed(&manifest, &key), &public, &profile).is_err());
    }
    #[test]
    fn unapproved_choices_do_not_add_resources() {
        let (profile, manifest, key) = fixture();
        let parsed = verified(
            &signed(&manifest, &key),
            &BASE64.encode(key.public_key().as_ref()),
            &profile,
        )
        .unwrap();
        let result = apply(profile, parsed, choices("unapproved.resource"), &[]).unwrap();
        assert!(result.registry_artifacts.is_empty());
    }
}
