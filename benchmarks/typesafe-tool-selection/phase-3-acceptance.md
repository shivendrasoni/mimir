# Phase 3 controlled acceptance

Status: complete. Tool and MCP shortlisting is ready to launch with Phase 2 behind the existing explicit TypeSafe switch. No production-retention claim is made here.

## Capability accepted

When `--typesafe on` is explicit, Mimir sends one shared Jev request containing the existing skill questions and one independent Noul for each optional configured tool. The state contains the current request and bounded skill and tool names and descriptions; provider parameter schemas are never sent to TypeSafe.

Rust converts those probabilities into a conservative tool pool:

1. include each optional tool at probability 0.60 or above;
2. retain the complete pool if an omitted tool is at or above 0.55;
3. retain the complete pool on skipped, invalid, timed-out, configuration, or service failure;
4. retain the complete pool if recovery is unavailable or estimated provider-context savings are below 256 tokens; and
5. always retain `search_tools`, `search_skills`, and enabled autonomous completion.

`search_tools` searches the configured built-in, extension, and MCP catalog without exposing every full schema. Its bounded matches become active on the next model step for the current run. If an extension removes this recovery tool from the active set, the TypeSafe shortlist is ignored and the extension-filtered full pool is used.

## Labelled replay

The versioned 32-case corpus covers no-tool requests, filesystem inspection and mutation, process execution, Python analysis, explicit memory, extensions, GitHub, Linear, Slack, calendar, browser control, and multi-service workflows.

| Metric | Full pool | TypeSafe | Gate |
| --- | ---: | ---: | --- |
| Required-tool recall | 100.0% | 100.0% | Pass: no regression |
| Tool context per provider call | 9,450 | 3,371.2 tokens | Pass: 64.3% lower |
| Net first-step savings after complete Jev input | 0 | 2,978.3 tokens/case | Pass: at least 500 |
| Selection p95 latency | 0 | 739 ms | Pass: at most 2.5 seconds |
| TypeSafe input | 0 | 3,100.4 tokens/case | Pass: at most 6,000 |
| Safe full-pool fallbacks | n/a | 7 of 32 | Expected |

The replay made one TypeSafe request per case and estimated $0.004167 total selection cost. Net savings conservatively charge the complete shared request against only the first provider step; every later tool-loop step reuses the shortlist and increases savings.

Reproduce the reports with:

```bash
cargo run --example typesafe_tool_benchmark --offline -- \
  benchmarks/typesafe-tool-selection/cases.json /tmp/typesafe-phase-3-baseline.md

cargo run --example typesafe_tool_benchmark --offline -- \
  --jev benchmarks/typesafe-tool-selection/cases.json /tmp/typesafe-phase-3-shadow.md
```

`--offline` applies only to Cargo dependency resolution. The Jev replay calls TypeSafe and requires `TYPESAFE_API_KEY`.

## Runtime acceptance

Contract coverage verifies that skill and multi-label tool judgments share one request, uncertainty retains the full pool, a confident result filters provider-visible schemas, hidden tools cannot execute directly, and `search_tools` can activate an omitted registered tool for the next provider step. Tool selection cannot grant permissions, register capabilities, bypass an explicit allowlist, choose a provider, or execute a tool.

Each evaluated run records privacy-safe `typesafe_tool_selection` and `typesafe_tool_outcome` events. Recovery adds `typesafe_tool_recovery`. The diagnostics include probabilities, decision, tool counts, shared request usage, latency, estimated context savings, request hash, and coarse failure category; they exclude request text, parameter schemas, raw service errors, and credentials.

## Activation and rollback

```bash
# Enable both accepted capabilities
mimir --typesafe on

# Immediate rollback to lexical skill discovery and the full tool pool
mimir --typesafe off
```

`MIMIR_TYPESAFE=on|off` is the environment equivalent. TypeSafe remains off by default for the first release.

## Combined production-retention gate

After release, compare similar `on` and `off` cohorts over the same observation window. Retain the combined capability only if task completion is equal or better, skill corrections do not rise materially, tool recovery and tool-disabled errors remain bounded, net provider-token savings stay positive after TypeSafe input, p95 selection latency remains at or below 2.5 seconds, and average TypeSafe input remains at or below 6,000 tokens per evaluated turn. Use `--typesafe off` immediately if quality regresses or net savings turn negative.
