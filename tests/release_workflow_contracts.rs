use std::{fs, path::PathBuf};

fn workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn ci_checks_every_supported_operating_system_natively() {
    let ci = workflow("ci.yml");

    assert!(
        ci.contains("workflow_dispatch:"),
        "CI must retain a manual verification fallback"
    );
    for runner in ["ubuntu-latest", "macos-14", "windows-latest"] {
        assert!(
            ci.contains(runner),
            "CI must exercise the platform-specific code on {runner}"
        );
    }
    for command in [
        "cargo clippy --workspace --all-targets --all-features",
        "cargo test --workspace --all-features",
        "cargo build --release",
    ] {
        assert!(ci.contains(command), "CI must run `{command}`");
    }
    assert!(
        ci.contains("-- -D warnings"),
        "CI must reject every Clippy warning"
    );
}

#[test]
fn release_starts_only_after_successful_main_ci() {
    let release = workflow("release.yml");

    for contract in [
        "workflow_run:",
        "workflows: [\"ci\"]",
        "github.event.workflow_run.conclusion == 'success'",
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.event == 'workflow_dispatch'",
        "github.event.workflow_run.head_branch == 'main'",
    ] {
        assert!(
            release.contains(contract),
            "release workflow is missing the gate `{contract}`"
        );
    }
}

#[test]
fn platform_failures_are_isolated_and_successful_artifacts_publish_independently() {
    let ci = workflow("ci.yml");
    let release = workflow("release.yml");

    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ] {
        assert!(
            release.contains(target),
            "release workflow must build {target}"
        );
    }
    assert!(
        ci.contains("continue-on-error: true"),
        "a failed native platform must not fail the entire CI workflow"
    );
    assert!(
        release.contains("continue-on-error: true"),
        "a failed release target must not block successful targets"
    );
    for command in [
        "cargo clippy --workspace --all-targets --all-features --locked",
        "cargo test --workspace --all-features --locked",
        "cargo build --release --locked",
    ] {
        assert!(
            release.contains(command),
            "each release target must pass its native `{command}` gate"
        );
    }
    assert!(
        release.contains("gh release upload"),
        "each successful target must upload its own release artifact"
    );
    assert!(
        !release.contains("actions/download-artifact"),
        "platform releases must not depend on a combined artifact-publishing job"
    );
    assert!(
        release.contains("Update package and lockfile version")
            && release.contains("Cargo.lock mimir package version was not found")
            && release.contains("cargo metadata --locked --no-deps"),
        "release preparation must synchronize and validate Cargo.toml and Cargo.lock"
    );
}
