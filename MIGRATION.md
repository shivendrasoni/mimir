# Migration record

## Scope

The new `mimir/` folder is an independent Rust product. It migrates the reference harness's behavioral core while replacing the legacy TypeScript package split and Python RLM sidecar with one typed binary and library.

| Reference capability | Rust replacement |
|---|---|
| AI message/provider types | `model`, `provider` |
| Agent loop and event stream | `runtime`, `EventSink` |
| Coding-agent filesystem/search/bash tools | `tools` with stricter workspace/process policy |
| Session JSONL and resume | `session` |
| AGENTS.md and skills loading | `resources` |
| Goals, schedules, autonomous work | `orchestration::{goal,schedule}` |
| Subagent lifecycle and messages | `orchestration::{subagent,message_bus}` |
| Print/TUI/JSON/RPC CLI | `cli`, `tui` |
| Login, OAuth, provider status | `auth`, provider registry |
| Background daemon and resumable prompt IPC | `daemon` |
| Extension catalog, host ABI, RLM state | `extensions` |
| Legacy state import, journaled rollback | `migration` |
| TypeScript extension runtime | Sandboxed embedded QuickJS with bounded TypeScript transformation |
| Python RLM sidecar | Native Rust RLM; optional persistent Python is isolated behind the authorized `ipython` tool |

The package publishing layout is not copied line-for-line. Runtime-facing seams are represented by stable events, JSON-RPC/ACP, provider traits, daemon IPC, tool schemas, and a bounded embedded JavaScript/TypeScript extension ABI. Migrated extensions can register tools, commands, renderers, providers, resources, OAuth handlers, streaming providers, lifecycle interception, and session actions.

## Compatibility boundaries

- Session records use a new schema and state directory; legacy state is not modified in place.
- OpenAI-compatible providers use catalog-selected Chat Completions or Responses semantics; ChatGPT subscription login uses the native Codex Responses stream.
- Anthropic Messages is native for API keys and OAuth bearer credentials. Bedrock, Vertex, Google, Mistral, Cloudflare routes, safe migrated custom OpenAI-compatible providers, and extension providers are wired and contract-tested. GitHub Copilot is intentionally outside this migration.
- JSON mode is a versioned event stream. RPC supports the legacy JSONL surface and JSON-RPC 2.0; ACP and the versioned public daemon protocol are also native.
- The interactive default is the Rust TUI; `--no-tui` keeps the line-oriented fallback.
- Process and `!`/`!!` inputs are intentionally stricter than the reference: explicit opt-in, a non-empty exact executable allowlist, no compound shell syntax, bounded output, timeout, and process-group cancellation.

## Rollback

The legacy TypeScript tree remains untouched. An applied state import returns a journal path; pass that exact path to `mimir migrate rollback --journal <journal-path>` to restore replaced files and remove newly created files.

## Self-reference

Local planning artifacts contain the planning contract, tasks, execution record, test evidence, review results, and state snapshot used to build this codebase. Remote publication is disabled and was skipped at the user's request.
