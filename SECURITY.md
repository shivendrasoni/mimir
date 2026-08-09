# Security model

## Trust boundaries

Prompts, model responses, tool arguments, workspace contents, session files, RPC input, and provider error bodies are untrusted. Credentials and the host filesystem outside the selected roots are sensitive.

## Controls

- API keys are held in `SecretString`, omitted from request previews, redacted from `Debug`, and never included in diagnostics.
- `.env` loading is optional convenience; the file is never parsed into logs or persisted state.
- Provider adapters use TLS, request timeouts, an 8 MiB stream/response ceiling, sanitized errors, and redacted debug output.
- Anthropic API-key and OAuth bearer authentication are separate typed paths. Migrated custom providers accept only HTTPS or loopback HTTP endpoints and environment-variable credential references.
- File tools canonicalize a workspace root, reject traversal and escaping symlinks, cap reads/writes/searches, and atomically replace writes.
- Process execution is disabled by default. When enabled, a missing allowlist denies all execution. Matching is exact; `/tmp/cargo` does not match `cargo`. Commands receive direct argv, cleared environment, a fixed PATH, null stdin, bounded combined output, timeout, and process-group termination.
- State writes use atomic replacement, path-scoped in-process locks, and reject symlinked state components. Credential, daemon-metadata, and session files are mode `0600` on Unix.
- Daemon IPC rejects frames larger than 1 MiB, refuses to unlink non-socket paths, bounds inactive history, and cancels in-flight connections during shutdown.
- Migration plans hash every source artifact, revalidate hashes before apply, journal before destination replacement, and support deterministic rollback.
- RPC errors contain no stack traces. JSON parsing and tool schemas reject malformed or unknown fields.
- Budgets cap turns, tools, tokens, elapsed time, context size, child concurrency, provider retries, and captured bytes.
- The embedded QuickJS extension runtime has bounded source/module graphs, deadlines, response sizes, capability checks, correlated UI continuations, confined resource paths, and credential-redacted provider/OAuth errors.
- Google service-account JWT signing uses `ring`; the vulnerable direct `rsa` dependency was removed after the final RustSec gate identified RUSTSEC-2023-0071.

## Operational guidance

- Keep `--allow-process` off unless the task requires it; use the smallest exact `--allowed-programs` list.
- Use a dedicated writable state directory with restrictive OS permissions.
- Run dependency auditing in CI and review `Cargo.lock` changes.
- Treat provider responses as instructions, never authority to expand policy.

## Reporting

Do not include credentials, `.env` contents, private prompts, or session transcripts in reports. Provide a minimal reproduction and affected version for security issues.
