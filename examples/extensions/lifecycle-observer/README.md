# Lifecycle observer

This extension subscribes to the lifecycle events that surround session transitions, streamed messages, tool progress, and compaction. It keeps process-local counters and exposes them through `lifecycle-counts` without writing every streaming update into the transcript.

Install it for the current workspace:

```bash
mimir package install ./examples/extensions/lifecycle-observer --local
mimir extension list
mimir extension run lifecycle-counts
```

Restart a running Mimir process after installing or removing an extension package so its extension catalog is rebuilt.
