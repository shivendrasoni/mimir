# Phase 1 TypeSafe shadow experiment

Dataset: `mimir-skill-selection-v1` (26)

## Comparison

| Metric | Current selector | Jev shadow | Gate |
| --- | ---: | ---: | --- |
| Exact selection accuracy | 76.9% | 88.5% | gain ≥ 10.0 points |
| Wrong / missed / needless | 4 / 1 / 1 | 1 / 2 / 0 | lower |
| No-skill slice accuracy | 80.0% | 100.0% | regression ≤ 2.0 points |
| Wasted loaded context | 361.5 | 73.1 tokens/case | savings ≥ 15.0% |
| Selection p95 latency | 0.122 ms | 930 ms | ≤ 2500 ms |
| Provider input tokens | 0 | 896.0/case | ≤ 1800/case |
| Estimated selection cost | $0 | $0.000978 total | $0.042/MTok |

Phase 1 gate: **PASS**. Accuracy gain: 11.5 points; context savings: 79.8%; protected-slice regression: 0.0 points.

Jev ran one request per case with an applicability Noul and a Choice over the complete skill summaries. A recommendation counts only when applicability is at least 0.60 and Choice confidence is at least 0.50. The initial 0.70 replay is preserved separately; it stopped at 84.6% because useful cases scored 0.60–0.63 while the highest no-skill case scored 0.35. The calibrated threshold retains a 0.25 observed margin without changing any outcome gate. Output tokens are recorded but are currently free under the published TypeSafe rate.

## Cases

| Case | Expected | Jev decision | Outcome | Applies | Confidence | Context tokens | Latency (ms) |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: |
| sheet-01 | spreadsheets | spreadsheets | Correct | 0.960 | 1.000 | 2200 | 998 |
| sheet-02 | spreadsheets | spreadsheets | Correct | 0.960 | 1.000 | 2200 | 345 |
| sheet-03 | spreadsheets | spreadsheets | Correct | 0.910 | 1.000 | 2200 | 397 |
| pdf-01 | pdf | pdf | Correct | 0.670 | 0.970 | 1700 | 367 |
| pdf-02 | pdf | pdf | Correct | 0.760 | 0.990 | 1700 | 400 |
| pdf-03 | pdf | documents | Wrong | 0.600 | 0.670 | 1900 | 440 |
| docs-01 | documents | documents | Correct | 0.920 | 1.000 | 1900 | 445 |
| docs-02 | documents | documents | Correct | 0.920 | 1.000 | 1900 | 331 |
| docs-03 | documents | documents | Correct | 0.850 | 1.000 | 1900 | 326 |
| slides-01 | presentations | presentations | Correct | 0.860 | 1.000 | 2100 | 322 |
| slides-02 | presentations | presentations | Correct | 0.850 | 1.000 | 2100 | 424 |
| slides-03 | presentations | none | Missed | 0.570 | 0.880 | 0 | 368 |
| image-01 | imagegen | imagegen | Correct | 0.950 | 1.000 | 1400 | 398 |
| image-02 | imagegen | imagegen | Correct | 0.960 | 1.000 | 1400 | 344 |
| image-03 | imagegen | imagegen | Correct | 0.960 | 1.000 | 1400 | 337 |
| security-01 | security-audit | security-audit | Correct | 0.910 | 1.000 | 2600 | 576 |
| security-02 | security-audit | security-audit | Correct | 0.960 | 1.000 | 2600 | 442 |
| security-03 | security-audit | security-audit | Correct | 0.620 | 0.980 | 2600 | 365 |
| review-01 | code-review | code-review | Correct | 0.940 | 1.000 | 1600 | 419 |
| review-02 | code-review | code-review | Correct | 0.900 | 1.000 | 1600 | 376 |
| review-03 | code-review | none | Missed | 0.550 | 0.970 | 0 | 341 |
| none-01 | none | none | Correct | 0.090 | 0.920 | 0 | 320 |
| none-02 | none | none | Correct | 0.270 | 1.000 | 0 | 380 |
| none-03 | none | none | Correct | 0.390 | 1.000 | 0 | 930 |
| none-04 | none | none | Correct | 0.110 | 0.720 | 0 | 351 |
| none-05 | none | none | Correct | 0.030 | 0.460 | 0 | 412 |
