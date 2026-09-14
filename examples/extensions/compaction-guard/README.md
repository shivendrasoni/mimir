# Compaction guard

This extension demonstrates an intercepting pre-hook and an observational post-hook. It blocks compaction below 8,000 context tokens and reports successful compaction through the normal extension UI channel.

Install it for the current workspace:

```bash
mimir package install ./examples/extensions/compaction-guard --local
mimir extension list
```

Change `minimumContextTokens` in `index.mjs` before installation to use a different threshold.
