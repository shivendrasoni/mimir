# Phase 2 explicit TypeSafe activation

Status: implementation complete; production retention gate pending sustained real-turn telemetry.

## Capability

Mimir now exposes TypeSafe as one cohesive configuration with exactly two states: `off` and `on`. The default remains `off`. When explicitly turned on, Mimir:

1. asks Jev to judge applicability and rank the bounded skill catalog;
2. loads the recommended skill only when applicability is at least 0.60 and Choice confidence is at least 0.50; and
3. falls back to the existing `search_skills` workflow on every uncertain, invalid, timed-out, configuration, activation, or service failure.

An explicit `/skill:<name>` invocation always wins and skips automatic selection. The normal skill-search tool remains available after TypeSafe activation, so the model can correct a recommendation.

## Monitoring contract

Each evaluated turn emits a privacy-safe `typesafe_skill_selection` runtime event containing:

- state, decision, selected skill, top probabilities, and whether thresholds passed;
- request byte count and request SHA-256;
- TypeSafe model, input/output tokens, estimated cost, and end-to-end latency;
- added skill-context tokens and a coarse activation/failure category.

The request text and API key are never copied into this diagnostic. A `typesafe_skill_outcome` event records whether the Mimir turn completed, and `typesafe_skill_correction` records a later activation of a different skill. These fields cover task success, corrections, skill-context use, latency, and total Jev cost without creating a second telemetry framework.

## Controls and rollback

```bash
# Enable TypeSafe
mimir --typesafe on

# Immediate off switch
mimir --typesafe off

# Environment equivalents
MIMIR_TYPESAFE=on
```

There is no runtime rollout-percentage control. If a deployment needs a staged rollout, it selects which processes receive `MIMIR_TYPESAFE=on`; Mimir's activation contract remains binary.

## Retention gate

Do not make TypeSafe the default or expand it to another use case until real-turn telemetry sustains all of the following against a comparable off cohort:

- equal or better task completion;
- fewer wrong and needless skill loads;
- no material increase in corrections;
- positive net provider-token savings after Jev input cost;
- p95 selection latency at or below 2.5 seconds; and
- average Jev input at or below 1,800 tokens per evaluated turn.

The Phase 1 labelled replay passed its experiment gate, but it is not a substitute for this production retention gate.
