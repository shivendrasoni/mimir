# Mimir — a durable RLM runtime in Rust

Mimir is a standalone agentic runtime written in safe Rust. Its core is a **Recursive Language Model (RLM) runtime**: an agent can programmatically start model-backed child agents, observe them, and cancel or remove them while their work remains durable and scoped to the parent session.

The RLM runtime is built for long-running, inspectable work:

- **Recursive execution:** `spawn_agent` starts a child agent immediately; the parent can list, cancel, or delete it with `rlm_list_subagents`, `rlm_cancel_subagent`, and `rlm_delete_subagent`.
- **Safe long-running execution:** top-level operational budgets are provider-aware, while recursion depth, child count and concurrency, prompt and state size, tool output, and authenticated model discovery remain bounded.
- **Project-isolated session state:** each ordinary launch starts fresh; explicit resume restores transcripts, daemon state, and namespaced RLM extension state only from the current Git project's hashed state namespace.
- **Continual Harness:** explicit “remember this” requests use the built-in `remember` tool. Completed local task spans are queued by reference, privately projected, and evaluated off the critical path by Jev; only bounded Memory refinements can be generated. Rust owns clustering, canaries, rollback, and permissions. None of these paths rewrites the base system prompt, executes generated playbooks, or trains model weights.

Around that RLM foundation, Mimir provides provider adaptation, model/tool execution, workspace tools, context and skill loading, goals, schedules, TUI/CLI/JSON/JSON-RPC operation, auth and OAuth login, daemon IPC, extension hosting, state migration helpers, and deterministic offline testing.

User-level skills are discovered from the cross-harness `~/.agents/skills/` directory. Project-specific skills remain discoverable from `.agents/skills/` at each workspace level; an explicit `--skill <PATH>` still has highest precedence.

Optional TypeSafe turn selection can load one relevant skill and shortlist the configured built-in, extension, and MCP tool pool before the main provider call. With `TYPESAFE_API_KEY`, TypeSafe also enables automatically unless explicitly disabled. `--typesafe off` remains the immediate rollback. Rust still owns permissions, execution policy, learning lifecycle, providers, and every fallback.

It does not require Node.js. Python 3 is optional and is started only when the explicitly authorized `ipython` tool is enabled with `--allow-process` and a non-empty program allowlist.

If the optional [RTK](https://github.com/rtk-ai/rtk) binary is on `PATH`, Mimir automatically asks `rtk rewrite` to route supported Bash commands through RTK's compact-output filters. Mimir still authorizes the original command and retains its existing timeout, cancellation, and output limits; an absent, unsupported, or failing RTK installation falls back to the original command. Use RTK's `RTK_DISABLED=1` command prefix for a one-command bypass and `rtk gain` to inspect estimated Bash-output savings. RTK is a companion executable, not a Rust library dependency; install the official project with `cargo install --git https://github.com/rtk-ai/rtk --branch master --locked`.

## Install

Binary releases are not currently published. Build and install Mimir from source:

```bash
rustup toolchain install 1.97.1 --profile minimal --component rustfmt,clippy
cargo install --path . --force
```

For an uninstalled build, run `cargo build --release`; the binary is `target/release/mimir`. Previously published crates.io versions of `mimir-ai` have been yanked; build from source until a newly licensed version is published. See [GitHub Releases](https://github.com/shivendrasoni/mimir/releases) for future binaries and [Releasing](docs/RELEASING.md) for packaging and release-workflow details.

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

# Optional TypeSafe skill and tool-pool selection (requires TYPESAFE_API_KEY)
mimir --typesafe on --print "create a formatted project spreadsheet"
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
- [Extensions and lifecycle hooks](docs/EXTENSIONS.md) — hook reference, interception rules, installation, and examples
- [Diagnostics](docs/DIAGNOSTICS.md) — privacy-safe run evidence, querying, annotation, and replay
- [TypeSafe roadmap](docs/TYPESAFE_ROADMAP.md) — scoped skill and tool-pool phases, evidence, activation, and rollback
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

## License

Mimir 0.20.0 and later is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). You may use, study, modify, and share it for permitted noncommercial purposes. Commercial use requires a separate license from the copyright holder.

Previously published `mimir-ai` versions 0.5.0, 0.11.0, 0.13.0, and 0.19.0 remain available under the MIT terms included with those versions. See [Licensing](LICENSING.md) for the transition details.

## Quality gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --doc
cargo build --release
cargo audit --deny warnings --no-fetch
```
