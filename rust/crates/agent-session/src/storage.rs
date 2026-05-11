use std::collections::BTreeMap;

use agent_core::TranscriptItem;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTranscriptEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u64,
    pub item: TranscriptItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    pub session_id: String,
    pub active_leaf_id: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub entries: Vec<StoredTranscriptEntry>,
}

impl StoredSession {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            active_leaf_id: None,
            metadata: BTreeMap::new(),
            entries: Vec::new(),
        }
    }
}
