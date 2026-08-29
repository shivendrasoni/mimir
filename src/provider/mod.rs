mod anthropic;
mod bedrock;
pub mod cloudflare;
mod codex;
mod fake;
mod google;
mod google_adc;
mod mistral;
mod openai;
mod openai_stream;
pub mod registry;
mod responses;
mod vertex;

use async_trait::async_trait;
use thiserror::Error;

use crate::model::{Content, ModelRequest, ModelResponse};

pub use anthropic::{AnthropicCredentialKind, AnthropicProvider};
pub use bedrock::{
    AwsCredentials, BedrockCredentialSource, BedrockProvider, EnvironmentCredentialSource,
    resolve_bedrock_region,
};
pub use fake::FakeProvider;
pub use google::GoogleProvider;
pub use google_adc::{
    GoogleAdcCredential, GoogleAdcEnvironment, GoogleAdcError, GoogleAdcResolver,
};
pub use mistral::MistralProvider;
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;
pub use vertex::VertexProvider;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    #[error("provider authentication failed")]
    Authentication,
    #[error("provider rejected the authentication credential")]
    AuthenticationRejected,
    #[error("provider rate limited the request: {message}")]
    RateLimited { message: String },
    #[error("provider is temporarily unavailable: {message}")]
    Unavailable { message: String },
    #[error("provider protocol error: {message}")]
    Protocol { message: String },
    #[error("provider request aborted")]
    Aborted,
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Unavailable { .. })
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ProviderError>;

    /// Updates an optional provider-specific service tier.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the provider or tier is unsupported.
    fn set_service_tier(&self, tier: Option<&str>) -> Result<(), ProviderError> {
        match tier {
            None | Some("default") => Ok(()),
            Some(value) => Err(ProviderError::Protocol {
                message: format!("service tier '{value}' is unsupported by this provider"),
            }),
        }
    }

    async fn available_model_ids(&self) -> Result<Option<Vec<String>>, ProviderError> {
        Ok(None)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.complete(request).await?;
        for block in &response.message.content {
            match block {
                Content::Text { text } => sink.emit(ProviderEvent::TextDelta(text.clone())).await,
                Content::Thinking { text, .. } => {
                    sink.emit(ProviderEvent::ThinkingDelta(text.clone())).await;
                }
                Content::Image { .. } | Content::ToolCall(_) | Content::ToolResult(_) => {}
            }
        }
        Ok(response)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    TextDelta(String),
    ThinkingDelta(String),
    AuthenticationRefresh {
        provider: String,
        status: AuthenticationRefreshStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationRefreshStatus {
    Started,
    Succeeded,
    Failed,
}

#[async_trait]
pub trait ProviderEventSink: Send + Sync {
    async fn emit(&self, event: ProviderEvent);
}
pub use codex::CodexProvider;
