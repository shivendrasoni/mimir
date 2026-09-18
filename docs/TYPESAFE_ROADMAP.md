# TypeSafe in Mimir: Lean Roadmap

Status: proposed

## The first bet

Test one hypothesis:

> Jev can help Mimir select the relevant skill for a turn, improving correctness
> while reducing unnecessary prompt content.

This comes first because skill selection can affect the whole turn, irrelevant
skill instructions consume provider tokens, and Jev can start as a read-only
recommendation with Mimir's current behavior as the fallback.

Do not build a general TypeSafe framework yet. Model routing, verification,
guardrails, continual learning, and orchestration remain out of scope until this
bet produces measured value.

## Rules

1. Rust code owns policy, permissions, execution, and fallbacks.
2. Start in `shadow` mode; TypeSafe failure must not block a turn.
3. Measure total cost and latency, including the Jev request.
4. Redact secrets and unnecessary user content from diagnostics.
5. Add a reusable abstraction only when a second proven use case needs it.

## Phase 0 — Prove there is a problem

1. Build a small, representative set of Mimir requests labelled with the correct
   skill or `none`. Include similar skills and requests needing no skill.
2. Measure current correct, wrong, missed, and needless skill loads, plus context
   tokens, task success, latency, and provider cost.
3. Agree on quality, savings, latency, and cost thresholds before testing Jev.

**Gate:** stop if wrong or wasteful skill loading is not a meaningful Mimir
problem.

**Deliverable:** a reproducible baseline report; no runtime integration.

## Phase 1 — Run the smallest shadow experiment

1. At the existing skill-selection boundary, ask Jev to rank the available skill
   summaries and judge whether any skill applies.
2. Add only a small HTTP client, configuration, `off | shadow` mode, fallback,
   and privacy-safe diagnostics.
3. Replay the Phase 0 set and compare selection errors, needless suggestions,
   task success, net cost, and latency.

Begin with one TypeSafe request. Add shortlist verification only if the results
show recurring confusion between similar skills.

**Gate:** stop if Jev does not create a material net improvement without a
meaningful quality regression.

**Deliverable:** evidence for or against the hypothesis, with no user-visible
behavior change.

## Phase 2 — Controlled assisted rollout

1. Add `assist` mode so the Jev result can prioritize or load a skill.
2. Fall back to current behavior on uncertainty, invalid output, timeout, or
   service failure.
3. Roll out to a small share of eligible turns and monitor task success,
   corrections, token use, latency, and total cost. Keep an immediate off switch.

**Gate:** retain the capability only if it sustains equal or better task success,
fewer wrong and needless loads, and positive net savings.

**Deliverable:** one proven production capability—or a documented decision to
remove it.

## Phase 3 — Pick one next bet

Only after Phase 2 succeeds, use measured Mimir data to select one:

1. **Thinking-level routing** if reasoning spend is the largest avoidable cost.
2. **Tool or MCP shortlisting** if tool context or wrong-tool calls are costly.
3. **Completion verification** if false completion is a frequent quality problem.

The selected bet repeats the same cycle: baseline, shadow test, explicit gate,
assisted rollout, and rollback. If the data supports none, stop expanding.

## Deferred—not committed

Retry selection, compaction checks, semantic guardrails, citation verification,
continual-learning critics, multi-agent routing, memory reranking, heartbeat
prioritization, natural-language commands, and semantic linting remain ideas only.

Review this roadmap at each gate. Expand it from measured Mimir problems, not
from the number of things Jev could theoretically do.
