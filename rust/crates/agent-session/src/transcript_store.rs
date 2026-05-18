use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model_context::{ModelContext, ModelContextEntry};
use crate::storage::StoredTranscriptEntry;
use agent_vocab::{ProviderReplayItem, TranscriptItem};
use uuid::Uuid;

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
    pub provider_replay: Vec<ProviderReplayItem>,
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
    /// with `active_leaf_id()`.
    pub fn entries(&self) -> Vec<TranscriptStorageNode> {
        self.insertion_order
            .iter()
            .filter_map(|id| self.entries_by_id.get(id).cloned())
            .collect()
    }

    pub fn all_leaf_ids(&self) -> impl Iterator<Item = &str> {
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

    pub fn active_leaf_id(&self) -> Option<&str> {
        self.active_leaf_id.as_deref()
    }

    pub fn contains_entry(&self, id: &str) -> bool {
        self.entries_by_id.contains_key(id)
    }

    pub fn is_turn_boundary(&self) -> bool {
        self.is_turn_boundary_at(self.active_leaf_id())
    }

    /// True when `leaf_id` points at a turn boundary: a finished turn, a
    /// compacted root, or the empty-log sentinel.
    pub fn is_turn_boundary_at<'a>(&'a self, entry_id: Option<&'a str>) -> bool {
        let Some(id) = entry_id else {
            return true;
        };
        let Some(entry) = self.get_entry(id) else {
            return false;
        };
        match &entry.item {
            TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_) => true,
            _ => false,
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

    pub fn set_active_leaf_to_boundary(
        &mut self,
        entry_id: &str,
    ) -> Result<(), TranscriptStoreError> {
        if !self.contains_entry(entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        if !self.is_turn_boundary_at(Some(entry_id)) {
            return Err(TranscriptStoreError::NotTurnBoundary);
        }
        self.active_leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn set_active_leaf_to_entry(&mut self, entry_id: &str) -> Result<(), TranscriptStoreError> {
        if !self.contains_entry(entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        self.active_leaf_id = Some(entry_id.to_string());
        Ok(())
    }

    pub fn reset_active_leaf(&mut self) {
        self.active_leaf_id = None;
    }

    fn active_path_entries(&self) -> Vec<TranscriptStorageNode> {
        self.path_entries(self.active_leaf_id.as_deref())
    }

    pub fn path_entries_to(&self, entry_id: &str) -> Vec<TranscriptStorageNode> {
        self.path_entries(Some(entry_id))
    }

    fn path_entries(&self, entry_id: Option<&str>) -> Vec<TranscriptStorageNode> {
        let mut path = Vec::new();
        let mut current = entry_id.and_then(|id| self.entries_by_id.get(id));

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

    pub fn copy_path_to_entry(&self, entry_id: &str) -> Result<Self, TranscriptStoreError> {
        if !self.contains_entry(entry_id) {
            return Err(TranscriptStoreError::EntryNotFound);
        }
        Ok(Self::from_trusted_entries(
            self.path_entries_to(entry_id),
            Some(entry_id.to_string()),
        ))
    }

    /// Materialize the active branch into a `ModelContext`.
    ///
    /// Materialize the full active path in model-visible order.
    pub fn model_context(&self) -> ModelContext {
        let path = self.active_path_entries();
        ModelContext::from_entries(
            path.into_iter()
                .map(|entry| ModelContextEntry {
                    item: entry.item,
                    provider_replay: entry.provider_replay,
                })
                .collect(),
        )
    }

    pub fn append_root_item(
        &mut self,
        item: TranscriptItem,
        provider_replay: Vec<ProviderReplayItem>,
    ) -> String {
        let entry = TranscriptStorageNode {
            id: Uuid::new_v4().to_string(),
            parent_id: None,
            timestamp_ms: now_ms(),
            item,
            provider_replay,
        };
        self.append_entry(entry)
    }

    pub(crate) fn append_transcript_item(&mut self, item: TranscriptItem) -> String {
        self.append_item(item, Vec::new())
    }

    pub(crate) fn append_item(
        &mut self,
        item: TranscriptItem,
        provider_replay: Vec<ProviderReplayItem>,
    ) -> String {
        let entry = TranscriptStorageNode {
            id: Uuid::new_v4().to_string(),
            parent_id: self.active_leaf_id.clone(),
            timestamp_ms: now_ms(),
            item,
            provider_replay,
        };
        self.append_entry(entry)
    }

    pub fn append_storage_node(&mut self, entry: TranscriptStorageNode) -> String {
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
            let id = entry.id.clone();
            let parent_id = entry.parent_id.clone();
            ctx.parent_by_id.insert(id.clone(), parent_id.clone());
            ctx.children_by_parent
                .entry(parent_id)
                .or_default()
                .push(id.clone());
            ctx.leaf_ids.insert(id.clone());
            ctx.insertion_order.push(id.clone());
            ctx.entries_by_id.insert(id, entry);
        }
        for parent_id in ctx.parent_by_id.values().flatten() {
            ctx.leaf_ids.remove(parent_id);
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
            provider_replay: value.provider_replay,
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
            provider_replay: value.provider_replay,
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
    use agent_vocab::{
        AssistantItem, AssistantMessage, CompactionSummary, TurnId, TurnOutcome, UserMessage,
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

        ctx.set_active_leaf_to_boundary(&first_ids[3])
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

        ctx.set_active_leaf_to_boundary(&first_ids[3])
            .expect("T1 boundary is a valid fork point");
        let alternate_second_ids = ctx.append_transcript_items(turn(3, "alternate", "done"));

        let children = ctx.child_ids(Some(&first_ids[3]));
        assert!(children.contains(&original_second_ids[0].as_str()));
        assert!(children.contains(&alternate_second_ids[0].as_str()));

        let leaves = ctx.all_leaf_ids().collect::<Vec<_>>();
        assert!(leaves.contains(&original_second_ids[3].as_str()));
        assert!(leaves.contains(&alternate_second_ids[3].as_str()));
        assert_eq!(
            ctx.parent_id(&alternate_second_ids[0]),
            Some(Some(first_ids[3].as_str()))
        );
    }

    #[test]
    fn restore_rebuilds_indexes_without_parent_first_order() {
        let parent = TranscriptStorageNode {
            id: "parent".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("parent")),
            provider_replay: Vec::new(),
        };
        let child = TranscriptStorageNode {
            id: "child".to_string(),
            parent_id: Some("parent".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("child")),
            provider_replay: Vec::new(),
        };

        let store =
            TranscriptStore::from_storage_entries(vec![child, parent], Some("child".to_string()))
                .expect("restore should not depend on parent-first order");

        assert_eq!(
            store
                .active_path_entries()
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec!["parent", "child"]
        );
        assert_eq!(store.child_ids(None), vec!["parent"]);
        assert_eq!(store.child_ids(Some("parent")), vec!["child"]);
        assert_eq!(store.all_leaf_ids().collect::<Vec<_>>(), vec!["child"]);
    }

    #[test]
    fn compaction_summary_root_is_a_boundary() {
        let summary = TranscriptStorageNode {
            id: "summary".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::CompactionSummary(CompactionSummary::new(
                "session",
                "source",
                "summary text",
                Some(128),
                TurnId(4),
            )),
            provider_replay: Vec::new(),
        };

        let store =
            TranscriptStore::from_storage_entries(vec![summary], Some("summary".to_string()))
                .expect("compacted root should restore");

        assert!(store.is_turn_boundary());
        assert_eq!(store.model_context().last_turn_id(), TurnId(4));
    }

    #[test]
    fn branched_store_can_end_at_any_existing_entry() {
        let mut ctx = TranscriptStore::new();
        let ids = ctx.append_transcript_items(turn(1, "hi", "done"));
        let user_message_id = &ids[1];

        let forked = ctx
            .copy_path_to_entry(user_message_id)
            .expect("any existing transcript entry can be copied for fork");
        assert_eq!(forked.active_leaf_id(), Some(user_message_id.as_str()));
        assert!(!forked.is_turn_boundary());

        assert_eq!(
            ctx.copy_path_to_entry("missing-entry").err(),
            Some(TranscriptStoreError::EntryNotFound)
        );
    }
}
