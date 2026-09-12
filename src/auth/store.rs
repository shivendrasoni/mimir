use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    atomic::{canonical_state_root, path_lock, prepare_state_path, read_json},
    error::{MimirError, Result},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey { key: String },
    OAuth(OAuthCredential),
}

impl fmt::Debug for AuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => formatter.write_str("ApiKey([REDACTED])"),
            Self::OAuth(value) => value.fmt(formatter),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
}

impl OAuthCredential {
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms <= now_ms
    }
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("access", &"[REDACTED]")
            .field("refresh", &"[REDACTED]")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("account_id", &self.account_id)
            .field("enterprise_url", &self.enterprise_url)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    pub provider: String,
    pub auth_type: String,
    pub expired: bool,
    pub source: AuthSource,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
    Environment,
    Stored,
}

impl AuthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Stored => "stored",
        }
    }
}

pub struct ResolvedCredential {
    secret: SecretString,
    pub source: AuthSource,
    pub auth_type: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialType {
    ApiKey,
    OAuthToken,
}

impl CredentialType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthToken => "oauth",
        }
    }
}

impl ResolvedCredential {
    pub fn expose_for_provider(&self) -> &str {
        self.secret.expose_secret()
    }
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedCredential")
            .field("secret", &"[REDACTED]")
            .field("source", &self.source)
            .field("auth_type", &self.auth_type)
            .finish()
    }
}

#[derive(Clone)]
pub struct AuthStore {
    root: PathBuf,
    path: PathBuf,
}

impl AuthStore {
    /// Opens the durable auth store rooted under the selected state directory.
    ///
    /// # Errors
    ///
    /// Returns an I/O or configuration error when the state root cannot be created or resolved.
    pub fn new(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root)?;
        let root = canonical_state_root(state_root);
        let path = root.join("auth.json");
        Ok(Self { root, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads one stored credential when present.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error for an unreadable or malformed auth store.
    pub async fn get(&self, provider: &str) -> Result<Option<AuthCredential>> {
        Ok(self.load().await?.remove(provider))
    }

    /// Stores an API-key credential for one provider.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for invalid identifiers or blank secrets,
    /// or a persistence error when the auth store cannot be updated.
    pub async fn set_api_key(&self, provider: &str, key: &str) -> Result<()> {
        validate_provider_and_secret(provider, key)?;
        self.update(provider, Some(AuthCredential::ApiKey { key: key.into() }))
            .await
    }

    /// Stores an OAuth credential for one provider.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for invalid identifiers or blank tokens,
    /// or a persistence error when the auth store cannot be updated.
    pub async fn set_oauth(&self, provider: &str, credential: OAuthCredential) -> Result<()> {
        validate_provider_and_secret(provider, &credential.access)?;
        if !credential.refresh.is_empty() {
            validate_provider_and_secret(provider, &credential.refresh)?;
        }
        self.update(provider, Some(AuthCredential::OAuth(credential)))
            .await
    }

    /// Removes a stored credential idempotently.
    ///
    /// # Errors
    ///
    /// Returns a configuration or persistence error when the provider id is invalid
    /// or the auth store cannot be updated.
    pub async fn logout(&self, provider: &str) -> Result<bool> {
        validate_provider(provider)?;
        let lock = path_lock(&self.path);
        let _guard = lock.lock().await;
        let mut data = self.load_unlocked().await?;
        let removed = data.remove(provider).is_some();
        if removed {
            self.persist_unlocked(&data).await?;
        }
        Ok(removed)
    }

    /// Returns the stored credential inventory without exposing raw secrets.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error for an unreadable or malformed auth store.
    pub async fn statuses(&self) -> Result<Vec<AuthStatus>> {
        let now = now_ms();
        Ok(self
            .load()
            .await?
            .into_iter()
            .map(|(provider, credential)| match credential {
                AuthCredential::ApiKey { .. } => AuthStatus {
                    provider,
                    auth_type: "api_key".into(),
                    expired: false,
                    source: AuthSource::Stored,
                },
                AuthCredential::OAuth(value) => AuthStatus {
                    provider,
                    auth_type: "oauth".into(),
                    expired: value.is_expired(now),
                    source: AuthSource::Stored,
                },
            })
            .collect())
    }

    async fn update(&self, provider: &str, credential: Option<AuthCredential>) -> Result<()> {
        validate_provider(provider)?;
        let lock = path_lock(&self.path);
        let _guard = lock.lock().await;
        let mut data = self.load_unlocked().await?;
        if let Some(credential) = credential {
            data.insert(provider.into(), credential);
        } else {
            data.remove(provider);
        }
        self.persist_unlocked(&data).await
    }

    async fn load(&self) -> Result<BTreeMap<String, AuthCredential>> {
        let lock = path_lock(&self.path);
        let _guard = lock.lock().await;
        self.load_unlocked().await
    }

    async fn load_unlocked(&self) -> Result<BTreeMap<String, AuthCredential>> {
        prepare_state_path(&self.root, &self.path).await?;
        Ok(read_json(&self.path).await?.unwrap_or_default())
    }

    async fn persist_unlocked(&self, data: &BTreeMap<String, AuthCredential>) -> Result<()> {
        prepare_state_path(&self.root, &self.path).await?;
        write_private_credentials(&self.path, data).await
    }
}

/// Resolves a provider credential from environment variables or the durable auth store.
///
/// # Errors
///
/// Returns a configuration, persistence, or deserialization error when provider validation
/// fails or the durable store cannot be read safely.
pub async fn resolve_credential(
    store: &AuthStore,
    provider: &str,
    environment: Option<&str>,
) -> Result<Option<ResolvedCredential>> {
    resolve_credential_typed(
        store,
        provider,
        environment.map(|value| (value, CredentialType::ApiKey)),
    )
    .await
}

/// Resolves a provider credential while preserving the environment variable's
/// explicit credential protocol. This prevents OAuth bearer tokens from being
/// silently relabeled as API keys.
///
/// # Errors
///
/// Returns a configuration, persistence, or deserialization error when
/// provider validation fails or the durable store cannot be read safely.
pub async fn resolve_credential_typed(
    store: &AuthStore,
    provider: &str,
    environment: Option<(&str, CredentialType)>,
) -> Result<Option<ResolvedCredential>> {
    if let Some((value, credential_type)) =
        environment.filter(|(value, _)| !value.trim().is_empty())
    {
        return Ok(Some(ResolvedCredential {
            secret: SecretString::from(value.to_owned()),
            source: AuthSource::Environment,
            auth_type: credential_type.as_str(),
        }));
    }
    Ok(store
        .get(provider)
        .await?
        .map(|credential| match credential {
            AuthCredential::ApiKey { key } => ResolvedCredential {
                secret: SecretString::from(key),
                source: AuthSource::Stored,
                auth_type: "api_key",
            },
            AuthCredential::OAuth(value) => ResolvedCredential {
                secret: SecretString::from(value.access),
                source: AuthSource::Stored,
                auth_type: "oauth",
            },
        }))
}

fn validate_provider(provider: &str) -> Result<()> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
    {
        return Err(MimirError::Configuration(
            "provider id contains invalid characters".into(),
        ));
    }
    Ok(())
}

fn validate_provider_and_secret(provider: &str, secret: &str) -> Result<()> {
    validate_provider(provider)?;
    if secret.trim().is_empty() {
        return Err(MimirError::Configuration(
            "credential must not be blank".into(),
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

async fn write_private_credentials(
    path: &Path,
    data: &BTreeMap<String, AuthCredential>,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| MimirError::Session {
        path: path.to_owned(),
        message: "auth state path has no parent".into(),
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".auth-{}.tmp", Uuid::new_v4()));
    let mut file = create_private_file(&temporary)?;
    let bytes = serde_json::to_vec_pretty(data)?;
    if let Err(error) = async {
        file.write_all(&bytes).await?;
        file.write_all(b"\n").await?;
        file.sync_all().await
    }
    .await
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    set_private_permissions(path).await
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<tokio::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<tokio::fs::File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> std::future::Ready<Result<()>> {
    std::future::ready(Ok(()))
}
