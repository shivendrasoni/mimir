# Phase 3 next-bet decision

Decision: stop expansion; select none of the three deferred bets.

## Why

Phase 1 established that Jev can materially improve labelled skill selection: the calibrated replay reached 88.5% exact accuracy versus 76.9% for the current lexical selector, reduced wasted loaded context by 79.8%, preserved the no-skill slice, stayed below the latency and token gates, and cost about $0.000978 for 26 calls.

Phase 2 therefore ships one controlled capability, but its production retention gate still requires sustained real-turn telemetry. The repository does not yet contain comparable measurements showing that reasoning spend, tool/MCP selection, or false completion is Mimir's largest remaining avoidable problem. Choosing a second use case now would violate the roadmap's measured-problem rule and its instruction not to build a general TypeSafe framework.

## What remains in scope

- Keep TypeSafe limited to skill selection.
- Keep TypeSafe `off` by default and expose only the explicit `on | off` activation contract.
- Aggregate `typesafe_skill_selection`, `typesafe_skill_outcome`, and `typesafe_skill_correction` events across a comparable cohort.
- Use `--typesafe off` as the immediate rollback if task success regresses, corrections rise materially, or net savings turn negative.

## Evidence required before revisiting

Select exactly one next bet only after Phase 2 sustains its gate and one candidate dominates measured avoidable cost or quality loss:

1. thinking-level routing: reasoning-token and latency distributions by task difficulty;
2. tool/MCP shortlisting: tool-context size, wrong-tool rate, and recovery cost; or
3. completion verification: false-completion rate and user correction cost.

If none dominates, continue to stop. Retry selection, compaction checks, semantic guardrails, citation verification, continual-learning critics, multi-agent routing, memory reranking, heartbeat prioritization, natural-language commands, and semantic linting remain deferred.
