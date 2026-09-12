use std::{fs, path::PathBuf};

fn workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn manifest() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn package_name_does_not_change_the_public_mimir_targets() {
    let manifest = manifest();

    assert!(
        manifest.contains("[package]\nname = \"mimir-ai\""),
        "the publishable package must use the available mimir-ai name"
    );
    assert!(
        manifest.contains("[lib]\nname = \"mimir\"\npath = \"src/lib.rs\""),
        "the Rust library target must remain named mimir"
    );
    assert!(
        manifest.contains("[[bin]]\nname = \"mimir\"\npath = \"src/main.rs\""),
        "cargo install mimir-ai must continue to install the mimir executable"
    );
}

#[test]
fn ci_checks_enabled_operating_systems_natively() {
    let ci = workflow("ci.yml");

    assert!(
        ci.contains("workflow_dispatch:"),
        "CI must retain a manual verification fallback"
    );
    assert!(
        !ci.contains("push:") && !ci.contains("pull_request:"),
        "CI is intentionally paused except for explicit manual dispatch"
    );
    for runner in ["ubuntu-latest", "macos-14"] {
        assert!(
            ci.contains(runner),
            "CI must exercise the platform-specific code on {runner}"
        );
    }
    assert!(
        !ci.contains("windows-latest") && !ci.contains("x86_64-pc-windows-msvc"),
        "Windows verification is intentionally disabled until native support returns"
    );
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
fn release_runs_only_on_manual_dispatch() {
    let release = workflow("release.yml");

    assert!(
        release.contains("workflow_dispatch:"),
        "releases must support explicit manual dispatch"
    );
    assert!(
        !release.contains("workflow_run:") && !release.contains("github.event.workflow_run"),
        "CI completion must not publish a release automatically"
    );
}

#[test]
fn platform_failures_are_isolated_and_successful_artifacts_publish_independently() {
    let ci = workflow("ci.yml");
    let release = workflow("release.yml");

    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        assert!(
            release.contains(target),
            "release workflow must build {target}"
        );
    }
    assert!(
        !release.contains("windows-latest") && !release.contains("x86_64-pc-windows-msvc"),
        "Windows publishing is intentionally disabled until native support returns"
    );
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
            && release.contains("Cargo.lock mimir-ai package version was not found")
            && release.contains("cargo metadata --locked --no-deps"),
        "release preparation must synchronize and validate Cargo.toml and Cargo.lock"
    );
}
