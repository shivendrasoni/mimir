# RLM and continual harness

When running an interactive, session-backed agent, Mimir registers its RLM tools automatically. The agent can choose from its currently authenticated models and recursively delegate bounded work; each child is tracked independently from admission through completion, cancellation, or deletion.

The default maximum recursion depth is 3, and all child work remains subject to the runtime's budgets and tool policy. Use `/rlm-max-depth` in the TUI to inspect or set the recursion limit for a session.

## Refinement

Use `/refine --scope session|project|user <instructions>` when you want Mimir to review the current trajectory and persist a focused lesson. Session remains the default, and the deprecated `--global` alias still maps to user scope. Fleet scope is read-only.

Undo a recorded update with:

```text
/refine rollback <refinement-id>
# or, for a global refinement
/refine rollback <refinement-id> --global
```

## Continual learning

Initialize project learning with `mimir learning init`, then inspect or change it with `mimir learning status`, `mimir learning mode observe|auto|off`, or `/learn`.

With `TYPESAFE_API_KEY`, the local learning lifecycle is automatic; `--typesafe off` makes the Jev path inert immediately. Candidates still need three applicable attributed verified successes before activation, while an attributed verified failure quarantines them. Fleet contribution is separately opt-in.

See [Continual learning](CONTINUAL_LEARNING.md) for the storage, trust, privacy, rollout, and rollback contract.

This is harness refinement, not model-weight training: Mimir proposes and validates small durable operating-context changes, then records the before-and-after state needed to inspect or reverse them.
