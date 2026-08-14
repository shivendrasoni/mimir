# Architecture

## Runtime flow

1. `cli` validates workspace and state roots, resolves typed credentials, and selects a native, custom, or extension-provided model transport.
2. `resources` loads context, package, user, project, explicit skill, prompt-template, and theme resources with deterministic precedence.
3. `runtime` restores the versioned session, persists the user message, and starts a bounded provider/tool loop.
4. `provider` translates typed messages and tool schemas for OpenAI/Codex, Anthropic, Bedrock, Vertex/Google, Mistral, Cloudflare-routed, and compatible custom transports; catalog limits and compatibility flags shape each request.
5. `tools` validates JSON inputs and enforces canonical workspace, byte, timeout, process, and exact-program policies.
6. Every assistant message and tool result is durably appended before the next provider turn. Context is deterministically compacted at the configured limit.

## Modules

| Module | Responsibility |
|---|---|
| `model` | Roles, content blocks, tool calls/results, usage, stop reasons |
| `budget` | Turn, tool, token, elapsed, and context bounds |
| `provider` | Async interface plus fake, OpenAI/Codex, Anthropic, Bedrock, Google/Vertex, Mistral, Cloudflare, and compatible custom adapters |
| `auth` | Stored API keys, OAuth/device credentials, provider login status |
| `tools` | Registry plus read/write/edit/list/search/process tools |
| `session` | Schema-versioned JSONL, bounded streaming replay, recovery, and atomic compaction checkpoints |
| `resources` | Context and skill discovery with deterministic precedence |
| `runtime` | Serialized model/tool state machine, cancellation, events, compaction |
| `orchestration` | Durable goals/schedules, message bus, bounded child agents |
| `tui` | Full-screen terminal UI, selectors/settings/themes, auth, sessions, streamed rendering, and bounded `!`/`!!` execution |
| `daemon` | Versioned Unix-socket IPC and public 97-command protocol with leases, recovery, scheduling, controls, and live events |
| `extensions` | Manifest/package discovery, embedded QuickJS TypeScript ABI, native subprocess ABI, lifecycle/provider/OAuth/stream bridges, and RLM persistence |
| `migration` | Legacy state planning, apply, journal, and rollback |
| `cli` | Human output, versioned JSON events, JSON-RPC 2.0, management commands |

## Invariants

- The crate forbids unsafe Rust.
- Provider configuration cannot be constructed with blank URL, model, or key; debug output redacts the key.
- Tools are registered explicitly. File operations cannot escape the canonical workspace.
- Process execution fails closed, requires explicit authority plus an exact non-empty allowlist, rejects compound shell syntax, clears the environment, and uses process groups, timeouts, and bounded output.
- Internal state rejects symlinked path components and shares path-scoped mutation locks across store instances.
- Auth and imported credential files are owner-readable only on Unix.
- Daemon metadata persists timestamps and lease state but not raw prompt text.
- Daemon IPC frames are capped at 1 MiB, and shutdown cancels in-flight connection tasks before reporting completion.
- A corrupt incomplete final JSONL record is recoverable; interior corruption is not silently ignored.
- Provider and tool activity is bounded by budgets, response sizes, timeouts, and concurrency admission.

## Persistence order

The runtime appends a state transition before publishing the next dependent transition. A tool call therefore produces: assistant message → tool started event → bounded observation → persisted tool-result message → tool finished event → next provider request. Compaction is persisted and checkpointed before the in-memory transcript changes.

## Extension points

Implement `Provider` for another model backend, `SessionStore` for another durable store, or `EventSink` for another transport. Capability-scoped JavaScript/TypeScript extensions can register bounded provider, OAuth, streaming, command, tool, renderer, resource, and lifecycle surfaces. Tools remain internal to `ToolRegistry` so policy validation cannot be bypassed by callers.
