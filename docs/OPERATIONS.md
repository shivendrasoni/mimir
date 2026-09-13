# Operations

## State

State defaults to the global `$HOME/.mimir/` directory and uses versioned JSON and JSONL formats. Set `MIMIR_STATE_DIR` or pass `--state-dir` to use an isolated state directory.

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
