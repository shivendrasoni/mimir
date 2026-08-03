use std::{
    env,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use reqwest::{StatusCode, Url, redirect::Policy};
use ring::{rand::SystemRandom, rsa::KeyPair, signature::RSA_PKCS1_SHA256};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;

const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_LOCATION: &str = "us-central1";
const DEFAULT_METADATA_BASE: &str = "http://metadata.google.internal/computeMetadata/v1";
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PRIVATE_KEY_DER_BYTES: usize = 32 * 1024;
const REFRESH_SKEW: Duration = Duration::from_secs(60);

/// Environment inputs used by Google's Application Default Credentials lookup.
///
/// Keeping these values in a plain data object makes the resolver deterministic
/// and lets tests avoid mutating the process environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleAdcEnvironment {
    pub oauth_access_token: Option<String>,
    pub application_credentials: Option<PathBuf>,
    pub well_known_credentials: Option<PathBuf>,
    pub project_id: Option<String>,
    pub quota_project_id: Option<String>,
    pub location: Option<String>,
}

impl GoogleAdcEnvironment {
    /// Captures the supported ADC-related process environment variables.
    #[must_use]
    pub fn from_process_env() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        Self {
            oauth_access_token: non_blank_env("GOOGLE_OAUTH_ACCESS_TOKEN"),
            application_credentials: env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
                .map(PathBuf::from),
            well_known_credentials: home.map(|path| {
                path.join(".config")
                    .join("gcloud")
                    .join("application_default_credentials.json")
            }),
            project_id: non_blank_env("GOOGLE_CLOUD_PROJECT")
                .or_else(|| non_blank_env("GCLOUD_PROJECT")),
            quota_project_id: non_blank_env("GOOGLE_CLOUD_QUOTA_PROJECT"),
            location: non_blank_env("GOOGLE_CLOUD_LOCATION")
                .or_else(|| non_blank_env("GOOGLE_CLOUD_REGION")),
        }
    }
}

fn non_blank_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// A resolved, short-lived Google bearer credential and its Vertex routing
/// context. Secret material is redacted by `Debug` through `SecretString`.
#[derive(Clone, Debug)]
pub struct GoogleAdcCredential {
    pub access_token: SecretString,
    pub project_id: String,
    pub quota_project_id: Option<String>,
    pub location: String,
    pub expires_at: Option<SystemTime>,
}

#[derive(Debug, Error)]
pub enum GoogleAdcError {
    #[error("Google Application Default Credentials are unavailable")]
    Unavailable,
    #[error("Google ADC configuration is invalid: {0}")]
    InvalidConfiguration(String),
    #[error("cannot read Google ADC credential file: {0}")]
    CredentialFile(String),
    #[error("Google ADC token endpoint rejected authentication")]
    Authentication,
    #[error("Google ADC endpoint is temporarily unavailable")]
    UnavailableEndpoint,
    #[error("invalid response from Google ADC endpoint: {0}")]
    Protocol(String),
}

#[derive(Clone)]
pub struct GoogleAdcResolver {
    environment: GoogleAdcEnvironment,
    metadata_base_url: String,
    client: reqwest::Client,
    cache: Arc<Mutex<Option<GoogleAdcCredential>>>,
}

impl GoogleAdcResolver {
    /// Creates a resolver from the current process environment.
    ///
    /// # Errors
    ///
    /// Returns an error if the bounded, no-redirect HTTP client cannot be built.
    pub fn from_process_env() -> Result<Self, GoogleAdcError> {
        Self::new(GoogleAdcEnvironment::from_process_env())
    }

    /// Creates a deterministic resolver from explicit environment inputs.
    ///
    /// # Errors
    ///
    /// Returns an error if the bounded, no-redirect HTTP client cannot be built.
    pub fn new(environment: GoogleAdcEnvironment) -> Result<Self, GoogleAdcError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                GoogleAdcError::Protocol(format!("cannot build HTTP client: {error}"))
            })?;
        Ok(Self {
            environment,
            metadata_base_url: DEFAULT_METADATA_BASE.to_owned(),
            client,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    /// Overrides the metadata base URL. Only the real metadata host or an HTTP
    /// loopback endpoint (for hermetic tests) is accepted.
    ///
    /// # Errors
    ///
    /// Returns an error for an SSRF-capable endpoint.
    pub fn with_metadata_base_url(
        mut self,
        base_url: impl Into<String>,
    ) -> Result<Self, GoogleAdcError> {
        let base_url = base_url.into();
        validate_metadata_base(&base_url)?;
        base_url
            .trim_end_matches('/')
            .clone_into(&mut self.metadata_base_url);
        Ok(self)
    }

    /// Resolves ADC using Google's precedence: explicit token seam, explicit
    /// credential file, well-known local ADC file, then attached-service-account
    /// metadata. Refreshable tokens are cached until shortly before expiry.
    ///
    /// # Errors
    ///
    /// Returns an error when credentials are absent, malformed, rejected, or
    /// require an unsafe network endpoint.
    pub async fn resolve(&self) -> Result<GoogleAdcCredential, GoogleAdcError> {
        let mut cache = self.cache.lock().await;
        if let Some(credential) = cache.as_ref().filter(|value| cache_is_fresh(value)) {
            return Ok(credential.clone());
        }

        let credential = self.resolve_uncached().await?;
        *cache = Some(credential.clone());
        Ok(credential)
    }

    async fn resolve_uncached(&self) -> Result<GoogleAdcCredential, GoogleAdcError> {
        if let Some(token) = self.environment.oauth_access_token.as_deref() {
            validate_non_blank(token, "GOOGLE_OAUTH_ACCESS_TOKEN")?;
            let project_id = self.environment.project_id.clone().ok_or_else(|| {
                GoogleAdcError::InvalidConfiguration(
                    "GOOGLE_CLOUD_PROJECT is required with GOOGLE_OAUTH_ACCESS_TOKEN".into(),
                )
            })?;
            return self.finish_credential(token.to_owned(), project_id, None, None);
        }

        if let Some(path) = self.credential_path().await {
            let file = read_credential_file(&path).await?;
            return match file.kind.as_str() {
                "authorized_user" => self.exchange_authorized_user(file).await,
                "service_account" => self.exchange_service_account(file).await,
                _ => Err(GoogleAdcError::InvalidConfiguration(
                    "unsupported ADC credential type".into(),
                )),
            };
        }

        self.resolve_metadata().await
    }

    async fn credential_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.environment.application_credentials {
            return Some(path.clone());
        }
        match &self.environment.well_known_credentials {
            Some(path) if tokio::fs::try_exists(path).await.unwrap_or(false) => Some(path.clone()),
            _ => None,
        }
    }

    async fn exchange_authorized_user(
        &self,
        file: CredentialFile,
    ) -> Result<GoogleAdcCredential, GoogleAdcError> {
        let client_id = required(file.client_id, "client_id")?;
        let client_secret = required(file.client_secret, "client_secret")?;
        let refresh_token = required(file.refresh_token, "refresh_token")?;
        let token_uri = file
            .token_uri
            .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
        validate_oauth_endpoint(&token_uri)?;
        let fields = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
        ];
        let response = self
            .client
            .post(token_uri)
            .form(&fields)
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let token = read_token_response(response).await?;
        let project_id = self
            .environment
            .project_id
            .clone()
            .or(file.project_id)
            .ok_or_else(|| {
                GoogleAdcError::InvalidConfiguration(
                    "GOOGLE_CLOUD_PROJECT is required for authorized_user ADC".into(),
                )
            })?;
        self.finish_credential(
            token.access_token,
            project_id,
            file.quota_project_id,
            Some(token.expires_in),
        )
    }

    async fn exchange_service_account(
        &self,
        file: CredentialFile,
    ) -> Result<GoogleAdcCredential, GoogleAdcError> {
        let client_email = required(file.client_email, "client_email")?;
        let private_key = required(file.private_key, "private_key")?;
        let token_uri = file
            .token_uri
            .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
        validate_oauth_endpoint(&token_uri)?;
        let assertion = create_service_account_assertion(&client_email, &private_key, &token_uri)?;
        let fields = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ];
        let response = self
            .client
            .post(token_uri)
            .form(&fields)
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        let token = read_token_response(response).await?;
        let project_id = self
            .environment
            .project_id
            .clone()
            .or(file.project_id)
            .ok_or_else(|| {
                GoogleAdcError::InvalidConfiguration(
                    "service_account ADC is missing project_id".into(),
                )
            })?;
        self.finish_credential(
            token.access_token,
            project_id,
            file.quota_project_id,
            Some(token.expires_in),
        )
    }

    async fn resolve_metadata(&self) -> Result<GoogleAdcCredential, GoogleAdcError> {
        validate_metadata_base(&self.metadata_base_url)?;
        let project_id = match &self.environment.project_id {
            Some(project) => project.clone(),
            None => self.metadata_text("project/project-id").await?,
        };
        let response = self
            .metadata_get("instance/service-accounts/default/token")
            .await?;
        let token: TokenResponse = serde_json::from_slice(&response).map_err(|_| {
            GoogleAdcError::Protocol("metadata token response is not valid JSON".into())
        })?;
        self.finish_credential(token.access_token, project_id, None, Some(token.expires_in))
    }

    async fn metadata_text(&self, path: &str) -> Result<String, GoogleAdcError> {
        let body = self.metadata_get(path).await?;
        let value = String::from_utf8(body)
            .map_err(|_| GoogleAdcError::Protocol("metadata response is not UTF-8".into()))?;
        let value = value.trim().to_owned();
        validate_non_blank(&value, "metadata value")?;
        Ok(value)
    }

    async fn metadata_get(&self, path: &str) -> Result<Vec<u8>, GoogleAdcError> {
        let endpoint = format!("{}/{path}", self.metadata_base_url);
        let response = self
            .client
            .get(endpoint)
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        if !response.status().is_success() {
            return Err(classify_status(response.status()));
        }
        let flavor_is_google = response
            .headers()
            .get("Metadata-Flavor")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("Google"));
        if !flavor_is_google {
            return Err(GoogleAdcError::Protocol(
                "metadata response omitted Metadata-Flavor: Google".into(),
            ));
        }
        read_bounded(response).await
    }

    fn finish_credential(
        &self,
        access_token: String,
        project_id: String,
        file_quota_project: Option<String>,
        expires_in: Option<u64>,
    ) -> Result<GoogleAdcCredential, GoogleAdcError> {
        validate_non_blank(&access_token, "access token")?;
        validate_identifier(&project_id, "project id")?;
        let location = self
            .environment
            .location
            .clone()
            .unwrap_or_else(|| DEFAULT_LOCATION.into());
        validate_identifier(&location, "location")?;
        let quota_project_id = self
            .environment
            .quota_project_id
            .clone()
            .or(file_quota_project)
            .map(|value| {
                validate_identifier(&value, "quota project id")?;
                Ok(value)
            })
            .transpose()?;
        let expires_at = expires_in
            .map(|seconds| SystemTime::now() + Duration::from_secs(seconds.min(24 * 60 * 60)));
        Ok(GoogleAdcCredential {
            access_token: SecretString::from(access_token),
            project_id,
            quota_project_id,
            location,
            expires_at,
        })
    }
}

fn cache_is_fresh(credential: &GoogleAdcCredential) -> bool {
    credential.expires_at.is_none_or(|expiry| {
        SystemTime::now()
            .checked_add(REFRESH_SKEW)
            .is_some_and(|threshold| threshold < expiry)
    })
}

#[derive(Deserialize)]
struct CredentialFile {
    #[serde(rename = "type")]
    kind: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    client_email: Option<String>,
    private_key: Option<String>,
    token_uri: Option<String>,
    project_id: Option<String>,
    quota_project_id: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

async fn read_credential_file(path: &Path) -> Result<CredentialFile, GoogleAdcError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| GoogleAdcError::CredentialFile(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_CREDENTIAL_BYTES {
        return Err(GoogleAdcError::CredentialFile(
            "credential path must be a regular file no larger than 1 MiB".into(),
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| GoogleAdcError::CredentialFile(error.to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| GoogleAdcError::CredentialFile("credential file is not valid ADC JSON".into()))
}

fn required(value: Option<String>, field: &str) -> Result<String, GoogleAdcError> {
    let value =
        value.ok_or_else(|| GoogleAdcError::InvalidConfiguration(format!("missing {field}")))?;
    validate_non_blank(&value, field)?;
    Ok(value)
}

fn validate_non_blank(value: &str, field: &str) -> Result<(), GoogleAdcError> {
    if value.trim().is_empty() {
        Err(GoogleAdcError::InvalidConfiguration(format!(
            "blank {field}"
        )))
    } else {
        Ok(())
    }
}

fn validate_identifier(value: &str, field: &str) -> Result<(), GoogleAdcError> {
    validate_non_blank(value, field)?;
    if value.len() > 253
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(GoogleAdcError::InvalidConfiguration(format!(
            "invalid {field}"
        )));
    }
    Ok(())
}

fn validate_metadata_base(value: &str) -> Result<(), GoogleAdcError> {
    let url = Url::parse(value)
        .map_err(|_| GoogleAdcError::InvalidConfiguration("invalid metadata URL".into()))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GoogleAdcError::InvalidConfiguration(
            "unsafe metadata URL".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| GoogleAdcError::InvalidConfiguration("metadata URL has no host".into()))?;
    let allowed = host.eq_ignore_ascii_case("metadata.google.internal") || is_loopback(host);
    if !allowed {
        return Err(GoogleAdcError::InvalidConfiguration(
            "metadata URL must use the Google metadata host or loopback".into(),
        ));
    }
    Ok(())
}

fn validate_oauth_endpoint(value: &str) -> Result<(), GoogleAdcError> {
    let url = Url::parse(value)
        .map_err(|_| GoogleAdcError::InvalidConfiguration("invalid OAuth token URI".into()))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GoogleAdcError::InvalidConfiguration(
            "unsafe OAuth token URI".into(),
        ));
    }
    let host = url.host_str().ok_or_else(|| {
        GoogleAdcError::InvalidConfiguration("OAuth token URI has no host".into())
    })?;
    let lower_host = host.to_ascii_lowercase();
    let is_google_endpoint = lower_host == "accounts.google.com"
        || lower_host == "googleapis.com"
        || lower_host
            .strip_suffix(".googleapis.com")
            .is_some_and(|prefix| !prefix.is_empty());
    let is_test_endpoint = is_loopback(host);
    if !(url.scheme() == "https" && is_google_endpoint)
        && !((url.scheme() == "http" || url.scheme() == "https") && is_test_endpoint)
    {
        return Err(GoogleAdcError::InvalidConfiguration(
            "OAuth token URI must be a Google HTTPS endpoint".into(),
        ));
    }
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn create_service_account_assertion(
    client_email: &str,
    private_key: &str,
    token_uri: &str,
) -> Result<String, GoogleAdcError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| GoogleAdcError::Protocol("system clock is before Unix epoch".into()))?
        .as_secs();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss": client_email,
            "scope": CLOUD_PLATFORM_SCOPE,
            "aud": token_uri,
            "iat": now,
            "exp": now.saturating_add(3600),
        }))
        .map_err(|_| GoogleAdcError::Protocol("cannot encode service account claims".into()))?,
    );
    let signing_input = format!("{header}.{claims}");
    let key_der = decode_pkcs8_private_key(private_key)?;
    let key = KeyPair::from_pkcs8(&key_der).map_err(|_| {
        GoogleAdcError::InvalidConfiguration("invalid service account private_key".into())
    })?;
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| GoogleAdcError::Protocol("cannot sign service account assertion".into()))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn decode_pkcs8_private_key(private_key: &str) -> Result<Vec<u8>, GoogleAdcError> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let trimmed = private_key.trim();
    let body = trimmed
        .strip_prefix(BEGIN)
        .and_then(|value| value.strip_suffix(END))
        .ok_or_else(|| {
            GoogleAdcError::InvalidConfiguration("invalid service account private_key".into())
        })?;
    let encoded = body
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    if encoded.is_empty() || encoded.len() > MAX_PRIVATE_KEY_DER_BYTES.saturating_mul(2) {
        return Err(GoogleAdcError::InvalidConfiguration(
            "invalid service account private_key".into(),
        ));
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            GoogleAdcError::InvalidConfiguration("invalid service account private_key".into())
        })?;
    if der.is_empty() || der.len() > MAX_PRIVATE_KEY_DER_BYTES {
        return Err(GoogleAdcError::InvalidConfiguration(
            "invalid service account private_key".into(),
        ));
    }
    Ok(der)
}

async fn read_token_response(response: reqwest::Response) -> Result<TokenResponse, GoogleAdcError> {
    if !response.status().is_success() {
        return Err(classify_status(response.status()));
    }
    let bytes = read_bounded(response).await?;
    let token: TokenResponse = serde_json::from_slice(&bytes)
        .map_err(|_| GoogleAdcError::Protocol("token response is not valid JSON".into()))?;
    validate_non_blank(&token.access_token, "access token")?;
    if token.expires_in == 0 {
        return Err(GoogleAdcError::Protocol(
            "token expiry must be positive".into(),
        ));
    }
    Ok(token)
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, GoogleAdcError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err(GoogleAdcError::Protocol(
            "ADC response exceeds 64 KiB".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| classify_transport(&error))?;
        if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            return Err(GoogleAdcError::Protocol(
                "ADC response exceeds 64 KiB".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn classify_status(status: StatusCode) -> GoogleAdcError {
    if status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::BAD_REQUEST
    {
        GoogleAdcError::Authentication
    } else if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        GoogleAdcError::UnavailableEndpoint
    } else {
        GoogleAdcError::Protocol(format!("ADC endpoint returned HTTP {status}"))
    }
}

fn classify_transport(error: &reqwest::Error) -> GoogleAdcError {
    if error.is_timeout() || error.is_connect() {
        GoogleAdcError::UnavailableEndpoint
    } else {
        GoogleAdcError::Protocol("ADC HTTP transport failed".into())
    }
}

impl From<GoogleAdcError> for super::ProviderError {
    fn from(error: GoogleAdcError) -> Self {
        match error {
            GoogleAdcError::Authentication => Self::Authentication,
            GoogleAdcError::UnavailableEndpoint => Self::Unavailable {
                message: "Google ADC endpoint unavailable".into(),
            },
            other => Self::Protocol {
                message: other.to_string(),
            },
        }
    }
}

/// Convenience accessor for factory wiring without exposing tokens in logs.
impl GoogleAdcCredential {
    #[must_use]
    pub fn exposed_access_token(&self) -> &str {
        self.access_token.expose_secret()
    }
}
