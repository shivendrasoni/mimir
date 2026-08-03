use std::{fmt, path::PathBuf, time::Duration};

use secrecy::SecretString;

use crate::{budget::Budget, error::MimirError};

#[derive(Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    api_key: SecretString,
    pub request_timeout: Duration,
    pub max_retries: u32,
}

impl ProviderConfig {
    /// Creates a validated OpenAI-compatible provider configuration.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the URL, model, or API key is blank.
    pub fn openai(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, MimirError> {
        let base_url = base_url.into();
        let model = model.into();
        let api_key = api_key.into();
        for (name, value) in [
            ("provider base URL", base_url.as_str()),
            ("model", model.as_str()),
            ("API key", api_key.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(MimirError::Configuration(format!(
                    "{name} must not be blank"
                )));
            }
        }
        Ok(Self {
            name: "openai".into(),
            base_url,
            model,
            api_key: SecretString::from(api_key),
            request_timeout: Duration::from_secs(120),
            max_retries: 2,
        })
    }

    pub fn api_key_secret(&self) -> &SecretString {
        &self.api_key
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .field("request_timeout", &self.request_timeout)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

#[derive(Debug)]
pub struct Config {
    pub workspace: PathBuf,
    pub state_dir: PathBuf,
    pub provider: ProviderConfig,
    pub budget: Budget,
    pub offline: bool,
}

impl Config {
    pub fn for_test(provider: ProviderConfig) -> Self {
        Self {
            workspace: PathBuf::from("."),
            state_dir: PathBuf::from(".mimir-test"),
            provider,
            budget: Budget::default(),
            offline: false,
        }
    }

    /// Resolves production configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the provider credential is absent.
    pub fn from_env(workspace: PathBuf, state_dir: PathBuf) -> Result<Self, MimirError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            MimirError::Configuration(
                "OPENAI_API_KEY is missing; set it in the environment or use offline fake mode"
                    .into(),
            )
        })?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("MIMIR_MODEL").unwrap_or_else(|_| "gpt-5-mini".into());

        Ok(Self {
            workspace,
            state_dir,
            provider: ProviderConfig::openai(base_url, model, api_key)?,
            budget: Budget::default(),
            offline: false,
        })
    }
}
