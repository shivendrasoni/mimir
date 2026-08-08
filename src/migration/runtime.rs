use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{
    error::{MimirError, Result},
    model::ThinkingLevel,
    provider::registry::{ModelDefinition, ProviderRegistry, model_catalog},
};

const MAX_ACTIVATION_FILE_BYTES: u64 = 4 * 1024 * 1024;

type LoadedModels = (
    Vec<ModelDefinition>,
    BTreeMap<String, MigratedOpenAiProvider>,
    Vec<String>,
);

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigratedPreferences {
    pub schema_version: u16,
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub recent_models: Vec<String>,
    #[serde(default)]
    pub enabled_models: Vec<String>,
    #[serde(default)]
    pub default_thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub default_service_tier: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MigratedRuntimeState {
    pub preferences: Option<MigratedPreferences>,
    models: Vec<ModelDefinition>,
    custom_openai_providers: BTreeMap<String, MigratedOpenAiProvider>,
    blocked_model_providers: Vec<String>,
    blocked_extensions: Vec<String>,
}

/// Audited activation metadata for a migrated OpenAI-compatible provider.
/// Only the environment variable name is retained; credential values are
/// resolved at runtime and never enter migration state or debug output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedOpenAiProvider {
    credential_env: String,
}

impl MigratedOpenAiProvider {
    #[must_use]
    pub fn credential_env(&self) -> &str {
        &self.credential_env
    }
}

impl MigratedRuntimeState {
    pub fn load(state_root: &Path) -> Result<Self> {
        let config = state_root.join("config");
        let preferences = read_optional_json::<MigratedPreferences>(
            state_root,
            &config.join("preferences.json"),
        )?;
        if let Some(preferences) = &preferences {
            if preferences.schema_version != 1 {
                return Err(configuration(format!(
                    "unsupported migrated preferences schema version {}",
                    preferences.schema_version
                )));
            }
            validate_preferences(preferences)?;
        }
        let (models, custom_openai_providers, blocked_model_providers) =
            load_models(state_root, &config.join("models.json"))?;
        let blocked_extensions = blocked_extensions(state_root, &config.join("inventory.json"))?;
        Ok(Self {
            preferences,
            models,
            custom_openai_providers,
            blocked_model_providers,
            blocked_extensions,
        })
    }

    #[must_use]
    pub fn model(&self, provider: &str, model: &str) -> Option<ModelDefinition> {
        self.models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model)
            .cloned()
    }

    #[must_use]
    pub fn models(&self) -> &[ModelDefinition] {
        &self.models
    }

    #[must_use]
    pub fn custom_openai_provider(&self, provider: &str) -> Option<&MigratedOpenAiProvider> {
        self.custom_openai_providers.get(provider)
    }

    #[must_use]
    pub fn blocked_extensions(&self) -> &[String] {
        &self.blocked_extensions
    }

    #[must_use]
    pub fn blocked_model_providers(&self) -> &[String] {
        &self.blocked_model_providers
    }

    #[must_use]
    pub fn model_is_enabled(&self, provider: &str, model: &str) -> bool {
        let Some(preferences) = &self.preferences else {
            return true;
        };
        if preferences.enabled_models.is_empty() {
            return true;
        }
        let selector = format!("{provider}/{model}");
        preferences.enabled_models.iter().any(|pattern| {
            pattern == &selector
                || pattern == model
                || pattern
                    .strip_suffix("/*")
                    .is_some_and(|candidate| candidate == provider)
        })
    }
}

fn validate_preferences(preferences: &MigratedPreferences) -> Result<()> {
    for (field, value) in [
        ("default_provider", preferences.default_provider.as_deref()),
        ("default_model", preferences.default_model.as_deref()),
        (
            "default_service_tier",
            preferences.default_service_tier.as_deref(),
        ),
        ("transport", preferences.transport.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty() || value.len() > 512) {
            return Err(configuration(format!(
                "migrated preference {field} is invalid"
            )));
        }
    }
    if preferences.recent_models.len() > 512 || preferences.enabled_models.len() > 512 {
        return Err(configuration("migrated model preference list is too large"));
    }
    if preferences
        .default_service_tier
        .as_deref()
        .is_some_and(|tier| !matches!(tier, "default" | "priority"))
    {
        return Err(configuration(
            "migrated default service tier must be default or priority",
        ));
    }
    if preferences
        .transport
        .as_deref()
        .is_some_and(|transport| !matches!(transport, "sse" | "auto"))
    {
        return Err(configuration(
            "migrated provider transport is unsupported by the native Rust runtime; use sse or auto",
        ));
    }
    Ok(())
}

fn load_models(state_root: &Path, path: &Path) -> Result<LoadedModels> {
    let Some(value) = read_optional_value(state_root, path)? else {
        return Ok((Vec::new(), BTreeMap::new(), Vec::new()));
    };
    let providers = value
        .as_object()
        .and_then(|root| root.get("providers"))
        .and_then(Value::as_object)
        .ok_or_else(|| configuration("migrated models must contain a providers object"))?;
    let registry = ProviderRegistry::builtin();
    let mut resolved = BTreeMap::new();
    let mut custom_openai_providers = BTreeMap::new();
    let mut blocked = Vec::new();
    for (provider_id, provider_value) in providers {
        if let Some(provider) = registry.get(provider_id) {
            load_provider_models(provider_id, provider_value, provider, &mut resolved)?;
        } else if let Some(provider) =
            load_custom_openai_provider(provider_id, provider_value, &mut resolved)?
        {
            custom_openai_providers.insert(provider_id.clone(), provider);
        } else {
            blocked.push(provider_id.clone());
        }
    }
    blocked.sort();
    Ok((
        resolved.into_values().collect(),
        custom_openai_providers,
        blocked,
    ))
}

fn load_custom_openai_provider(
    provider_id: &str,
    provider_value: &Value,
    resolved: &mut BTreeMap<(String, String), ModelDefinition>,
) -> Result<Option<MigratedOpenAiProvider>> {
    if !valid_provider_id(provider_id) {
        return Ok(None);
    }
    let config = provider_value.as_object().ok_or_else(|| {
        configuration(format!("migrated provider {provider_id} must be an object"))
    })?;
    let api = optional_string(config, "api")?;
    let base_url = optional_secure_url(config, "baseUrl")?;
    // Native activation is deliberately limited to the one wire protocol this
    // adapter can prove. Custom headers and executable/literal credentials stay
    // archived instead of being interpreted.
    if api.as_deref() != Some("openai-completions")
        || base_url.is_none()
        || config.contains_key("headers")
    {
        return Ok(None);
    }
    let Some(credential_env) = optional_string(config, "apiKey")? else {
        return Ok(None);
    };
    if !valid_environment_name(&credential_env) {
        return Ok(None);
    }
    let Some(models) = config.get("models").and_then(Value::as_array) else {
        return Ok(None);
    };
    if models.is_empty() || models.len() > 512 {
        return Ok(None);
    }
    let overrides = optional_object(config, "modelOverrides")?;
    let provider_compat = optional_compat(config, "compat")?;
    let base_url = base_url.expect("checked above");
    let mut activated_models = Vec::with_capacity(models.len());
    for value in models {
        let (id, fields) = migrated_model_entry(provider_id, value)?;
        let mut model = ModelDefinition::from_runtime(
            provider_id,
            id,
            Some(&base_url),
            crate::provider::registry::RuntimeSupport::OpenAiCompatible,
        );
        model.compat.clone_from(&provider_compat);
        if let Some(fields) = fields {
            apply_model_fields(&mut model, fields, provider_id)?;
        }
        if let Some(value) = overrides.get(id) {
            let fields = value.as_object().ok_or_else(|| {
                configuration(format!(
                    "migrated model override {provider_id}/{id} must be an object"
                ))
            })?;
            apply_model_fields(&mut model, fields, provider_id)?;
        }
        if model.api != "openai-completions" || model.base_url.is_empty() {
            return Ok(None);
        }
        activated_models.push(((provider_id.to_owned(), id.to_owned()), model));
    }
    for (key, model) in activated_models {
        resolved.insert(key, model);
    }
    Ok(Some(MigratedOpenAiProvider { credential_env }))
}

fn valid_provider_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_environment_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn load_provider_models(
    provider_id: &str,
    provider_value: &Value,
    provider: &crate::provider::registry::ProviderDefinition,
    resolved: &mut BTreeMap<(String, String), ModelDefinition>,
) -> Result<()> {
    let config = provider_value.as_object().ok_or_else(|| {
        configuration(format!("migrated provider {provider_id} must be an object"))
    })?;
    let api = optional_string(config, "api")?;
    let base_url = optional_secure_url(config, "baseUrl")?;
    let compat = optional_compat(config, "compat")?;
    let overrides = optional_object(config, "modelOverrides")?;
    load_overridden_builtin_models(
        provider_id,
        provider,
        api.as_deref(),
        base_url.as_deref(),
        compat.as_ref(),
        overrides,
        resolved,
    )?;
    load_custom_models(
        provider_id,
        provider,
        config,
        api.as_deref(),
        base_url.as_deref(),
        compat.as_ref(),
        overrides,
        resolved,
    )
}

fn load_overridden_builtin_models(
    provider_id: &str,
    provider: &crate::provider::registry::ProviderDefinition,
    api: Option<&str>,
    base_url: Option<&str>,
    compat: Option<&Value>,
    overrides: &Map<String, Value>,
    resolved: &mut BTreeMap<(String, String), ModelDefinition>,
) -> Result<()> {
    for builtin in model_catalog()
        .iter()
        .filter(|model| model.provider == provider_id)
    {
        if base_url.is_none() && api.is_none() && !overrides.contains_key(&builtin.id) {
            continue;
        }
        let mut model = builtin.clone();
        apply_provider_defaults(&mut model, api, base_url, compat);
        if let Some(value) = overrides.get(&builtin.id) {
            let fields = value.as_object().ok_or_else(|| {
                configuration(format!(
                    "migrated model override {provider_id}/{} must be an object",
                    builtin.id
                ))
            })?;
            apply_model_fields(&mut model, fields, provider_id)?;
        }
        validate_runtime_model(provider, &model)?;
        resolved.insert((model.provider.clone(), model.id.clone()), model);
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "model activation receives explicit audited provider configuration inputs"
)]
fn load_custom_models(
    provider_id: &str,
    provider: &crate::provider::registry::ProviderDefinition,
    config: &Map<String, Value>,
    api: Option<&str>,
    base_url: Option<&str>,
    compat: Option<&Value>,
    overrides: &Map<String, Value>,
    resolved: &mut BTreeMap<(String, String), ModelDefinition>,
) -> Result<()> {
    let Some(models) = config.get("models") else {
        return Ok(());
    };
    let models = models.as_array().ok_or_else(|| {
        configuration(format!(
            "migrated provider {provider_id} models must be an array"
        ))
    })?;
    if models.len() > 512 {
        return Err(configuration(format!(
            "migrated provider {provider_id} has too many models"
        )));
    }
    for value in models {
        let (id, fields) = migrated_model_entry(provider_id, value)?;
        let mut model = ModelDefinition::from_runtime(
            provider_id,
            id,
            base_url.or(provider.base_url),
            provider.runtime_support,
        );
        apply_provider_defaults(&mut model, api, base_url, compat);
        if let Some(fields) = fields {
            apply_model_fields(&mut model, fields, provider_id)?;
        }
        if let Some(value) = overrides.get(id) {
            let fields = value.as_object().ok_or_else(|| {
                configuration(format!(
                    "migrated model override {provider_id}/{id} must be an object"
                ))
            })?;
            apply_model_fields(&mut model, fields, provider_id)?;
        }
        validate_runtime_model(provider, &model)?;
        resolved.insert((provider_id.to_owned(), id.to_owned()), model);
    }
    Ok(())
}

fn migrated_model_entry<'a>(
    provider_id: &str,
    value: &'a Value,
) -> Result<(&'a str, Option<&'a Map<String, Value>>)> {
    let (id, fields) = match value {
        Value::String(id) => (id.trim(), None),
        Value::Object(fields) => (
            fields
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default(),
            Some(fields),
        ),
        _ => {
            return Err(configuration(format!(
                "migrated provider {provider_id} has an invalid model entry"
            )));
        }
    };
    if id.is_empty() || id.len() > 512 {
        return Err(configuration(format!(
            "migrated provider {provider_id} has an invalid model id"
        )));
    }
    Ok((id, fields))
}

fn apply_provider_defaults(
    model: &mut ModelDefinition,
    api: Option<&str>,
    base_url: Option<&str>,
    compat: Option<&Value>,
) {
    if let Some(api) = api {
        api.clone_into(&mut model.api);
    }
    if let Some(base_url) = base_url {
        base_url.clone_into(&mut model.base_url);
    }
    if let Some(compat) = compat {
        model.compat = Some(compat.clone());
    }
}

fn apply_model_fields(
    model: &mut ModelDefinition,
    fields: &Map<String, Value>,
    provider: &str,
) -> Result<()> {
    if let Some(value) = optional_string(fields, "name")? {
        model.name = value;
    }
    if let Some(value) = optional_string(fields, "api")? {
        model.api = value;
    }
    if let Some(value) = optional_secure_url(fields, "baseUrl")? {
        model.base_url = value;
    }
    if let Some(value) = fields.get("reasoning") {
        model.reasoning = value.as_bool().ok_or_else(|| {
            configuration(format!(
                "migrated model {provider}/{} reasoning is invalid",
                model.id
            ))
        })?;
    }
    if let Some(value) = fields.get("contextWindow") {
        model.context_window = bounded_u32(value, "contextWindow", provider, &model.id)?;
    }
    if let Some(value) = fields.get("maxTokens") {
        model.max_tokens = bounded_u32(value, "maxTokens", provider, &model.id)?;
    }
    if let Some(value) = fields.get("input") {
        let input = value.as_array().ok_or_else(|| {
            configuration(format!(
                "migrated model {provider}/{} input is invalid",
                model.id
            ))
        })?;
        let parsed = input
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| matches!(*value, "text" | "image"))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        configuration(format!(
                            "migrated model {provider}/{} input is invalid",
                            model.id
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if parsed.is_empty() {
            return Err(configuration(format!(
                "migrated model {provider}/{} input is empty",
                model.id
            )));
        }
        model.input = parsed;
    }
    if fields.contains_key("compat") {
        model.compat = optional_compat(fields, "compat")?;
    }
    Ok(())
}

fn validate_runtime_model(
    provider: &crate::provider::registry::ProviderDefinition,
    model: &ModelDefinition,
) -> Result<()> {
    if model.base_url.is_empty() {
        return Err(configuration(format!(
            "migrated model {}/{} has no runtime endpoint",
            model.provider, model.id
        )));
    }
    provider.runtime_for_model(model).ok_or_else(|| {
        configuration(format!(
            "migrated model {}/{} uses unsupported API {}",
            model.provider, model.id, model.api
        ))
    })?;
    Ok(())
}

fn blocked_extensions(state_root: &Path, path: &Path) -> Result<Vec<String>> {
    let Some(value) = read_optional_value(state_root, path)? else {
        return Ok(Vec::new());
    };
    let mut blocked = value
        .get("discovered")
        .and_then(|value| value.get("extensions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|path| {
            Path::new(path)
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| matches!(extension, "cjs" | "tsx"))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

fn read_optional_json<T: for<'de> Deserialize<'de>>(
    state_root: &Path,
    path: &Path,
) -> Result<Option<T>> {
    read_optional_bytes(state_root, path)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(MimirError::from))
        .transpose()
}

fn read_optional_value(state_root: &Path, path: &Path) -> Result<Option<Value>> {
    read_optional_json(state_root, path)
}

fn read_optional_bytes(state_root: &Path, path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(configuration(format!(
            "migrated runtime input {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_ACTIVATION_FILE_BYTES {
        return Err(configuration(format!(
            "migrated runtime input {} exceeds {MAX_ACTIVATION_FILE_BYTES} bytes",
            path.display()
        )));
    }
    let canonical_root = std::fs::canonicalize(state_root)?;
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(&canonical_root) {
        return Err(configuration(
            "migrated runtime input escaped the state root",
        ));
    }
    std::fs::read(canonical).map(Some).map_err(MimirError::from)
}

fn optional_string(object: &Map<String, Value>, field: &str) -> Result<Option<String>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 4096 => {
            Ok(Some(value.trim().to_owned()))
        }
        Some(_) => Err(configuration(format!(
            "migrated model field {field} is invalid"
        ))),
    }
}

fn optional_secure_url(object: &Map<String, Value>, field: &str) -> Result<Option<String>> {
    let Some(value) = optional_string(object, field)? else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(&value)
        .map_err(|_| configuration(format!("migrated model {field} is not a valid URL")))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(configuration(format!(
            "migrated model {field} must use HTTPS or loopback HTTP"
        )));
    }
    Ok(Some(value.trim_end_matches('/').to_owned()))
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Map<String, Value>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(empty_object()),
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(configuration(format!(
            "migrated model field {field} must be an object"
        ))),
    }
}

fn optional_compat(object: &Map<String, Value>, field: &str) -> Result<Option<Value>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) if value.len() <= 64 => Ok(Some(Value::Object(value.clone()))),
        Some(_) => Err(configuration(format!(
            "migrated model field {field} must be a bounded object"
        ))),
    }
}

fn empty_object() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn bounded_u32(value: &Value, field: &str, provider: &str, model: &str) -> Result<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            configuration(format!(
                "migrated model {provider}/{model} {field} is invalid"
            ))
        })
}

fn configuration(message: impl Into<String>) -> MimirError {
    MimirError::Configuration(message.into())
}
