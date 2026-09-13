# Using Mimir

Mimir supports interactive terminal use, one-shot prompts, autonomous execution, structured event output, and JSON-RPC operation.

## Common commands

```bash
# Fully offline
mimir --provider fake --fake-response "Hello from Rust" --print "hello"

# OpenAI-compatible provider; .env is loaded but never displayed
OPENAI_API_KEY=... mimir --model gpt-5-mini --print "inspect this repository"

# Interactive full-screen TUI; /help lists commands and /quit exits
mimir

# Auto mode lets the agent run Bash commands and workspace edits without prompts
mimir --agent-mode auto

# Plan mode inspects and clarifies, then writes one reviewable plan artifact
mimir --agent-mode plan

# Apply or remove explicit operational limits
mimir --max-turns 128
mimir --max-run-tokens 2000000
mimir --max-turns unlimited --max-run-tokens unlimited

# Versioned JSON events
mimir --output json --print "summarize the project"

# JSON-RPC 2.0, one request per line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"health"}' | mimir --provider fake --fake-response ok --output rpc
```

## Agent modes

The model has a workspace-rooted `bash` tool. In `default` mode every model-issued shell command requires confirmation, while `/mode auto` runs shell commands and workspace edits immediately. Use `/mode default` to return to confirmation.

`/mode plan` limits the model to `read_file`, `list_files`, `search`, `ask_user`, and `write_plan`. It can inspect the repository, pause for a structured clarification, and create or revise one session-bound Markdown file under `plans/`; direct shell commands, extensions, MCP, IPython, child agents, and autonomous continuations are disabled. Plan mode supports native model providers only.

When the plan is ready, enter standalone `implement` or `/implement [additional instructions]`. Mimir validates the artifact, switches to auto mode, and starts implementation from the accepted plan. The plan is never committed or pushed automatically.

## Workspace references

Inside the TUI, type `@` anywhere in the composer to pick a file or folder from the active workspace. Continue typing to filter, use Up/Down or Tab to select, and press Enter to insert the workspace-relative reference.

Referenced paths are validated against the workspace and are made explicit to the model. Folders are inspected selectively instead of being injected wholesale.

## Process execution

Long-running servers should be backgrounded with stdout and stderr redirected to a workspace log. Direct TUI commands use `!command` or `!!command` to exclude the result from model context. Auto mode runs those commands without an allowlist.

The narrower argument-vector `run_process` tool remains opt-in and requires an exact program allowlist:

```bash
mimir --allow-process --allowed-programs cargo,rg --print "run the tests"
```

Python 3 is optional and starts only when the explicitly authorized `ipython` tool is enabled with `--allow-process` and a non-empty program allowlist.

## Runtime budgets

Official `anthropic` and `openai-codex` providers default to unrestricted top-level turns, tool calls, operational tokens, elapsed time, and autonomous continuations. Other providers default to a 1,000,000-token ceiling per logical task, counting fresh input plus output. Provider-cached input remains visible in diagnostics and context measurements but is not charged to the operational budget again.

Use `--max-turns`, `--max-run-tokens`, or their `MIMIR_MAX_TURNS` and `MIMIR_MAX_RUN_TOKENS` environment equivalents to override that policy with a positive number or `unlimited`. Autonomous-specific limit flags accept the same `unlimited` value and supersede ordinary run limits for the full initial-prompt-plus-continuations task.

Context windows are never unlimited. Automatic compaction remains enabled by default and runs before the projected provider request reaches the model's configured context threshold. Per-request provider timeouts, cancellation, tool/output bounds, recursive-child budgets, and provider quota errors also remain enforced.

Autonomous mode exposes `finish_task` only while it is active. The agent calls it after completing and validating the work; configured quality gates must pass before completion is accepted. `/autonomous cancel` or normal cancellation remains available at any time.
