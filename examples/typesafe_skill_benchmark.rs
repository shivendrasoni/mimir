use std::{env, fs, path::PathBuf, process::ExitCode};

use mimir::skill_evaluation::SkillEvaluationDataset;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("typesafe skill benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let dataset_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("benchmarks/typesafe-skill-selection/cases.json"));
    let report_path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("usage: typesafe_skill_benchmark [DATASET] [REPORT]".into());
    }
    let dataset: SkillEvaluationDataset = serde_json::from_slice(&fs::read(&dataset_path)?)?;
    let summary = dataset.baseline();
    let report = summary.markdown("Phase 0 skill-selection baseline", &dataset.gates);
    if let Some(path) = report_path {
        fs::write(path, &report)?;
    }
    print!("{report}");
    Ok(())
}
