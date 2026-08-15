# Mimir — Rust

A standalone, bounded agentic runtime written in safe Rust. Mimir combines provider adaptation, model/tool execution, workspace tools, durable sessions, context and skill loading, goals, schedules, subagents, TUI/CLI/JSON/JSON-RPC operation, auth and OAuth login, daemon IPC, extension hosting, RLM state, state migration helpers, and deterministic offline testing.

It does not require Node.js. Python 3 is optional and is started only when the explicitly authorized `ipython` tool is enabled with `--allow-process` and a non-empty program allowlist.

## Build

```bash
rustup toolchain install stable --profile minimal --component rustfmt,clippy
cargo build --release
```

The binary is `target/release/mimir`.

## Releases and installation

Pushing a version tag such as `v0.1.0` starts the release workflow. It verifies the tag against the version in `Cargo.toml`, builds native binaries for Linux x86_64, macOS Intel, macOS Apple Silicon, and Windows x86_64, and publishes archives with SHA-256 checksums on the GitHub Releases page.

Download the latest release from [github.com/shivendrasoni/mimir/releases/latest](https://github.com/shivendrasoni/mimir/releases/latest). For example, on Linux x86_64:

```bash
version=v0.1.0
curl -fL "https://github.com/shivendrasoni/mimir/releases/latest/download/mimir-${version}-x86_64-unknown-linux-gnu.tar.gz" -o /tmp/mimir.tar.gz
tar -xzf /tmp/mimir.tar.gz -C /tmp
mkdir -p "$HOME/.local/bin"
install -m 755 /tmp/mimir "$HOME/.local/bin/mimir"
```

On macOS, use `aarch64-apple-darwin` for Apple Silicon or `x86_64-apple-darwin` for Intel. On Windows, download the `x86_64-pc-windows-msvc.zip` archive, extract `mimir.exe`, and add its directory to `PATH`.

To publish a release, update the package version, commit it, and push the matching tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Quick start

```bash
# Fully offline
mimir --provider fake --fake-response "Hello from Rust" --print "hello"

# OpenAI-compatible provider; .env is loaded but never displayed
OPENAI_API_KEY=... mimir --model gpt-5-mini --print "inspect this repository"

# Interactive full-screen TUI; /help lists commands and /quit exits
mimir

# Versioned JSON events
mimir --output json --print "summarize the project"

# JSON-RPC 2.0, one request per line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"health"}' | mimir --provider fake --fake-response ok --output rpc
```

Process execution is off by default. Enabling it still requires an exact program allowlist:

```bash
mimir --allow-process --allowed-programs cargo,rg --print "run the tests"
```

## Auth and provider login

```bash
mimir providers
printf '%s\n' "$OPENAI_API_KEY" | mimir login openai --api-key-stdin
mimir auth status
mimir logout openai
mimir login openai-codex
printf '%s\n' "$ANTHROPIC_API_KEY" | mimir login anthropic --api-key-stdin
```

Inside the TUI, use `/login`; credentials are masked while entered. ChatGPT Codex OAuth uses the native Codex Responses transport. Anthropic is native and distinguishes API-key authentication (`x-api-key`) from OAuth bearer authentication; the two credential types are not interchangeable. Bedrock, Vertex, Google, Mistral, OpenAI-compatible, safe custom providers, and extension-provided transports are selected through the same typed runtime factory.

Providers that are discovery-only or unsupported by the selected runtime fail closed before execution.

## State and management

State defaults to `.mimir/` and uses versioned JSON/JSONL formats.

```bash
mimir doctor
mimir session list
mimir --session work session show
mimir --session work session export
mimir daemon start
mimir daemon status
mimir daemon prompt "continue"
mimir daemon stop
mimir migrate plan --legacy-root <legacy-state-directory>
mimir migrate apply --legacy-root <legacy-state-directory>
# Use the journal_path printed by apply:
mimir migrate rollback --journal <journal-path>
mimir goal set "finish the next milestone" --token-budget 80000
mimir goal show
mimir schedule add heartbeat "continue the goal" --every-seconds 300
mimir schedule list
mimir extension list
mimir rlm list sample-extension workspace
mimir --provider fake --fake-response ok benchmark prompt "smoke test"
```

## Quality gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --doc
cargo build --release
cargo audit --deny warnings --no-fetch
```

See [ARCHITECTURE.md](ARCHITECTURE.md), [STATE-MIGRATION.md](STATE-MIGRATION.md), [SECURITY.md](SECURITY.md), and [BENCHMARKS.md](BENCHMARKS.md).
