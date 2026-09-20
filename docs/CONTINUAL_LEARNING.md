# Continual learning

Mimir's continual learning changes bounded harness context, not model weights. Raw prompts, source code, tool payloads, credentials, absolute paths, and raw trajectories remain local.

## Scope and storage

The nearest ancestor containing `.mimir/project.json` defines the project boundary, but discovery never crosses the nearest Git repository root. An umbrella marker therefore cannot claim a nested repository. `mimir learning init` creates the marker and `.mimir/learning/state.json`; in Git repositories it adds those two paths to the repository-local exclude file so project identity and learned state remain untracked by default.

Schema v2 has four scopes, ordered from most to least specific:

1. `session`: `.mimir/learning/sessions/<session-id>/` beneath the discovered project root.
2. `project`: `.mimir/learning/` beneath the discovered marker.
3. `user`: existing global harness state under the configured Mimir state root.
4. `fleet`: read-only signed packs cached under `<state-root>/learning/fleet/`.

The old serialized `local` and `global` names and the `--global` CLI option remain accepted as aliases for `session` and `user`. Refinement history retains the before/after records required for rollback. Legacy global session transcripts and session-learning directories are quarantined under `<state-root>/quarantine/` instead of being attached to whichever project starts next.

At runtime, applicable entries are deduplicated with session over project over user over fleet precedence. Selection scores scope, explicit priority, query relevance, and recency, then enforces a 24-entry, 12 KiB context ceiling. Loading context is read-only and does not initialize a project.

## Evidence and candidates

Project learning begins in `observe` mode. Automatic mode remains locked until the project has accumulated at least three observe-only evidence records. Diagnostics, objective validation gates, corrections, explicit rollbacks, and Yes/No feedback become bounded structured evidence. Silence is neutral: Mimir asks for feedback only when a canary decision can change.

In `auto` mode, a read-only RLM proposer receives only a bounded, redacted evidence summary. It never receives raw trajectory text or feedback notes. A separate read-only critic accepts or rejects the proposal. Neither child has tools, and only the coordinator can write learning state.

Approved proposals enter a project canary. A candidate activates only after three applicable verified successes. One verified failure quarantines it immediately. Rollback marks it rolled back and removes it from assembled context. Prompt, memory, skill, and subagent edits are supported, including deletion; learned skills may only point to an existing Python import and callable with a declared argument map.

Candidate validation rejects unknown kinds, base-prompt changes, permission expansion, dependency installation, and generated executable code. State updates use atomic replacement, restrictive file permissions, bounded collections, an in-process mutex, and a stale-recovering cross-process lock.

## Explicit natural-language memory

The parent agent has a built-in `remember` tool. When the user explicitly says “remember this”, “always remember”, or otherwise asks Mimir to retain guidance, the model converts that request into a concise standalone memory and invokes the tool. The tool writes through the same versioned refinement coordinator as `/refine`, so the result is bounded, scoped, visible in harness context, and reversible with `/refine rollback <refinement-id>`.

Project is the default scope. Session scope is used only for explicitly temporary guidance. User scope requires explicit global or across-project wording in the current persisted user turn; the host verifies that evidence rather than trusting the model's tool arguments. User memories are classified as portable preferences, procedures, safety rules, or tooling heuristics. Project status, phases, branches, commits, releases, versions, completion claims, and local paths are rejected. Ambiguous legacy global entries without a portable classification remain on disk for rollback but are excluded from assembled context. The tool is unavailable in plan mode and is not inherited by RLM children.

For precise manual control, `/refine --scope session|project|user <instructions>` remains available.

## Fleet packs and contribution

Fleet consumption and contribution are independent:

- Consumption accepts only versioned Ed25519-signed envelopes over HTTPS. Mimir validates the content digest, signature, schema, expiry, minimum compatible Mimir version, entry bounds, and safe version path before atomic activation. Installed versions remain cached for offline fallback and can be pinned per project. Revoked entry IDs are omitted from context. Installing an older signed version or pinning a cached version provides fleet rollback.
- Contribution defaults off. `learning contribution enable` opts one project in, and `learning submit <candidate-id>` accepts only an active verified candidate. The upload is an aggregate structural record: a rotating pseudonymous alias, bounded public summary, edit kinds, applicability tags/languages, outcome counts, and hashes. It excludes prompts, code, paths, project IDs, credentials, tool inputs/outputs, and evidence notes.

The aggregation service is outside the local runtime trust boundary. It must enforce minimum independent contributors, minimum verified successes, regression-rate ceilings, duplicate and poisoning checks, privacy review, and staged fleet rollout before signing a pack. Local clients never accept unsigned aggregation output.

## Commands

```text
mimir learning init
mimir learning status
mimir learning mode observe|auto|off
mimir learning candidates
mimir learning feedback yes|no [--note <bounded-note>]
mimir learning rollback <candidate-id>
mimir learning contribution enable|disable
mimir learning check
mimir learning update
mimir learning submit <candidate-id>
mimir learning pin [version]

/refine --scope session|project|user <instructions>
/learn status|candidates|propose|feedback yes|no|rollback <id>
/learn mode off|observe|auto
/learn contribution enable|disable
/learn check|update|submit <id>|pin [version]
```

Fleet endpoints and the Ed25519 public key are operator configuration: `MIMIR_LEARNING_PACK_URL`, `MIMIR_LEARNING_PUBLIC_KEY`, and `MIMIR_LEARNING_CONTRIBUTION_URL`. `--offline` disables update and submission while cached compatible packs remain available.
