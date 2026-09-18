# Local diagnostics

Every agent process writes a versioned, privacy-safe bundle under `.mimir/diagnostics/`, or `<state-dir>/diagnostics/` when `--state-dir` is set:

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

## Recording and retention

The recorder runs best-effort in a background thread and cannot fail an agent run. It records correlation IDs, event types, timing, raw input, cached input, fresh input, output, operational-budget usage, peak context, byte counts, hashes, status, and error classes.

Prompt text, model output, tool arguments, tool output, credentials, environment values, and absolute host paths are not stored. If a process exits before its terminal write, readers synthesize an `incomplete` outcome instead of presenting the run as successful.

Diagnostic directories are mode `0700` and files are mode `0600` on Unix. Each run is bounded to 100,000 events or 128 MiB of event data, and startup retention keeps at most 100 runs and 512 MiB of bundle data.

## Inspecting a run

```bash
mimir diagnose list
mimir diagnose show <run-id>
mimir diagnose query <run-id> --kind tool_finished --status error --json
mimir diagnose export <run-id> --redacted --output diagnostic.json
mimir diagnose annotate <run-id> --file assessment.json
mimir diagnose replay <run-id>
```

`annotate` appends a typed external assessment without modifying raw evidence. The assessment file uses this portable shape; referenced event IDs must belong to the run:

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

`replay` is verification-only: it validates schema, correlation, sequence, and terminal evidence and never calls a provider or executes a tool. The TUI `/traces preview` remains available and links its session metadata to matching diagnostic run IDs.

## Process coverage

Direct text, JSON, JSON-RPC, ACP, autonomous, REPL, and TUI processes attach the diagnostic collector at CLI dispatch. Each daemon-managed prompt attaches its own collector for the complete prompt lifecycle, including queued follow-ups and autonomous continuations, so long-lived daemon sessions produce one bounded bundle per admitted prompt.

## TypeSafe skill-selection evidence

When TypeSafe is `on`, an evaluated turn adds three runtime-event kinds:

- `typesafe_skill_selection` records the decision, selected skill, bounded probabilities, threshold result, request byte count and hash, model, token usage, estimated cost, latency, added context, and a coarse failure category;
- `typesafe_skill_outcome` records whether the enclosing Mimir turn completed; and
- `typesafe_skill_correction` records when the main model later activates a different skill.

These events never contain the TypeSafe API key or request text. They are intended for aggregate canary comparison of completion, wrong or needless loads, corrections, net token savings, cost, and latency. They do not replace the session transcript and must not be interpreted as proof of task quality in isolation.
