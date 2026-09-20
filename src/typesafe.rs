use std::{collections::BTreeSet, sync::Arc, time::Duration};

use serde::Serialize;
use sha2::{Digest, Sha256};
use typesafe_client::{
    CallOptions, ChoiceQuestion, Client, Error, NoulAnswer, NoulQuestion, QuestionKey, Questions,
    RetryPolicy, SystemOne, SystemOneRequest,
};

const APPLICABILITY_QUESTION: &str = "skill_applies";
const RANKING_QUESTION: &str = "best_skill";
const TOOL_QUESTION_PREFIX: &str = "tool_needed_";
const MAX_SELECTION_REQUEST_BYTES: usize = 64 * 1_024;
const MAX_TOOL_CATALOG_SIZE: usize = 255;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 2_048;
const MAX_SELECTION_STATE_BYTES: usize = 512 * 1_024;

/// Name and bounded description supplied to the `TypeSafe` selector.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TypeSafeSkill {
    /// Exact runtime skill name.
    pub name: String,
    /// Discovery summary; full skill instructions are never sent.
    pub description: String,
}

/// Name and bounded description supplied for one configured runtime tool.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TypeSafeTool {
    /// Exact registered tool name.
    pub name: String,
    /// Provider-facing summary; the parameter schema is deliberately excluded.
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
    /// Tool-pool shortlisting policy owned by the `TypeSafe` integration.
    pub tool_selection: TypeSafeToolSelectionConfig,
}

/// Internal policy for TypeSafe-backed skill selection.
#[derive(Debug, Clone)]
pub struct TypeSafeSkillSelectionConfig {
    /// Minimum probability that one catalog skill applies.
    pub applicability_threshold: f64,
    /// Minimum concentration of the Choice distribution.
    pub confidence_threshold: f64,
}

/// Conservative policy for turning independent tool-need judgments into a shortlist.
#[derive(Debug, Clone)]
pub struct TypeSafeToolSelectionConfig {
    /// Include a tool when its probability of being needed reaches this value.
    pub inclusion_threshold: f64,
    /// Fall back to the full pool when an omitted tool is at or above this value.
    pub uncertainty_floor: f64,
    /// Minimum provider-context reduction required before applying a shortlist.
    pub minimum_context_savings_tokens: u64,
}

impl Default for TypeSafeConfig {
    fn default() -> Self {
        Self {
            mode: TypeSafeMode::Off,
            model: "jev-latest".into(),
            timeout: Duration::from_millis(2_000),
            skill_selection: TypeSafeSkillSelectionConfig::default(),
            tool_selection: TypeSafeToolSelectionConfig::default(),
        }
    }
}

impl Default for TypeSafeToolSelectionConfig {
    fn default() -> Self {
        Self {
            inclusion_threshold: 0.60,
            uncertainty_floor: 0.55,
            minimum_context_savings_tokens: 256,
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

/// Probability that one configured tool may be needed during the current run.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RankedTool {
    /// Exact tool name from the bounded catalog.
    pub name: String,
    /// Probability that the tool may be needed at any point in the run.
    pub probability: f64,
}

/// Conservative tool-pool recommendation from the shared `TypeSafe` request.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TypeSafeToolRecommendation {
    /// Whether tool questions were skipped, successful, or unavailable.
    pub status: TypeSafeRecommendationStatus,
    /// Tools meeting the pre-committed inclusion threshold.
    pub selected_tools: Vec<String>,
    /// Every tool probability in deterministic catalog order.
    pub probabilities: Vec<RankedTool>,
    /// Whether no omitted tool fell into the configured uncertainty band.
    pub meets_thresholds: bool,
    /// Number of omitted tools whose probabilities require full-pool fallback.
    pub uncertain_tool_count: usize,
    /// Coarse failure category without raw service or credential material.
    pub error_kind: Option<&'static str>,
}

/// Skill and tool decisions evaluated together over one request state.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TypeSafeTurnRecommendation {
    /// At-most-one skill recommendation and shared request diagnostics.
    pub skill: TypeSafeSkillRecommendation,
    /// Multi-label tool-pool recommendation from independent Noul questions.
    pub tools: TypeSafeToolRecommendation,
}

/// Coarse result category that never contains credentials or request content.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypeSafeRecommendationStatus {
    /// Feature is off.
    Disabled,
    /// No text or no selection candidates were available.
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

    /// Sends the Phase 2 skill questions without tool-pool questions.
    ///
    /// This remains available for the versioned skill-selection benchmark. The
    /// runtime uses [`Self::recommend_turn`] so Phase 2 and Phase 3 share one call.
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
        self.recommend_turn(request, skills, &[]).await.skill
    }

    /// Sends one speculative request containing the independent skill and tool
    /// judgments needed for the current turn. Every setup, service, or answer
    /// failure becomes an unavailable result so the runtime can use its full
    /// deterministic fallback.
    #[allow(
        clippy::cast_precision_loss,
        clippy::too_many_lines,
        reason = "one auditable adapter keeps shared-call accounting and fallback semantics together"
    )]
    pub async fn recommend_turn(
        &self,
        request: &str,
        skills: &[TypeSafeSkill],
        tools: &[TypeSafeTool],
    ) -> TypeSafeTurnRecommendation {
        let request_bytes = request.len();
        let request_sha256 = request_hash(request);
        let mode = self.config.mode.as_str();
        if self.config.mode == TypeSafeMode::Off {
            return empty_turn_recommendation(
                mode,
                TypeSafeRecommendationStatus::Disabled,
                request_bytes,
                request_sha256,
                None,
            );
        }
        let unique_tools = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        if request.trim().is_empty()
            || (skills.is_empty() && tools.is_empty())
            || request_bytes > MAX_SELECTION_REQUEST_BYTES
            || tools.len() > MAX_TOOL_CATALOG_SIZE
            || unique_tools.len() != tools.len()
        {
            let error_kind = if request_bytes > MAX_SELECTION_REQUEST_BYTES {
                Some("request_too_large")
            } else if tools.len() > MAX_TOOL_CATALOG_SIZE {
                Some("tool_catalog_too_large")
            } else if unique_tools.len() != tools.len() {
                Some("duplicate_tool")
            } else {
                None
            };
            return empty_turn_recommendation(
                mode,
                TypeSafeRecommendationStatus::Skipped,
                request_bytes,
                request_sha256,
                error_kind,
            );
        }
        let Some(transport) = self.transport.as_ref() else {
            return empty_turn_recommendation(
                mode,
                TypeSafeRecommendationStatus::Unavailable,
                request_bytes,
                request_sha256,
                self.initialization_error,
            );
        };

        let mut questions = Questions::new();
        let skill_questions = if skills.is_empty() {
            None
        } else {
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
            Some((applicability, ranking))
        };
        let bounded_tools = tools
            .iter()
            .map(|tool| TypeSafeTool {
                name: tool.name.clone(),
                description: truncate_utf8(&tool.description, MAX_TOOL_DESCRIPTION_BYTES),
            })
            .collect::<Vec<_>>();
        let tool_questions = bounded_tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                let key = questions.add(
                    format!("{TOOL_QUESTION_PREFIX}{index}"),
                    NoulQuestion::new(format!(
                        "Could the tool at `tools[{index}]` be needed at any point to complete `request` correctly, including likely prerequisites and follow-up steps?"
                    ))
                    .with_criteria(
                        format!(
                            "Yes: `{}` provides a concrete capability that may be required during the complete multi-step task, even if it is not the first action",
                            tool.name
                        ),
                        format!(
                            "No: `{}` is unrelated or redundant for the task; do not count it merely as a fallback discovery option",
                            tool.name
                        ),
                    ),
                );
                (tool.name.clone(), key)
            })
            .collect::<Vec<(String, QuestionKey<NoulAnswer>)>>();
        let state = serde_json::json!({
            "request": request,
            "skills": skills,
            "tools": bounded_tools,
        });
        if serde_json::to_vec(&state)
            .map_or(true, |encoded| encoded.len() > MAX_SELECTION_STATE_BYTES)
        {
            return empty_turn_recommendation(
                mode,
                TypeSafeRecommendationStatus::Skipped,
                request_bytes,
                request_sha256,
                Some("selection_state_too_large"),
            );
        }
        let Ok(content) = typesafe_client::Content::json(&state) else {
            return empty_turn_recommendation(
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
                let mut unavailable = empty_turn_recommendation(
                    mode,
                    TypeSafeRecommendationStatus::Unavailable,
                    request_bytes,
                    request_sha256,
                    Some(error_kind(&error)),
                );
                unavailable.skill.latency_ms = latency_ms;
                return unavailable;
            }
        };
        let (selected_skill, applicable_probability, choice_confidence, ranking) =
            if let Some((applicability, ranking)) = skill_questions {
                let applicable = match response.answer(&applicability) {
                    Ok(answer) => answer.noul,
                    Err(_) => {
                        return invalid_answer_turn(
                            mode,
                            request_bytes,
                            request_sha256,
                            latency_ms,
                        );
                    }
                };
                let Ok(choice) = response.answer(&ranking) else {
                    return invalid_answer_turn(mode, request_bytes, request_sha256, latency_ms);
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
                (
                    Some(choice.choice.clone()),
                    Some(applicable),
                    Some(choice.confidence),
                    ranked,
                )
            } else {
                (None, None, None, Vec::new())
            };
        let mut tool_probabilities = Vec::with_capacity(tool_questions.len());
        for (name, key) in tool_questions {
            let Ok(answer) = response.answer(&key) else {
                return invalid_answer_turn(mode, request_bytes, request_sha256, latency_ms);
            };
            tool_probabilities.push(RankedTool {
                name,
                probability: answer.noul,
            });
        }
        let selected_tools = tool_probabilities
            .iter()
            .filter(|tool| tool.probability >= self.config.tool_selection.inclusion_threshold)
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let uncertain_tool_count = tool_probabilities
            .iter()
            .filter(|tool| {
                tool.probability < self.config.tool_selection.inclusion_threshold
                    && tool.probability >= self.config.tool_selection.uncertainty_floor
            })
            .count();
        let skill_meets_thresholds = applicable_probability.is_some_and(|applicable| {
            applicable >= self.config.skill_selection.applicability_threshold
                && choice_confidence.is_some_and(|confidence| {
                    confidence >= self.config.skill_selection.confidence_threshold
                })
        });
        let tool_status = if tools.is_empty() {
            TypeSafeRecommendationStatus::Skipped
        } else {
            TypeSafeRecommendationStatus::Success
        };
        TypeSafeTurnRecommendation {
            skill: TypeSafeSkillRecommendation {
                status: TypeSafeRecommendationStatus::Success,
                mode,
                selected_skill,
                applicable_probability,
                choice_confidence,
                ranking,
                meets_thresholds: skill_meets_thresholds,
                model: Some(response.model),
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                estimated_cost_usd: response.usage.input_tokens as f64 * 0.042 / 1_000_000.0,
                latency_ms,
                request_bytes,
                request_sha256,
                error_kind: None,
            },
            tools: TypeSafeToolRecommendation {
                status: tool_status,
                selected_tools,
                probabilities: tool_probabilities,
                meets_thresholds: tool_status == TypeSafeRecommendationStatus::Success
                    && uncertain_tool_count == 0,
                uncertain_tool_count,
                error_kind: None,
            },
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

fn empty_tool_recommendation(
    status: TypeSafeRecommendationStatus,
    error_kind: Option<&'static str>,
) -> TypeSafeToolRecommendation {
    TypeSafeToolRecommendation {
        status,
        selected_tools: Vec::new(),
        probabilities: Vec::new(),
        meets_thresholds: false,
        uncertain_tool_count: 0,
        error_kind,
    }
}

fn empty_turn_recommendation(
    mode: &'static str,
    status: TypeSafeRecommendationStatus,
    request_bytes: usize,
    request_sha256: String,
    error_kind: Option<&'static str>,
) -> TypeSafeTurnRecommendation {
    TypeSafeTurnRecommendation {
        skill: empty_recommendation(mode, status, request_bytes, request_sha256, error_kind),
        tools: empty_tool_recommendation(status, error_kind),
    }
}

fn invalid_answer_turn(
    mode: &'static str,
    request_bytes: usize,
    request_sha256: String,
    latency_ms: u64,
) -> TypeSafeTurnRecommendation {
    let mut result = empty_turn_recommendation(
        mode,
        TypeSafeRecommendationStatus::Unavailable,
        request_bytes,
        request_sha256,
        Some("invalid_answer"),
    );
    result.skill.latency_ms = latency_ms;
    result
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
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

    fn tools() -> Vec<TypeSafeTool> {
        vec![
            TypeSafeTool {
                name: "read_file".into(),
                description: "Read a workspace file".into(),
            },
            TypeSafeTool {
                name: "write_file".into(),
                description: "Create or replace a workspace file".into(),
            },
            TypeSafeTool {
                name: "calendar_events".into(),
                description: "List calendar events".into(),
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
    async fn one_turn_request_selects_multiple_tools_with_the_skill() {
        let fake = Arc::new(FakeSystemOne::new());
        fake.set_noul(APPLICABILITY_QUESTION, 0.94);
        fake.set_choice_probabilities(RANKING_QUESTION, [("spreadsheets", 0.9), ("pdf", 0.1)]);
        fake.set_noul("tool_needed_0", 0.92);
        fake.set_noul("tool_needed_1", 0.84);
        fake.set_noul("tool_needed_2", 0.03);
        let selector = TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            fake.clone(),
        );

        let result = selector
            .recommend_turn("Update the workbook on disk", &skills(), &tools())
            .await;

        assert_eq!(fake.request_count(), 1);
        assert_eq!(
            result.tools.selected_tools,
            ["read_file".to_owned(), "write_file".to_owned()]
        );
        assert!(result.tools.meets_thresholds);
        assert_eq!(result.skill.selected_skill.as_deref(), Some("spreadsheets"));
        let sent = serde_json::to_value(fake.last_request().expect("request").state)
            .expect("serialized state");
        assert_eq!(sent["tools"].as_array().map(Vec::len), Some(3));
        assert!(sent["tools"][0].get("parameters").is_none());
    }

    #[tokio::test]
    async fn uncertain_omission_requires_full_pool_fallback() {
        let fake = Arc::new(FakeSystemOne::new());
        fake.set_noul("tool_needed_0", 0.91);
        fake.set_noul("tool_needed_1", 0.57);
        fake.set_noul("tool_needed_2", 0.02);
        let selector = TypeSafeSkillSelector::with_transport(
            TypeSafeConfig {
                mode: TypeSafeMode::On,
                ..TypeSafeConfig::default()
            },
            fake,
        );

        let result = selector
            .recommend_turn("Inspect a file", &[], &tools())
            .await;

        assert!(!result.tools.meets_thresholds);
        assert_eq!(result.tools.uncertain_tool_count, 1);
        assert_eq!(result.tools.selected_tools, ["read_file".to_owned()]);
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
