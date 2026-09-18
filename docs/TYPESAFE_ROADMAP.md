# TypeSafe in Mimir: Lean Roadmap

Status: Phases 0–3 implementation and controlled acceptance complete; combined first release pending

## The first bet

Test one hypothesis:

> Jev can help Mimir select the relevant skill for a turn, improving correctness
> while reducing unnecessary prompt content.

This comes first because skill selection can affect the whole turn, irrelevant
skill instructions consume provider tokens, and Jev can start as a read-only
recommendation with Mimir's current behavior as the fallback.

Do not build a general TypeSafe framework. Phase 3 adds only the measured
tool/MCP shortlisting bet; model routing, verification, guardrails, continual
learning, and orchestration remain out of scope.

## Rules

1. Rust code owns policy, permissions, execution, and fallbacks.
2. Validate with an offline shadow replay before enabling runtime behavior; TypeSafe failure must not block a turn.
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
2. Add only a small HTTP client, a cohesive TypeSafe configuration, fallback,
   and privacy-safe diagnostics. Keep the replay outside the runtime activation contract.
3. Replay the Phase 0 set and compare selection errors, needless suggestions,
   task success, net cost, and latency.

Begin with one TypeSafe request. Add shortlist verification only if the results
show recurring confusion between similar skills.

**Gate:** stop if Jev does not create a material net improvement without a
meaningful quality regression.

**Deliverable:** evidence for or against the hypothesis, with no user-visible
behavior change.

## Phase 2 — Explicit activation

1. Add one explicit TypeSafe switch: `off | on`. When on, a confident Jev result
   can prioritize or load a skill.
2. Fall back to current behavior on uncertainty, invalid output, timeout, or
   service failure.
3. Monitor task success, corrections, token use, latency, and total cost. Keep
   `off` as the immediate rollback; rollout percentages are deployment policy,
   not a Mimir runtime flag.

**Controlled gate:** proceed to a production canary only if labelled replay and
runtime contracts show equal or better selection quality, fewer wrong and
needless loads, bounded cost and latency, safe fallback, and working rollback.

**Deliverable:** one canary-ready capability with a separate production
retention plan. The canary—not this implementation phase—determines whether the
capability is retained and whether it may ever become the default.

## Phase 3 — Tool and MCP shortlisting

The harness had not been released, so there was no production cohort on which to
run the planned Phase 2 canary. The product decision was to complete one Phase 3
bet and launch both capabilities together while preserving the explicit switch
and immediate rollback.

1. Measure the complete configured tool-schema context and label every tool that
   may be needed across representative multi-step turns.
2. Ask one independent Noul per optional tool in the same Jev request as skill
   selection. Keep parameter schemas out of TypeSafe state.
3. Include a tool at probability 0.60 or above. Fall back to the complete pool
   if any omitted tool is at or above 0.55, the call fails, recovery is
   unavailable, or estimated provider-context savings are below 256 tokens.
4. Always retain `search_tools`, `search_skills`, and enabled autonomous
   completion. `search_tools` can activate an omitted configured capability on
   the next model step.
5. Keep `--typesafe off` as one immediate rollback for skill and tool selection.

**Controlled gate:** the calibrated 32-case replay passed with 100% required-tool
recall, 64.3% provider tool-context savings, 2,978.3 net first-step tokens saved
per case after charging the complete Jev input, 739 ms p95 latency, and seven
safe full-pool fallbacks. Runtime contracts cover shortlist enforcement,
service/uncertainty fallback, and same-run tool recovery.

**Deliverable:** skill selection and conservative tool/MCP shortlisting share one
TypeSafe request and one `on | off` activation contract. Production retention is
evaluated after the first combined release.

## Deferred—not committed

Retry selection, compaction checks, semantic guardrails, citation verification,
continual-learning critics, multi-agent routing, memory reranking, heartbeat
prioritization, natural-language commands, and semantic linting remain ideas only.

Review this roadmap at each gate. Expand it from measured Mimir problems, not
from the number of things Jev could theoretically do.

## Evidence

- [Phase 0 baseline](../benchmarks/typesafe-skill-selection/phase-0-baseline.md)
- [Phase 1 calibrated replay](../benchmarks/typesafe-skill-selection/phase-1-shadow-final.md)
- [Phase 2 controlled acceptance](../benchmarks/typesafe-skill-selection/phase-2-acceptance.md)
- [Phase 2 canary history](../benchmarks/typesafe-skill-selection/phase-2-canary.md)
- [Phase 3 selection decision](../benchmarks/typesafe-skill-selection/phase-3-decision.md)
- [Phase 3 tool-pool baseline](../benchmarks/typesafe-tool-selection/phase-3-baseline.md)
- [Phase 3 initial stopped replay](../benchmarks/typesafe-tool-selection/phase-3-shadow-initial.md)
- [Phase 3 calibrated replay](../benchmarks/typesafe-tool-selection/phase-3-shadow-final.md)
- [Phase 3 controlled acceptance](../benchmarks/typesafe-tool-selection/phase-3-acceptance.md)
