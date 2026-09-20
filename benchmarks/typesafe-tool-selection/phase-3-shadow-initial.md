# Phase 3 initial tool-pool replay

Status: stopped safely and retained as calibration evidence.

The pre-committed 32-case corpus was first replayed with a 0.35 inclusion threshold and a 0.20 uncertainty floor. Every required tool remained available, but 28 of 32 cases fell back to the complete pool because at least one harmless omitted tool landed in the broad uncertainty band.

| Metric | Result | Gate |
| --- | ---: | ---: |
| Required-tool recall | 100.0% | at least 100.0% |
| Provider tool-context savings | 9.2% | at least 60.0% |
| Net first-step savings after Jev input | -2,231.7 tokens/case | at least 500 |
| Selection p95 latency | 1,179 ms | at most 2,500 ms |
| Average TypeSafe input | 3,100.4 tokens/case | at most 6,000 |
| Full-pool uncertainty fallbacks | 28 of 32 | bounded and safe |

Initial gate: **STOP**.

The replay exposed a clear required-tool range: every labelled required tool scored at least 0.68 across two stable replays. The broad initial policy—not service latency or required-tool recognition—prevented savings. Calibration therefore moved the inclusion threshold to 0.60 and the uncertainty floor to 0.55, retaining an observed 0.08 margin below the weakest required tool. The final report evaluates that policy against the unchanged dataset and unchanged gates.
