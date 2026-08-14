# State migration

## Scope

Mimir includes a guarded state-import utility for bringing compatible data into its versioned `.mimir/` state directory. Imports are explicit, planned, hashed before application, journaled during writes, and reversible through the recorded rollback journal.

## Imported state

| State area | Mimir destination |
|---|---|
| Provider and model settings | Typed configuration and provider catalog |
| Sessions and transcripts | Versioned JSONL session storage |
| Context and skills | Deterministically loaded workspace resources |
| Extension packages and resources | Sandboxed extension and resource directories |
| Credentials | Owner-readable credential storage |

The import process never modifies source files in place. Unsupported formats, symlinked entries, changed source hashes, insecure endpoints, and oversized inputs fail closed before they can affect runtime state.

## Compatibility boundaries

- Imported session records are stored in the current schema and state directory.
- OpenAI-compatible providers use catalog-selected Chat Completions or Responses semantics; ChatGPT subscription login uses the native Codex Responses stream.
- Anthropic Messages is native for API keys and OAuth bearer credentials. Bedrock, Vertex, Google, Mistral, Cloudflare routes, safe custom OpenAI-compatible providers, and extension providers are wired and contract-tested.
- JSON mode is a versioned event stream. RPC supports JSONL and JSON-RPC 2.0; ACP and the versioned public daemon protocol are also native.
- The interactive default is the Rust TUI; `--no-tui` keeps the line-oriented fallback.
- Process and `!`/`!!` inputs are intentionally stricter than the reference: explicit opt-in, a non-empty exact executable allowlist, no compound shell syntax, bounded output, timeout, and process-group cancellation.

## Rollback

An applied state import returns a journal path. Pass that exact path to `mimir migrate rollback --journal <journal-path>` to restore replaced files and remove newly created files. Rollback is idempotent and refuses ambiguous or modified journal inputs.
