use std::{net::IpAddr, time::Duration};

use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

use crate::{
    auth::OAuthCredential,
    error::{MimirError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    OpenAiCodex,
    Anthropic,
    GitHubCopilot,
}

impl OAuthProvider {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "openai-codex" => Some(Self::OpenAiCodex),
            "anthropic" => Some(Self::Anthropic),
            "github-copilot" => Some(Self::GitHubCopilot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingOAuth {
    pub provider: String,
    pub verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub authorize_url: String,
    token_url: String,
    client_id: String,
}

impl PendingOAuth {
    /// Builds a PKCE authorization request for providers that support browser-based OAuth.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the provider requires a different auth flow
    /// or when the authorize URL cannot be constructed safely.
    pub fn begin(provider: OAuthProvider) -> Result<Self> {
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let state = Uuid::new_v4().simple().to_string();
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        let (id, authorize, token_url, client_id, redirect_uri, scope) = match provider {
            OAuthProvider::OpenAiCodex => (
                "openai-codex",
                "https://auth.openai.com/oauth/authorize",
                "https://auth.openai.com/oauth/token",
                "app_EMoamEEZ73f0CkXaXp7hrann",
                "http://localhost:1455/auth/callback",
                "openid profile email offline_access",
            ),
            OAuthProvider::Anthropic => (
                "anthropic",
                "https://claude.ai/oauth/authorize",
                "https://platform.claude.com/v1/oauth/token",
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "http://localhost:53692/callback",
                "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            ),
            OAuthProvider::GitHubCopilot => {
                return Err(MimirError::Configuration(
                    "GitHub Copilot uses the device-code flow".into(),
                ));
            }
        };
        let mut url = Url::parse(authorize).map_err(|error| {
            MimirError::Configuration(format!("invalid OAuth authorize URL: {error}"))
        })?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scope)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        if provider == OAuthProvider::OpenAiCodex {
            url.query_pairs_mut()
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true")
                .append_pair("originator", "mimir");
        } else {
            url.query_pairs_mut().append_pair("code", "true");
        }
        Ok(Self {
            provider: id.into(),
            verifier,
            state,
            redirect_uri: redirect_uri.into(),
            authorize_url: url.into(),
            token_url: token_url.into(),
            client_id: client_id.into(),
        })
    }

    pub fn new_for_test(provider: &str, verifier: &str, state: &str, redirect_uri: &str) -> Self {
        Self {
            provider: provider.into(),
            verifier: verifier.into(),
            state: state.into(),
            redirect_uri: redirect_uri.into(),
            authorize_url: String::new(),
            token_url: "http://localhost/token".into(),
            client_id: "test-client".into(),
        }
    }

    /// Extracts the authorization code from a redirect URL or `code#state` shorthand.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the input is blank, malformed, missing a code,
    /// or carries a mismatched OAuth state value.
    pub fn parse_authorization_input(&self, input: &str) -> Result<String> {
        let value = input.trim();
        if value.is_empty() {
            return Err(MimirError::Configuration(
                "authorization code must not be blank".into(),
            ));
        }
        if let Ok(url) = Url::parse(value) {
            let code = url
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.into_owned())
                .ok_or_else(|| MimirError::Configuration("OAuth redirect has no code".into()))?;
            let returned_state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned());
            if returned_state.as_deref() != Some(self.state.as_str()) {
                return Err(MimirError::Configuration("OAuth state mismatch".into()));
            }
            return Ok(code);
        }
        if let Some((code, state)) = value.split_once('#') {
            if state != self.state {
                return Err(MimirError::Configuration("OAuth state mismatch".into()));
            }
            return Ok(code.into());
        }
        Ok(value.into())
    }

    /// Receives one OAuth redirect on the configured loopback callback address.
    ///
    /// The listener accepts only an HTTP `GET` for the exact configured path, validates
    /// OAuth state before acknowledging the browser, and never reflects the code or state
    /// into the completion page.
    ///
    /// # Errors
    ///
    /// Returns a configuration or I/O error when the redirect is not plain loopback HTTP,
    /// the listener cannot bind, the request is malformed or oversized, state validation
    /// fails, or the bounded wait expires.
    pub async fn receive_browser_callback(&self, timeout: Duration) -> Result<String> {
        let redirect = Url::parse(&self.redirect_uri).map_err(|error| {
            MimirError::Configuration(format!("invalid OAuth redirect URL: {error}"))
        })?;
        if redirect.scheme() != "http" {
            return Err(MimirError::Configuration(
                "OAuth browser callback must use loopback HTTP".into(),
            ));
        }
        let host = redirect
            .host_str()
            .ok_or_else(|| MimirError::Configuration("OAuth redirect URL has no host".into()))?;
        let bind_ip = loopback_bind_ip(host)?;
        let port = redirect
            .port_or_known_default()
            .ok_or_else(|| MimirError::Configuration("OAuth redirect URL has no port".into()))?;
        let listener = TcpListener::bind((bind_ip, port)).await?;
        let callback_path = redirect.path().to_owned();

        tokio::time::timeout(timeout, async {
            let (mut stream, peer) = listener.accept().await?;
            if !peer.ip().is_loopback() {
                return Err(MimirError::Configuration(
                    "OAuth callback did not originate from loopback".into(),
                ));
            }
            let request = read_callback_request(&mut stream).await?;
            let callback = parse_callback_request(&request, &callback_path)?;
            if let Err(error) = self.parse_authorization_input(&callback) {
                write_callback_page(
                    &mut stream,
                    "400 Bad Request",
                    "Authentication rejected",
                    "The callback could not be validated. Return to the terminal and use the manual fallback.",
                )
                .await?;
                return Err(error);
            }
            write_callback_page(
                &mut stream,
                "200 OK",
                "Authentication received",
                "You can close this page and return to Mimir.",
            )
            .await?;
            Ok(callback)
        })
        .await
        .map_err(|_| MimirError::Configuration("OAuth browser callback timed out".into()))?
    }

    /// Exchanges an interactive OAuth authorization result for durable tokens.
    ///
    /// # Errors
    ///
    /// Returns a configuration, transport, or provider error when the authorization input
    /// is invalid or the upstream token exchange fails.
    pub async fn exchange(&self, input: &str) -> Result<OAuthCredential> {
        let code = self.parse_authorization_input(input)?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| provider_error(&error))?;
        let response = if self.provider == "anthropic" {
            client
                .post(&self.token_url)
                .json(&serde_json::json!({
                    "grant_type": "authorization_code",
                    "client_id": self.client_id,
                    "code": code,
                    "state": self.state,
                    "redirect_uri": self.redirect_uri,
                    "code_verifier": self.verifier
                }))
                .send()
                .await
        } else {
            client
                .post(&self.token_url)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", self.client_id.as_str()),
                    ("code", code.as_str()),
                    ("code_verifier", self.verifier.as_str()),
                    ("redirect_uri", self.redirect_uri.as_str()),
                ])
                .send()
                .await
        }
        .map_err(|error| provider_error(&error))?;
        parse_token_response(response).await
    }
}

fn loopback_bind_ip(host: &str) -> Result<IpAddr> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    let ip = host
        .parse::<IpAddr>()
        .map_err(|_| MimirError::Configuration("OAuth redirect host must be loopback".into()))?;
    if !ip.is_loopback() {
        return Err(MimirError::Configuration(
            "OAuth redirect host must be loopback".into(),
        ));
    }
    Ok(ip)
}

async fn read_callback_request(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>> {
    const MAX_REQUEST_BYTES: usize = 8 * 1024;
    let mut request = Vec::with_capacity(1024);
    loop {
        if request.len() == MAX_REQUEST_BYTES {
            return Err(MimirError::Configuration(
                "OAuth callback request is too large".into(),
            ));
        }
        let mut chunk = [0_u8; 1024];
        let chunk_len = (MAX_REQUEST_BYTES - request.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..chunk_len]).await?;
        if read == 0 {
            return Err(MimirError::Configuration(
                "OAuth callback request ended before its headers".into(),
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn parse_callback_request(request: &[u8], expected_path: &str) -> Result<String> {
    let request = std::str::from_utf8(request).map_err(|_| {
        MimirError::Configuration("OAuth callback request is not valid UTF-8".into())
    })?;
    let request_line = request.lines().next().ok_or_else(|| {
        MimirError::Configuration("OAuth callback request has no request line".into())
    })?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "GET" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(MimirError::Configuration(
            "OAuth callback request line is invalid".into(),
        ));
    }
    let callback = Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| MimirError::Configuration("OAuth callback target is invalid".into()))?;
    if callback.path() != expected_path {
        return Err(MimirError::Configuration(
            "OAuth callback path mismatch".into(),
        ));
    }
    Ok(callback.into())
}

async fn write_callback_page(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    title: &str,
    message: &str,
) -> Result<()> {
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title><body><main><h1>{title}</h1><p>{message}</p></main></body></html>"
    );
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

impl DeviceAuthorization {
    /// Starts GitHub's device-code flow for Copilot-compatible OAuth login.
    ///
    /// # Errors
    ///
    /// Returns a configuration, transport, or provider error when the GitHub domain
    /// is invalid or the device authorization request fails.
    pub async fn begin_github(domain: &str) -> Result<Self> {
        let domain = normalize_domain(domain)?;
        let client = oauth_client()?;
        let url = format!("https://{domain}/login/device/code");
        let response = client
            .post(url)
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .form(&[("client_id", github_client_id()), ("scope", "read:user")])
            .send()
            .await
            .map_err(|error| provider_error(&error))?;
        checked_json(response).await
    }

    /// Polls GitHub's device-code endpoint until an access token is issued or times out.
    ///
    /// # Errors
    ///
    /// Returns a configuration, transport, or provider error when polling fails,
    /// the flow expires, or GitHub returns an invalid token response.
    pub async fn poll_github(&self, domain: &str) -> Result<OAuthCredential> {
        let domain = normalize_domain(domain)?;
        let client = oauth_client()?;
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(self.expires_in.min(900));
        let mut interval = self.interval.max(1);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(MimirError::Provider(
                    "GitHub device authorization timed out".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            let response = client
                .post(format!("https://{domain}/login/oauth/access_token"))
                .header("Accept", "application/json")
                .header("User-Agent", "GitHubCopilotChat/0.35.0")
                .form(&[
                    ("client_id", github_client_id()),
                    ("device_code", self.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .await
                .map_err(|error| provider_error(&error))?;
            let status = response.status();
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|error| provider_error(&error))?;
            if !status.is_success() {
                return Err(MimirError::Provider(format!(
                    "GitHub device authorization failed with HTTP {status}"
                )));
            }
            if let Some(token) = value
                .get("access_token")
                .and_then(serde_json::Value::as_str)
            {
                return refresh_github_copilot(token, &domain).await;
            }
            match value.get("error").and_then(serde_json::Value::as_str) {
                Some("authorization_pending") => {}
                Some("slow_down") => interval = interval.saturating_add(5).min(30),
                Some(error) => {
                    return Err(MimirError::Provider(format!(
                        "GitHub device authorization failed: {error}"
                    )));
                }
                None => {
                    return Err(MimirError::Provider(
                        "GitHub device authorization returned an invalid response".into(),
                    ));
                }
            }
        }
    }
}

/// Refreshes a durable OAuth credential using the provider-specific refresh mechanism.
///
/// # Errors
///
/// Returns a configuration, transport, or provider error when refresh inputs are invalid
/// or the upstream token endpoint rejects the refresh request.
pub async fn refresh_oauth(
    provider: OAuthProvider,
    credential: &OAuthCredential,
) -> Result<OAuthCredential> {
    match provider {
        OAuthProvider::GitHubCopilot => {
            let domain = credential.enterprise_url.as_deref().unwrap_or("github.com");
            refresh_github_copilot(&credential.refresh, domain).await
        }
        OAuthProvider::OpenAiCodex | OAuthProvider::Anthropic => {
            let (url, client_id, json_body) = match provider {
                OAuthProvider::OpenAiCodex => (
                    "https://auth.openai.com/oauth/token",
                    "app_EMoamEEZ73f0CkXaXp7hrann",
                    false,
                ),
                OAuthProvider::Anthropic => (
                    "https://platform.claude.com/v1/oauth/token",
                    "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                    true,
                ),
                OAuthProvider::GitHubCopilot => unreachable!(),
            };
            let client = oauth_client()?;
            let response = if json_body {
                client
                    .post(url)
                    .json(&serde_json::json!({
                        "grant_type": "refresh_token",
                        "client_id": client_id,
                        "refresh_token": credential.refresh
                    }))
                    .send()
                    .await
            } else {
                client
                    .post(url)
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("refresh_token", credential.refresh.as_str()),
                    ])
                    .send()
                    .await
            }
            .map_err(|error| provider_error(&error))?;
            parse_token_response(response).await
        }
    }
}

async fn refresh_github_copilot(token: &str, domain: &str) -> Result<OAuthCredential> {
    let client = oauth_client()?;
    let response = client
        .get(format!("https://api.{domain}/copilot_internal/v2/token"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .send()
        .await
        .map_err(|error| provider_error(&error))?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| provider_error(&error))?;
    if !status.is_success() {
        return Err(MimirError::Provider(format!(
            "GitHub Copilot token request failed with HTTP {status}"
        )));
    }
    let access = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MimirError::Provider("Copilot token response has no token".into()))?;
    let expires_at = value
        .get("expires_at")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| MimirError::Provider("Copilot token response has no expiry".into()))?;
    Ok(OAuthCredential {
        access: access.into(),
        refresh: token.into(),
        expires_at_ms: expires_at.saturating_mul(1000).saturating_sub(300_000),
        account_id: None,
        enterprise_url: (domain != "github.com").then(|| domain.into()),
    })
}

async fn parse_token_response(response: reqwest::Response) -> Result<OAuthCredential> {
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| provider_error(&error))?;
    if !status.is_success() {
        return Err(MimirError::Provider(format!(
            "OAuth token exchange failed with HTTP {status}"
        )));
    }
    let access = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MimirError::Provider("OAuth response has no access token".into()))?;
    let refresh = value
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MimirError::Provider("OAuth response has no refresh token".into()))?;
    let expires_in = value
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| MimirError::Provider("OAuth response has no expiry".into()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    Ok(OAuthCredential {
        access: access.into(),
        refresh: refresh.into(),
        expires_at_ms: now
            .saturating_add(expires_in.saturating_mul(1000))
            .saturating_sub(300_000),
        account_id: None,
        enterprise_url: None,
    })
}

async fn checked_json<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        return Err(MimirError::Provider(format!(
            "OAuth endpoint returned HTTP {status}"
        )));
    }
    response
        .json()
        .await
        .map_err(|error| provider_error(&error))
}

fn oauth_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| provider_error(&error))
}

fn normalize_domain(domain: &str) -> Result<String> {
    let value = domain.trim();
    let value = if value.is_empty() {
        "github.com"
    } else {
        value
    };
    let parsed = if value.contains("://") {
        Url::parse(value)
    } else {
        Url::parse(&format!("https://{value}"))
    }
    .map_err(|_| MimirError::Configuration("invalid GitHub domain".into()))?;
    parsed
        .host_str()
        .map(str::to_owned)
        .ok_or_else(|| MimirError::Configuration("invalid GitHub domain".into()))
}

fn github_client_id() -> &'static str {
    "Iv1.b507a08c87ecfe98"
}

fn provider_error(error: &reqwest::Error) -> MimirError {
    MimirError::Provider(error.to_string())
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0x3f) as usize] as char);
        }
    }
    output
}
