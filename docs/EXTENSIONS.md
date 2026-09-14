# Extensions and lifecycle hooks

Mimir extensions can register tools, commands, renderers, providers, resources, UI requests, and lifecycle hooks. Embedded JavaScript or TypeScript extensions run in an isolated QuickJS host. Native-process extensions use the same versioned JSON-line ABI but require the explicit `unrestricted_native` capability because they execute with the caller's operating-system permissions.

## Install a local extension

A local package contains a `package.json` with one or more `pi.extensions` entrypoints:

```json
{
  "name": "my-mimir-extension",
  "version": "1.0.0",
  "pi": { "extensions": ["./index.mjs"] }
}
```

Install it for the current workspace, inspect the catalog, and restart any already-running Mimir process:

```bash
mimir package install ./path/to/my-mimir-extension --local
mimir extension list
```

The complete examples are:

- [`lifecycle-observer`](../examples/extensions/lifecycle-observer/) counts the session, message-stream, tool-progress, and compaction hooks without adding every update to the transcript.
- [`compaction-guard`](../examples/extensions/compaction-guard/) blocks a pre-compaction event and observes the matching post-compaction event.

## Register a hook

Use `pi.on` in the extension entrypoint. Event payloads use camelCase in embedded JavaScript and TypeScript:

```js
export default function activate(pi) {
  pi.on("tool_execution_update", (event, ctx) => {
    ctx.ui.notify(`Tool update: ${event.toolName}`, "info");
  });
}
```

Extensions do not receive Node.js globals, filesystem access, network access, or shell access implicitly. Use the capability-scoped extension APIs instead.

## Lifecycle reference

| Area | Events |
|---|---|
| Resources | `resources_discover` |
| Sessions | `session_start`, `session_before_switch`, `session_before_fork`, `session_before_compact`, `session_compact`, `session_shutdown`, `session_before_tree`, `session_tree` |
| Provider and agent | `before_provider_request`, `after_provider_response`, `before_agent_start`, `agent_start`, `agent_end`, `context` |
| Turns and messages | `turn_start`, `turn_end`, `message_start`, `message_update`, `message_end`, `input` |
| Selection | `model_select`, `thinking_level_select` |
| Tools and shell | `tool_call`, `tool_result`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `user_bash` |
| Refinement | `refine_complete` |

Pre-hooks that may block an operation are `session_before_switch`, `session_before_fork`, `session_before_compact`, `session_before_tree`, `before_agent_start`, `model_select`, `thinking_level_select`, `tool_call`, `user_bash`, and `input`:

```js
pi.on("session_before_compact", (event) => {
  if (event.contextTokens < 8_000) {
    return { cancel: true, reason: "keep the active context" };
  }
});
```

Payload mutation or replacement is accepted only for the matching `context`, `model_select`, `thinking_level_select`, `tool_call`, `tool_result`, `user_bash`, `input`, `refine_complete`, or `message_end` event. A hook cannot combine cancellation with mutation, and malformed or mismatched outcomes fail closed.

`message_update` is emitted for streamed text and thinking deltas. `tool_execution_update` is emitted when the runtime has a bounded tool observation, between `tool_execution_start` and `tool_execution_end`. Session pre-hooks run before durable mutation; their corresponding completion hooks run only after the operation succeeds.

## Disable or remove

```bash
mimir extension disable my-mimir-extension
mimir package remove my-mimir-extension --local
```

Disabling preserves the installed package. Removing a package moves it into the extension trash area so the operation remains recoverable.
