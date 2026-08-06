use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use regex::Regex;

use crate::{
    error::{MimirError, Result},
    mcp::protocol,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub server: String,
    pub label: String,
    pub stdio: McpStdioConfig,
    pub remote: Option<McpHttpConfig>,
    pub oauth: bool,
    pub bearer_token_env_var: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpConfig {
    pub url: String,
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub io_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_tool_payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub startup_timeout: Duration,
    pub io_timeout: Duration,
    pub max_frame_bytes: usize,
    pub max_tool_payload_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for McpStdioConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            startup_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(15),
            max_frame_bytes: protocol::MAX_FRAME_BYTES,
            max_tool_payload_bytes: 128 * 1024,
            max_stderr_bytes: 16 * 1024,
        }
    }
}

impl McpServerConfig {
    /// Builds and validates an MCP server configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when any server or stdio field is invalid.
    pub fn new(
        server: impl Into<String>,
        label: impl Into<String>,
        stdio: McpStdioConfig,
    ) -> Result<Self> {
        let config = Self {
            server: server.into(),
            label: label.into(),
            stdio,
            remote: None,
            oauth: false,
            bearer_token_env_var: None,
            headers: BTreeMap::new(),
            enabled: true,
        };
        config.validate()?;
        Ok(config)
    }

    /// Builds a validated Streamable HTTP MCP server configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier or remote transport metadata is invalid.
    pub fn remote(
        server: impl Into<String>,
        label: impl Into<String>,
        remote: McpHttpConfig,
    ) -> Result<Self> {
        let config = Self {
            server: server.into(),
            label: label.into(),
            stdio: McpStdioConfig::default(),
            remote: Some(remote),
            oauth: false,
            bearer_token_env_var: None,
            headers: BTreeMap::new(),
            enabled: true,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn provider_id(&self) -> String {
        format!("mcp:{}", self.server)
    }

    /// Validates server metadata, auth metadata, and stdio settings.
    ///
    /// # Panics
    ///
    /// Panics only if the built-in validation regexes are invalid.
    ///
    /// # Errors
    ///
    /// Returns an error when any server or stdio field is invalid.
    pub fn validate(&self) -> Result<()> {
        let server_pattern = Regex::new(r"^[a-z][a-z0-9_-]{0,63}$")
            .expect("MCP server identifier regex must be valid");
        if !server_pattern.is_match(&self.server) {
            return Err(MimirError::Configuration(format!(
                "MCP server '{}' is invalid",
                self.server
            )));
        }
        if self.label.trim().is_empty() {
            return Err(MimirError::Configuration(
                "MCP server label must not be blank".into(),
            ));
        }
        if let Some(env_var) = &self.bearer_token_env_var {
            validate_env_key("bearer token env var", env_var)?;
        }
        validate_headers(&self.headers)?;
        if let Some(remote) = &self.remote {
            if !self.stdio.program.as_os_str().is_empty()
                || !self.stdio.args.is_empty()
                || !self.stdio.env.is_empty()
            {
                return Err(MimirError::Configuration(
                    "MCP server must configure exactly one transport".into(),
                ));
            }
            remote.validate()
        } else {
            self.stdio.validate()
        }
    }
}

impl McpHttpConfig {
    /// Creates a bounded Streamable HTTP configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL is not HTTPS or loopback HTTP.
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let config = Self {
            url: url.into(),
            client_id: None,
            scopes: Vec::new(),
            headers: BTreeMap::new(),
            io_timeout: Duration::from_secs(20),
            max_response_bytes: protocol::MAX_FRAME_BYTES,
            max_tool_payload_bytes: 128 * 1024,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validates remote URL, OAuth metadata, headers, timeouts, and size bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when any remote transport field is unsafe or out of bounds.
    pub fn validate(&self) -> Result<()> {
        validate_remote_url("MCP remote URL", &self.url)?;
        if self.io_timeout.is_zero() {
            return Err(MimirError::Configuration(
                "MCP remote I/O timeout must be greater than zero".into(),
            ));
        }
        if self.max_response_bytes == 0 || self.max_response_bytes > protocol::MAX_FRAME_BYTES {
            return Err(MimirError::Configuration(format!(
                "MCP remote response limit must be between 1 and {} bytes",
                protocol::MAX_FRAME_BYTES
            )));
        }
        if self.max_tool_payload_bytes == 0 || self.max_tool_payload_bytes > self.max_response_bytes
        {
            return Err(MimirError::Configuration(
                "MCP remote tool payload limit must not exceed the response limit".into(),
            ));
        }
        if self.client_id.as_ref().is_some_and(|value| {
            value.trim().is_empty() || value.len() > 2048 || value.chars().any(char::is_control)
        }) {
            return Err(MimirError::Configuration(
                "MCP OAuth client id is invalid".into(),
            ));
        }
        if self.scopes.len() > 64
            || self.scopes.iter().any(|scope| {
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
        validate_headers(&self.headers)
    }
}

impl McpStdioConfig {
    /// Validates the stdio program path, environment, and safety limits.
    ///
    /// # Errors
    ///
    /// Returns an error when the stdio command or bounds are invalid.
    pub fn validate(&self) -> Result<()> {
        if self.program.as_os_str().is_empty() {
            return Err(MimirError::Configuration(
                "MCP stdio program must not be blank".into(),
            ));
        }
        if !self.program.is_absolute() {
            return Err(MimirError::Configuration(
                "MCP stdio program must be an absolute path".into(),
            ));
        }
        for argument in &self.args {
            validate_process_value("MCP stdio argument", argument)?;
        }
        for (key, value) in &self.env {
            validate_env_key("MCP stdio env key", key)?;
            validate_process_value("MCP stdio env value", value)?;
        }
        for (label, duration) in [
            ("MCP startup timeout", self.startup_timeout),
            ("MCP I/O timeout", self.io_timeout),
        ] {
            if duration.is_zero() {
                return Err(MimirError::Configuration(format!(
                    "{label} must be greater than zero"
                )));
            }
        }
        for (label, value) in [
            ("MCP frame size limit", self.max_frame_bytes),
            ("MCP tool payload size limit", self.max_tool_payload_bytes),
            ("MCP stderr size limit", self.max_stderr_bytes),
        ] {
            if value == 0 {
                return Err(MimirError::Configuration(format!(
                    "{label} must be greater than zero"
                )));
            }
        }
        if self.max_frame_bytes > protocol::MAX_FRAME_BYTES {
            return Err(MimirError::Configuration(format!(
                "MCP frame size limit exceeds the hard cap of {} bytes",
                protocol::MAX_FRAME_BYTES
            )));
        }
        if self.max_tool_payload_bytes > self.max_frame_bytes {
            return Err(MimirError::Configuration(
                "MCP tool payload size limit must not exceed the frame size limit".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_headers(headers: &BTreeMap<String, String>) -> Result<()> {
    for (name, value) in headers {
        if name.trim().is_empty() {
            return Err(MimirError::Configuration(
                "MCP header names must not be blank".into(),
            ));
        }
        for (label, field) in [
            ("header name", name.as_str()),
            ("header value", value.as_str()),
        ] {
            if field.contains('\n') || field.contains('\r') || field.contains('\0') {
                return Err(MimirError::Configuration(format!(
                    "MCP {label} must not contain control characters"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_env_key(label: &str, value: &str) -> Result<()> {
    let pattern =
        Regex::new(r"^[A-Z_][A-Z0-9_]*$").expect("environment variable regex must be valid");
    if pattern.is_match(value) {
        Ok(())
    } else {
        Err(MimirError::Configuration(format!(
            "{label} '{value}' is invalid"
        )))
    }
}

pub(crate) fn validate_process_value(label: &str, value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') || value.contains('\0') {
        return Err(MimirError::Configuration(format!(
            "{label} must not contain control characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate_remote_url(label: &str, value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| MimirError::Configuration(format!("{label} is invalid")))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(MimirError::Configuration(format!(
            "{label} must not contain credentials or fragments"
        )));
    }
    let secure = url.scheme() == "https";
    let loopback = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if !secure && !loopback {
        return Err(MimirError::Configuration(format!(
            "{label} must use HTTPS (HTTP is allowed only for loopback)"
        )));
    }
    Ok(url)
}
