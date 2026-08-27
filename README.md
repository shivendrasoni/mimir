# Mimir — a bounded RLM runtime in Rust

Mimir is a standalone, bounded agentic runtime written in safe Rust. Its core is a **Recursive Language Model (RLM) runtime**: an agent can programmatically start model-backed child agents, observe them, and cancel or remove them while their work remains durable and scoped to the parent session.

The RLM runtime is built for long-running, inspectable work:

- **Recursive execution:** `rlm_run` admits a child agent immediately; the parent can list, cancel, or delete it with `rlm_list_subagents`, `rlm_cancel_subagent`, and `rlm_delete_subagent`.
- **Bounded by design:** recursion depth, child count and concurrency, prompt and state size, duration, output tokens, and authenticated model discovery are all limited by the runtime.
- **Durable session state:** child sessions and namespaced RLM extension state persist under Mimir's state root, so orchestration can survive an interrupted terminal session.
- **Continual Harness:** `/refine` turns evidence from a session into small, structured updates to supplemental prompts, memories, skills, or reusable subagent specifications. It never rewrites the base system prompt, records refinement history, and supports rollback with `/refine rollback <refinement-id>`.

Around that RLM foundation, Mimir provides provider adaptation, model/tool execution, workspace tools, context and skill loading, goals, schedules, TUI/CLI/JSON/JSON-RPC operation, auth and OAuth login, daemon IPC, extension hosting, state migration helpers, and deterministic offline testing.

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

## RLM and continual harness

When running an interactive, session-backed agent, Mimir registers its RLM tools automatically. The agent can choose from its currently authenticated models and recursively delegate bounded work; each child is tracked independently from admission through completion, cancellation, or deletion. The default maximum recursion depth is 3, and all child work remains subject to the runtime's budgets and tool policy.

Use `/rlm-max-depth` in the TUI to inspect or set the recursion limit for a session. Use `/refine <instructions>` when you want Mimir to review the current trajectory and persist a focused lesson. Refinements are local to the session by default; pass `--global` only when a lesson should be shared, and undo a recorded update with:

```text
/refine rollback <refinement-id>
# or, for a global refinement
/refine rollback <refinement-id> --global
```

This is harness refinement, not model-weight training: Mimir proposes and validates small durable operating-context changes, then records the before/after state needed to inspect or reverse them.

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
