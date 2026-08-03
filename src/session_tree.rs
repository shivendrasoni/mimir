//! Pure, bounded session-tree and session-derivation primitives.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::{MimirError, Result},
    model::{Content, Message, Role},
    session::{SESSION_SCHEMA_VERSION, SessionPayload, SessionRecord},
};

/// Maximum number of records accepted by one in-memory branch catalog.
pub const MAX_SESSION_TREE_RECORDS: usize = 100_000;
/// Maximum encoded bytes accepted by the pure JSONL parser.
pub const MAX_SESSION_TREE_JSONL_BYTES: usize = 64 * 1024 * 1024;
/// Maximum encoded bytes accepted for one JSONL record.
pub const MAX_SESSION_TREE_RECORD_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of characters exposed in a TUI branch selector.
pub const MAX_BRANCH_SELECTOR_CHARS: usize = 512;
/// Maximum number of characters accepted in a persisted session name.
pub const MAX_SESSION_NAME_CHARS: usize = 256;
/// Maximum messages copied into one native branch-summary source payload.
pub const MAX_BRANCH_SUMMARY_SOURCE_MESSAGES: usize = 16_384;
/// Maximum encoded message bytes copied into one branch-summary source payload.
pub const MAX_BRANCH_SUMMARY_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Display-relevant kind of one native session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionNodeKind {
    /// A conversation message with its role.
    Message { role: Role },
    /// A compaction checkpoint.
    Compaction,
    /// A non-message runtime event.
    RuntimeEvent { name: String },
}

/// One stable, flat tree node suitable for a non-recursive TUI renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTreeNode {
    pub record_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub kind: SessionNodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub children: Vec<Uuid>,
}

/// A bounded user-message choice for the `/fork` selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessageBranchSelector {
    pub entry_id: Uuid,
    pub text: String,
    pub truncated: bool,
}

/// Pure output of a fork or clone operation, ready for durable persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionDerivation {
    pub records: Vec<SessionRecord>,
    pub selected_text: Option<String>,
}

/// Parent-aware catalog over native Rust session records.
///
/// Older native transcripts did not populate `parent_id`. When every parent is
/// absent, append order is interpreted as a single branch. Once any explicit
/// parent exists, null parents retain their reference-format root semantics,
/// except for the native tail following a core-authored lineage/name anchor.
#[derive(Debug, Clone)]
pub struct SessionBranchCatalog {
    records: Vec<SessionRecord>,
    nodes: Vec<SessionTreeNode>,
    node_indexes: HashMap<Uuid, usize>,
    roots: Vec<Uuid>,
    leaf_id: Option<Uuid>,
}

impl SessionBranchCatalog {
    /// Parses strict native JSONL and constructs a bounded catalog.
    ///
    /// Empty physical lines are ignored. Malformed records, unsupported schema
    /// versions, duplicate IDs, and cycles fail closed.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the input violates the session contract.
    pub fn from_jsonl(jsonl: &str) -> Result<Self> {
        if jsonl.len() > MAX_SESSION_TREE_JSONL_BYTES {
            return Err(protocol_error(
                "session JSONL exceeds the 64 MiB catalog limit",
            ));
        }
        let mut records = Vec::new();
        for (line_index, line) in jsonl.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if line.len() > MAX_SESSION_TREE_RECORD_BYTES {
                return Err(protocol_error(format!(
                    "session record at line {} exceeds the 4 MiB limit",
                    line_index + 1
                )));
            }
            if records.len() == MAX_SESSION_TREE_RECORDS {
                return Err(protocol_error(
                    "session tree exceeds the 100000 record limit",
                ));
            }
            let record = serde_json::from_str(line).map_err(|error| {
                protocol_error(format!(
                    "invalid session JSON at line {}: {error}",
                    line_index + 1
                ))
            })?;
            records.push(record);
        }
        Self::from_records(records)
    }

    /// Constructs a parent-aware catalog from already decoded native records.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an oversized, unsupported, duplicate, or
    /// cyclic record graph.
    pub fn from_records(records: Vec<SessionRecord>) -> Result<Self> {
        validate_records(&records)?;
        let effective_parents = effective_parents(&records);
        validate_acyclic(&records, &effective_parents)?;

        let mut nodes = Vec::with_capacity(records.len());
        let mut node_indexes = HashMap::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            node_indexes.insert(record.record_id, index);
            nodes.push(node_from_record(record, effective_parents[index]));
        }
        populate_children(&mut nodes, &node_indexes);
        let mut roots = nodes
            .iter()
            .filter(|node| node.parent_id.is_none())
            .map(|node| node.record_id)
            .collect::<Vec<_>>();
        sort_ids(&mut roots, &nodes, &node_indexes);
        let leaf_id = records.last().map(|record| record.record_id);

        Ok(Self {
            records,
            nodes,
            node_indexes,
            roots,
            leaf_id,
        })
    }

    /// Returns stable flat nodes in append order.
    #[must_use]
    pub fn nodes(&self) -> &[SessionTreeNode] {
        &self.nodes
    }

    /// Returns stable root IDs ordered by timestamp and ID.
    #[must_use]
    pub fn roots(&self) -> &[Uuid] {
        &self.roots
    }

    /// Returns the last appended record, matching the native active-leaf rule.
    #[must_use]
    pub const fn leaf_id(&self) -> Option<Uuid> {
        self.leaf_id
    }

    /// Looks up one stable flat node.
    #[must_use]
    pub fn node(&self, record_id: Uuid) -> Option<&SessionTreeNode> {
        self.node_indexes
            .get(&record_id)
            .and_then(|index| self.nodes.get(*index))
    }

    /// Lists all user messages in append order for a bounded fork selector.
    #[must_use]
    pub fn user_message_selectors(&self) -> Vec<UserMessageBranchSelector> {
        self.records
            .iter()
            .filter_map(|record| {
                let SessionPayload::Message(message) = &record.payload else {
                    return None;
                };
                if message.role != Role::User {
                    return None;
                }
                let (text, truncated) = message_text_preview(message, MAX_BRANCH_SELECTOR_CHARS);
                if text.is_empty() {
                    return None;
                }
                Some(UserMessageBranchSelector {
                    entry_id: record.record_id,
                    text,
                    truncated,
                })
            })
            .collect()
    }

    /// Returns normalized records on the root-to-entry ancestor path.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the entry does not exist.
    pub fn branch_records(&self, leaf_id: Uuid) -> Result<Vec<SessionRecord>> {
        let mut indexes = Vec::new();
        let mut current = Some(leaf_id);
        while let Some(record_id) = current {
            let index = *self
                .node_indexes
                .get(&record_id)
                .ok_or_else(|| protocol_error(format!("entry {record_id} was not found")))?;
            indexes.push(index);
            current = self.nodes[index].parent_id;
        }
        indexes.reverse();
        Ok(indexes
            .into_iter()
            .map(|index| {
                let mut record = self.records[index].clone();
                record.parent_id = self.nodes[index].parent_id;
                record
            })
            .collect())
    }

    /// Returns the exact chronological message slice abandoned when moving
    /// from the current leaf to `target_id`. The deepest common ancestor is
    /// excluded; disconnected trees include the complete current-root branch.
    ///
    /// The result fails closed rather than returning an approximate slice when
    /// its fixed message-count or encoded-byte budget is exceeded.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown target or a source slice above
    /// the native branch-summary payload limits.
    pub fn abandoned_branch_messages(&self, target_id: Uuid) -> Result<Vec<Message>> {
        if !self.node_indexes.contains_key(&target_id) {
            return Err(protocol_error(format!("entry {target_id} was not found")));
        }
        let Some(old_leaf_id) = self.leaf_id else {
            return Ok(Vec::new());
        };

        let mut old_path = HashSet::new();
        let mut current = Some(old_leaf_id);
        while let Some(record_id) = current {
            old_path.insert(record_id);
            let index = self.node_indexes[&record_id];
            current = self.nodes[index].parent_id;
        }

        let mut common_ancestor_id = None;
        current = Some(target_id);
        while let Some(record_id) = current {
            if old_path.contains(&record_id) {
                common_ancestor_id = Some(record_id);
                break;
            }
            let index = self.node_indexes[&record_id];
            current = self.nodes[index].parent_id;
        }

        let mut messages = Vec::new();
        let mut encoded_bytes = 0_usize;
        current = Some(old_leaf_id);
        while let Some(record_id) = current {
            if Some(record_id) == common_ancestor_id {
                break;
            }
            let index = self.node_indexes[&record_id];
            if let SessionPayload::Message(message) = &self.records[index].payload {
                if messages.len() == MAX_BRANCH_SUMMARY_SOURCE_MESSAGES {
                    return Err(protocol_error(format!(
                        "abandoned branch exceeds the {MAX_BRANCH_SUMMARY_SOURCE_MESSAGES} message limit"
                    )));
                }
                let message_bytes = serde_json::to_vec(message)?.len();
                encoded_bytes = encoded_bytes.checked_add(message_bytes).ok_or_else(|| {
                    protocol_error("abandoned branch message byte count overflowed")
                })?;
                if encoded_bytes > MAX_BRANCH_SUMMARY_SOURCE_BYTES {
                    return Err(protocol_error(format!(
                        "abandoned branch exceeds the {MAX_BRANCH_SUMMARY_SOURCE_BYTES}-byte message limit"
                    )));
                }
                messages.push(message.clone());
            }
            current = self.nodes[index].parent_id;
        }
        messages.reverse();
        Ok(messages)
    }

    /// Creates a fork plan immediately before a selected user message.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown/non-user entry or invalid source ID.
    pub fn fork_before_user_message(
        &self,
        entry_id: Uuid,
        source_session_id: &str,
    ) -> Result<SessionDerivation> {
        validate_source_session_id(source_session_id)?;
        let index = *self
            .node_indexes
            .get(&entry_id)
            .ok_or_else(|| protocol_error("entryId was not found"))?;
        let selected_text = match &self.records[index].payload {
            SessionPayload::Message(message) if message.role == Role::User => message.text(),
            _ => return Err(protocol_error("entryId must identify a user message")),
        };
        let parent_id = self.nodes[index].parent_id;
        let mut records = match parent_id {
            Some(parent_id) => self.branch_records(parent_id)?,
            None => Vec::new(),
        };
        append_lineage(
            &mut records,
            "session_forked_from",
            format!("{source_session_id}:{entry_id}"),
        );
        Ok(SessionDerivation {
            records,
            selected_text: Some(selected_text),
        })
    }

    /// Creates a clone plan containing only the selected active branch.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown entry or invalid source ID.
    pub fn clone_at(&self, leaf_id: Uuid, source_session_id: &str) -> Result<SessionDerivation> {
        validate_source_session_id(source_session_id)?;
        let mut records = self.branch_records(leaf_id)?;
        append_lineage(
            &mut records,
            "session_cloned_from",
            source_session_id.to_owned(),
        );
        Ok(SessionDerivation {
            records,
            selected_text: None,
        })
    }

    /// Creates a clone plan at the current native leaf.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when the catalog is empty or the source ID is invalid.
    pub fn clone_active(&self, source_session_id: &str) -> Result<SessionDerivation> {
        let leaf_id = self
            .leaf_id
            .ok_or_else(|| protocol_error("session has no active leaf to clone"))?;
        self.clone_at(leaf_id, source_session_id)
    }

    /// Creates an append-only session-name record at the current leaf.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for blank, control-containing, or oversized names.
    pub fn rename_record(&self, name: &str) -> Result<SessionRecord> {
        let name = name.trim();
        if name.is_empty() {
            return Err(MimirError::Configuration(
                "session name must not be blank".into(),
            ));
        }
        if name.chars().count() > MAX_SESSION_NAME_CHARS {
            return Err(MimirError::Configuration(format!(
                "session name exceeds the {MAX_SESSION_NAME_CHARS} character limit"
            )));
        }
        if name.chars().any(char::is_control) {
            return Err(MimirError::Configuration(
                "session name must not contain control characters".into(),
            ));
        }
        let mut record = SessionRecord::new(SessionPayload::RuntimeEvent {
            name: "session_name".into(),
            detail: name.into(),
        });
        record.parent_id = self.leaf_id;
        Ok(record)
    }

    /// Returns the latest append-only session name, if present.
    #[must_use]
    pub fn session_name(&self) -> Option<&str> {
        for record in self.records.iter().rev() {
            if let SessionPayload::RuntimeEvent { name, detail } = &record.payload
                && name == "session_name"
            {
                let name = detail.trim();
                return (!name.is_empty()).then_some(name);
            }
        }
        None
    }
}

fn validate_records(records: &[SessionRecord]) -> Result<()> {
    if records.len() > MAX_SESSION_TREE_RECORDS {
        return Err(protocol_error(
            "session tree exceeds the 100000 record limit",
        ));
    }
    let mut ids = HashSet::with_capacity(records.len());
    for record in records {
        if record.schema_version != SESSION_SCHEMA_VERSION {
            return Err(protocol_error(format!(
                "unsupported session schema version {}",
                record.schema_version
            )));
        }
        if !ids.insert(record.record_id) {
            return Err(protocol_error(format!(
                "duplicate session record id {}",
                record.record_id
            )));
        }
    }
    Ok(())
}

fn effective_parents(records: &[SessionRecord]) -> Vec<Option<Uuid>> {
    let known_ids = records
        .iter()
        .map(|record| record.record_id)
        .collect::<HashSet<_>>();
    let has_explicit_parent = records.iter().any(|record| record.parent_id.is_some());
    let mut native_tail = !has_explicit_parent;
    let mut parents = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let native_anchor = is_native_tail_anchor(record);
        let explicit_parent = record
            .parent_id
            .filter(|parent| *parent != record.record_id && known_ids.contains(parent));
        let inferred_parent = if (native_tail || native_anchor) && index > 0 {
            Some(records[index - 1].record_id)
        } else {
            None
        };
        parents.push(explicit_parent.or(inferred_parent));
        if native_anchor {
            native_tail = true;
        }
    }
    parents
}

fn is_native_tail_anchor(record: &SessionRecord) -> bool {
    matches!(
        &record.payload,
        SessionPayload::RuntimeEvent { name, .. }
            if matches!(
                name.as_str(),
                "session_forked_from" | "session_cloned_from" | "session_name"
            )
    )
}

fn validate_acyclic(records: &[SessionRecord], parents: &[Option<Uuid>]) -> Result<()> {
    let indexes = records
        .iter()
        .enumerate()
        .map(|(index, record)| (record.record_id, index))
        .collect::<HashMap<_, _>>();
    // 0 = unseen, 1 = on the current path, 2 = fully validated. This keeps
    // validation linear and avoids recursion on very deep transcripts.
    let mut states = vec![0_u8; records.len()];
    for start in 0..records.len() {
        if states[start] == 2 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(index) = current {
            match states[index] {
                0 => {
                    states[index] = 1;
                    path.push(index);
                    current = parents[index].and_then(|parent| indexes.get(&parent).copied());
                }
                1 => {
                    return Err(protocol_error(
                        "session record parent graph contains a cycle",
                    ));
                }
                _ => break,
            }
        }
        for index in path {
            states[index] = 2;
        }
    }
    Ok(())
}

fn node_from_record(record: &SessionRecord, parent_id: Option<Uuid>) -> SessionTreeNode {
    let (kind, preview) = match &record.payload {
        SessionPayload::Message(message) => {
            let (text, _) = message_text_preview(message, MAX_BRANCH_SELECTOR_CHARS);
            let preview = (!text.is_empty()).then_some(text);
            (SessionNodeKind::Message { role: message.role }, preview)
        }
        SessionPayload::Compaction { summary, .. } => (
            SessionNodeKind::Compaction,
            Some(truncate_chars(summary, MAX_BRANCH_SELECTOR_CHARS).0),
        ),
        SessionPayload::RuntimeEvent { name, detail } => (
            SessionNodeKind::RuntimeEvent { name: name.clone() },
            (!detail.is_empty()).then(|| truncate_chars(detail, MAX_BRANCH_SELECTOR_CHARS).0),
        ),
    };
    SessionTreeNode {
        record_id: record.record_id,
        parent_id,
        created_at: record.created_at,
        kind,
        preview,
        children: Vec::new(),
    }
}

fn populate_children(nodes: &mut [SessionTreeNode], indexes: &HashMap<Uuid, usize>) {
    let relationships = nodes
        .iter()
        .filter_map(|node| node.parent_id.map(|parent| (parent, node.record_id)))
        .collect::<Vec<_>>();
    for (parent, child) in relationships {
        if let Some(index) = indexes.get(&parent) {
            nodes[*index].children.push(child);
        }
    }
    for index in 0..nodes.len() {
        let mut children = std::mem::take(&mut nodes[index].children);
        sort_ids(&mut children, nodes, indexes);
        nodes[index].children = children;
    }
}

fn sort_ids(ids: &mut [Uuid], nodes: &[SessionTreeNode], indexes: &HashMap<Uuid, usize>) {
    ids.sort_unstable_by_key(|id| {
        let node = &nodes[indexes[id]];
        (node.created_at, node.record_id)
    });
}

fn append_lineage(records: &mut Vec<SessionRecord>, name: &str, detail: String) {
    let parent_id = records.last().map(|record| record.record_id);
    let mut lineage = SessionRecord::new(SessionPayload::RuntimeEvent {
        name: name.into(),
        detail,
    });
    lineage.parent_id = parent_id;
    records.push(lineage);
}

fn validate_source_session_id(source_session_id: &str) -> Result<()> {
    if source_session_id.trim() != source_session_id
        || source_session_id.is_empty()
        || source_session_id.len() > 1024
        || source_session_id.chars().any(char::is_control)
    {
        return Err(protocol_error("source session id is invalid"));
    }
    Ok(())
}

fn message_text_preview(message: &Message, limit: usize) -> (String, bool) {
    let mut preview = String::new();
    let mut count = 0_usize;
    for (part_index, text) in message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .enumerate()
    {
        if part_index > 0 {
            if count == limit {
                return (preview, true);
            }
            preview.push('\n');
            count += 1;
        }
        for character in text.chars() {
            if count == limit {
                return (preview, true);
            }
            preview.push(character);
            count += 1;
        }
    }
    (preview, false)
}

fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = chars.clone().nth(limit).is_some();
    (chars.by_ref().take(limit).collect(), truncated)
}

fn protocol_error(message: impl Into<String>) -> MimirError {
    MimirError::Protocol(message.into())
}
