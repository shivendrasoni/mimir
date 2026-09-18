# Phase 0 skill-selection baseline

Dataset: `mimir-skill-selection-v1` (26)

## Result

| Metric | Value |
| --- | ---: |
| Correct | 20 |
| Wrong | 4 |
| Missed | 1 |
| Needless | 1 |
| Accuracy | 76.9% |
| Problem rate | 23.1% |
| Average loaded skill context | 1538.5 tokens |
| Selection p95 latency | 0.113 ms |
| Selection provider tokens | 0 input / 0 output |

Phase 0 gate: **PASS** (problem rate 23.1% vs required 20.0%).

Task success is represented by exact labelled skill selection. Provider cost is zero for the current local selector. Context tokens count the selected fixture's full instruction budget; latency measures only selection.

## Pre-committed experiment thresholds

- Accuracy improvement: at least 10.0 percentage points.
- Quality regression on any protected slice: at most 2.0 percentage points.
- Loaded-context savings: at least 15.0%.
- Jev p95 latency: at most 2500 ms.
- Jev average input budget: at most 1800 tokens per eligible turn.

## Cases

| Case | Expected | Selected | Outcome | Context tokens | Latency (µs) |
| --- | --- | --- | --- | ---: | ---: |
| sheet-01 | spreadsheets | spreadsheets | Correct | 2200 | 127 |
| sheet-02 | spreadsheets | spreadsheets | Correct | 2200 | 85 |
| sheet-03 | spreadsheets | code-review | Wrong | 1600 | 105 |
| pdf-01 | pdf | pdf | Correct | 1700 | 85 |
| pdf-02 | pdf | pdf | Correct | 1700 | 83 |
| pdf-03 | pdf | pdf | Correct | 1700 | 111 |
| docs-01 | documents | documents | Correct | 1900 | 77 |
| docs-02 | documents | documents | Correct | 1900 | 89 |
| docs-03 | documents | documents | Correct | 1900 | 98 |
| slides-01 | presentations | presentations | Correct | 2100 | 84 |
| slides-02 | presentations | none | Missed | 0 | 86 |
| slides-03 | presentations | presentations | Correct | 2100 | 103 |
| image-01 | imagegen | imagegen | Correct | 1400 | 87 |
| image-02 | imagegen | imagegen | Correct | 1400 | 83 |
| image-03 | imagegen | presentations | Wrong | 2100 | 92 |
| security-01 | security-audit | security-audit | Correct | 2600 | 86 |
| security-02 | security-audit | security-audit | Correct | 2600 | 95 |
| security-03 | security-audit | spreadsheets | Wrong | 2200 | 113 |
| review-01 | code-review | code-review | Correct | 1600 | 86 |
| review-02 | code-review | code-review | Correct | 1600 | 105 |
| review-03 | code-review | documents | Wrong | 1900 | 86 |
| none-01 | none | none | Correct | 0 | 83 |
| none-02 | none | none | Correct | 0 | 84 |
| none-03 | none | none | Correct | 0 | 85 |
| none-04 | none | none | Correct | 0 | 72 |
| none-05 | none | code-review | Needless | 1600 | 73 |
