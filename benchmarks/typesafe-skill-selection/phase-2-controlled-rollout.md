# Phase 2 controlled assisted rollout

Status: implementation complete; production retention gate pending sustained real-turn telemetry.

## Capability

Mimir now supports `off`, `shadow`, and `assist` skill-selection modes. The default remains `off`. Explicit `assist` mode:

1. assigns each request a stable 0–99 bucket from its SHA-256 digest;
2. calls Jev only when the bucket is inside the configured rollout percentage (10% by default);
3. loads the recommended skill only when applicability is at least 0.60 and Choice confidence is at least 0.50; and
4. falls back to the existing `search_skills` workflow on every unsampled, uncertain, invalid, timed-out, configuration, activation, or service failure.

An explicit `/skill:<name>` invocation always wins and skips automatic selection. The normal skill-search tool remains available after assisted activation, so the model can correct a recommendation.

## Monitoring contract

Each evaluated turn emits a privacy-safe `typesafe_skill_selection` runtime event containing:

- mode, decision, selected skill, top probabilities, and whether thresholds passed;
- stable rollout bucket, request byte count, and request SHA-256;
- TypeSafe model, input/output tokens, estimated cost, and end-to-end latency;
- added skill-context tokens and a coarse activation/failure category.

The request text and API key are never copied into this diagnostic. A `typesafe_skill_outcome` event records whether the Mimir turn completed, and `typesafe_skill_correction` records a later activation of a different skill. These fields cover task success, corrections, skill-context use, latency, and total Jev cost without creating a second telemetry framework.

## Controls and rollback

```bash
# 10% controlled rollout
mimir --typesafe-skill-selection assist

# Immediate off switch
mimir --typesafe-skill-selection off

# Environment equivalents
MIMIR_TYPESAFE_SKILL_SELECTION=assist
MIMIR_TYPESAFE_ASSIST_ROLLOUT_PERCENT=10
```

Changing the rollout percentage does not reshuffle existing requests because sampling is deterministic. Setting it to `0` exercises assist configuration without sending TypeSafe requests or changing behavior.

## Retention gate

Do not make assist the default or expand TypeSafe to another use case until real-turn telemetry sustains all of the following against a comparable off/shadow cohort:

- equal or better task completion;
- fewer wrong and needless skill loads;
- no material increase in corrections;
- positive net provider-token savings after Jev input cost;
- p95 selection latency at or below 2.5 seconds; and
- average Jev input at or below 1,800 tokens per evaluated turn.

The Phase 1 labelled replay passed its experiment gate, but it is not a substitute for this production retention gate.
