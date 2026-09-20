# Phase 2 controlled acceptance

Status: complete as the historical skill-only gate. Phase 3 subsequently added tool-pool shortlisting to the same TypeSafe request and activation switch before Mimir's first release.

## Capability accepted

Mimir exposes TypeSafe as one cohesive configuration with exactly two states: `off` and `on`. The default is `off`. When explicitly turned on, Mimir:

1. asks Jev to judge applicability and rank the bounded skill catalog;
2. loads the recommendation only when applicability is at least 0.60 and Choice confidence is at least 0.50; and
3. falls back to the existing `search_skills` workflow on every uncertain, invalid, timed-out, configuration, activation, or service failure.

An explicit `/skill:<name>` invocation always wins and skips automatic selection. The normal skill-search tool remains available after TypeSafe activation so the main model can correct a recommendation. TypeSafe does not shortlist tools or MCP servers and has no authority over providers, permissions, or execution.

## Live labelled replay

The versioned 26-case corpus was replayed against TypeSafe on 2026-09-19 with the production thresholds used by the runtime.

| Metric | Existing selector | TypeSafe | Gate |
| --- | ---: | ---: | --- |
| Exact selection accuracy | 76.9% | 88.5% | Pass: gain 11.5 points |
| Wrong / missed / needless | 4 / 1 / 1 | 1 / 2 / 0 | Pass: fewer total errors |
| No-skill slice accuracy | 80.0% | 100.0% | Pass: no regression |
| Wasted loaded context | 361.5 tokens/case | 73.1 tokens/case | Pass: 79.8% lower |
| Selection p95 latency | 0.134 ms | 741 ms | Pass: at most 2.5 seconds |
| TypeSafe input | 0 | 896.0 tokens/case | Pass: at most 1,800 |

The replay made one TypeSafe request per case and estimated $0.000978 total selection cost. It produced one wrong choice, two misses that safely fell back, and no needless skill load. The full case-level result is reproducible with:

```bash
cargo run --example typesafe_skill_benchmark --offline -- \
  --jev benchmarks/typesafe-skill-selection/cases.json /tmp/typesafe-phase-2.md
```

`--offline` applies to Cargo dependency resolution; the benchmark still calls TypeSafe and requires `TYPESAFE_API_KEY`.

## Runtime acceptance

The runtime contract covers the complete enabled path with a deterministic TypeSafe transport: a confident recommendation loads the skill before the main provider call; the selection and outcome events are emitted; diagnostics contain no request text or secret; and a later different skill activation is observable as a correction. Unit coverage also verifies the off-by-default CLI, the single `on | off` switch, threshold fallback, invalid selections, timeouts, and service errors.

## Monitoring contract

Each evaluated turn emits a privacy-safe `typesafe_skill_selection` runtime event containing the decision, selected skill, bounded probabilities, threshold result, request byte count and hash, model, usage, estimated cost, latency, added context, and a coarse failure category. `typesafe_skill_outcome` records whether the enclosing Mimir turn completed, and `typesafe_skill_correction` records a later activation of a different skill.

The request text and API key are never copied into these events. The user request and bounded skill names and descriptions are sent to the TypeSafe API only when TypeSafe is explicitly on.

## Controls and rollback

```bash
# Enable TypeSafe
mimir --typesafe on

# Immediate rollback
mimir --typesafe off

# Environment equivalent
MIMIR_TYPESAFE=on
```

There is no runtime rollout percentage or shadow state. Canary targeting is deployment policy: only canary processes receive `MIMIR_TYPESAFE=on`.

## Scope boundary

This report closes the Phase 2 skill-only implementation and controlled acceptance. The planned standalone canary was superseded before release; TypeSafe remains off by default, and the combined release contract is documented in the Phase 3 acceptance report.
