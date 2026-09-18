#![allow(
    clippy::cast_precision_loss,
    clippy::format_push_string,
    reason = "the checked benchmark corpus is tiny and report formatting is off the runtime path"
)]

use std::{collections::BTreeSet, env, fs, path::PathBuf, process::ExitCode, time::Duration};

use mimir::typesafe::{
    TypeSafeConfig, TypeSafeMode, TypeSafeRecommendationStatus, TypeSafeSkillSelector, TypeSafeTool,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Dataset {
    name: String,
    protected_context_tokens: u64,
    tools: Vec<ToolFixture>,
    cases: Vec<Case>,
    gates: Gates,
}

#[derive(Debug, Deserialize)]
struct ToolFixture {
    name: String,
    description: String,
    context_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    request: String,
    required_tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Gates {
    minimum_required_tool_recall_percent: f64,
    minimum_context_savings_percent: f64,
    minimum_net_savings_tokens_per_case: f64,
    maximum_p95_latency_ms: f64,
    maximum_average_input_tokens: f64,
}

struct CaseResult {
    id: String,
    required_tools: Vec<String>,
    suggested_tools: Vec<String>,
    missing_tools: Vec<String>,
    fallback: bool,
    context_tokens: u64,
    input_tokens: u64,
    latency_ms: u64,
    minimum_required_probability: f64,
    maximum_irrelevant_probability: f64,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("typesafe tool benchmark failed: {error}");
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
        || PathBuf::from("benchmarks/typesafe-tool-selection/cases.json"),
        PathBuf::from,
    );
    let report_path = args.get(1).map(PathBuf::from);
    if args.len() > 2 {
        return Err("usage: typesafe_tool_benchmark [--jev] [DATASET] [REPORT]".into());
    }
    let dataset: Dataset = serde_json::from_slice(&fs::read(&dataset_path)?)?;
    validate(&dataset)?;
    let report = if jev {
        shadow_report(&dataset, &evaluate(&dataset).await?)
    } else {
        baseline_report(&dataset)
    };
    if let Some(path) = report_path {
        fs::write(path, &report)?;
    }
    print!("{report}");
    Ok(())
}

fn validate(dataset: &Dataset) -> Result<(), Box<dyn std::error::Error>> {
    let names = dataset
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    if names.len() != dataset.tools.len() {
        return Err("tool fixture names must be unique".into());
    }
    for case in &dataset.cases {
        if let Some(unknown) = case
            .required_tools
            .iter()
            .find(|name| !names.contains(name.as_str()))
        {
            return Err(format!("case {} requires unknown tool {unknown}", case.id).into());
        }
    }
    Ok(())
}

async fn evaluate(dataset: &Dataset) -> Result<Vec<CaseResult>, Box<dyn std::error::Error>> {
    let selector = TypeSafeSkillSelector::from_env(TypeSafeConfig {
        mode: TypeSafeMode::On,
        timeout: Duration::from_millis(2_500),
        ..TypeSafeConfig::default()
    });
    let tools = dataset
        .tools
        .iter()
        .map(|tool| TypeSafeTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
        })
        .collect::<Vec<_>>();
    let all = dataset
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let mut results = Vec::with_capacity(dataset.cases.len());
    for case in &dataset.cases {
        let recommendation = selector.recommend_turn(&case.request, &[], &tools).await;
        if recommendation.skill.status != TypeSafeRecommendationStatus::Success
            || recommendation.tools.status != TypeSafeRecommendationStatus::Success
        {
            return Err(format!(
                "Jev request {} failed ({})",
                case.id,
                recommendation
                    .skill
                    .error_kind
                    .or(recommendation.tools.error_kind)
                    .unwrap_or("unknown")
            )
            .into());
        }
        let fallback = !recommendation.tools.meets_thresholds;
        let suggested = recommendation
            .tools
            .selected_tools
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let selected = if fallback {
            all.clone()
        } else {
            suggested.clone()
        };
        let minimum_required_probability = case
            .required_tools
            .iter()
            .filter_map(|required| {
                recommendation
                    .tools
                    .probabilities
                    .iter()
                    .find(|tool| &tool.name == required)
                    .map(|tool| tool.probability)
            })
            .reduce(f64::min)
            .unwrap_or(1.0);
        let maximum_irrelevant_probability = recommendation
            .tools
            .probabilities
            .iter()
            .filter(|tool| !case.required_tools.contains(&tool.name))
            .map(|tool| tool.probability)
            .reduce(f64::max)
            .unwrap_or_default();
        let missing_tools = case
            .required_tools
            .iter()
            .filter(|name| !selected.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        let context_tokens = dataset.protected_context_tokens
            + dataset
                .tools
                .iter()
                .filter(|tool| selected.contains(&tool.name))
                .map(|tool| tool.context_tokens)
                .sum::<u64>();
        results.push(CaseResult {
            id: case.id.clone(),
            required_tools: case.required_tools.clone(),
            suggested_tools: suggested.into_iter().collect(),
            missing_tools,
            fallback,
            context_tokens,
            input_tokens: recommendation.skill.input_tokens,
            latency_ms: recommendation.skill.latency_ms,
            minimum_required_probability,
            maximum_irrelevant_probability,
        });
    }
    Ok(results)
}

fn baseline_report(dataset: &Dataset) -> String {
    let full_context = full_context_tokens(dataset);
    let required = dataset
        .cases
        .iter()
        .map(|case| case.required_tools.len())
        .sum::<usize>();
    format!(
        "# Phase 3 tool-pool baseline\n\nDataset: `{}` ({})\n\n## Result\n\n| Metric | Value |\n| --- | ---: |\n| Configured optional tools | {} |\n| Required-tool recall | 100.0% |\n| Tool context per provider call | {} tokens |\n| Average required tools per case | {:.2} |\n| Avoidable tool context | {:.1}% |\n| Selection calls | 0 |\n\nThe current runtime sends the complete configured tool pool on every provider step. This baseline deliberately gives it perfect required-tool recall, then treats every non-required schema as avoidable context. The recovery tools retained by the Phase 3 design account for {} additional tokens in both arms.\n\n## Pre-committed gate\n\n- Required-tool recall: at least {:.1}%.\n- Provider tool-context savings: at least {:.1}%.\n- Net savings after the complete Jev input: at least {:.0} tokens per case on the first provider step.\n- Jev p95 latency: at most {:.0} ms.\n- Average Jev input: at most {:.0} tokens per case.\n",
        dataset.name,
        dataset.cases.len(),
        dataset.tools.len(),
        full_context,
        required as f64 / dataset.cases.len().max(1) as f64,
        avoidable_context_percent(dataset),
        dataset.protected_context_tokens,
        dataset.gates.minimum_required_tool_recall_percent,
        dataset.gates.minimum_context_savings_percent,
        dataset.gates.minimum_net_savings_tokens_per_case,
        dataset.gates.maximum_p95_latency_ms,
        dataset.gates.maximum_average_input_tokens,
    )
}

fn shadow_report(dataset: &Dataset, results: &[CaseResult]) -> String {
    let full_context = full_context_tokens(dataset);
    let total_required = results
        .iter()
        .map(|result| result.required_tools.len())
        .sum::<usize>();
    let missing = results
        .iter()
        .map(|result| result.missing_tools.len())
        .sum::<usize>();
    let recall =
        (total_required.saturating_sub(missing)) as f64 * 100.0 / total_required.max(1) as f64;
    let average_context = results
        .iter()
        .map(|result| result.context_tokens as f64)
        .sum::<f64>()
        / results.len().max(1) as f64;
    let context_savings =
        (full_context as f64 - average_context) * 100.0 / (full_context as f64).max(1.0);
    let average_input = results
        .iter()
        .map(|result| result.input_tokens as f64)
        .sum::<f64>()
        / results.len().max(1) as f64;
    let average_net_savings = full_context as f64 - average_context - average_input;
    let mut latencies = results
        .iter()
        .map(|result| result.latency_ms)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let p95_index = latencies
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    let p95_latency = latencies.get(p95_index).copied().unwrap_or_default() as f64;
    let input_tokens = results
        .iter()
        .map(|result| result.input_tokens)
        .sum::<u64>();
    let estimated_cost = input_tokens as f64 * 0.042 / 1_000_000.0;
    let fallback_cases = results.iter().filter(|result| result.fallback).count();
    let passed = recall >= dataset.gates.minimum_required_tool_recall_percent
        && context_savings >= dataset.gates.minimum_context_savings_percent
        && average_net_savings >= dataset.gates.minimum_net_savings_tokens_per_case
        && p95_latency <= dataset.gates.maximum_p95_latency_ms
        && average_input <= dataset.gates.maximum_average_input_tokens;
    let mut output = format!(
        "# Phase 3 TypeSafe tool-pool shadow experiment\n\nDataset: `{}` ({})\n\n## Comparison\n\n| Metric | Full pool | TypeSafe shortlist | Gate |\n| --- | ---: | ---: | --- |\n| Required-tool recall | 100.0% | {:.1}% | at least {:.1}% |\n| Tool context per provider call | {} | {:.1} tokens | savings at least {:.1}% |\n| Net first-step savings after Jev input | 0 | {:.1} tokens/case | at least {:.0} |\n| Selection p95 latency | 0 | {:.0} ms | at most {:.0} ms |\n| TypeSafe input | 0 | {:.1} tokens/case | at most {:.0} |\n| Full-pool uncertainty fallbacks | n/a | {} | bounded and safe |\n| Estimated selection cost | $0 | ${:.6} total | $0.042/MTok |\n\nPhase 3 gate: **{}**. Context savings: {:.1}%. Missing required tools: {} of {}.\n\nJev evaluated every optional tool with an independent Noul in the same request used by skill selection. A tool is included at probability 0.60 or above. Any omitted tool at 0.55 or above makes the runtime keep the full pool. `search_tools`, `search_skills`, and `finish_task` remain available outside the evaluated pool, and `search_tools` can activate an omitted configured tool on the next model step. Net savings charge the complete TypeSafe request against only one provider step; later tool-loop steps increase the savings.\n\n## Cases\n\n| Case | Required | Jev shortlist | Missing after policy | Fallback | Min required p | Max irrelevant p | Tool context | Jev input | Latency (ms) |\n| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |\n",
        dataset.name,
        results.len(),
        recall,
        dataset.gates.minimum_required_tool_recall_percent,
        full_context,
        average_context,
        dataset.gates.minimum_context_savings_percent,
        average_net_savings,
        dataset.gates.minimum_net_savings_tokens_per_case,
        p95_latency,
        dataset.gates.maximum_p95_latency_ms,
        average_input,
        dataset.gates.maximum_average_input_tokens,
        fallback_cases,
        estimated_cost,
        if passed { "PASS" } else { "STOP" },
        context_savings,
        missing,
        total_required,
    );
    for result in results {
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.2} | {:.2} | {} | {} | {} |\n",
            result.id,
            display_names(&result.required_tools),
            display_names(&result.suggested_tools),
            display_names(&result.missing_tools),
            if result.fallback { "full pool" } else { "no" },
            result.minimum_required_probability,
            result.maximum_irrelevant_probability,
            result.context_tokens,
            result.input_tokens,
            result.latency_ms,
        ));
    }
    output
}

fn full_context_tokens(dataset: &Dataset) -> u64 {
    dataset.protected_context_tokens
        + dataset
            .tools
            .iter()
            .map(|tool| tool.context_tokens)
            .sum::<u64>()
}

fn avoidable_context_percent(dataset: &Dataset) -> f64 {
    let full = full_context_tokens(dataset) as f64;
    let average_required = dataset
        .cases
        .iter()
        .map(|case| {
            dataset.protected_context_tokens
                + dataset
                    .tools
                    .iter()
                    .filter(|tool| case.required_tools.contains(&tool.name))
                    .map(|tool| tool.context_tokens)
                    .sum::<u64>()
        })
        .map(|tokens| tokens as f64)
        .sum::<f64>()
        / dataset.cases.len().max(1) as f64;
    (full - average_required) * 100.0 / full.max(1.0)
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}
