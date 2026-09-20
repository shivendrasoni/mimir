# Security model

## Trust boundaries

Prompts, model responses, tool arguments, workspace contents, session files, RPC input, and provider error bodies are untrusted. Credentials and the host filesystem outside the selected roots are sensitive.

## Controls

- API keys are held in `SecretString`, omitted from request previews, redacted from `Debug`, and never included in diagnostics.
- `.env` loading is optional convenience; the file is never parsed into logs or persisted state.
- TypeSafe is off by default. When explicitly enabled, Mimir sends the current user request and bounded discovered-skill names and descriptions to the TypeSafe API over its Rust client. The API key and request text are not copied into TypeSafe runtime events; those events retain only bounded decision metadata, counts, hashes, timing, and usage.
- A TypeSafe result has no authority over tools, permissions, providers, or execution. Rust-owned applicability and confidence thresholds gate at most one skill activation, an explicit skill wins, and all TypeSafe failures fall back to the normal skill-search path.
- Provider adapters use TLS, request timeouts, an 8 MiB stream/response ceiling, sanitized errors, and redacted debug output.
- Anthropic API-key and OAuth bearer authentication are separate typed paths. Custom providers accept only HTTPS or loopback HTTP endpoints and environment-variable credential references.
- File tools canonicalize a workspace root, reject traversal and escaping symlinks, cap reads/writes/searches, and atomically replace writes.
- Process execution is disabled by default. When enabled, a missing allowlist denies all execution. Matching is exact; `/tmp/cargo` does not match `cargo`. Commands receive direct argv, cleared environment, a fixed PATH, null stdin, bounded combined output, timeout, and process-group termination.
- Bash execution may invoke an optional `rtk` executable from the inherited `PATH` to rewrite an already-authorized command before execution. Treat that binary and `PATH` as trusted executable dependencies. Missing, rejected, malformed, or slow RTK rewrites fall back to the original command; Mimir's approval and allowlist decisions remain based on that original command.
- State writes use atomic replacement, path-scoped in-process locks, and reject symlinked state components. Credential, daemon-metadata, and session files are mode `0600` on Unix.
- Diagnostic bundles are metadata-only, redact absolute paths and credential-like literals, use mode `0700` directories and `0600` files on Unix, and enforce event-count, file-size, total-size, and retained-run bounds.
- Daemon IPC rejects frames larger than 1 MiB, refuses to unlink non-socket paths, bounds inactive history, and cancels in-flight connections during shutdown.
- Migration plans hash every source artifact, revalidate hashes before apply, journal before destination replacement, and support deterministic rollback.
- RPC errors contain no stack traces. JSON parsing and tool schemas reject malformed or unknown fields.
- Budgets cap turns, tools, tokens, elapsed time, context size, child concurrency, provider retries, and captured bytes.
- The embedded QuickJS extension runtime has bounded source/module graphs, deadlines, response sizes, capability checks, correlated UI continuations, confined resource paths, and credential-redacted provider/OAuth errors.
- Google service-account JWT signing uses `ring`; the vulnerable direct `rsa` dependency was removed after the final RustSec gate identified RUSTSEC-2023-0071.

## Operational guidance

- Keep `--allow-process` off unless the task requires it; use the smallest exact `--allowed-programs` list.
- Install RTK only from its official repository and keep untrusted directories out of `PATH` when Bash execution is enabled.
- Use a dedicated writable state directory with restrictive OS permissions.
- Run dependency auditing in CI and review `Cargo.lock` changes.
- Treat provider responses as instructions, never authority to expand policy.
- Enable TypeSafe only when sending the current request and skill summaries to that external service is acceptable; use `--typesafe off` as the immediate rollback.

## Reporting

Do not include credentials, `.env` contents, private prompts, or session transcripts in reports. Provide a minimal reproduction and affected version for security issues.
