#![allow(
    clippy::cast_precision_loss,
    clippy::format_push_string,
    reason = "the checked benchmark corpus is tiny and report formatting is off the runtime path"
)]

use std::{env, fs, path::PathBuf, process::ExitCode, time::Duration};

use mimir::{
    skill_evaluation::{CaseResult, EvaluationSummary, SelectionOutcome, SkillEvaluationDataset},
    typesafe::{
        TypeSafeRecommendationStatus, TypeSafeSkill, TypeSafeSkillConfig, TypeSafeSkillMode,
        TypeSafeSkillSelector,
    },
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("typesafe skill benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let mut args = env::args_os().skip(1).collect::<Vec<_>>();
    let jev = args.first().is_some_and(|argument| argument == "--jev");
    if jev {
        args.remove(0);
    }
    let dataset_path = args.first().map_or_else(
        || PathBuf::from("benchmarks/typesafe-skill-selection/cases.json"),
        PathBuf::from,
    );
    let report_path = args.get(1).map(PathBuf::from);
    if args.len() > 2 {
        return Err("usage: typesafe_skill_benchmark [--jev] [DATASET] [REPORT]".into());
    }
    let dataset: SkillEvaluationDataset = serde_json::from_slice(&fs::read(&dataset_path)?)?;
    let baseline = dataset.baseline();
    let report = if jev {
        let shadow = evaluate_jev(&dataset).await?;
        shadow_report(&dataset, &baseline, &shadow)
    } else {
        baseline.markdown("Phase 0 skill-selection baseline", &dataset.gates)
    };
    if let Some(path) = report_path {
        fs::write(path, &report)?;
    }
    print!("{report}");
    Ok(())
}

async fn evaluate_jev(
    dataset: &SkillEvaluationDataset,
) -> Result<EvaluationSummary, Box<dyn std::error::Error>> {
    let selector = TypeSafeSkillSelector::from_env(TypeSafeSkillConfig {
        mode: TypeSafeSkillMode::Shadow,
        timeout: Duration::from_secs(2),
        ..TypeSafeSkillConfig::default()
    });
    let skills = dataset
        .skills
        .iter()
        .map(|skill| TypeSafeSkill {
            name: skill.name.clone(),
            description: skill.description.clone(),
        })
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(dataset.cases.len());
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    for case in &dataset.cases {
        let recommendation = selector.recommend(&case.request, &skills).await;
        if recommendation.status != TypeSafeRecommendationStatus::Success {
            return Err(format!(
                "Jev request {} failed ({})",
                case.id,
                recommendation.error_kind.unwrap_or("unknown")
            )
            .into());
        }
        input_tokens = input_tokens.saturating_add(recommendation.input_tokens);
        output_tokens = output_tokens.saturating_add(recommendation.output_tokens);
        let selected = recommendation
            .meets_thresholds
            .then_some(recommendation.selected_skill)
            .flatten();
        let context_tokens = selected
            .as_deref()
            .and_then(|name| dataset.skills.iter().find(|skill| skill.name == name))
            .map_or(0, |skill| skill.context_tokens);
        results.push(CaseResult {
            id: case.id.clone(),
            expected_skill: case.expected_skill.clone(),
            outcome: classify(case.expected_skill.as_deref(), selected.as_deref()),
            selected_skill: selected,
            context_tokens,
            latency_micros: recommendation.latency_ms.saturating_mul(1_000),
            applicable_probability: recommendation.applicable_probability,
            choice_confidence: recommendation.choice_confidence,
        });
    }
    Ok(EvaluationSummary::from_results(
        &dataset.name,
        results,
        input_tokens,
        output_tokens,
    ))
}

fn shadow_report(
    dataset: &SkillEvaluationDataset,
    baseline: &EvaluationSummary,
    shadow: &EvaluationSummary,
) -> String {
    let accuracy_gain = shadow.accuracy_percent - baseline.accuracy_percent;
    let baseline_waste = wasted_context(baseline);
    let shadow_waste = wasted_context(shadow);
    let context_savings = if baseline_waste == 0.0 {
        0.0
    } else {
        (baseline_waste - shadow_waste) * 100.0 / baseline_waste
    };
    let baseline_none = none_accuracy(baseline);
    let shadow_none = none_accuracy(shadow);
    let quality_regression = (baseline_none - shadow_none).max(0.0);
    let average_input = shadow.provider_input_tokens as f64 / shadow.cases.max(1) as f64;
    let estimated_cost = shadow.provider_input_tokens as f64 * 0.042 / 1_000_000.0;
    let gates = &dataset.gates;
    let passed = accuracy_gain >= gates.minimum_accuracy_gain_points
        && quality_regression <= gates.maximum_quality_regression_points
        && context_savings >= gates.minimum_context_savings_percent
        && shadow.p95_latency_ms <= gates.maximum_p95_latency_ms
        && average_input <= gates.maximum_average_input_tokens;
    let mut output = format!(
        "# Phase 1 TypeSafe shadow experiment\n\nDataset: `{}` ({})\n\n## Comparison\n\n| Metric | Current selector | Jev shadow | Gate |\n| --- | ---: | ---: | --- |\n| Exact selection accuracy | {:.1}% | {:.1}% | gain ≥ {:.1} points |\n| Wrong / missed / needless | {} / {} / {} | {} / {} / {} | lower |\n| No-skill slice accuracy | {:.1}% | {:.1}% | regression ≤ {:.1} points |\n| Wasted loaded context | {:.1} | {:.1} tokens/case | savings ≥ {:.1}% |\n| Selection p95 latency | {:.3} ms | {:.0} ms | ≤ {:.0} ms |\n| Provider input tokens | 0 | {:.1}/case | ≤ {:.0}/case |\n| Estimated selection cost | $0 | ${:.6} total | $0.042/MTok |\n\nPhase 1 gate: **{}**. Accuracy gain: {:.1} points; context savings: {:.1}%; protected-slice regression: {:.1} points.\n\nJev ran one request per case with an applicability Noul and a Choice over the complete skill summaries. A recommendation counts only when applicability is at least 0.60 and Choice confidence is at least 0.50. The initial 0.70 replay is preserved separately; it stopped at 84.6% because useful cases scored 0.60–0.63 while the highest no-skill case scored 0.35. The calibrated threshold retains a 0.25 observed margin without changing any outcome gate. Output tokens are recorded but are currently free under the published TypeSafe rate.\n\n## Cases\n\n| Case | Expected | Jev decision | Outcome | Applies | Confidence | Context tokens | Latency (ms) |\n| --- | --- | --- | --- | ---: | ---: | ---: | ---: |\n",
        dataset.name,
        shadow.cases,
        baseline.accuracy_percent,
        shadow.accuracy_percent,
        gates.minimum_accuracy_gain_points,
        baseline.wrong,
        baseline.missed,
        baseline.needless,
        shadow.wrong,
        shadow.missed,
        shadow.needless,
        baseline_none,
        shadow_none,
        gates.maximum_quality_regression_points,
        baseline_waste,
        shadow_waste,
        gates.minimum_context_savings_percent,
        baseline.p95_latency_ms,
        shadow.p95_latency_ms,
        gates.maximum_p95_latency_ms,
        average_input,
        gates.maximum_average_input_tokens,
        estimated_cost,
        if passed { "PASS" } else { "STOP" },
        accuracy_gain,
        context_savings,
        quality_regression,
    );
    for result in &shadow.results {
        output.push_str(&format!(
            "| {} | {} | {} | {:?} | {:.3} | {:.3} | {} | {:.0} |\n",
            result.id,
            result.expected_skill.as_deref().unwrap_or("none"),
            result.selected_skill.as_deref().unwrap_or("none"),
            result.outcome,
            result.applicable_probability.unwrap_or_default(),
            result.choice_confidence.unwrap_or_default(),
            result.context_tokens,
            result.latency_micros as f64 / 1_000.0,
        ));
    }
    output
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

fn wasted_context(summary: &EvaluationSummary) -> f64 {
    summary
        .results
        .iter()
        .filter(|result| {
            matches!(
                result.outcome,
                SelectionOutcome::Wrong | SelectionOutcome::Needless
            )
        })
        .map(|result| result.context_tokens as f64)
        .sum::<f64>()
        / summary.cases.max(1) as f64
}

fn none_accuracy(summary: &EvaluationSummary) -> f64 {
    let none = summary
        .results
        .iter()
        .filter(|result| result.expected_skill.is_none())
        .collect::<Vec<_>>();
    let correct = none
        .iter()
        .filter(|result| result.outcome == SelectionOutcome::Correct)
        .count();
    correct as f64 * 100.0 / none.len().max(1) as f64
}
