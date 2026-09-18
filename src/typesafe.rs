use std::{sync::Arc, time::Duration};

use serde::Serialize;
use sha2::{Digest, Sha256};
use typesafe_client::{
    CallOptions, ChoiceQuestion, Client, Error, NoulQuestion, Questions, RetryPolicy, SystemOne,
    SystemOneRequest,
};

const APPLICABILITY_QUESTION: &str = "skill_applies";
const RANKING_QUESTION: &str = "best_skill";
const MAX_SELECTION_REQUEST_BYTES: usize = 64 * 1_024;

/// Name and bounded description supplied to the `TypeSafe` selector.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TypeSafeSkill {
    /// Exact runtime skill name.
    pub name: String,
    /// Discovery summary; full skill instructions are never sent.
    pub description: String,
}

/// Top-level runtime state for all TypeSafe-backed features.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TypeSafeMode {
    /// Do not initialize or call `TypeSafe`.
    #[default]
    Off,
    /// Enable configured TypeSafe-backed features.
    On,
}

impl TypeSafeMode {
    /// Stable value used in diagnostics and configuration displays.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// `TypeSafe` configuration shared by current and future `TypeSafe`-backed features.
#[derive(Debug, Clone)]
pub struct TypeSafeConfig {
    /// Whether TypeSafe-backed features are enabled.
    pub mode: TypeSafeMode,
    /// `TypeSafe` model or alias used for the request.
    pub model: String,
    /// Hard deadline for the complete request, with retries disabled.
    pub timeout: Duration,
    /// Skill-selection policy owned by the `TypeSafe` integration.
    pub skill_selection: TypeSafeSkillSelectionConfig,
}

/// Internal policy for TypeSafe-backed skill selection.
#[derive(Debug, Clone)]
pub struct TypeSafeSkillSelectionConfig {
    /// Minimum probability that one catalog skill applies.
    pub applicability_threshold: f64,
    /// Minimum concentration of the Choice distribution.
    pub confidence_threshold: f64,
}

impl Default for TypeSafeConfig {
    fn default() -> Self {
        Self {
            mode: TypeSafeMode::Off,
            model: "jev-latest".into(),
            timeout: Duration::from_millis(2_000),
            skill_selection: TypeSafeSkillSelectionConfig::default(),
        }
    }
}

impl Default for TypeSafeSkillSelectionConfig {
    fn default() -> Self {
        Self {
            applicability_threshold: 0.60,
            confidence_threshold: 0.50,
        }
    }
}

/// One ranked skill returned by Jev.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RankedSkill {
    /// Exact skill name from the supplied catalog.
    pub name: String,
    /// Jev probability for this option.
    pub probability: f64,
}

/// A privacy-safe result from one `TypeSafe` selection call.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TypeSafeSkillRecommendation {
    /// Whether the call was disabled, skipped, successful, or unavailable.
    pub status: TypeSafeRecommendationStatus,
    /// Effective top-level `TypeSafe` state.
    pub mode: &'static str,
    /// Highest-probability skill, when a valid response was returned.
    pub selected_skill: Option<String>,
    /// Probability that any catalog skill applies.
    pub applicable_probability: Option<f64>,
    /// Concentration of the Choice distribution.
    pub choice_confidence: Option<f64>,
    /// Up to five choices in descending probability order.
    pub ranking: Vec<RankedSkill>,
    /// Whether both pre-committed activation thresholds passed.
    pub meets_thresholds: bool,
    /// Concrete model reported by `TypeSafe`.
    pub model: Option<String>,
    /// Billable `TypeSafe` input tokens.
    pub input_tokens: u64,
    /// `TypeSafe` output tokens.
    pub output_tokens: u64,
    /// Estimated cost at the current public $0.042 per million input-token rate.
    pub estimated_cost_usd: f64,
    /// End-to-end `TypeSafe` call latency.
    pub latency_ms: u64,
    /// User request size without recording its contents.
    pub request_bytes: usize,
    /// SHA-256 digest used to correlate repeated cases without storing text.
    pub request_sha256: String,
    /// Coarse failure category without a raw error or credential material.
    pub error_kind: Option<&'static str>,
}

/// Coarse result category that never contains credentials or request content.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypeSafeRecommendationStatus {
    /// Feature is off.
    Disabled,
    /// No text or no skills were available.
    Skipped,
    /// A verified typed response was returned.
    Success,
    /// Client setup, transport, API, or response validation failed.
    Unavailable,
}

/// Small, failure-isolated `TypeSafe` adapter for the skill-selection experiment.
pub struct TypeSafeSkillSelector {
    config: TypeSafeConfig,
    transport: Option<Arc<dyn SystemOne>>,
    initialization_error: Option<&'static str>,
}

impl TypeSafeSkillSelector {
    /// Builds a selector from `TYPESAFE_*` environment variables. Setup failure
    /// is retained as a coarse diagnostic and never blocks the caller.
    #[must_use]
    pub fn from_env(config: TypeSafeConfig) -> Self {
        if config.mode == TypeSafeMode::Off {
            return Self {
                config,
                transport: None,
                initialization_error: None,
            };
        }
        let retry = RetryPolicy::disabled().with_total_timeout(Some(config.timeout));
        match Client::builder()
            .default_model(&config.model)
            .timeout(config.timeout)
            .retry(retry)
            .build()
        {
            Ok(client) => Self {
                config,
                transport: Some(Arc::new(client)),
                initialization_error: None,
            },
            Err(error) => Self {
                config,
                transport: None,
                initialization_error: Some(error_kind(&error)),
            },
        }
    }

    /// Injects a transport for contract tests and offline evaluation.
    #[doc(hidden)]
    #[must_use]
    pub fn with_transport(config: TypeSafeConfig, transport: Arc<dyn SystemOne>) -> Self {
        Self {
            config,
            transport: Some(transport),
            initialization_error: None,
        }
    }

    /// Returns the configured runtime mode.
    #[must_use]
    pub const fn mode(&self) -> TypeSafeMode {
        self.config.mode
    }

    /// Sends one request containing an applicability Noul and a skill Choice.
    /// Every failure is converted into an unavailable recommendation.
    #[allow(
        clippy::cast_precision_loss,
        clippy::too_many_lines,
        reason = "the adapter keeps one auditable request/response path; token counts are bounded by the API"
    )]
    pub async fn recommend(
        &self,
        request: &str,
        skills: &[TypeSafeSkill],
    ) -> TypeSafeSkillRecommendation {
        let request_bytes = request.len();
        let request_sha256 = request_hash(request);
        let mode = self.config.mode.as_str();
        if self.config.mode == TypeSafeMode::Off {
            return empty_recommendation(
                mode,
                TypeSafeRecommendationStatus::Disabled,
                request_bytes,
                request_sha256,
                None,
            );
        }
        if request.trim().is_empty()
            || skills.is_empty()
            || request_bytes > MAX_SELECTION_REQUEST_BYTES
        {
            return empty_recommendation(
                mode,
                TypeSafeRecommendationStatus::Skipped,
                request_bytes,
                request_sha256,
                (request_bytes > MAX_SELECTION_REQUEST_BYTES).then_some("request_too_large"),
            );
        }
        let Some(transport) = self.transport.as_ref() else {
            return empty_recommendation(
                mode,
                TypeSafeRecommendationStatus::Unavailable,
                request_bytes,
                request_sha256,
                self.initialization_error,
            );
        };

        let mut questions = Questions::new();
        let applicability = questions.add(
            APPLICABILITY_QUESTION,
            NoulQuestion::new(
                "Does exactly one skill in `skills` clearly apply to `request` and provide specialized instructions that would materially help complete it?",
            )
            .with_criteria(
                "A listed skill directly covers the requested artifact or workflow; yes includes paraphrases of its description",
                "No listed skill directly applies, or only a generic coding or writing response is needed",
            ),
        );
        let ranking_question = skills.iter().fold(
            ChoiceQuestion::new(
                "Which skill in `skills` is the best direct match for `request`? Compare similar skills by their complete descriptions.",
            ),
            |question, skill| question.with_option(&skill.name, skill.description.clone()),
        );
        let ranking = questions.add(RANKING_QUESTION, ranking_question);
        let state = serde_json::json!({"request": request, "skills": skills});
        let Ok(content) = typesafe_client::Content::json(&state) else {
            return empty_recommendation(
                mode,
                TypeSafeRecommendationStatus::Unavailable,
                request_bytes,
                request_sha256,
                Some("encode"),
            );
        };
        let mut api_request = SystemOneRequest::new(content, questions);
        api_request.model = Some(self.config.model.clone());
        let options = CallOptions::default()
            .with_timeout(self.config.timeout)
            .with_retry(RetryPolicy::disabled().with_total_timeout(Some(self.config.timeout)));
        let started = std::time::Instant::now();
        let response = transport.send(&api_request, &options).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let mut unavailable = empty_recommendation(
                    mode,
                    TypeSafeRecommendationStatus::Unavailable,
                    request_bytes,
                    request_sha256,
                    Some(error_kind(&error)),
                );
                unavailable.latency_ms = latency_ms;
                return unavailable;
            }
        };
        let applicable = match response.answer(&applicability) {
            Ok(answer) => answer.noul,
            Err(_) => {
                return empty_recommendation(
                    mode,
                    TypeSafeRecommendationStatus::Unavailable,
                    request_bytes,
                    request_sha256,
                    Some("invalid_answer"),
                );
            }
        };
        let Ok(choice) = response.answer(&ranking) else {
            return empty_recommendation(
                mode,
                TypeSafeRecommendationStatus::Unavailable,
                request_bytes,
                request_sha256,
                Some("invalid_answer"),
            );
        };
        let mut ranked = choice
            .ranked()
            .into_iter()
            .map(|(name, probability)| RankedSkill {
                name: name.into(),
                probability,
            })
            .collect::<Vec<_>>();
        ranked.truncate(5);
        TypeSafeSkillRecommendation {
            status: TypeSafeRecommendationStatus::Success,
            mode,
            selected_skill: Some(choice.choice.clone()),
            applicable_probability: Some(applicable),
            choice_confidence: Some(choice.confidence),
            ranking: ranked,
            meets_thresholds: applicable >= self.config.skill_selection.applicability_threshold
                && choice.confidence >= self.config.skill_selection.confidence_threshold,
            model: Some(response.model),
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            estimated_cost_usd: response.usage.input_tokens as f64 * 0.042 / 1_000_000.0,
            latency_ms,
            request_bytes,
            request_sha256,
            error_kind: None,
        }
    }
}

fn empty_recommendation(
    mode: &'static str,
    status: TypeSafeRecommendationStatus,
    request_bytes: usize,
    request_sha256: String,
    error_kind: Option<&'static str>,
) -> TypeSafeSkillRecommendation {
    TypeSafeSkillRecommendation {
        status,
        mode,
        selected_skill: None,
        applicable_probability: None,
        choice_confidence: None,
        ranking: Vec::new(),
        meets_thresholds: false,
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: 0.0,
        latency_ms: 0,
        request_bytes,
        request_sha256,
        error_kind,
    }
}

fn request_hash(request: &str) -> String {
    format!("{:x}", Sha256::digest(request.as_bytes()))
}

fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::MissingApiKey | Error::InvalidApiKey | Error::InvalidBaseUrl { .. } => {
            "configuration"
        }
        Error::InvalidRequest(_) | Error::Encode(_) => "invalid_request",
        Error::Api(_) => "api",
        Error::Timeout(_) => "timeout",
        Error::Connection(_) | Error::HttpClient(_) => "connection",
        Error::Request(_) => "request",
        Error::Decode { .. } | Error::Answer(_) => "invalid_response",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use typesafe_client::fake::FakeSystemOne;

    use super::*;

    fn skills() -> Vec<TypeSafeSkill> {
        vec![
            TypeSafeSkill {
                name: "pdf".into(),
                description: "Create and inspect PDF forms".into(),
            },
            TypeSafeSkill {
                name: "spreadsheets".into(),
                description: "Analyze XLSX and CSV workbooks".into(),
            },
        ]
    }

    #[tokio::test]
    async fn asks_one_request_and_returns_ranked_typed_answers() {
        let fake = Arc::new(FakeSystemOne::new());
        fake.set_noul(APPLICABILITY_QUESTION, 0.94);
        fake.set_choice_probabilities(RANKING_QUESTION, [("spreadsheets", 0.9), ("pdf", 0.1)]);
        let selector = TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            fake.clone(),
        );

        let result = selector
            .recommend("Compare revenue by region", &skills())
            .await;

        assert_eq!(result.status, TypeSafeRecommendationStatus::Success);
        assert_eq!(result.selected_skill.as_deref(), Some("spreadsheets"));
        assert!(result.meets_thresholds);
        assert_eq!(fake.request_count(), 1);
        let diagnostic = serde_json::to_string(&result).expect("diagnostic");
        assert!(!diagnostic.contains("Compare revenue by region"));
        assert!(diagnostic.contains(&result.request_sha256));
        let sent = serde_json::to_value(fake.last_request().expect("request").state)
            .expect("serialized state");
        assert_eq!(sent["request"], "Compare revenue by region");
    }

    #[tokio::test]
    async fn off_mode_never_calls_the_transport() {
        let fake = Arc::new(FakeSystemOne::new());
        let selector =
            TypeSafeSkillSelector::with_transport(TypeSafeConfig::default(), fake.clone());

        let result = selector.recommend("make a PDF", &skills()).await;

        assert_eq!(result.status, TypeSafeRecommendationStatus::Disabled);
        assert_eq!(fake.request_count(), 0);
    }

    #[tokio::test]
    async fn service_failure_becomes_a_coarse_non_blocking_result() {
        let fake = Arc::new(FakeSystemOne::new());
        fake.push_error(Error::Timeout(typesafe_client::TransportError::new(
            "test timeout",
        )));
        let selector = TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            fake,
        );

        let result = selector.recommend("make a PDF", &skills()).await;

        assert_eq!(result.status, TypeSafeRecommendationStatus::Unavailable);
        assert_eq!(result.error_kind, Some("timeout"));
        assert!(result.selected_skill.is_none());
    }
}
