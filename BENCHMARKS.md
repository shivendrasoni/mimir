# Performance evidence

Benchmarks are captured from the release binary on the migration machine after all quality gates pass. Reproduce with:

```bash
cargo build --release
target/release/mimir --provider fake --fake-response ok --print hello
wc -c < target/release/mimir
```

Measured results are recorded in the final verification record together with the exact commands that were rerun during the final verification pass. The release profile uses thin LTO, one codegen unit, symbol stripping, and abort-on-panic.

Final local release measurements on 2026-08-11:

- Stripped binary: `14,755,104` bytes (`14.07 MiB`).
- `--version` peak RSS: `8,470,528` bytes (`8.08 MiB`).
- Fully assembled offline fake-provider prompt: `0.295 ms` inside the runtime benchmark.
- Full all-target/all-feature test inventory: `448` passing tests.
- Live provider calls used for verification: `0`.

The steady-state runtime bounds transcript context, provider bodies, daemon frames/history, file content, process output, turns, tool calls, and child concurrency. Session replay is streaming and each compaction atomically replaces superseded history with a checkpoint plus retained messages, preventing long-lived sessions from paying for the entire historical transcript on every resume.
