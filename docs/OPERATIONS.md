# Operations

## State

Global configuration, fleet packs, and caches default to `$HOME/.mimir/`. Provider and MCP credentials are always read from the user-global `$HOME/.mimir/auth.json`; changing the state directory never changes or duplicates login state.

Durable transcripts, daemon state, and the daemon socket are stored under `<state-root>/projects/<project-hash>/`, where the hash identifies the canonical nearest Git project without exposing its path. Session learning is stored under `<project>/.mimir/learning/sessions/`. Ordinary launches create a fresh session; use `--continue`, `--resume`, or an explicit `--session` to restore one from the current project. Legacy cross-project `<state-root>/sessions/` and `<state-root>/harness/sessions/` directories are atomically moved under `<state-root>/quarantine/` on first use.

## Management commands

```bash
mimir doctor
mimir session list
mimir --session work session show
mimir --session work session export
mimir daemon start
mimir daemon status
mimir daemon prompt "continue"
mimir daemon stop
mimir goal set "finish the next milestone" --token-budget 80000
mimir goal show
mimir schedule add heartbeat "continue the goal" --every-seconds 300
mimir schedule list
mimir extension list
mimir rlm list sample-extension workspace
mimir --provider fake --fake-response ok benchmark prompt "smoke test"
```

## State migration

```bash
mimir migrate plan --legacy-root <legacy-state-directory>
mimir migrate apply --legacy-root <legacy-state-directory>
# Use the journal_path printed by apply:
mimir migrate rollback --journal <journal-path>
```

See [State migration](../STATE-MIGRATION.md) for compatibility boundaries and rollback behavior. See [Diagnostics](DIAGNOSTICS.md) for the operational evidence recorded by each agent process.
