use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{MimirError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: Uuid,
    pub sender: String,
    pub receiver: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Default)]
pub struct AgentMessageBus {
    mailboxes: Mutex<BTreeMap<String, Vec<AgentMessage>>>,
}

impl AgentMessageBus {
    /// Queues a message for one named agent.
    ///
    /// # Errors
    ///
    /// Returns a validation error when a name or body is blank.
    pub async fn send(&self, sender: &str, receiver: &str, body: &str) -> Result<AgentMessage> {
        if sender.trim().is_empty() || receiver.trim().is_empty() || body.trim().is_empty() {
            return Err(MimirError::Protocol(
                "agent message sender, receiver, and body must not be blank".into(),
            ));
        }
        let message = AgentMessage {
            id: Uuid::new_v4(),
            sender: sender.trim().into(),
            receiver: receiver.trim().into(),
            body: body.trim().into(),
            created_at: Utc::now(),
        };
        self.mailboxes
            .lock()
            .await
            .entry(message.receiver.clone())
            .or_default()
            .push(message.clone());
        Ok(message)
    }

    pub async fn drain(&self, receiver: &str) -> Vec<AgentMessage> {
        self.mailboxes
            .lock()
            .await
            .remove(receiver)
            .unwrap_or_default()
    }
}
