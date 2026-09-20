#![allow(
    missing_docs,
    reason = "public only so the repository benchmark example can reuse the measured fixtures"
)]
#![allow(
    clippy::cast_precision_loss,
    clippy::format_push_string,
    reason = "the checked benchmark corpus is tiny and report formatting is off the runtime path"
)]

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::skills::{SkillSummary, rank_skill_summaries};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvaluationDataset {
    pub name: String,
    pub skills: Vec<SkillFixture>,
    pub cases: Vec<SkillCase>,
    pub gates: EvaluationGates,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFixture {
    pub name: String,
    pub description: String,
    pub context_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCase {
    pub id: String,
    pub request: String,
    pub expected_skill: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationGates {
    pub baseline_problem_rate_percent: f64,
    pub minimum_accuracy_gain_points: f64,
    pub maximum_quality_regression_points: f64,
    pub minimum_context_savings_percent: f64,
    pub maximum_p95_latency_ms: f64,
    pub maximum_average_input_tokens: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub expected_skill: Option<String>,
    pub selected_skill: Option<String>,
    pub outcome: SelectionOutcome,
    pub context_tokens: u64,
    pub latency_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicable_probability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_confidence: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionOutcome {
    Correct,
    Wrong,
    Missed,
    Needless,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationSummary {
    pub dataset: String,
    pub cases: usize,
    pub correct: usize,
    pub wrong: usize,
    pub missed: usize,
    pub needless: usize,
    pub accuracy_percent: f64,
    pub problem_rate_percent: f64,
    pub average_context_tokens: f64,
    pub p95_latency_ms: f64,
    pub provider_input_tokens: u64,
    pub provider_output_tokens: u64,
    pub results: Vec<CaseResult>,
}

impl SkillEvaluationDataset {
    #[must_use]
    pub fn baseline(&self) -> EvaluationSummary {
        let summaries = self
            .skills
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(self.cases.len());
        for case in &self.cases {
            let started = Instant::now();
            let selected = rank_skill_summaries(&case.request, &summaries, 1)
                .first()
                .map(|skill| skill.name.clone());
            let elapsed = started.elapsed();
            let context_tokens = selected
                .as_deref()
                .and_then(|name| self.skills.iter().find(|skill| skill.name == name))
                .map_or(0, |skill| skill.context_tokens);
            results.push(CaseResult {
                id: case.id.clone(),
                expected_skill: case.expected_skill.clone(),
                outcome: classify(case.expected_skill.as_deref(), selected.as_deref()),
                selected_skill: selected,
                context_tokens,
                latency_micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
                applicable_probability: None,
                choice_confidence: None,
            });
        }
        EvaluationSummary::from_results(&self.name, results, 0, 0)
    }
}

impl EvaluationSummary {
    #[must_use]
    pub fn from_results(
        dataset: &str,
        results: Vec<CaseResult>,
        provider_input_tokens: u64,
        provider_output_tokens: u64,
    ) -> Self {
        let cases = results.len();
        let correct = count(&results, SelectionOutcome::Correct);
        let wrong = count(&results, SelectionOutcome::Wrong);
        let missed = count(&results, SelectionOutcome::Missed);
        let needless = count(&results, SelectionOutcome::Needless);
        let divisor = cases.max(1) as f64;
        let mut latencies = results
            .iter()
            .map(|result| result.latency_micros)
            .collect::<Vec<_>>();
        latencies.sort_unstable();
        let p95_index = cases.saturating_mul(95).div_ceil(100).saturating_sub(1);
        let p95_latency_ms = latencies.get(p95_index).copied().unwrap_or_default() as f64 / 1_000.0;
        Self {
            dataset: dataset.into(),
            cases,
            correct,
            wrong,
            missed,
            needless,
            accuracy_percent: correct as f64 * 100.0 / divisor,
            problem_rate_percent: (wrong + missed + needless) as f64 * 100.0 / divisor,
            average_context_tokens: results
                .iter()
                .map(|result| result.context_tokens as f64)
                .sum::<f64>()
                / divisor,
            p95_latency_ms,
            provider_input_tokens,
            provider_output_tokens,
            results,
        }
    }

    #[must_use]
    pub fn markdown(&self, title: &str, gates: &EvaluationGates) -> String {
        let gate_passed = self.problem_rate_percent >= gates.baseline_problem_rate_percent;
        let mut output = format!(
            "# {title}\n\nDataset: `{}` ({})\n\n## Result\n\n| Metric | Value |\n| --- | ---: |\n| Correct | {} |\n| Wrong | {} |\n| Missed | {} |\n| Needless | {} |\n| Accuracy | {:.1}% |\n| Problem rate | {:.1}% |\n| Average loaded skill context | {:.1} tokens |\n| Selection p95 latency | {:.3} ms |\n| Selection provider tokens | {} input / {} output |\n\nPhase 0 gate: **{}** (problem rate {:.1}% vs required {:.1}%).\n\nTask success is represented by exact labelled skill selection. Provider cost is zero for the current local selector. Context tokens count the selected fixture's full instruction budget; latency measures only selection.\n\n## Pre-committed experiment thresholds\n\n- Accuracy improvement: at least {:.1} percentage points.\n- Quality regression on any protected slice: at most {:.1} percentage points.\n- Loaded-context savings: at least {:.1}%.\n- Jev p95 latency: at most {:.0} ms.\n- Jev average input budget: at most {:.0} tokens per eligible turn.\n\n## Cases\n\n| Case | Expected | Selected | Outcome | Context tokens | Latency (µs) |\n| --- | --- | --- | --- | ---: | ---: |\n",
            self.dataset,
            self.cases,
            self.correct,
            self.wrong,
            self.missed,
            self.needless,
            self.accuracy_percent,
            self.problem_rate_percent,
            self.average_context_tokens,
            self.p95_latency_ms,
            self.provider_input_tokens,
            self.provider_output_tokens,
            if gate_passed { "PASS" } else { "STOP" },
            self.problem_rate_percent,
            gates.baseline_problem_rate_percent,
            gates.minimum_accuracy_gain_points,
            gates.maximum_quality_regression_points,
            gates.minimum_context_savings_percent,
            gates.maximum_p95_latency_ms,
            gates.maximum_average_input_tokens,
        );
        for result in &self.results {
            output.push_str(&format!(
                "| {} | {} | {} | {:?} | {} | {} |\n",
                result.id,
                result.expected_skill.as_deref().unwrap_or("none"),
                result.selected_skill.as_deref().unwrap_or("none"),
                result.outcome,
                result.context_tokens,
                result.latency_micros,
            ));
        }
        output
    }
}

fn classify(expected: Option<&str>, selected: Option<&str>) -> SelectionOutcome {
    match (expected, selected) {
        (None, None) | (Some(_), Some(_)) if expected == selected => SelectionOutcome::Correct,
        (Some(_), None) => SelectionOutcome::Missed,
        (None, Some(_)) => SelectionOutcome::Needless,
        (Some(_), Some(_)) => SelectionOutcome::Wrong,
        (None, None) => SelectionOutcome::Correct,
    }
}

fn count(results: &[CaseResult], outcome: SelectionOutcome) -> usize {
    results
        .iter()
        .filter(|result| result.outcome == outcome)
        .count()
}
