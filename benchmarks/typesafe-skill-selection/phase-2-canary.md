# Phase 2 production canary

Status: planned; run after publishing a canary build.

## Deployment

- Publish one canary build from the accepted Phase 2 commit.
- Keep the normal build on `--typesafe off` and run the canary with `--typesafe on`.
- Compare similar real turns over the same observation window. Cohort assignment belongs to deployment; Mimir has no rollout-percentage flag.
- Do not send requests through the canary unless sharing their text and bounded skill summaries with TypeSafe is acceptable.

## Retention gate

Retain the capability only if the canary sustains all of the following against the off cohort:

- equal or better task completion;
- fewer wrong and needless skill loads;
- no material increase in skill corrections;
- positive net provider-token savings after TypeSafe input cost;
- p95 skill-selection latency at or below 2.5 seconds; and
- average TypeSafe input at or below 1,800 tokens per evaluated turn.

Use `typesafe_skill_selection`, `typesafe_skill_outcome`, and `typesafe_skill_correction` events for the comparison. Record cohort sizes, observation dates, build commits, and any exclusions before making the retention decision.

## Decision

- Pass: retain the explicit capability and decide separately whether sufficient evidence exists to change the default.
- Fail or inconclusive: set `--typesafe off`; extend the observation window only when the reason is insufficient sample size, not to erase a quality regression.
- Do not begin tool/MCP shortlisting or another TypeSafe use case as part of this canary.
