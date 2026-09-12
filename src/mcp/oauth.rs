use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    atomic,
    auth::OAuthCredential,
    error::{MimirError, Result},
    mcp::{
        config::{McpHttpConfig, validate_remote_url},
        protocol::{JSONRPC_VERSION, PROTOCOL_VERSION},
    },
};

const EXPIRY_BUFFER_SECONDS: u64 = 300;
const DEFAULT_EXPIRY_SECONDS: u64 = 3600;
const MAX_METADATA_ENTRIES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpOAuthClientMetadata {
    pub token_endpoint: String,
    pub client_id: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

#[derive(Clone)]
pub struct McpOAuthCredentialBundle {
    pub credential: OAuthCredential,
    pub metadata: McpOAuthClientMetadata,
}

impl fmt::Debug for McpOAuthCredentialBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthCredentialBundle")
            .field("credential", &self.credential)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Clone)]
pub struct McpOAuthAuthorization {
    authorization_url: String,
    state: String,
    code_verifier: String,
    redirect_uri: String,
    metadata: McpOAuthClientMetadata,
}

impl McpOAuthAuthorization {
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }
}

impl fmt::Debug for McpOAuthAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthAuthorization")
            .field("authorization_url", &safe_url_text(&self.authorization_url))
            .field("state", &"[REDACTED]")
            .field("code_verifier", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[async_trait]
pub trait McpOAuthCodeReceiver: Send + Sync {
    async fn receive_code(&self, authorization: &McpOAuthAuthorization) -> Result<String>;
}

#[derive(Clone)]
pub struct McpOAuthClient {
    client: reqwest::Client,
    max_response_bytes: usize,
    request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct McpOAuthClientMetadataStore {
    root: PathBuf,
    path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataState {
    #[serde(default = "metadata_schema_version")]
    schema_version: u16,
    #[serde(default)]
    servers: BTreeMap<String, McpOAuthClientMetadata>,
}

impl Default for MetadataState {
    fn default() -> Self {
        Self {
            schema_version: metadata_schema_version(),
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
}

struct Discovery {
    resource: ProtectedResourceMetadata,
    authorization: AuthorizationServerMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpAuthorizationChallenge {
    resource_metadata: Option<String>,
    scopes: Option<Vec<String>>,
}

impl McpAuthorizationChallenge {
    pub fn resource_metadata(&self) -> Option<&str> {
        self.resource_metadata.as_deref()
    }

    pub fn scopes(&self) -> Option<&[String]> {
        self.scopes.as_deref()
    }
}

impl McpOAuthClient {
    /// Builds a redirect-disabled, bounded OAuth HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error when timeout or response bounds are invalid.
    pub fn new(timeout: Duration, max_response_bytes: usize) -> Result<Self> {
        if timeout.is_zero() || max_response_bytes == 0 || max_response_bytes > 1024 * 1024 {
            return Err(MimirError::Configuration(
                "MCP OAuth HTTP bounds are invalid".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| MimirError::Configuration("MCP OAuth client setup failed".into()))?;
        Ok(Self {
            client,
            max_response_bytes,
            request_timeout: timeout,
        })
    }

    /// Runs discovery, authorization handoff, callback receipt, and token exchange.
    ///
    /// # Errors
    ///
    /// Returns an error for discovery, registration, callback validation, or token failures.
    pub async fn authorize<R: McpOAuthCodeReceiver>(
        &self,
        remote: &McpHttpConfig,
        resource_metadata_hint: Option<&str>,
        redirect_uri: &str,
        receiver: &R,
    ) -> Result<McpOAuthCredentialBundle> {
        self.authorize_with_headers(
            remote,
            &BTreeMap::new(),
            resource_metadata_hint,
            redirect_uri,
            receiver,
        )
        .await
    }

    /// Runs authorization while applying the same resolved non-secret routing headers used by
    /// normal MCP HTTP requests to the initial challenge probe.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid headers, discovery, registration, callback validation, or
    /// token failures.
    pub async fn authorize_with_headers<R: McpOAuthCodeReceiver>(
        &self,
        remote: &McpHttpConfig,
        request_headers: &BTreeMap<String, String>,
        resource_metadata_hint: Option<&str>,
        redirect_uri: &str,
        receiver: &R,
    ) -> Result<McpOAuthCredentialBundle> {
        let challenge = self
            .authorization_challenge(remote, request_headers)
            .await?;
        let resource_metadata_hint = challenge
            .resource_metadata
            .as_deref()
            .or(resource_metadata_hint);
        let flow = self
            .begin_authorization_with_scopes(
                remote,
                resource_metadata_hint,
                redirect_uri,
                challenge.scopes.as_deref(),
            )
            .await?;
        let callback = receiver.receive_code(&flow).await?;
        self.exchange_code(&flow, &callback).await
    }

    /// Runs a bounded reauthorization flow using authoritative metadata and scopes retained from
    /// an MCP `WWW-Authenticate` challenge.
    ///
    /// # Errors
    ///
    /// Returns an error for discovery, registration, callback validation, or token failures.
    pub async fn authorize_for_challenge<R: McpOAuthCodeReceiver>(
        &self,
        remote: &McpHttpConfig,
        challenge: &McpAuthorizationChallenge,
        redirect_uri: &str,
        receiver: &R,
    ) -> Result<McpOAuthCredentialBundle> {
        let flow = self
            .begin_authorization_with_scopes(
                remote,
                challenge.resource_metadata(),
                redirect_uri,
                challenge.scopes(),
            )
            .await?;
        let callback = receiver.receive_code(&flow).await?;
        self.exchange_code(&flow, &callback).await
    }

    /// Discovers OAuth metadata and creates a PKCE-protected authorization request.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe metadata, unsupported registration, or invalid redirects.
    pub async fn begin_authorization(
        &self,
        remote: &McpHttpConfig,
        resource_metadata_hint: Option<&str>,
        redirect_uri: &str,
    ) -> Result<McpOAuthAuthorization> {
        self.begin_authorization_with_scopes(remote, resource_metadata_hint, redirect_uri, None)
            .await
    }

    async fn begin_authorization_with_scopes(
        &self,
        remote: &McpHttpConfig,
        resource_metadata_hint: Option<&str>,
        redirect_uri: &str,
        challenged_scopes: Option<&[String]>,
    ) -> Result<McpOAuthAuthorization> {
        remote.validate()?;
        validate_redirect_uri(redirect_uri)?;
        let discovery = tokio::time::timeout(
            self.request_timeout,
            self.discover(remote, resource_metadata_hint),
        )
        .await
        .map_err(|_| MimirError::Protocol("MCP OAuth discovery timed out".into()))??;
        let client_id = if let Some(client_id) = &remote.client_id {
            client_id.clone()
        } else {
            let endpoint = discovery
                .authorization
                .registration_endpoint
                .as_deref()
                .ok_or_else(|| {
                    MimirError::Configuration(
                        "MCP OAuth server has no registration endpoint and no client id is configured"
                            .into(),
                    )
                })?;
            self.register_client(endpoint, redirect_uri).await?
        };
        let scopes = if let Some(scopes) = challenged_scopes {
            scopes.to_vec()
        } else if remote.scopes.is_empty() {
            discovery.resource.scopes_supported.clone()
        } else {
            remote.scopes.clone()
        };
        let code_verifier = random_urlsafe();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        let state = random_urlsafe();
        let mut authorization_url = secure_url(
            "MCP OAuth authorization endpoint",
            &discovery.authorization.authorization_endpoint,
        )?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("client_id", &client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("resource", &discovery.resource.resource);
            if !scopes.is_empty() {
                query.append_pair("scope", &scopes.join(" "));
            }
        }
        Ok(McpOAuthAuthorization {
            authorization_url: authorization_url.into(),
            state,
            code_verifier,
            redirect_uri: redirect_uri.into(),
            metadata: McpOAuthClientMetadata {
                token_endpoint: discovery.authorization.token_endpoint,
                client_id,
                resource: discovery.resource.resource,
                scopes,
            },
        })
    }

    /// Validates a callback/manual input and exchanges its code for bounded credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for state mismatch, missing codes, or invalid token responses.
    pub async fn exchange_code(
        &self,
        flow: &McpOAuthAuthorization,
        callback_input: &str,
    ) -> Result<McpOAuthCredentialBundle> {
        let code = parse_callback_input(callback_input, &flow.state, &flow.redirect_uri)?;
        let form = BTreeMap::from([
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("redirect_uri", flow.redirect_uri.clone()),
            ("client_id", flow.metadata.client_id.clone()),
            ("code_verifier", flow.code_verifier.clone()),
            ("resource", flow.metadata.resource.clone()),
        ]);
        let token = self
            .token_request(&flow.metadata.token_endpoint, &form)
            .await?;
        Ok(McpOAuthCredentialBundle {
            credential: token_credential(token, None),
            metadata: flow.metadata.clone(),
        })
    }

    /// Refreshes a stored MCP credential while retaining a rotated-or-omitted refresh token.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, refresh credentials, or the token response is invalid.
    pub async fn refresh(
        &self,
        remote: &McpHttpConfig,
        metadata: &McpOAuthClientMetadata,
        credential: &OAuthCredential,
    ) -> Result<McpOAuthCredentialBundle> {
        remote.validate()?;
        validate_metadata(metadata)?;
        if metadata.resource != remote.url {
            return Err(MimirError::Configuration(
                "MCP OAuth metadata no longer matches the configured server; login is required"
                    .into(),
            ));
        }
        if remote
            .client_id
            .as_ref()
            .is_some_and(|client_id| client_id != &metadata.client_id)
        {
            return Err(MimirError::Configuration(
                "MCP OAuth client metadata changed; login is required".into(),
            ));
        }
        if credential.refresh.trim().is_empty() {
            return Err(MimirError::Configuration(
                "MCP OAuth refresh token is unavailable; login is required".into(),
            ));
        }
        let form = BTreeMap::from([
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", credential.refresh.clone()),
            ("client_id", metadata.client_id.clone()),
            ("resource", metadata.resource.clone()),
        ]);
        let token = self.token_request(&metadata.token_endpoint, &form).await?;
        Ok(McpOAuthCredentialBundle {
            credential: token_credential(token, Some(&credential.refresh)),
            metadata: metadata.clone(),
        })
    }

    async fn discover(
        &self,
        remote: &McpHttpConfig,
        resource_metadata_hint: Option<&str>,
    ) -> Result<Discovery> {
        let endpoint = validate_remote_url("MCP remote URL", &remote.url)?;
        let resource = self
            .discover_resource(&endpoint, resource_metadata_hint)
            .await?;
        if resource.resource != remote.url {
            return Err(MimirError::Protocol(
                "MCP protected-resource metadata does not match the configured server".into(),
            ));
        }
        if resource.authorization_servers.is_empty() || resource.authorization_servers.len() > 8 {
            return Err(MimirError::Protocol(
                "MCP protected-resource metadata has invalid authorization servers".into(),
            ));
        }
        validate_scopes(&resource.scopes_supported)?;
        let mut authorization = None;
        for issuer in &resource.authorization_servers {
            if let Ok(candidate) = self.discover_authorization_server(issuer).await {
                authorization = Some(candidate);
                break;
            }
        }
        let authorization = authorization.ok_or_else(|| {
            MimirError::Protocol(
                "MCP OAuth authorization-server discovery found no usable PKCE issuer".into(),
            )
        })?;
        Ok(Discovery {
            resource,
            authorization,
        })
    }

    async fn discover_resource(
        &self,
        endpoint: &reqwest::Url,
        hint: Option<&str>,
    ) -> Result<ProtectedResourceMetadata> {
        let mut candidates = Vec::new();
        if let Some(hint) = hint {
            candidates.push(secure_url("MCP resource metadata URL", hint)?);
        }
        let mut path_specific = endpoint.clone();
        path_specific.set_query(None);
        path_specific.set_fragment(None);
        path_specific.set_path(&format!(
            "/.well-known/oauth-protected-resource{}",
            endpoint.path()
        ));
        candidates.push(path_specific);
        let mut root = endpoint.clone();
        root.set_query(None);
        root.set_fragment(None);
        root.set_path("/.well-known/oauth-protected-resource");
        if !candidates.contains(&root) {
            candidates.push(root);
        }
        self.fetch_first(&candidates, "MCP protected-resource metadata")
            .await
    }

    async fn discover_authorization_server(
        &self,
        issuer: &str,
    ) -> Result<AuthorizationServerMetadata> {
        let issuer_url = secure_url("MCP OAuth issuer", issuer)?;
        let suffix = issuer_url.path().trim_matches('/');
        let origin = issuer_url.origin().ascii_serialization();
        let mut paths = Vec::new();
        if suffix.is_empty() {
            paths.push("/.well-known/oauth-authorization-server".into());
            paths.push("/.well-known/openid-configuration".into());
        } else {
            paths.push(format!("/.well-known/oauth-authorization-server/{suffix}"));
            paths.push(format!("/.well-known/openid-configuration/{suffix}"));
            paths.push(format!("/{suffix}/.well-known/openid-configuration"));
        }
        let candidates = paths
            .into_iter()
            .map(|path| secure_url("MCP OAuth metadata URL", &format!("{origin}{path}")))
            .collect::<Result<Vec<_>>>()?;
        let metadata: AuthorizationServerMetadata = self
            .fetch_first(&candidates, "MCP authorization-server metadata")
            .await?;
        if metadata.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(MimirError::Protocol(
                "MCP OAuth metadata issuer mismatch".into(),
            ));
        }
        secure_url(
            "MCP OAuth authorization endpoint",
            &metadata.authorization_endpoint,
        )?;
        secure_url("MCP OAuth token endpoint", &metadata.token_endpoint)?;
        if let Some(endpoint) = &metadata.registration_endpoint {
            secure_url("MCP OAuth registration endpoint", endpoint)?;
        }
        if !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(MimirError::Protocol(
                "MCP OAuth authorization server does not advertise PKCE S256 support".into(),
            ));
        }
        Ok(metadata)
    }

    async fn authorization_challenge(
        &self,
        remote: &McpHttpConfig,
        request_headers: &BTreeMap<String, String>,
    ) -> Result<McpAuthorizationChallenge> {
        remote.validate()?;
        let mut configured_headers = request_headers.clone();
        configured_headers.extend(remote.headers.clone());
        let mut headers = HeaderMap::new();
        for (name, value) in configured_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                MimirError::Configuration("MCP OAuth probe header name is invalid".into())
            })?;
            let value = HeaderValue::from_str(&value).map_err(|_| {
                MimirError::Configuration("MCP OAuth probe header value is invalid".into())
            })?;
            headers.insert(name, value);
        }
        let response = self
            .client
            .post(&remote.url)
            .headers(headers)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mimir", "version": env!("CARGO_PKG_VERSION")}
                }
            }))
            .send()
            .await;
        let Ok(response) = response else {
            return Ok(McpAuthorizationChallenge::default());
        };
        if response.status().as_u16() != 401 && response.status().as_u16() != 403 {
            return Ok(McpAuthorizationChallenge::default());
        }
        for value in response.headers().get_all("WWW-Authenticate") {
            let Ok(value) = value.to_str() else {
                continue;
            };
            if let Some(challenge) = parse_bearer_challenge(value)? {
                return Ok(challenge);
            }
        }
        Ok(McpAuthorizationChallenge::default())
    }

    async fn fetch_first<T: DeserializeOwned>(
        &self,
        candidates: &[reqwest::Url],
        label: &str,
    ) -> Result<T> {
        for candidate in candidates {
            if let Ok(value) = self.get_json(candidate).await {
                return Ok(value);
            }
        }
        Err(MimirError::Protocol(format!(
            "{label} discovery failed after {} bounded attempts",
            candidates.len()
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &reqwest::Url) -> Result<T> {
        let response = self.client.get(url.clone()).send().await.map_err(|_| {
            MimirError::Protocol(format!("OAuth request to {} failed", safe_url(url)))
        })?;
        self.parse_json_response(response, "OAuth metadata request")
            .await
    }

    async fn register_client(&self, endpoint: &str, redirect_uri: &str) -> Result<String> {
        let endpoint = secure_url("MCP OAuth registration endpoint", endpoint)?;
        let response = self
            .client
            .post(endpoint)
            .json(&json!({
                "client_name": "Mimir",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            }))
            .send()
            .await
            .map_err(|_| MimirError::Protocol("MCP client registration failed".into()))?;
        let value: RegistrationResponse = self
            .parse_json_response(response, "MCP client registration")
            .await?;
        if value.client_id.trim().is_empty()
            || value.client_id.len() > 2048
            || value.client_id.chars().any(char::is_control)
        {
            return Err(MimirError::Protocol(
                "MCP client registration returned an invalid client id".into(),
            ));
        }
        Ok(value.client_id)
    }

    async fn token_request(
        &self,
        endpoint: &str,
        form: &BTreeMap<&str, String>,
    ) -> Result<TokenResponse> {
        let endpoint = secure_url("MCP OAuth token endpoint", endpoint)?;
        let response = self
            .client
            .post(endpoint)
            .form(form)
            .send()
            .await
            .map_err(|_| MimirError::Protocol("MCP OAuth token request failed".into()))?;
        let token: TokenResponse = self
            .parse_json_response(response, "MCP OAuth token request")
            .await?;
        if token.access_token.trim().is_empty()
            || token.access_token.len() > self.max_response_bytes
            || token
                .token_type
                .as_deref()
                .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
        {
            return Err(MimirError::Protocol(
                "MCP OAuth token response is invalid".into(),
            ));
        }
        Ok(token)
    }

    async fn parse_json_response<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        label: &str,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(MimirError::Protocol(format!(
                "{label} failed with HTTP {}",
                response.status().as_u16()
            )));
        }
        let bytes = bounded_bytes(response, self.max_response_bytes).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| MimirError::Protocol(format!("{label} returned invalid JSON")))
    }
}

impl McpOAuthClientMetadataStore {
    /// Opens the private, non-secret OAuth client metadata store.
    ///
    /// # Errors
    ///
    /// Returns an error when the state root cannot be prepared safely.
    pub fn new(state_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_root)?;
        let root = atomic::canonical_state_root(state_root);
        let path = root.join("mcp/oauth-clients.json");
        Ok(Self {
            lock: atomic::path_lock(&path),
            root,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads metadata for one validated MCP server id.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ids or malformed persisted state.
    pub async fn get(&self, server: &str) -> Result<Option<McpOAuthClientMetadata>> {
        validate_server_id(server)?;
        Ok(self.read().await?.servers.remove(server))
    }

    /// Persists non-secret token endpoint and public-client metadata with private permissions.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe metadata, oversized state, or persistence failures.
    pub async fn set(&self, server: &str, metadata: McpOAuthClientMetadata) -> Result<()> {
        validate_server_id(server)?;
        validate_metadata(&metadata)?;
        let _guard = self.lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        let mut state: MetadataState = atomic::read_json(&self.path).await?.unwrap_or_default();
        validate_state(&state)?;
        if state.servers.len() >= MAX_METADATA_ENTRIES && !state.servers.contains_key(server) {
            return Err(MimirError::Configuration(
                "MCP OAuth metadata store is full".into(),
            ));
        }
        state.servers.insert(server.into(), metadata);
        atomic::write_json(&self.path, &state).await?;
        set_private_permissions(&self.path).await
    }

    /// Removes stored public-client metadata idempotently.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ids, malformed state, or persistence failures.
    pub async fn remove(&self, server: &str) -> Result<bool> {
        validate_server_id(server)?;
        let _guard = self.lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        let mut state: MetadataState = atomic::read_json(&self.path).await?.unwrap_or_default();
        validate_state(&state)?;
        let removed = state.servers.remove(server).is_some();
        if removed {
            atomic::write_json(&self.path, &state).await?;
            set_private_permissions(&self.path).await?;
        }
        Ok(removed)
    }

    async fn read(&self) -> Result<MetadataState> {
        let _guard = self.lock.lock().await;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        let state: MetadataState = atomic::read_json(&self.path).await?.unwrap_or_default();
        validate_state(&state)?;
        Ok(state)
    }
}

fn validate_state(state: &MetadataState) -> Result<()> {
    if state.schema_version != metadata_schema_version()
        || state.servers.len() > MAX_METADATA_ENTRIES
    {
        return Err(MimirError::Configuration(
            "unsupported or oversized MCP OAuth metadata state".into(),
        ));
    }
    for (server, metadata) in &state.servers {
        validate_server_id(server)?;
        validate_metadata(metadata)?;
    }
    Ok(())
}

fn validate_metadata(metadata: &McpOAuthClientMetadata) -> Result<()> {
    secure_url("MCP OAuth token endpoint", &metadata.token_endpoint)?;
    validate_remote_url("MCP OAuth resource", &metadata.resource)?;
    if metadata.client_id.trim().is_empty()
        || metadata.client_id.len() > 2048
        || metadata.client_id.chars().any(char::is_control)
    {
        return Err(MimirError::Configuration(
            "MCP OAuth client metadata has an invalid client id".into(),
        ));
    }
    validate_scopes(&metadata.scopes)
}

fn validate_scopes(scopes: &[String]) -> Result<()> {
    if scopes.len() > 64
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 256
                || scope.chars().any(char::is_whitespace)
                || scope.chars().any(char::is_control)
        })
    {
        return Err(MimirError::Configuration(
            "MCP OAuth scopes are invalid".into(),
        ));
    }
    Ok(())
}

pub(crate) fn parse_bearer_challenge(value: &str) -> Result<Option<McpAuthorizationChallenge>> {
    let value = value.trim();
    let Some(separator) = value.find(char::is_whitespace) else {
        return Ok(value
            .eq_ignore_ascii_case("bearer")
            .then(McpAuthorizationChallenge::default));
    };
    if !value[..separator].eq_ignore_ascii_case("bearer") {
        return Ok(None);
    }
    let mut parameters = BTreeMap::new();
    for field in split_challenge_fields(value[separator..].trim())? {
        let (name, raw_value) = field.split_once('=').ok_or_else(|| {
            MimirError::Protocol("MCP authorization challenge is malformed".into())
        })?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || parameters.contains_key(&name) {
            return Err(MimirError::Protocol(
                "MCP authorization challenge has invalid parameters".into(),
            ));
        }
        parameters.insert(name, parse_challenge_value(raw_value.trim())?);
    }
    let resource_metadata = parameters
        .remove("resource_metadata")
        .map(|value| secure_url("MCP resource metadata URL", &value).map(|url| url.to_string()))
        .transpose()?;
    let scopes = parameters
        .remove("scope")
        .map(|value| {
            let scopes = value
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            validate_scopes(&scopes)?;
            Ok::<_, MimirError>(scopes)
        })
        .transpose()?;
    Ok(Some(McpAuthorizationChallenge {
        resource_metadata,
        scopes,
    }))
}

fn split_challenge_fields(value: &str) -> Result<Vec<&str>> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                let field = value[start..index].trim();
                if !field.is_empty() {
                    fields.push(field);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return Err(MimirError::Protocol(
            "MCP authorization challenge has an unterminated value".into(),
        ));
    }
    let field = value[start..].trim();
    if !field.is_empty() {
        fields.push(field);
    }
    Ok(fields)
}

fn parse_challenge_value(value: &str) -> Result<String> {
    let decoded = if let Some(value) = value.strip_prefix('"') {
        let value = value.strip_suffix('"').ok_or_else(|| {
            MimirError::Protocol("MCP authorization challenge value is malformed".into())
        })?;
        let mut decoded = String::new();
        let mut escaped = false;
        for character in value.chars() {
            if escaped {
                decoded.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                decoded.push(character);
            }
        }
        if escaped {
            return Err(MimirError::Protocol(
                "MCP authorization challenge value is malformed".into(),
            ));
        }
        decoded
    } else {
        value.to_owned()
    };
    if decoded.len() > 4096 || decoded.chars().any(char::is_control) {
        return Err(MimirError::Protocol(
            "MCP authorization challenge value is invalid".into(),
        ));
    }
    Ok(decoded)
}

fn parse_callback_input(
    input: &str,
    expected_state: &str,
    expected_redirect_uri: &str,
) -> Result<String> {
    let input = input.trim();
    if input.is_empty() || input.len() > 16 * 1024 || input.chars().any(|value| value == '\0') {
        return Err(MimirError::Configuration(
            "MCP OAuth callback input is invalid".into(),
        ));
    }
    let callback_url = reqwest::Url::parse(input).ok().or_else(|| {
        input
            .contains('=')
            .then(|| reqwest::Url::parse(&format!("{expected_redirect_uri}?{input}")).ok())
            .flatten()
    });
    if let Some(url) = callback_url {
        let expected = reqwest::Url::parse(expected_redirect_uri)
            .map_err(|_| MimirError::Configuration("MCP OAuth redirect URI is invalid".into()))?;
        if url.scheme() != expected.scheme()
            || url.host_str() != expected.host_str()
            || url.port_or_known_default() != expected.port_or_known_default()
            || url.path() != expected.path()
        {
            return Err(MimirError::Protocol(
                "MCP OAuth callback redirect mismatch".into(),
            ));
        }
        let values = url.query_pairs().collect::<BTreeMap<_, _>>();
        if let Some(error) = values.get("error") {
            return Err(MimirError::Protocol(format!(
                "MCP OAuth authorization failed: {}",
                bounded_public_value(error)
            )));
        }
        let state = values
            .get("state")
            .ok_or_else(|| MimirError::Protocol("MCP OAuth callback is missing state".into()))?;
        if !constant_time_equal(state.as_bytes(), expected_state.as_bytes()) {
            return Err(MimirError::Protocol("MCP OAuth state mismatch".into()));
        }
        return values
            .get("code")
            .filter(|code| !code.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| MimirError::Protocol("MCP OAuth callback is missing code".into()));
    }
    Err(MimirError::Protocol(
        "MCP OAuth manual input must include both code and state".into(),
    ))
}

fn token_credential(token: TokenResponse, previous_refresh: Option<&str>) -> OAuthCredential {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let lifetime = token.expires_in.unwrap_or(DEFAULT_EXPIRY_SECONDS);
    let usable = lifetime.saturating_sub(EXPIRY_BUFFER_SECONDS);
    OAuthCredential {
        access: token.access_token,
        refresh: token
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_owned))
            .unwrap_or_default(),
        expires_at_ms: now.saturating_add(usable.saturating_mul(1000)),
        account_id: None,
        enterprise_url: None,
    }
}

fn random_urlsafe() -> String {
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn secure_url(label: &str, value: &str) -> Result<reqwest::Url> {
    validate_remote_url(label, value)
}

fn validate_redirect_uri(value: &str) -> Result<()> {
    let url = secure_url("MCP OAuth redirect URI", value)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(MimirError::Configuration(
            "MCP OAuth redirect URI must not contain query or fragment data".into(),
        ));
    }
    Ok(())
}

async fn bounded_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > limit))
    {
        return Err(MimirError::Protocol(
            "MCP OAuth response exceeds the configured limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| MimirError::Protocol("MCP OAuth response stream failed".into()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(MimirError::Protocol(
                "MCP OAuth response exceeds the configured limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_server_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit() && index > 0
                || matches!(byte, b'_' | b'-') && index > 0
        })
    {
        return Err(MimirError::Configuration("MCP server id is invalid".into()));
    }
    Ok(())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn bounded_public_value(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(128)
        .collect()
}

fn safe_url(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("remote");
    format!("{}://{}{}", url.scheme(), host, url.path())
}

fn safe_url_text(value: &str) -> String {
    reqwest::Url::parse(value).map_or_else(|_| "[INVALID URL]".into(), |url| safe_url(&url))
}

const fn metadata_schema_version() -> u16 {
    1
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
