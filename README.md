# Mimir — a durable RLM runtime in Rust

Mimir is a standalone agentic runtime written in safe Rust. Its core is a **Recursive Language Model (RLM) runtime**: an agent can programmatically start model-backed child agents, observe them, and cancel or remove them while their work remains durable and scoped to the parent session.

The RLM runtime is built for long-running, inspectable work:

- **Recursive execution:** `spawn_agent` starts a child agent immediately; the parent can list, cancel, or delete it with `rlm_list_subagents`, `rlm_cancel_subagent`, and `rlm_delete_subagent`.
- **Safe long-running execution:** top-level operational budgets are provider-aware, while recursion depth, child count and concurrency, prompt and state size, tool output, and authenticated model discovery remain bounded.
- **Durable session state:** child sessions and namespaced RLM extension state persist under Mimir's state root, so orchestration can survive an interrupted terminal session.
- **Continual Harness:** explicit “remember this” requests use the built-in `remember` tool, while `/refine` turns evidence into small, scoped updates to supplemental prompts, memories, existing-code skills, or reusable subagent specifications. `/learn` manages observe-only project learning, verified canaries, redacted contribution, and signed fleet packs. None of these paths rewrites the base system prompt or trains model weights.

Around that RLM foundation, Mimir provides provider adaptation, model/tool execution, workspace tools, context and skill loading, goals, schedules, TUI/CLI/JSON/JSON-RPC operation, auth and OAuth login, daemon IPC, extension hosting, state migration helpers, and deterministic offline testing.

User-level skills are discovered from the cross-harness `~/.agents/skills/` directory. Project-specific skills remain discoverable from `.agents/skills/` at each workspace level; an explicit `--skill <PATH>` still has highest precedence.

It does not require Node.js. Python 3 is optional and is started only when the explicitly authorized `ipython` tool is enabled with `--allow-process` and a non-empty program allowlist.

## Install

Download the latest release from [GitHub Releases](https://github.com/shivendrasoni/mimir/releases/latest). On Linux x86_64:

```bash
version=v0.10.0
curl -fL "https://github.com/shivendrasoni/mimir/releases/latest/download/mimir-${version}-x86_64-unknown-linux-gnu.tar.gz" -o /tmp/mimir.tar.gz
tar -xzf /tmp/mimir.tar.gz -C /tmp
mkdir -p "$HOME/.local/bin"
install -m 755 /tmp/mimir "$HOME/.local/bin/mimir"
```

On macOS, use `aarch64-apple-darwin` for Apple Silicon or `x86_64-apple-darwin` for Intel. Windows binaries are not currently published.

To build from source:

```bash
rustup toolchain install 1.97.1 --profile minimal --component rustfmt,clippy
cargo build --release
```

The binary is `target/release/mimir`. The publishable crates.io package is named `mimir-ai`, but it has not been published yet. See [Releasing](docs/RELEASING.md) for packaging and release-workflow details.

## Quick start

```bash
# Fully offline
mimir --provider fake --fake-response "Hello from Rust" --print "hello"

# OpenAI-compatible provider
OPENAI_API_KEY=... mimir --model gpt-5-mini --print "inspect this repository"

# Interactive full-screen TUI
mimir

# Auto mode permits Bash commands and workspace edits without prompts
mimir --agent-mode auto

# Plan mode produces a reviewable plan before implementation
mimir --agent-mode plan
```

Anthropic OAuth and Claude Sonnet 5 are the default login and model:

```bash
mimir login
mimir
```

Inside the TUI, `/help` lists commands, `/quit` exits, and typing `@` opens the workspace path picker. See [Using Mimir](docs/USAGE.md) for agent modes, tool permissions, file references, token limits, and machine-readable output.

## Documentation

- [Using Mimir](docs/USAGE.md) — TUI, agent modes, workspace tools, and output formats
- [Providers and authentication](docs/PROVIDERS.md) — login, OAuth, API keys, and provider behavior
- [RLM and continual harness](docs/RLM.md) — child agents, refinement, learning, and rollback
- [Operations](docs/OPERATIONS.md) — state, sessions, daemon, goals, schedules, and extensions
- [Diagnostics](docs/DIAGNOSTICS.md) — privacy-safe run evidence, querying, annotation, and replay
- [Reliability](docs/RELIABILITY.md) — invariants, failures, fault injection, and release gates
- [Architecture](ARCHITECTURE.md) — runtime flow, modules, persistence, and extension points
- [Security](SECURITY.md) — trust boundaries, controls, and operational guidance
- [State migration](STATE-MIGRATION.md) — guarded import and rollback
- [Performance evidence](BENCHMARKS.md) — benchmark methodology and interpretation
- [Releasing](docs/RELEASING.md) — CI, packaging, versions, and artifacts

## Agent harness benchmarks

- [Mimir vs Claude Code — 6 tasks, 3 repeats](benchmarks/2026-09-12-mimir-vs-claude-code.pdf)
- [Mimir vs Codex — matched GPT-5.6 Terra model, 6 tasks, 3 repeats](benchmarks/2026-09-12-mimir-vs-codex.pdf)

Each report records the run identifiers, benchmark commit, harness and model versions, outcome scores, timing, token usage, scope violations, and adapter failures used for the comparison.

## Quality gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --doc
cargo build --release
cargo audit --deny warnings --no-fetch
```
