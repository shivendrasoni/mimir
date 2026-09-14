const minimumContextTokens = 8_000;

export default function activate(pi) {
  pi.on("session_before_compact", (event, ctx) => {
    if (event.contextTokens < minimumContextTokens) {
      ctx.ui.notify(
        `Compaction deferred at ${event.contextTokens} context tokens`,
        "info",
      );
      return { cancel: true };
    }
    return undefined;
  });

  pi.on("session_compact", (_event, ctx) => {
    ctx.ui.notify("Session compaction completed", "info");
  });
}
