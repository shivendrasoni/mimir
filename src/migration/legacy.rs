use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    auth::{AuthCredential, OAuthCredential},
    error::{MimirError, Result},
    migration::{
        ArtifactKind, AuthReport, CompatibilityReport, InventoryReport, ModelsReport,
        SettingsReport,
    },
};

use super::safe_fs::{
    MAX_RESOURCE_ARCHIVE_BYTES, MAX_RESOURCE_FILES, SourceFile, collect_resource_files,
    read_optional_bytes,
};
use super::session_import::import_sessions;
use super::types::{PreparedArtifact, PreparedReport};

const COMPATIBILITY_SCHEMA_VERSION: u16 = 1;
const SETTINGS_PREFERENCE_KEYS: &[&str] = &[
    "defaultProvider",
    "defaultModel",
    "recentModels",
    "enabledModels",
    "defaultThinkingLevel",
    "defaultServiceTier",
    "transport",
];
const SETTINGS_INVENTORY_KEYS: &[&str] = &["packages", "extensions", "skills"];

pub(crate) async fn collect(legacy_root: &Path) -> Result<(Vec<PreparedArtifact>, PreparedReport)> {
    let mut artifacts = Vec::new();
    let mut report = PreparedReport {
        auth: Vec::new(),
        sessions: Vec::new(),
        settings: None,
        models: None,
        inventory: None,
        compatibility: CompatibilityReport {
            schema_version: COMPATIBILITY_SCHEMA_VERSION,
            ..CompatibilityReport::default()
        },
    };

    if let Some((bytes, auth_report)) = import_auth(legacy_root).await? {
        artifacts.push(PreparedArtifact {
            kind: ArtifactKind::Auth,
            source: "auth.json".into(),
            target: "auth.json".into(),
            summary: format!("migrate {} auth entries", auth_report.len()),
            bytes,
        });
        report.auth = auth_report;
    }

    for (artifact, session_report) in import_sessions(legacy_root).await? {
        artifacts.push(artifact);
        report.sessions.push(session_report);
    }

    let settings = import_settings(legacy_root).await?;
    if let Some(settings) = &settings {
        artifacts.extend(settings.artifacts.clone());
        report.settings = Some(settings.report.clone());
        report
            .compatibility
            .unrepresentable
            .extend(settings.unrepresentable.clone());
        add_archive_totals(
            &mut report.compatibility,
            settings.archived_files,
            settings.archived_bytes,
        )?;
    }

    if let Some(models) = import_models(legacy_root).await? {
        artifacts.extend(models.artifacts);
        report.models = Some(models.report);
        report
            .compatibility
            .unrepresentable
            .extend(models.unrepresentable);
        add_archive_totals(
            &mut report.compatibility,
            models.archived_files,
            models.archived_bytes,
        )?;
    }

    let resources = collect_resources(legacy_root).await?;
    if !resources.extensions.is_empty() {
        report
            .compatibility
            .unrepresentable
            .push("extensions.runtime_code".into());
    }
    if !resources.skills.is_empty() {
        report
            .compatibility
            .unrepresentable
            .push("skills.installed_content".into());
    }
    add_archive_totals(
        &mut report.compatibility,
        resources.artifacts.len(),
        resources.total_bytes,
    )?;
    artifacts.extend(resources.artifacts);

    if let Some(inventory) = import_inventory(
        legacy_root,
        settings.as_ref().map(|settings| &settings.value),
        &resources.extensions,
        &resources.skills,
    )
    .await?
    {
        add_archive_totals(
            &mut report.compatibility,
            inventory.archived_files,
            inventory.archived_bytes,
        )?;
        artifacts.extend(inventory.artifacts);
        report.inventory = Some(inventory.report);
    }

    report.compatibility.unrepresentable.sort();
    report.compatibility.unrepresentable.dedup();
    Ok((artifacts, report))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyCredential {
    ApiKey {
        key: String,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        #[serde(alias = "expires")]
        expires_at_ms: u64,
        #[serde(default, alias = "accountId")]
        account_id: Option<String>,
        #[serde(default, alias = "enterpriseUrl")]
        enterprise_url: Option<String>,
    },
}

async fn import_auth(legacy_root: &Path) -> Result<Option<(Vec<u8>, Vec<AuthReport>)>> {
    let Some(bytes) = read_optional_bytes(legacy_root, "auth.json").await? else {
        return Ok(None);
    };
    let root: Value = serde_json::from_slice(&bytes)?;
    let root = root
        .as_object()
        .ok_or_else(|| MimirError::Configuration("auth.json must contain a JSON object".into()))?;
    let providers = match root.get("providers") {
        Some(Value::Object(providers)) => providers,
        Some(_) => {
            return Err(MimirError::Configuration(
                "auth.json providers must be an object".into(),
            ));
        }
        None => root,
    };

    let mut migrated = BTreeMap::new();
    let mut report = Vec::new();
    for (provider, value) in providers {
        let credential: LegacyCredential =
            serde_json::from_value(value.clone()).map_err(|error| {
                MimirError::Configuration(format!(
                    "legacy auth.json credential for {provider} is invalid: {error}"
                ))
            })?;
        let next = match credential {
            LegacyCredential::ApiKey { key } if !key.trim().is_empty() => {
                report.push(AuthReport {
                    provider: provider.clone(),
                    auth_type: "api_key".into(),
                });
                AuthCredential::ApiKey { key }
            }
            LegacyCredential::OAuth {
                access,
                refresh,
                expires_at_ms,
                account_id,
                enterprise_url,
            } if !access.trim().is_empty() && !refresh.trim().is_empty() => {
                report.push(AuthReport {
                    provider: provider.clone(),
                    auth_type: "oauth".into(),
                });
                AuthCredential::OAuth(OAuthCredential {
                    access,
                    refresh,
                    expires_at_ms,
                    account_id,
                    enterprise_url,
                })
            }
            _ => {
                return Err(MimirError::Configuration(
                    "legacy auth.json contains a blank secret".into(),
                ));
            }
        };
        migrated.insert(provider.clone(), next);
    }
    let migrated = pretty_json(&migrated)?;
    Ok(Some((migrated, report)))
}

#[derive(Clone)]
struct ImportedSettings {
    value: Value,
    artifacts: Vec<PreparedArtifact>,
    report: SettingsReport,
    unrepresentable: Vec<String>,
    archived_files: usize,
    archived_bytes: usize,
}

#[derive(Serialize)]
struct PreferencesV1 {
    schema_version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recent_models: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    enabled_models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
}

async fn import_settings(legacy_root: &Path) -> Result<Option<ImportedSettings>> {
    let Some(raw) = read_optional_bytes(legacy_root, "settings.json").await? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&raw)?;
    let object = value.as_object().ok_or_else(|| {
        MimirError::Configuration("settings.json must contain a JSON object".into())
    })?;
    let default_provider = optional_nonempty_string(object, "defaultProvider", "settings.json")?;
    let default_model = optional_nonempty_string(object, "defaultModel", "settings.json")?;
    let recent_models = optional_string_array(object, "recentModels", "settings.json")?;
    let enabled_models = optional_string_array(object, "enabledModels", "settings.json")?;
    let default_thinking_level = optional_enum(
        object,
        "defaultThinkingLevel",
        &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
        "settings.json",
    )?;
    let default_service_tier =
        optional_nonempty_string(object, "defaultServiceTier", "settings.json")?;
    let transport = optional_enum(
        object,
        "transport",
        &["auto", "sse", "websocket"],
        "settings.json",
    )?;
    validate_inventory_fields(object, "settings.json")?;

    let preferences = PreferencesV1 {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        default_provider: default_provider.clone(),
        default_model: default_model.clone(),
        recent_models: recent_models.clone(),
        enabled_models: enabled_models.clone(),
        default_thinking_level,
        default_service_tier,
        transport,
    };
    let unrepresentable = object
        .keys()
        .filter(|key| {
            !SETTINGS_PREFERENCE_KEYS.contains(&key.as_str())
                && !SETTINGS_INVENTORY_KEYS.contains(&key.as_str())
        })
        .map(|key| format!("settings.{key}"))
        .collect();
    Ok(Some(ImportedSettings {
        value: value.clone(),
        artifacts: vec![
            PreparedArtifact {
                kind: ArtifactKind::Settings,
                source: "settings.json".into(),
                target: "config/settings.json".into(),
                summary: "migrate legacy settings".into(),
                bytes: pretty_json(&value)?,
            },
            PreparedArtifact {
                kind: ArtifactKind::Preferences,
                source: "settings.json".into(),
                target: "config/preferences.json".into(),
                summary: "migrate versioned provider and model preferences".into(),
                bytes: pretty_json(&preferences)?,
            },
            archive_artifact("settings.json", raw.clone()),
        ],
        report: SettingsReport {
            provider_preference: default_provider.is_some(),
            model_preference: default_model.is_some(),
            recent_models: recent_models.len(),
            enabled_models: enabled_models.len(),
        },
        unrepresentable,
        archived_files: 1,
        archived_bytes: raw.len(),
    }))
}

struct ImportedModels {
    artifacts: Vec<PreparedArtifact>,
    report: ModelsReport,
    unrepresentable: Vec<String>,
    archived_files: usize,
    archived_bytes: usize,
}

#[derive(Serialize)]
struct ModelCatalogV1 {
    schema_version: u16,
    providers: Vec<ModelProviderV1>,
}

#[derive(Serialize)]
struct ModelProviderV1 {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    models: Vec<String>,
    model_overrides: Vec<String>,
    request_auth_archived: bool,
}

async fn import_models(legacy_root: &Path) -> Result<Option<ImportedModels>> {
    let Some(raw) = read_optional_bytes(legacy_root, "models.json").await? else {
        return Ok(None);
    };
    let stripped = strip_json_comments_and_trailing_commas(&raw)?;
    let value: Value = serde_json::from_slice(&stripped)?;
    let object = value.as_object().ok_or_else(|| {
        MimirError::Configuration("models.json must contain a JSON object".into())
    })?;
    let providers = object
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            MimirError::Configuration("models.json providers must be an object".into())
        })?;
    let mut catalog = Vec::new();
    let mut model_count = 0usize;
    let mut request_auth_entries = 0usize;
    let mut unrepresentable = Vec::new();
    for (id, value) in providers {
        if id.trim().is_empty() {
            return Err(MimirError::Configuration(
                "models.json provider ids must not be blank".into(),
            ));
        }
        let provider = value.as_object().ok_or_else(|| {
            MimirError::Configuration(format!("models.json provider {id} must be an object"))
        })?;
        let name = optional_nonempty_string(provider, "name", "models.json provider")?;
        let api = optional_nonempty_string(provider, "api", "models.json provider")?;
        let models = model_ids(provider.get("models"), id)?;
        let model_overrides = object_keys(provider.get("modelOverrides"), id, "modelOverrides")?;
        model_count = model_count
            .checked_add(models.len())
            .ok_or_else(|| MimirError::Configuration("models.json model count overflow".into()))?;
        let request_auth_archived =
            provider.contains_key("apiKey") || provider.contains_key("headers");
        if request_auth_archived {
            request_auth_entries += 1;
            unrepresentable.push(format!("models.providers.{id}.request_auth"));
        }
        if provider.contains_key("baseUrl")
            || provider.contains_key("compat")
            || provider.contains_key("authHeader")
        {
            unrepresentable.push(format!("models.providers.{id}.runtime_config"));
        }
        catalog.push(ModelProviderV1 {
            id: id.clone(),
            name,
            api,
            models,
            model_overrides,
            request_auth_archived,
        });
    }
    catalog.sort_by(|left, right| left.id.cmp(&right.id));
    let catalog = ModelCatalogV1 {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        providers: catalog,
    };
    Ok(Some(ImportedModels {
        artifacts: vec![
            PreparedArtifact {
                kind: ArtifactKind::Models,
                source: "models.json".into(),
                target: "config/models.json".into(),
                summary: format!("migrate {} model providers", providers.len()),
                bytes: pretty_json(&value)?,
            },
            PreparedArtifact {
                kind: ArtifactKind::ModelCatalog,
                source: "models.json".into(),
                target: "config/model-catalog.json".into(),
                summary: "migrate redacted versioned model catalog".into(),
                bytes: pretty_json(&catalog)?,
            },
            archive_artifact("models.json", raw.clone()),
        ],
        report: ModelsReport {
            providers: providers.len(),
            models: model_count,
            request_auth_entries_archived: request_auth_entries,
        },
        unrepresentable,
        archived_files: 1,
        archived_bytes: raw.len(),
    }))
}

struct ImportedResources {
    artifacts: Vec<PreparedArtifact>,
    extensions: Vec<String>,
    skills: Vec<String>,
    total_bytes: usize,
}

async fn collect_resources(legacy_root: &Path) -> Result<ImportedResources> {
    let extension_files = collect_resource_files(legacy_root, "extensions").await?;
    let skill_files = collect_resource_files(legacy_root, "skills").await?;
    let file_count = extension_files.len() + skill_files.len();
    if file_count > MAX_RESOURCE_FILES {
        return Err(MimirError::Configuration(format!(
            "migration resource archive exceeds file limit of {MAX_RESOURCE_FILES}"
        )));
    }
    let total_bytes = extension_files
        .iter()
        .chain(&skill_files)
        .try_fold(0usize, |total, file| total.checked_add(file.bytes.len()))
        .ok_or_else(|| {
            MimirError::Configuration("migration resource archive size overflow".into())
        })?;
    if total_bytes > MAX_RESOURCE_ARCHIVE_BYTES {
        return Err(MimirError::Configuration(format!(
            "migration resource archive exceeds size limit of {MAX_RESOURCE_ARCHIVE_BYTES} bytes"
        )));
    }
    let extensions = extension_files
        .iter()
        .map(|file| file.relative.clone())
        .collect();
    let skills = skill_files
        .iter()
        .filter(|file| file.relative.ends_with("/SKILL.md"))
        .map(|file| file.relative.clone())
        .collect();
    let artifacts = extension_files
        .into_iter()
        .chain(skill_files)
        .map(resource_archive_artifact)
        .collect();
    Ok(ImportedResources {
        artifacts,
        extensions,
        skills,
        total_bytes,
    })
}

struct ImportedInventory {
    artifacts: Vec<PreparedArtifact>,
    report: InventoryReport,
    archived_files: usize,
    archived_bytes: usize,
}

async fn import_inventory(
    legacy_root: &Path,
    settings: Option<&Value>,
    discovered_extensions: &[String],
    discovered_skills: &[String],
) -> Result<Option<ImportedInventory>> {
    let inventory_raw = read_optional_bytes(legacy_root, "inventory.json").await?;
    let (value, derived_from_settings, source_name) = match inventory_raw.as_deref() {
        Some(raw) => {
            let value: Value = serde_json::from_slice(raw)?;
            (value, false, "inventory.json")
        }
        None => match settings {
            Some(value) => (value.clone(), true, "settings.json"),
            None if discovered_extensions.is_empty() && discovered_skills.is_empty() => {
                return Ok(None);
            }
            None => (json!({}), true, "resource directories"),
        },
    };
    let object = value.as_object().ok_or_else(|| {
        MimirError::Configuration(format!("{source_name} must contain a JSON object"))
    })?;
    validate_inventory_fields(object, source_name)?;
    let packages = object.get("packages").cloned().unwrap_or_else(|| json!([]));
    let extensions = object
        .get("extensions")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let skills = object.get("skills").cloned().unwrap_or_else(|| json!([]));
    let inventory = json!({
        "schema_version": COMPATIBILITY_SCHEMA_VERSION,
        "packages": packages,
        "extensions": extensions,
        "skills": skills,
        "discovered": {
            "extensions": discovered_extensions,
            "skills": discovered_skills,
        },
    });
    let packages_count = array_len(&inventory["packages"]);
    let extensions_count = array_len(&inventory["extensions"]);
    let skills_count = array_len(&inventory["skills"]);
    let mut artifacts = vec![PreparedArtifact {
        kind: ArtifactKind::Inventory,
        source: source_name.into(),
        target: "config/inventory.json".into(),
        summary: "migrate versioned package, extension, and skill inventory".into(),
        bytes: pretty_json(&inventory)?,
    }];
    let (archived_files, archived_bytes) = if let Some(raw) = inventory_raw {
        let bytes = raw.len();
        artifacts.push(archive_artifact("inventory.json", raw));
        (1, bytes)
    } else {
        (0, 0)
    };
    Ok(Some(ImportedInventory {
        artifacts,
        report: InventoryReport {
            extensions: extensions_count,
            packages: packages_count,
            skills: skills_count,
            derived_from_settings,
        },
        archived_files,
        archived_bytes,
    }))
}

fn validate_inventory_fields(object: &Map<String, Value>, source: &str) -> Result<()> {
    if let Some(packages) = object.get("packages") {
        let packages = packages.as_array().ok_or_else(|| {
            MimirError::Configuration(format!("{source} packages must be an array"))
        })?;
        for package in packages {
            match package {
                Value::String(value) if !value.trim().is_empty() => {}
                Value::Object(value) => {
                    optional_required_source(value, source)?;
                    for field in ["extensions", "skills", "prompts", "themes"] {
                        optional_string_array(value, field, source)?;
                    }
                }
                _ => {
                    return Err(MimirError::Configuration(format!(
                        "{source} package entries must be non-empty strings or source objects"
                    )));
                }
            }
        }
    }
    for field in ["extensions", "skills"] {
        optional_string_array(object, field, source)?;
    }
    Ok(())
}

fn optional_required_source(object: &Map<String, Value>, source: &str) -> Result<()> {
    match object.get("source") {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(()),
        _ => Err(MimirError::Configuration(format!(
            "{source} package source must be a non-empty string"
        ))),
    }
}

fn optional_nonempty_string(
    object: &Map<String, Value>,
    key: &str,
    source: &str,
) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(MimirError::Configuration(format!(
            "{source} {key} must be a non-empty string"
        ))),
    }
}

fn optional_enum(
    object: &Map<String, Value>,
    key: &str,
    allowed: &[&str],
    source: &str,
) -> Result<Option<String>> {
    let value = optional_nonempty_string(object, key, source)?;
    if let Some(value) = &value
        && !allowed.contains(&value.as_str())
    {
        return Err(MimirError::Configuration(format!(
            "{source} {key} has unsupported value"
        )));
    }
    Ok(value)
}

fn optional_string_array(
    object: &Map<String, Value>,
    key: &str,
    source: &str,
) -> Result<Vec<String>> {
    match object.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(value) if !value.trim().is_empty() => Ok(value.clone()),
                _ => Err(MimirError::Configuration(format!(
                    "{source} {key} entries must be non-empty strings"
                ))),
            })
            .collect(),
        Some(_) => Err(MimirError::Configuration(format!(
            "{source} {key} must be an array"
        ))),
    }
}

fn model_ids(value: Option<&Value>, provider: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let models = value.as_array().ok_or_else(|| {
        MimirError::Configuration(format!(
            "models.json provider {provider} models must be an array"
        ))
    })?;
    models
        .iter()
        .map(|model| {
            model
                .as_object()
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    MimirError::Configuration(format!(
                        "models.json provider {provider} model ids must be non-empty strings"
                    ))
                })
        })
        .collect()
}

fn object_keys(value: Option<&Value>, provider: &str, field: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or_else(|| {
        MimirError::Configuration(format!(
            "models.json provider {provider} {field} must be an object"
        ))
    })?;
    Ok(object.keys().cloned().collect())
}

fn strip_json_comments_and_trailing_commas(raw: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| MimirError::Configuration("models.json must contain valid UTF-8".into()))?;
    let bytes = text.as_bytes();
    let mut without_comments = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            without_comments.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            without_comments.push(byte);
            index += 1;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else {
            without_comments.push(byte);
            index += 1;
        }
    }

    let mut normalized = Vec::with_capacity(without_comments.len());
    let mut index = 0usize;
    in_string = false;
    escaped = false;
    while index < without_comments.len() {
        let byte = without_comments[index];
        if in_string {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            normalized.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut lookahead = index + 1;
            while lookahead < without_comments.len()
                && without_comments[lookahead].is_ascii_whitespace()
            {
                lookahead += 1;
            }
            if matches!(without_comments.get(lookahead), Some(b'}' | b']')) {
                index += 1;
                continue;
            }
        }
        normalized.push(byte);
        index += 1;
    }
    Ok(normalized)
}

fn archive_artifact(source: &str, bytes: Vec<u8>) -> PreparedArtifact {
    PreparedArtifact {
        kind: ArtifactKind::CompatibilityArchive,
        source: source.into(),
        target: format!("migration/compatibility/v1/{source}"),
        summary: format!("archive original {source} losslessly"),
        bytes,
    }
}

fn resource_archive_artifact(file: SourceFile) -> PreparedArtifact {
    PreparedArtifact {
        kind: ArtifactKind::ResourceArchive,
        source: file.relative.clone(),
        target: format!("migration/compatibility/v1/resources/{}", file.relative),
        summary: format!("archive legacy resource {}", file.relative),
        bytes: file.bytes,
    }
}

fn add_archive_totals(report: &mut CompatibilityReport, files: usize, bytes: usize) -> Result<()> {
    report.archived_files = report.archived_files.checked_add(files).ok_or_else(|| {
        MimirError::Configuration("compatibility archive file count overflow".into())
    })?;
    report.archived_bytes = report
        .archived_bytes
        .checked_add(bytes)
        .ok_or_else(|| MimirError::Configuration("compatibility archive size overflow".into()))?;
    let max_files = MAX_RESOURCE_FILES + 3;
    let max_bytes = MAX_RESOURCE_ARCHIVE_BYTES + 3 * 16 * 1024 * 1024;
    if report.archived_files > max_files || report.archived_bytes > max_bytes {
        return Err(MimirError::Configuration(
            "compatibility archive exceeds bounded migration limits".into(),
        ));
    }
    Ok(())
}

fn pretty_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}
