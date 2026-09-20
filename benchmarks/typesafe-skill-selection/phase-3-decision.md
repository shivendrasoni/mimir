# Phase 3 next-bet decision

Decision: select tool and MCP shortlisting.

## Why the earlier stop changed

The original decision stopped because Mimir had no comparable evidence for a second use case and Phase 2 was intended to run in production first. Mimir has not yet been released, so no production cohort exists. The product decision is to complete one Phase 3 bet and launch it with Phase 2 rather than publish an intermediate canary build.

A pre-committed 32-case baseline then measured the complete 20-tool pool at 9,450 provider-context tokens per model step while only 1.72 tools were required on average. The labelled corpus therefore identified tool context as a material avoidable cost: 86.1% of the full pool was unnecessary for the average case.

## Selected scope

- Judge optional built-in, extension, and MCP tools independently with one Noul per tool.
- Put the tool questions in the same Jev request as skill selection rather than adding a second request.
- Send bounded names and descriptions to TypeSafe, never parameter schemas.
- Keep Rust authoritative over thresholds, permissions, execution, fallback, and recovery.
- Keep TypeSafe off by default with the existing `on | off` activation contract.

Thinking-level routing and completion verification remain deferred. This is not authorization for a general TypeSafe framework.

## Evidence

The initial conservative policy stopped safely because 28 of 32 cases fell back to the full pool. The calibrated policy passed the pre-committed gate with 100% required-tool recall, 64.3% provider-context savings, 2,978.3 net first-step tokens saved per case after the complete TypeSafe input, 739 ms p95 latency, and seven safe full-pool fallbacks. See the versioned baseline, initial replay, final replay, and controlled-acceptance report in `benchmarks/typesafe-tool-selection/`.
