# Phase 3 tool-pool baseline

Dataset: `mimir-tool-selection-v1` (32)

## Result

| Metric | Value |
| --- | ---: |
| Configured optional tools | 20 |
| Required-tool recall | 100.0% |
| Tool context per provider call | 9450 tokens |
| Average required tools per case | 1.72 |
| Avoidable tool context | 86.1% |
| Selection calls | 0 |

The current runtime sends the complete configured tool pool on every provider step. This baseline deliberately gives it perfect required-tool recall, then treats every non-required schema as avoidable context. The recovery tools retained by the Phase 3 design account for 600 additional tokens in both arms.

## Pre-committed gate

- Required-tool recall: at least 100.0%.
- Provider tool-context savings: at least 60.0%.
- Net savings after the complete Jev input: at least 500 tokens per case on the first provider step.
- Jev p95 latency: at most 2500 ms.
- Average Jev input: at most 6000 tokens per case.
