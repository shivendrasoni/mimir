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
rustup toolchain install 1.97.1 --profile minimal --component rustfmt,clippy
cargo build --release
```

The binary is `target/release/mimir`.

## Releases and installation

CI is paused for pushes and pull requests. When manually dispatched, it verifies Linux and macOS in isolated native jobs. A failed platform remains visible on its matrix row without cancelling the healthy platform or failing the aggregate CI workflow. Releases also run only when manually dispatched. The release workflow creates a tagged commit with the next minor version in `Cargo.toml` and `Cargo.lock`, then each native target independently passes Clippy, tests, and a release build before uploading its own archive and SHA-256 checksum to the GitHub Releases page. With the manifest at `0.6.0`, the next manual release starts at `v0.6.0`. A platform failure withholds only that platform's artifacts. The generated version commit is kept on the release tag instead of being pushed back to `main`. Windows verification and artifacts are temporarily disabled.

Download the latest release from [github.com/shivendrasoni/mimir/releases/latest](https://github.com/shivendrasoni/mimir/releases/latest). For example, on Linux x86_64:

```bash
version=v0.6.0
curl -fL "https://github.com/shivendrasoni/mimir/releases/latest/download/mimir-${version}-x86_64-unknown-linux-gnu.tar.gz" -o /tmp/mimir.tar.gz
tar -xzf /tmp/mimir.tar.gz -C /tmp
mkdir -p "$HOME/.local/bin"
install -m 755 /tmp/mimir "$HOME/.local/bin/mimir"
```

On macOS, use `aarch64-apple-darwin` for Apple Silicon or `x86_64-apple-darwin` for Intel. Windows binaries are not currently published.

The publishable crates.io package is named `mimir-ai`; the library and installed executable remain `mimir`. The package has not been published yet. After its first publication, Rust users will be able to install it with:

```bash
cargo install mimir-ai
```

Releases are started manually from the GitHub Actions **Release** workflow. To start a new major release line, first set the package version to the next `<major>.0.0`; the workflow preserves that manual major version instead of incrementing it:

```bash
cargo metadata --no-deps --format-version 1
```

## Quick start

```bash
# Fully offline
mimir --provider fake --fake-response "Hello from Rust" --print "hello"

# OpenAI-compatible provider; .env is loaded but never displayed
OPENAI_API_KEY=... mimir --model gpt-5-mini --print "inspect this repository"

# Interactive full-screen TUI; /help lists commands and /quit exits
mimir

# Auto mode lets the agent run Bash commands and workspace edits without prompts
mimir --agent-mode auto

# Allow a longer agentic tool loop for one prompt (default: 64 provider turns)
mimir --max-turns 128
mimir --max-run-tokens 2000000

# Versioned JSON events
mimir --output json --print "summarize the project"

# JSON-RPC 2.0, one request per line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"health"}' | mimir --provider fake --fake-response ok --output rpc
```

The model has a workspace-rooted `bash` tool. In `default` mode every model-issued shell command
requires confirmation, while `/mode auto` runs shell commands and workspace edits immediately.
Use `/mode default` to return to confirmation. Long-running servers should be backgrounded with
stdout and stderr redirected to a workspace log. Direct TUI commands use `!command` (or
`!!command` to exclude the result from model context); auto mode runs those without an allowlist.

The narrower argument-vector `run_process` tool remains opt-in and requires an exact program
allowlist:

```bash
mimir --allow-process --allowed-programs cargo,rg --print "run the tests"
```

## Auth and provider login

```bash
mimir providers
# Anthropic OAuth is the default login and Claude Sonnet 5 is the default model
mimir login
printf '%s\n' "$OPENAI_API_KEY" | mimir login openai --api-key-stdin
mimir auth status
mimir logout openai
mimir login openai-codex
printf '%s\n' "$ANTHROPIC_API_KEY" | mimir login anthropic --api-key-stdin
```

Inside the TUI, use `/login`; credentials are masked while entered. ChatGPT Codex OAuth uses the native Codex Responses transport. Anthropic is native and distinguishes API-key authentication (`x-api-key`) from OAuth bearer authentication; the two credential types are not interchangeable. Bedrock, Vertex, Google, Mistral, OpenAI-compatible, safe custom providers, and extension-provided transports are selected through the same typed runtime factory.

Providers that are discovery-only or unsupported by the selected runtime fail closed before execution.

## State and management

State defaults to the global `$HOME/.mimir/` directory and uses versioned JSON/JSONL formats.
Set `MIMIR_STATE_DIR` or pass `--state-dir` to use an isolated state directory.
The per-prompt provider-turn budget defaults to 64 and can be changed with
`--max-turns` or `MIMIR_MAX_TURNS`. Reaching it is a recoverable budget pause:
the session stays intact and a new message starts a fresh per-prompt budget.
The cumulative run-token budget defaults to 1,000,000 fresh-input plus output
tokens and can be changed with `--max-run-tokens` or
`MIMIR_MAX_RUN_TOKENS`. Provider-cached input remains visible in diagnostics
and context measurements, but replaying it does not consume the run budget a
second time.

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

## Local diagnostics

Every agent process writes a versioned, privacy-safe bundle under
`.mimir/diagnostics/` (or `<state-dir>/diagnostics/` when `--state-dir` is set):

```text
diagnostics/
├── index.jsonl
└── runs/<run-id>/
    ├── manifest.json
    ├── events.jsonl
    ├── summary.json
    ├── analysis.jsonl
    └── artifacts/
```

The recorder runs best-effort in a background thread and cannot fail an agent
run. It records correlation IDs, event types, timing, raw input, cached input,
fresh input, output, operational-budget usage, peak context, byte counts,
hashes, status, and error classes. Prompt text, model output, tool arguments,
tool output, credentials, environment values, and absolute host paths are not
stored. If a process exits before its terminal write, readers synthesize an
`incomplete` outcome instead of presenting the run as successful.
Diagnostic directories are mode `0700` and files are mode `0600` on Unix.
Each run is bounded to 100,000 events or 128 MiB of event data, and startup
retention keeps at most 100 runs and 512 MiB of bundle data.

```bash
mimir diagnose list
mimir diagnose show <run-id>
mimir diagnose query <run-id> --kind tool_finished --status error --json
mimir diagnose export <run-id> --redacted --output diagnostic.json
mimir diagnose annotate <run-id> --file assessment.json
mimir diagnose replay <run-id>
```

`annotate` appends a typed external assessment without modifying raw evidence.
The assessment file uses this portable shape (event IDs must belong to the run):

```json
{
  "author": "another-harness",
  "finding": "The process exit event is missing",
  "confidence": 0.9,
  "evidence_event_ids": [],
  "proposed_fix": "Inspect pipe-drain completion",
  "verification": "Replay the bounded fixture"
}
```

`replay` is deliberately verification-only: it validates schema, correlation,
sequence, and terminal evidence and never calls a provider or executes a tool.
The TUI `/traces preview` remains available and now links its session metadata
to matching diagnostic run IDs.

Direct text, JSON, JSON-RPC, ACP, autonomous, REPL, and TUI processes attach the
diagnostic collector at CLI dispatch. Each daemon-managed prompt attaches its
own collector for the complete prompt lifecycle, including queued follow-ups
and autonomous continuations, so long-lived daemon sessions produce one bounded
bundle per admitted prompt.

## RLM and continual harness

When running an interactive, session-backed agent, Mimir registers its RLM tools automatically. The agent can choose from its currently authenticated models and recursively delegate bounded work; each child is tracked independently from admission through completion, cancellation, or deletion. The default maximum recursion depth is 3, and all child work remains subject to the runtime's budgets and tool policy.

Use `/rlm-max-depth` in the TUI to inspect or set the recursion limit for a session. Use `/refine <instructions>` when you want Mimir to review the current trajectory and persist a focused lesson. Refinements are local to the session by default; pass `--global` only when a lesson should be shared, and undo a recorded update with:

```text
/refine rollback <refinement-id>
# or, for a global refinement
/refine rollback <refinement-id> --global
```

This is harness refinement, not model-weight training: Mimir proposes and validates small durable operating-context changes, then records the before/after state needed to inspect or reverse them.

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

See [ARCHITECTURE.md](ARCHITECTURE.md), [STATE-MIGRATION.md](STATE-MIGRATION.md), [SECURITY.md](SECURITY.md), and [BENCHMARKS.md](BENCHMARKS.md).
