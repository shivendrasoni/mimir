# Performance evidence

Mimir is evaluated at two levels: comparative agent-harness tasks and local runtime measurements. The comparative results below are intentionally condensed; the linked source reports contain the run identifiers, per-task results, charts, methodology, and caveats.

## Agent harness benchmarks

Both September 2026 comparisons used six tasks with three repeats per task: 18 recorded runs for each harness, or 36 runs per comparison. All figures below come from the reports' recorded summaries.

### Mimir vs Claude Code

Mimir used `claude-sonnet-5` on Mimir `0.5.0`; Claude Code used its `sonnet` selection on Claude Code `2.1.269`.

| Metric | Claude Code | Mimir | Mimir difference |
| --- | ---: | ---: | ---: |
| Solve rate | 100.0% | 100.0% | 0.0% |
| Mean combined score | 1.000 | 1.000 | 0.0% |
| Median elapsed time | 40.15 s | 30.44 s | -24.2% |
| P95 elapsed time | 64.49 s | 70.91 s | +10.0% |
| Mean total tokens | 447,913 | 69,434 | -84.5% |
| Scope violations | 0 | 0 | - |
| Adapter failures | 0 | 0 | - |

Mimir matched the solve rate and score, used fewer reported tokens, and had a lower median runtime. Its P95 runtime and elapsed-time variability were higher.

[Read the complete Mimir vs Claude Code report](benchmarks/2026-09-12-mimir-vs-claude-code.pdf).

### Mimir vs Codex

This comparison matched both harnesses on `gpt-5.6-terra`. Mimir used version `0.5.0`; Codex used `codex-cli 0.153.4`.

| Metric | Codex | Mimir | Mimir difference |
| --- | ---: | ---: | ---: |
| Solve rate | 94.4% | 94.4% | 0.0% |
| Mean combined score | 0.961 | 0.989 | +2.9% |
| Median elapsed time | 98.76 s | 28.72 s | -70.9% |
| P95 elapsed time | 305.44 s | 92.91 s | -69.6% |
| Mean total tokens | 293,016 | 38,833 | -86.7% |
| Scope violations | 0 | 0 | - |
| Adapter failures | 0 | 0 | - |

With the underlying model matched, Mimir recorded the same solve rate, a slightly higher mean score, fewer reported tokens, and lower median and P95 runtime.

[Read the complete matched-model Mimir vs Codex report](benchmarks/2026-09-12-mimir-vs-codex.pdf).

These are harness-level results from a small, fixed task suite, not universal model-quality claims. Token accounting follows each harness adapter's recorded totals. Cost is omitted here because the Claude Code report mixes vendor-reported and locally estimated costs, while the Codex run has no recorded comparison cost.

## Local runtime measurements

Benchmarks are captured from the release binary on the benchmark machine after all quality gates pass. Reproduce the basic checks with:

```bash
cargo build --release
target/release/mimir --provider fake --fake-response ok --print hello
wc -c < target/release/mimir
```

Final local release measurements recorded on 2026-08-11:

- Stripped binary: `14,755,104` bytes (`14.07 MiB`).
- `--version` peak RSS: `8,470,528` bytes (`8.08 MiB`).
- Fully assembled offline fake-provider prompt: `0.295 ms` inside the runtime benchmark.
- Full all-target/all-feature test inventory: `448` passing tests.
- Live provider calls used for verification: `0`.

The release profile uses thin LTO, one codegen unit, symbol stripping, and abort-on-panic. The steady-state runtime bounds transcript context, provider bodies, daemon frames and history, file content, process output, turns, tool calls, and child concurrency. Session replay is streaming, and each compaction atomically replaces superseded history with a checkpoint plus retained messages, preventing long-lived sessions from paying for the entire historical transcript on every resume.
