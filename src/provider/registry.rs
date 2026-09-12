use std::{collections::BTreeMap, sync::OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ThinkingLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    OAuthPkce,
    OAuthDevice,
    Ambient,
}

impl AuthKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthPkce => "oauth_pkce",
            Self::OAuthDevice => "oauth_device",
            Self::Ambient => "ambient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSupport {
    AnthropicMessages,
    BedrockConverseStream,
    CatalogRouted,
    CodexResponses,
    GoogleGenerativeAi,
    GoogleVertex,
    MistralConversations,
    OpenAiCompatible,
    OpenAiCompatibleWithCustomBaseUrl,
    Unsupported,
}

impl RuntimeSupport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::BedrockConverseStream => "bedrock_converse_stream",
            Self::CatalogRouted => "catalog_routed",
            Self::CodexResponses => "codex_responses",
            Self::GoogleGenerativeAi => "google_generative_ai",
            Self::GoogleVertex => "google_vertex",
            Self::MistralConversations => "mistral_conversations",
            Self::OpenAiCompatible => "openai_compatible",
            Self::OpenAiCompatibleWithCustomBaseUrl => "openai_compatible_custom_base_url",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderDefinition {
    pub id: &'static str,
    pub name: &'static str,
    pub env_vars: &'static [&'static str],
    pub auth: &'static [AuthKind],
    pub base_url: Option<&'static str>,
    pub default_model: Option<&'static str>,
    pub runtime_support: RuntimeSupport,
}

impl ProviderDefinition {
    pub fn environment_variable(&self) -> Option<&'static str> {
        self.env_vars
            .iter()
            .copied()
            .find(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    }

    pub fn environment_key(&self) -> Option<String> {
        self.environment_variable()
            .and_then(|name| std::env::var(name).ok())
    }

    #[must_use]
    pub const fn supports_runtime(&self) -> bool {
        !matches!(self.runtime_support, RuntimeSupport::Unsupported)
    }

    /// Resolves the concrete native transport for one catalog model.
    ///
    /// Providers such as `OpenCode` and Cloudflare AI Gateway expose multiple
    /// wire protocols behind one credential. Their generated model entry is
    /// therefore the source of truth instead of a provider-wide guess.
    #[must_use]
    pub fn runtime_for_model(&self, model: &ModelDefinition) -> Option<RuntimeSupport> {
        if model.provider != self.id {
            return None;
        }
        let runtime = match self.runtime_support {
            RuntimeSupport::CatalogRouted => runtime_for_model_api(&model.api)?,
            support => support,
        };
        runtime.supports_model_api(&model.api).then_some(runtime)
    }
}

impl RuntimeSupport {
    #[must_use]
    pub fn supports_model_api(self, api: &str) -> bool {
        match self {
            Self::AnthropicMessages => api == "anthropic-messages",
            Self::BedrockConverseStream => api == "bedrock-converse-stream",
            Self::CatalogRouted => runtime_for_model_api(api).is_some(),
            Self::CodexResponses => api == "openai-codex-responses",
            Self::GoogleGenerativeAi => api == "google-generative-ai",
            Self::GoogleVertex => api == "google-vertex",
            Self::MistralConversations => api == "mistral-conversations",
            Self::OpenAiCompatible => {
                matches!(api, "openai-completions" | "openai-responses")
            }
            Self::OpenAiCompatibleWithCustomBaseUrl => matches!(
                api,
                "openai-completions" | "openai-responses" | "azure-openai-responses"
            ),
            Self::Unsupported => false,
        }
    }
}

fn runtime_for_model_api(api: &str) -> Option<RuntimeSupport> {
    match api {
        "anthropic-messages" => Some(RuntimeSupport::AnthropicMessages),
        "bedrock-converse-stream" => Some(RuntimeSupport::BedrockConverseStream),
        "google-generative-ai" => Some(RuntimeSupport::GoogleGenerativeAi),
        "google-vertex" => Some(RuntimeSupport::GoogleVertex),
        "mistral-conversations" => Some(RuntimeSupport::MistralConversations),
        "openai-completions" | "openai-responses" => Some(RuntimeSupport::OpenAiCompatible),
        _ => None,
    }
}

pub struct ProviderRegistry {
    providers: &'static [ProviderDefinition],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub featured: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelDefinition {
    #[must_use]
    pub fn from_runtime(
        provider: &str,
        model: &str,
        base_url: Option<&str>,
        support: RuntimeSupport,
    ) -> Self {
        if let Some(cataloged) = model_catalog()
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model)
        {
            return cataloged.clone();
        }
        let reasoning = model_supports_reasoning(provider, model);
        let input = if matches!(provider, "google" | "google-vertex") {
            vec!["text".into(), "image".into()]
        } else {
            vec!["text".into()]
        };
        Self {
            id: model.into(),
            name: model.into(),
            api: match support {
                RuntimeSupport::AnthropicMessages => "anthropic-messages",
                RuntimeSupport::BedrockConverseStream => "bedrock-converse-stream",
                RuntimeSupport::CatalogRouted | RuntimeSupport::Unsupported => "unsupported",
                RuntimeSupport::CodexResponses => "openai-codex-responses",
                RuntimeSupport::GoogleGenerativeAi => "google-generative-ai",
                RuntimeSupport::GoogleVertex => "google-vertex",
                RuntimeSupport::MistralConversations => "mistral-conversations",
                RuntimeSupport::OpenAiCompatible if provider == "openai" => "openai-responses",
                RuntimeSupport::OpenAiCompatibleWithCustomBaseUrl
                    if provider == "azure-openai-responses" =>
                {
                    "azure-openai-responses"
                }
                RuntimeSupport::OpenAiCompatible
                | RuntimeSupport::OpenAiCompatibleWithCustomBaseUrl => "openai-completions",
            }
            .into(),
            provider: provider.into(),
            base_url: base_url.unwrap_or_default().into(),
            reasoning,
            input,
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            thinking_level_map: None,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    #[must_use]
    pub fn fake(model: &str) -> Self {
        Self::from_runtime(
            "fake",
            model,
            Some("memory://fake"),
            RuntimeSupport::Unsupported,
        )
    }

    #[must_use]
    pub fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| {
                let mapped = self
                    .thinking_level_map
                    .as_ref()
                    .and_then(|mapping| mapping.get(level));
                if mapped.is_some_and(Option::is_none) {
                    return false;
                }
                !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) || mapped.is_some()
            })
            .collect()
    }
}

#[must_use]
/// Returns the generated reference model catalog embedded in the binary.
///
/// # Panics
///
/// Panics only when the checked-in generated JSON is malformed, which is a
/// build-time repository integrity failure covered by catalog tests.
pub fn model_catalog() -> &'static [ModelDefinition] {
    static CATALOG: OnceLock<Vec<ModelDefinition>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("models.generated.json"))
            .expect("generated model catalog must be valid JSON")
    })
}

fn model_supports_reasoning(provider: &str, model: &str) -> bool {
    provider == "openai-codex"
        || (provider == "openai"
            && (model.starts_with("gpt-5")
                || model.starts_with("o1")
                || model.starts_with("o3")
                || model.starts_with("o4")))
        || (provider == "deepseek"
            && (model.contains("reasoner") || model.starts_with("deepseek-v4")))
        || (matches!(provider, "google" | "google-vertex")
            && (model.starts_with("gemini-2.5") || model.starts_with("gemini-3")))
}

impl ProviderRegistry {
    pub fn builtin() -> Self {
        Self {
            providers: BUILTINS,
        }
    }

    pub fn get(&self, id: &str) -> Option<&'static ProviderDefinition> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static ProviderDefinition> {
        self.providers.iter()
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

const API: &[AuthKind] = &[AuthKind::ApiKey];
const API_PKCE: &[AuthKind] = &[AuthKind::ApiKey, AuthKind::OAuthPkce];
const API_AMBIENT: &[AuthKind] = &[AuthKind::ApiKey, AuthKind::Ambient];
const PKCE: &[AuthKind] = &[AuthKind::OAuthPkce];
const DEVICE: &[AuthKind] = &[AuthKind::OAuthDevice];

macro_rules! provider {
    ($id:literal, $name:literal, $env:expr, $auth:expr, $url:expr, $model:expr, $runtime:expr) => {
        ProviderDefinition {
            id: $id,
            name: $name,
            env_vars: $env,
            auth: $auth,
            base_url: $url,
            default_model: $model,
            runtime_support: $runtime,
        }
    };
}

use RuntimeSupport::{
    AnthropicMessages, BedrockConverseStream, CatalogRouted, CodexResponses, GoogleGenerativeAi,
    GoogleVertex, MistralConversations, OpenAiCompatible, OpenAiCompatibleWithCustomBaseUrl,
    Unsupported,
};

static BUILTINS: &[ProviderDefinition] = &[
    provider!(
        "openai-codex",
        "ChatGPT Codex",
        &[],
        PKCE,
        None,
        Some("gpt-5.1"),
        CodexResponses
    ),
    provider!(
        "github-copilot",
        "GitHub Copilot",
        &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        DEVICE,
        None,
        None,
        Unsupported
    ),
    provider!(
        "openai",
        "OpenAI",
        &["OPENAI_API_KEY"],
        API,
        Some("https://api.openai.com/v1"),
        Some("gpt-5-mini"),
        OpenAiCompatible
    ),
    provider!(
        "anthropic",
        "Anthropic",
        &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        API_PKCE,
        Some("https://api.anthropic.com"),
        Some("claude-sonnet-5"),
        AnthropicMessages
    ),
    provider!(
        "amazon-bedrock",
        "Amazon Bedrock",
        &["AWS_BEARER_TOKEN_BEDROCK"],
        API_AMBIENT,
        None,
        None,
        BedrockConverseStream
    ),
    provider!(
        "azure-openai-responses",
        "Azure OpenAI Responses",
        &["AZURE_OPENAI_API_KEY"],
        API,
        None,
        None,
        OpenAiCompatibleWithCustomBaseUrl
    ),
    provider!(
        "cerebras",
        "Cerebras",
        &["CEREBRAS_API_KEY"],
        API,
        Some("https://api.cerebras.ai/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "deepseek",
        "DeepSeek",
        &["DEEPSEEK_API_KEY"],
        API,
        Some("https://api.deepseek.com/v1"),
        Some("deepseek-chat"),
        OpenAiCompatible
    ),
    provider!(
        "google",
        "Google Gemini",
        &["GEMINI_API_KEY"],
        API,
        Some("https://generativelanguage.googleapis.com/v1beta"),
        Some("gemini-2.5-pro"),
        GoogleGenerativeAi
    ),
    provider!(
        "google-vertex",
        "Google Vertex AI",
        &["GOOGLE_CLOUD_API_KEY", "GOOGLE_API_KEY"],
        API_AMBIENT,
        None,
        Some("gemini-2.5-flash"),
        GoogleVertex
    ),
    provider!(
        "groq",
        "Groq",
        &["GROQ_API_KEY"],
        API,
        Some("https://api.groq.com/openai/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "xai",
        "xAI",
        &["XAI_API_KEY"],
        API,
        Some("https://api.x.ai/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "openrouter",
        "OpenRouter",
        &["OPENROUTER_API_KEY"],
        API,
        Some("https://openrouter.ai/api/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        &["AI_GATEWAY_API_KEY"],
        API,
        Some("https://ai-gateway.vercel.sh"),
        Some("zai/glm-5.1"),
        AnthropicMessages
    ),
    provider!(
        "zai",
        "ZAI",
        &["ZAI_API_KEY"],
        API,
        Some("https://api.z.ai/api/coding/paas/v4"),
        Some("glm-5.1"),
        OpenAiCompatible
    ),
    provider!(
        "mistral",
        "Mistral",
        &["MISTRAL_API_KEY"],
        API,
        Some("https://api.mistral.ai"),
        Some("devstral-medium-latest"),
        MistralConversations
    ),
    provider!(
        "minimax",
        "MiniMax",
        &["MINIMAX_API_KEY"],
        API,
        Some("https://api.minimax.io/anthropic"),
        Some("MiniMax-M2.7"),
        AnthropicMessages
    ),
    provider!(
        "minimax-cn",
        "MiniMax China",
        &["MINIMAX_CN_API_KEY"],
        API,
        Some("https://api.minimaxi.com/anthropic"),
        Some("MiniMax-M2.7"),
        AnthropicMessages
    ),
    provider!(
        "moonshotai",
        "Moonshot AI",
        &["MOONSHOT_API_KEY"],
        API,
        Some("https://api.moonshot.ai/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "moonshotai-cn",
        "Moonshot AI China",
        &["MOONSHOT_API_KEY"],
        API,
        Some("https://api.moonshot.cn/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "huggingface",
        "Hugging Face",
        &["HF_TOKEN"],
        API,
        Some("https://router.huggingface.co/v1"),
        Some("moonshotai/Kimi-K2.6"),
        OpenAiCompatible
    ),
    provider!(
        "fireworks",
        "Fireworks",
        &["FIREWORKS_API_KEY"],
        API,
        Some("https://api.fireworks.ai/inference"),
        Some("accounts/fireworks/models/kimi-k2p6"),
        AnthropicMessages
    ),
    provider!(
        "opencode",
        "OpenCode Zen",
        &["OPENCODE_API_KEY"],
        API,
        Some("https://opencode.ai/zen"),
        Some("kimi-k2.6"),
        CatalogRouted
    ),
    provider!(
        "opencode-go",
        "OpenCode Go",
        &["OPENCODE_API_KEY"],
        API,
        Some("https://opencode.ai/zen/go"),
        Some("kimi-k2.6"),
        CatalogRouted
    ),
    provider!(
        "kimi-coding",
        "Kimi For Coding",
        &["KIMI_API_KEY"],
        API,
        Some("https://api.kimi.com/coding"),
        Some("kimi-for-coding"),
        AnthropicMessages
    ),
    provider!(
        "prime-inference",
        "Prime Inference",
        &["PRIME_API_KEY"],
        API,
        Some("https://api.pinference.ai/api/v1"),
        None,
        OpenAiCompatible
    ),
    provider!(
        "cloudflare-workers-ai",
        "Cloudflare Workers AI",
        &["CLOUDFLARE_API_KEY"],
        API,
        Some("https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"),
        Some("@cf/moonshotai/kimi-k2.6"),
        OpenAiCompatible
    ),
    provider!(
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        &["CLOUDFLARE_API_KEY"],
        API,
        Some(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
        ),
        Some("workers-ai/@cf/moonshotai/kimi-k2.6"),
        CatalogRouted
    ),
    provider!(
        "xiaomi",
        "Xiaomi MiMo",
        &["XIAOMI_API_KEY"],
        API,
        Some("https://api.xiaomimimo.com/anthropic"),
        Some("mimo-v2.5-pro"),
        AnthropicMessages
    ),
    provider!(
        "xiaomi-token-plan-cn",
        "Xiaomi MiMo Token Plan China",
        &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        API,
        Some("https://token-plan-cn.xiaomimimo.com/anthropic"),
        Some("mimo-v2.5-pro"),
        AnthropicMessages
    ),
    provider!(
        "xiaomi-token-plan-ams",
        "Xiaomi MiMo Token Plan Amsterdam",
        &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        API,
        Some("https://token-plan-ams.xiaomimimo.com/anthropic"),
        Some("mimo-v2.5-pro"),
        AnthropicMessages
    ),
    provider!(
        "xiaomi-token-plan-sgp",
        "Xiaomi MiMo Token Plan Singapore",
        &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        API,
        Some("https://token-plan-sgp.xiaomimimo.com/anthropic"),
        Some("mimo-v2.5-pro"),
        AnthropicMessages
    ),
];
