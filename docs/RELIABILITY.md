# Mimir reliability contract

Reliability means a run either completes correctly or stops in a classified,
recoverable state with enough bounded evidence to explain what happened. A long
timeout, a syntactically valid transcript, or a successful provider response is
not sufficient by itself.

## Invariants

Every release must preserve these invariants:

1. Every started run reaches exactly one terminal outcome: `completed`,
   `cancelled`, `budget_paused`, `failed`, or `crashed`.
2. Every assistant tool call has exactly one tool result before the next provider
   request. Recovery may synthesize a clearly labelled non-executed result.
3. Process outcomes distinguish spawn failure, non-zero exit, execution timeout,
   pipe-drain timeout, cancellation, and output-limit termination.
4. A fast child that exits and closes its pipes is never reported as timed out.
5. Active context is measured independently from cumulative provider usage.
   Context compaction happens before the projected request exceeds its configured
   threshold.
6. Tool output placed in model context is bounded. Full retained output is an
   external artifact referenced by identifier and digest.
7. Workspace file tools accept only non-empty workspace-relative paths without
   parent traversal. Process argument checks are advisory; real process isolation
   requires an operating-system sandbox.
8. Diagnostic persistence never prevents a run from completing. A diagnostic
   write failure becomes a bounded warning and terminal state is still attempted.
9. Diagnostic exports do not contain credentials, authorization headers, raw
   environment values, home-directory paths, or unredacted workspace paths by
   default.
10. Raw evidence is immutable. Later harnesses append analysis with evidence
    references; they do not rewrite the observed event stream.

## Failure taxonomy

| Domain | Stable class | Examples | Retry policy |
| --- | --- | --- | --- |
| Provider | `provider_authentication` | expired Claude/OpenAI login | Never automatic |
| Provider | `provider_rate_limited` | HTTP 429 | Bounded backoff |
| Provider | `provider_unavailable` | timeout, connection reset, 5xx | Bounded backoff |
| Provider | `provider_protocol` | malformed SSE, incomplete tool JSON | No blind retry; preserve evidence |
| Budget | `budget_paused` | cumulative token ceiling reached | Resume after explicit budget change |
| Context | `context_pressure` | projected request crosses threshold | Compact before provider call |
| Process | `process_spawn_failed` | executable unavailable | No retry without changed input |
| Process | `process_exit_nonzero` | command exits with failure | Agent may adjust command |
| Process | `process_execution_timeout` | child still running at deadline | Kill group; narrower retry only |
| Process | `process_pipe_drain_timeout` | child exited but inherited pipe remains open | Kill descendants; preserve partial output |
| Process | `process_output_limit` | bounded output exceeded | Stop process; inspect artifact |
| Process | `process_cancelled` | user/runtime cancellation | Never automatic |
| Workspace | `workspace_path_invalid` | absolute path, `..`, empty path | Correct to a relative path |
| Workspace | `workspace_target_outside` | sibling repository/file | Broaden workspace or approved copy/access |
| Session | `session_tail_recovered` | torn final JSONL record | Drop only incomplete final record |
| Session | `session_corrupt` | malformed interior record | Stop and preserve original |
| Session | `session_orphan_tool_call` | tool call without result | Repair explicitly before next request |
| Diagnostics | `diagnostic_write_failed` | full disk, denied path | Warn; never fail the agent run |
| Integrity | `source_provenance_missing` | source read failed before derived write | Warn or pause; do not claim derivation |

Unknown failures remain `unclassified`; they must not be silently mapped to a
timeout or generic protocol error.

## Fault injection

Reusable integration-test fixtures live in `tests/support/reliability.rs`. They
provide:

- a loopback-only one-response SSE server;
- a bounded process repetition runner;
- JSONL torn-tail creation and orphan tool-call detection;
- recursive literal leak scanning that reports marker labels, never secret values.

Fault tests must be deterministic, local, and capability-safe. They may use a
temporary directory, a loopback socket, and an explicitly allowlisted inert
program such as `true`. They must not use real provider credentials, external
network access, shells, destructive commands, or user files.

Run the fast reliability layer with:

```bash
cargo test --test reliability_contracts
cargo test --test provider_stream_contracts
```

Run the 10,000-iteration process soak explicitly with:

```bash
cargo test --test reliability_contracts process_exit_and_pipe_close_soak_10000 -- --ignored --exact
```

The soak is ignored in the normal suite so local and CI feedback remains fast.

## Canary and rollback controls

Each subsystem must retain an independent control during rollout. These names
are the release contract; a subsystem must not be declared generally available
until its control is wired and documented by its implementation.

| Control | Canary-on behavior | Rollback behavior |
| --- | --- | --- |
| `diagnostics.journal` | Write versioned run bundles and terminal summaries | Disable new writes; retain readable bundles |
| `process.capture_v2` | Use coordinated child-exit and pipe-drain capture | Restore legacy capture without changing policy |
| `context.token_compaction` | Compact from projected token pressure | Restore message-count compaction |
| `context.artifact_offload` | Replace large tool output with bounded references | Keep bounded inline output only |
| `workspace.path_guidance` | Publish `$WORKSPACE` contract and actionable errors | Restore legacy descriptions, never weaken canonical file checks |
| `integrity.tool_pairs` | Validate/repair orphan tool calls before provider use | Stop with a classified session error |
| `integrity.provenance` | Warn or pause on unsupported source-derived claims | Emit warning-only observations |

Rollback must not delete diagnostic bundles, rewrite session history, weaken
credential redaction, or allow a provider request with an invalid tool-call tail.

## Release gates

A release candidate is blocked unless all applicable gates pass:

- 100% of started fixture runs have one terminal diagnostic outcome.
- No provider request contains an orphan assistant tool call.
- The 10,000-iteration fast-process soak has zero false execution timeouts and
  zero lost exit statuses on every supported release platform.
- Malformed and truncated provider streams fail with `provider_protocol`, retain
  bounded diagnostic evidence, and never execute a partial tool call.
- Torn session tails recover only the last incomplete record; interior corruption
  remains fatal and non-destructive.
- Token pressure is detected before the configured context threshold, including
  sessions with few messages and large tool results.
- Secret/path leak scanning passes for default diagnostic bundles and redacted
  exports using API keys, OAuth tokens, authorization headers, `$HOME`, and the
  canonical workspace path as injected markers.
- Fault injection proves diagnostic disk failure cannot change the run outcome.
- Old sessions remain readable and rollback controls preserve their prior behavior.
- `cargo fmt --all -- --check`, focused reliability tests, and Clippy pass, with
  any unrelated baseline exception listed explicitly in the release evidence.

Canaries compare equivalent tasks on completion rate, validation outcome,
provider attempts, token use, compaction count, tool retries, latency, and failure
classification. Enable one control at a time; do not use a single opaque
"reliability mode" switch.
