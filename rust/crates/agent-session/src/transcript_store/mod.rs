use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::storage::StoredTranscriptEntry;
use agent_core::{InjectedMessage, TranscriptItem};
use uuid::Uuid;

use crate::model_context::ModelContext;

/// Durable transcript storage node holding one model-visible transcript item.
///
/// Entries form a forest: each entry has at most one parent, while a parent may
/// have many children. A session points at one leaf and materializes model
/// context by walking parents from that leaf back to a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptStorageNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u64,
    pub item: TranscriptItem,
}

/// Append-only transcript forest plus one active session leaf.
///
/// Each `TranscriptStorageNode` holds a single `TranscriptItem` plus a parent
/// pointer. The store keeps direct indexes by entry id, parent id, and current
/// leaves so future storage layers can discover sibling paths and
/// common ancestors quickly. The active leaf is the one path this session is
/// currently using.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranscriptStore {
    entries_by_id: HashMap<String, TranscriptStorageNode>,
    parent_by_id: HashMap<String, Option<String>>,
    children_by_parent: HashMap<Option<String>, Vec<String>>,
    leaf_ids: BTreeSet<String>,
    insertion_order: Vec<String>,
    active_leaf_id: Option<String>,
}

impl TranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_model_context(model_context: &ModelContext) -> Self {
        let mut ctx = Self::new();
        ctx.append_transcript_items(model_context.transcript_items().iter().cloned());
        ctx
    }

    /// Return all transcript entries in append order.
    ///
    /// This is an owned snapshot because the store indexes entries by id
    /// internally. Future persistence code can serialize this vector together
    /// with `leaf_id()`.
    pub fn entries(&self) -> Vec<TranscriptStorageNode> {
        self.insertion_order
            .iter()
            .filter_map(|id| self.entries_by_id.get(id).cloned())
            .collect()
    }

    pub fn entry_count(&self) -> usize {
        self.insertion_order.len()
    }

    pub fn leaf_ids(&self) -> impl Iterator<Item = &str> {
        self.leaf_ids.iter().map(String::as_str)
    }

    pub fn parent_id(&self, entry_id: &str) -> Option<Option<&str>> {
        self.parent_by_id
            .get(entry_id)
            .map(|parent_id| parent_id.as_deref())
    }

    pub fn child_ids(&self, parent_id: Option<&str>) -> Vec<&str> {
        let key = parent_id.map(str::to_string);
        self.children_by_parent
            .get(&key)
            .into_iter()
            .flat_map(|ids| ids.iter().map(String::as_str))
            .collect()
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.active_leaf_id.as_deref()
    }

    pub fn contains_entry(&self, id: &str) -> bool {
        self.entries_by_id.contains_key(id)
    }

    pub fn is_turn_boundary(&self) -> bool {
        self.is_turn_boundary_leaf(self.leaf_id())
    }

    /// True when `leaf_id` points at a turn boundary (either a
    /// `TurnFinished` entry directly, or the empty-log sentinel). Trailing
    /// injected entries are transparent: the check walks past them to find the
    /// underlying boundary. An injected turn opener still resolves to
    /// `TurnStarted`, so it is not a boundary.
    pub fn is_turn_boundary_leaf<'a>(&'a self, leaf_id: Option<&'a str>) -> bool {
        let mut cursor = leaf_id;
        loop {
            let Some(id) = cursor else {
                return true;
            };
            let Some(entry) = self.get_entry(id) else {
                return false;
            };
            match &entry.item {
                TranscriptItem::TurnFinished { .. } => return true,
                TranscriptItem::Injected(_) => {
                    cursor = entry.parent_id.as_deref();
                }
                _ => return false,
            }
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&TranscriptStorageNode> {
        self.entries_by_id.get(id)
    }

    pub fn append_transcript_items(
        &mut self,
        items: impl IntoIterator<Item = TranscriptItem>,
    ) -> Vec<String> {
        items
            .into_iter()
            .map(|item| self.append_transcript_item(item))
            .collect()
    }

    /// Append a `TranscriptItem::Injected(injected)` entry and return its id.
    pub fn append_injected(&mut self, injected: InjectedMessage) -> String {
        self.append_transcript_item(TranscriptItem::Injected(injected))
    }

    pub(crate) fn replace_active_path(&mut self, model_context: &ModelContext) {
        self.reset_leaf();
        self.append_transcript_items(model_context.transcript_items().iter().cloned());
    }

    pub fn branch_at_turn_boundary(&mut self, entry_id: &str) -> Result<(), TranscriptStoreError> {
        if !self.contains_entry(entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        if !self.is_turn_boundary_leaf(Some(entry_id)) {
            return Err(TranscriptStoreError::NotTurnBoundary);
        }
        self.active_leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn reset_leaf(&mut self) {
        self.active_leaf_id = None;
    }

    pub fn branch_entries(&self, leaf_id: Option<&str>) -> Vec<TranscriptStorageNode> {
        let mut path = Vec::new();
        let mut current = leaf_id
            .or(self.active_leaf_id.as_deref())
            .and_then(|id| self.entries_by_id.get(id));

        while let Some(entry) = current {
            path.push(entry.clone());
            current = entry
                .parent_id
                .as_deref()
                .and_then(|parent_id| self.entries_by_id.get(parent_id));
        }

        path.reverse();
        path
    }

    pub fn create_branched_store_at_turn_boundary(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Self, TranscriptStoreError> {
        if let Some(leaf_id) = leaf_id {
            if !self.contains_entry(leaf_id) {
                return Err(TranscriptStoreError::EntryNotFound);
            }
        }
        if !self.is_turn_boundary_leaf(leaf_id) {
            return Err(TranscriptStoreError::NotTurnBoundary);
        }

        match leaf_id {
            Some(leaf_id) => Ok(Self::from_trusted_entries(
                self.branch_entries(Some(leaf_id)),
                Some(leaf_id.to_string()),
            )),
            None => Ok(Self::new()),
        }
    }

    pub fn create_branched_store_at_entry(
        &self,
        leaf_id: &str,
    ) -> Result<Self, TranscriptStoreError> {
        if !self.contains_entry(leaf_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        Ok(Self::from_trusted_entries(
            self.branch_entries(Some(leaf_id)),
            Some(leaf_id.to_string()),
        ))
    }

    /// Materialize the active branch into a `ModelContext`.
    ///
    /// Materialize the full active path in model-visible order.
    pub fn model_context(&self) -> ModelContext {
        let path = self.branch_entries(None);
        let items = path.into_iter().map(|entry| entry.item).collect();
        ModelContext::from_transcript_items(items)
    }

    pub(crate) fn append_transcript_item(&mut self, item: TranscriptItem) -> String {
        let entry = TranscriptStorageNode {
            id: Uuid::new_v4().to_string(),
            parent_id: self.active_leaf_id.clone(),
            timestamp_ms: now_ms(),
            item,
        };
        self.append_entry(entry)
    }

    fn append_entry(&mut self, entry: TranscriptStorageNode) -> String {
        let id = entry.id.clone();
        let parent_id = entry.parent_id.clone();
        self.parent_by_id.insert(id.clone(), parent_id.clone());
        self.children_by_parent
            .entry(parent_id.clone())
            .or_default()
            .push(id.clone());
        if let Some(parent_id) = parent_id {
            self.leaf_ids.remove(&parent_id);
        }
        self.leaf_ids.insert(id.clone());
        self.insertion_order.push(id.clone());
        self.active_leaf_id = Some(id.clone());
        self.entries_by_id.insert(id.clone(), entry);
        id
    }

    pub fn from_storage_entries(
        entries: Vec<TranscriptStorageNode>,
        active_leaf_id: Option<String>,
    ) -> Result<Self, TranscriptStoreError> {
        let mut ids = BTreeSet::new();
        for entry in &entries {
            if !ids.insert(entry.id.clone()) {
                return Err(TranscriptStoreError::DuplicateEntry);
            }
        }
        for entry in &entries {
            if let Some(parent_id) = &entry.parent_id {
                if !ids.contains(parent_id) {
                    return Err(TranscriptStoreError::MissingParent);
                }
            }
        }
        if let Some(active_leaf_id) = &active_leaf_id {
            if !ids.contains(active_leaf_id) {
                return Err(TranscriptStoreError::EntryNotFound);
            }
        }
        Ok(Self::from_trusted_entries(entries, active_leaf_id))
    }

    fn from_trusted_entries(
        entries: Vec<TranscriptStorageNode>,
        active_leaf_id: Option<String>,
    ) -> Self {
        let mut ctx = Self::new();
        for entry in entries {
            ctx.append_entry(entry);
        }
        ctx.active_leaf_id = active_leaf_id;
        ctx
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStoreError {
    EntryNotFound,
    NotTurnBoundary,
    DuplicateEntry,
    MissingParent,
}

impl From<TranscriptStorageNode> for StoredTranscriptEntry {
    fn from(value: TranscriptStorageNode) -> Self {
        Self {
            id: value.id,
            parent_id: value.parent_id,
            timestamp_ms: value.timestamp_ms,
            item: value.item,
        }
    }
}

impl From<StoredTranscriptEntry> for TranscriptStorageNode {
    fn from(value: StoredTranscriptEntry) -> Self {
        Self {
            id: value.id,
            parent_id: value.parent_id,
            timestamp_ms: value.timestamp_ms,
            item: value.item,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        AssistantItem, AssistantMessage, InjectedMessage, TurnId, TurnOutcome, UserMessage,
    };

    fn turn(turn_id: u64, user: &str, assistant: &str) -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::TurnStarted {
                turn_id: TurnId(turn_id),
            },
            TranscriptItem::UserMessage(UserMessage::text(user)),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(assistant.to_string())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(turn_id),
                outcome: TurnOutcome::Graceful,
            },
        ]
    }

    #[test]
    fn store_tracks_a_branch_path_from_the_active_leaf() {
        let mut ctx = TranscriptStore::new();
        let first_ids = ctx.append_transcript_items(turn(1, "first", "done"));
        ctx.append_transcript_items(turn(2, "second", "done"));

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("turn one should be a valid branch point");
        ctx.append_transcript_items(turn(3, "alternate", "done"));

        let transcript = ctx.model_context();
        assert_eq!(transcript.last_turn_id(), TurnId(3));
        assert_eq!(
            transcript.transcript_items()[1],
            TranscriptItem::UserMessage(UserMessage::text("first"))
        );
        assert_eq!(
            transcript.transcript_items()[5],
            TranscriptItem::UserMessage(UserMessage::text("alternate"))
        );
    }

    #[test]
    fn store_indexes_children_and_leaves_for_alternate_paths() {
        let mut ctx = TranscriptStore::new();
        let first_ids = ctx.append_transcript_items(turn(1, "first", "done"));
        let original_second_ids = ctx.append_transcript_items(turn(2, "second", "done"));

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        let alternate_second_ids = ctx.append_transcript_items(turn(3, "alternate", "done"));

        let children = ctx.child_ids(Some(&first_ids[3]));
        assert!(children.contains(&original_second_ids[0].as_str()));
        assert!(children.contains(&alternate_second_ids[0].as_str()));

        let leaves = ctx.leaf_ids().collect::<Vec<_>>();
        assert!(leaves.contains(&original_second_ids[3].as_str()));
        assert!(leaves.contains(&alternate_second_ids[3].as_str()));
        assert_eq!(
            ctx.parent_id(&alternate_second_ids[0]),
            Some(Some(first_ids[3].as_str()))
        );
    }

    #[test]
    fn transcript_materializes_the_full_active_branch_after_a_summary() {
        // Simulate a replacement branch manually at the store level: append two
        // turns, navigate back to the T1 boundary, append a summary there, then
        // re-append T2's items as descendants. The active branch is now
        // [T1 items..., summary, T2 items...], and the materialized view is
        // that full active path.
        let mut ctx = TranscriptStore::new();
        let first_ids = ctx.append_transcript_items(turn(1, "first", "done"));
        let second_ids = ctx.append_transcript_items(turn(2, "second", "done"));
        let kept_items = second_ids
            .iter()
            .map(|id| ctx.get_entry(id).expect("kept id exists").item.clone())
            .collect::<Vec<_>>();

        ctx.branch_at_turn_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        ctx.append_injected(InjectedMessage::new("compaction", "summary"));
        ctx.append_transcript_items(kept_items);

        let transcript = ctx.model_context();
        assert_eq!(transcript.last_turn_id(), TurnId(2));
        assert!(matches!(
            transcript.transcript_items()[4],
            TranscriptItem::Injected(_)
        ));
        assert!(matches!(
            transcript.transcript_items()[5],
            TranscriptItem::TurnStarted { turn_id: TurnId(2) }
        ));
        assert_eq!(transcript.transcript_items().len(), 9);
        assert!(ctx.is_turn_boundary());
    }

    #[test]
    fn fork_at_injected_tail_is_a_valid_turn_boundary() {
        let mut ctx = TranscriptStore::new();
        ctx.append_transcript_items(turn(1, "hi", "done"));
        let injected_id = ctx.append_injected(InjectedMessage::new("note", "note"));

        assert!(ctx.is_turn_boundary());
        let forked = ctx
            .create_branched_store_at_turn_boundary(Some(&injected_id))
            .expect("injected tail should be a valid fork boundary");
        assert_eq!(forked.leaf_id(), Some(injected_id.as_str()));
    }

    #[test]
    fn branched_store_can_end_at_any_existing_entry() {
        let mut ctx = TranscriptStore::new();
        let ids = ctx.append_transcript_items(turn(1, "hi", "done"));
        let user_message_id = &ids[1];

        let forked = ctx
            .create_branched_store_at_entry(user_message_id)
            .expect("any existing transcript entry can be copied for fork");
        assert_eq!(forked.leaf_id(), Some(user_message_id.as_str()));
        assert!(!forked.is_turn_boundary());

        assert_eq!(
            ctx.create_branched_store_at_entry("missing-entry").err(),
            Some(TranscriptStoreError::EntryNotFound)
        );
    }
}
