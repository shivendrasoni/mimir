# Phase 1 TypeSafe shadow experiment — initial threshold

Dataset: `mimir-skill-selection-v1` (26)

## Comparison

| Metric | Current selector | Jev shadow | Gate |
| --- | ---: | ---: | --- |
| Exact selection accuracy | 76.9% | 84.6% | gain ≥ 10.0 points |
| Wrong / missed / needless | 4 / 1 / 1 | 0 / 4 / 0 | lower |
| No-skill slice accuracy | 80.0% | 100.0% | regression ≤ 2.0 points |
| Wasted loaded context | 361.5 | -0.0 tokens/case | savings ≥ 15.0% |
| Selection p95 latency | 0.097 ms | 699 ms | ≤ 2500 ms |
| Provider input tokens | 0 | 896.0/case | ≤ 1800/case |
| Estimated selection cost | $0 | $0.000978 total | $0.042/MTok |

Phase 1 gate: **STOP**. Accuracy gain: 7.7 points; context savings: 100.0%; protected-slice regression: 0.0 points.

Jev ran one request per case with an applicability Noul and a Choice over the complete skill summaries. A recommendation counts only when applicability is at least 0.70 and Choice confidence is at least 0.50. Output tokens are recorded but are currently free under the published TypeSafe rate.

## Cases

| Case | Expected | Jev decision | Outcome | Applies | Confidence | Context tokens | Latency (ms) |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: |
| sheet-01 | spreadsheets | spreadsheets | Correct | 0.960 | 1.000 | 2200 | 1143 |
| sheet-02 | spreadsheets | spreadsheets | Correct | 0.960 | 1.000 | 2200 | 416 |
| sheet-03 | spreadsheets | spreadsheets | Correct | 0.920 | 1.000 | 2200 | 395 |
| pdf-01 | pdf | pdf | Correct | 0.720 | 0.980 | 1700 | 373 |
| pdf-02 | pdf | pdf | Correct | 0.740 | 0.990 | 1700 | 520 |
| pdf-03 | pdf | none | Missed | 0.600 | 0.680 | 0 | 370 |
| docs-01 | documents | documents | Correct | 0.920 | 1.000 | 1900 | 364 |
| docs-02 | documents | documents | Correct | 0.910 | 1.000 | 1900 | 367 |
| docs-03 | documents | documents | Correct | 0.860 | 1.000 | 1900 | 374 |
| slides-01 | presentations | presentations | Correct | 0.870 | 1.000 | 2100 | 401 |
| slides-02 | presentations | presentations | Correct | 0.850 | 1.000 | 2100 | 407 |
| slides-03 | presentations | none | Missed | 0.550 | 0.820 | 0 | 530 |
| image-01 | imagegen | imagegen | Correct | 0.950 | 1.000 | 1400 | 366 |
| image-02 | imagegen | imagegen | Correct | 0.960 | 1.000 | 1400 | 425 |
| image-03 | imagegen | imagegen | Correct | 0.960 | 1.000 | 1400 | 699 |
| security-01 | security-audit | security-audit | Correct | 0.920 | 1.000 | 2600 | 448 |
| security-02 | security-audit | security-audit | Correct | 0.960 | 1.000 | 2600 | 377 |
| security-03 | security-audit | none | Missed | 0.630 | 0.980 | 0 | 504 |
| review-01 | code-review | code-review | Correct | 0.940 | 1.000 | 1600 | 361 |
| review-02 | code-review | code-review | Correct | 0.890 | 1.000 | 1600 | 557 |
| review-03 | code-review | none | Missed | 0.500 | 0.970 | 0 | 404 |
| none-01 | none | none | Correct | 0.090 | 0.890 | 0 | 376 |
| none-02 | none | none | Correct | 0.240 | 1.000 | 0 | 410 |
| none-03 | none | none | Correct | 0.350 | 1.000 | 0 | 348 |
| none-04 | none | none | Correct | 0.120 | 0.700 | 0 | 402 |
| none-05 | none | none | Correct | 0.030 | 0.360 | 0 | 388 |
