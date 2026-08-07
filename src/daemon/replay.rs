use std::collections::{BTreeMap, VecDeque};

use uuid::Uuid;

use super::protocol::{
    DaemonEventCursor, DaemonReplayInfo, DaemonReplayStatus, DaemonSessionEvent,
    DaemonSessionEventKind,
};

const MAX_REPLAY_EVENTS_PER_SESSION: usize = 256;

pub(super) struct ReplayJournal {
    sessions: BTreeMap<String, SessionReplayJournal>,
}

impl Default for ReplayJournal {
    fn default() -> Self {
        Self::new()
    }
}

struct SessionReplayJournal {
    generation: String,
    sequence: u64,
    events: VecDeque<StoredSessionEvent>,
}

struct StoredSessionEvent {
    sequence: u64,
    emitted_at_ms: u64,
    event: DaemonSessionEventKind,
}

impl ReplayJournal {
    pub(super) fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
        }
    }

    pub(super) fn retain_sessions(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.sessions.retain(|session_id, _| keep(session_id));
    }

    pub(super) fn record(
        &mut self,
        session_id: &str,
        event: DaemonSessionEventKind,
        emitted_at_ms: u64,
    ) -> DaemonEventCursor {
        let journal = self.session_mut(session_id);
        if journal.sequence == u64::MAX {
            journal.generation = Uuid::new_v4().to_string();
            journal.sequence = 0;
            journal.events.clear();
        }
        journal.sequence += 1;
        let cursor = DaemonEventCursor {
            generation: journal.generation.clone(),
            sequence: journal.sequence,
        };
        journal.events.push_back(StoredSessionEvent {
            sequence: cursor.sequence,
            emitted_at_ms,
            event,
        });
        while journal.events.len() > MAX_REPLAY_EVENTS_PER_SESSION {
            journal.events.pop_front();
        }
        cursor
    }

    pub(super) fn attach(
        &mut self,
        session_id: &str,
        resume_cursor: Option<DaemonEventCursor>,
    ) -> (DaemonEventCursor, DaemonReplayInfo) {
        let journal = self.session_mut(session_id);
        let to_cursor = journal.cursor();
        let Some(from_cursor) = resume_cursor else {
            return (
                to_cursor.clone(),
                DaemonReplayInfo {
                    status: DaemonReplayStatus::Complete,
                    to_cursor,
                    ..DaemonReplayInfo::default()
                },
            );
        };
        let replay = journal.replay(from_cursor, to_cursor.clone());
        (to_cursor, replay)
    }

    pub(super) fn cursor(&mut self, session_id: &str) -> DaemonEventCursor {
        self.session_mut(session_id).cursor()
    }

    fn session_mut(&mut self, session_id: &str) -> &mut SessionReplayJournal {
        self.sessions
            .entry(session_id.into())
            .or_insert_with(|| SessionReplayJournal {
                generation: Uuid::new_v4().to_string(),
                sequence: 0,
                events: VecDeque::with_capacity(MAX_REPLAY_EVENTS_PER_SESSION),
            })
    }
}

impl SessionReplayJournal {
    fn cursor(&self) -> DaemonEventCursor {
        DaemonEventCursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }

    fn replay(
        &self,
        from_cursor: DaemonEventCursor,
        to_cursor: DaemonEventCursor,
    ) -> DaemonReplayInfo {
        if from_cursor.generation != self.generation {
            return unavailable_replay(from_cursor, to_cursor, "event_generation_changed");
        }
        if from_cursor.sequence > self.sequence {
            return unavailable_replay(from_cursor, to_cursor, "resume_cursor_ahead_of_session");
        }
        if from_cursor.sequence == self.sequence {
            return DaemonReplayInfo {
                status: DaemonReplayStatus::Complete,
                from_cursor: Some(from_cursor),
                to_cursor,
                ..DaemonReplayInfo::default()
            };
        }

        let oldest_sequence = self
            .events
            .front()
            .map_or(self.sequence.saturating_add(1), |event| event.sequence);
        let truncated = from_cursor.sequence.saturating_add(1) < oldest_sequence;
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence > from_cursor.sequence)
            .map(|event| DaemonSessionEvent {
                cursor: DaemonEventCursor {
                    generation: self.generation.clone(),
                    sequence: event.sequence,
                },
                emitted_at_ms: event.emitted_at_ms,
                event: event.event,
            })
            .collect();
        DaemonReplayInfo {
            status: if truncated {
                DaemonReplayStatus::Partial
            } else {
                DaemonReplayStatus::Complete
            },
            from_cursor: Some(from_cursor),
            to_cursor,
            events,
            reason: truncated.then(|| "event_history_truncated".into()),
            resync_required: truncated,
        }
    }
}

fn unavailable_replay(
    from_cursor: DaemonEventCursor,
    to_cursor: DaemonEventCursor,
    reason: &str,
) -> DaemonReplayInfo {
    DaemonReplayInfo {
        status: DaemonReplayStatus::Unavailable,
        from_cursor: Some(from_cursor),
        to_cursor,
        events: Vec::new(),
        reason: Some(reason.into()),
        resync_required: true,
    }
}
