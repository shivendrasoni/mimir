use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::runtime::RuntimeEvent;

/// Default number of runtime events retained for each live subscriber.
///
/// The channel is deliberately bounded: slow control-plane clients observe a
/// lag marker and resume from the newest retained event instead of growing the
/// agent process without limit.
pub const DEFAULT_RUNTIME_EVENT_CAPACITY: usize = 256;

/// Identifies the operation that produced an event.
///
/// ACP uses this identity to suppress the broadcast copy of events it already
/// receives through its prompt sink while still forwarding unrelated metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeEventSource(Uuid);

impl RuntimeEventSource {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RuntimeEventSource {
    fn default() -> Self {
        Self::new()
    }
}

/// One bounded broadcast item produced by an [`AgentRuntime`](crate::runtime::AgentRuntime).
#[derive(Debug, Clone)]
pub struct RuntimeEventEnvelope {
    pub sequence: u64,
    pub source: Option<RuntimeEventSource>,
    pub event: RuntimeEvent,
}

/// Session-lifetime fan-out for runtime and control-plane events.
#[derive(Debug, Clone)]
pub struct RuntimeEventBus {
    sender: broadcast::Sender<RuntimeEventEnvelope>,
    sequence: Arc<AtomicU64>,
}

impl Default for RuntimeEventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RUNTIME_EVENT_CAPACITY)
    }
}

impl RuntimeEventBus {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            sender,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.sender.subscribe()
    }

    pub fn publish(&self, source: Option<RuntimeEventSource>, event: RuntimeEvent) {
        // A runtime can legitimately have no daemon/ACP observer. Sending is
        // best-effort and never delays the provider/tool loop.
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let _ = self.sender.send(RuntimeEventEnvelope {
            sequence,
            source,
            event,
        });
    }

    pub fn publish_session_event(&self, event: Value) {
        self.publish(None, RuntimeEvent::SessionEvent { event });
    }
}
