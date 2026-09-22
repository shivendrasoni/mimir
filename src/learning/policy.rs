//! Deterministic learning gates. Jev supplies judgments; this module owns decisions.

use crate::typesafe::TypeSafeLearningEvaluation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationDisposition {
    Discard,
    NewCluster,
    MatchCluster,
}

pub fn disposition(evaluation: &TypeSafeLearningEvaluation) -> EvaluationDisposition {
    let confident = evaluation
        .categorical_confidence
        .is_some_and(|value| value >= 0.75);
    let strong_reuse = evaluation.reuse_value.is_some_and(|value| value >= 0.65);
    let safe = evaluation.overfit_risk.is_some_and(|value| value <= 0.34);
    let resolved = matches!(
        evaluation.resolution.as_deref(),
        Some("validated" | "observed")
    );
    if !confident
        || !strong_reuse
        || !safe
        || !resolved
        || matches!(evaluation.lesson_kind.as_deref(), None | Some("none"))
    {
        return EvaluationDisposition::Discard;
    }
    if matches!(evaluation.cluster_match.as_deref(), Some(value) if value != "none") {
        EvaluationDisposition::MatchCluster
    } else {
        EvaluationDisposition::NewCluster
    }
}

pub fn correction_is_strong(evaluation: &TypeSafeLearningEvaluation) -> bool {
    evaluation.lesson_kind.as_deref() == Some("correction")
        && evaluation.resolution.as_deref() == Some("validated")
        && evaluation
            .human_correction_probability
            .is_some_and(|value| value >= 0.85)
        && evaluation
            .categorical_confidence
            .is_some_and(|value| value >= 0.75)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typesafe::{TypeSafeLearningEvaluation, TypeSafeRecommendationStatus};
    fn sample() -> TypeSafeLearningEvaluation {
        TypeSafeLearningEvaluation {
            status: TypeSafeRecommendationStatus::Success,
            lesson_kind: Some("workflow".into()),
            resolution: Some("validated".into()),
            scope: Some("project".into()),
            cluster_match: Some("none".into()),
            reuse_value: Some(0.9),
            overfit_risk: Some(0.1),
            human_correction_probability: Some(0.0),
            categorical_confidence: Some(0.9),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
            rubric_hash: "x".into(),
            error_kind: None,
        }
    }
    #[test]
    fn low_confidence_never_forces_a_cluster() {
        let mut value = sample();
        value.categorical_confidence = Some(0.2);
        assert_eq!(disposition(&value), EvaluationDisposition::Discard);
    }
}
