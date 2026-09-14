const observedEvents = [
  "session_before_switch",
  "session_before_fork",
  "session_before_compact",
  "session_compact",
  "session_shutdown",
  "session_before_tree",
  "session_tree",
  "message_update",
  "tool_execution_update",
];

export default function activate(pi) {
  const counts = new Map(observedEvents.map((event) => [event, 0]));

  for (const event of observedEvents) {
    pi.on(event, () => {
      counts.set(event, counts.get(event) + 1);
    });
  }

  pi.registerCommand("lifecycle-counts", {
    description: "Show lifecycle events observed by this process",
    async handler() {
      const output = Object.fromEntries(counts);
      return {
        message: Object.entries(output)
          .map(([event, count]) => `${event}: ${count}`)
          .join("\n"),
        output,
      };
    },
  });
}
