#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agent_vocab::TranscriptItem;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid store record: {0}")]
    InvalidRecord(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load_session(&self, session_id: &str) -> StoreResult<Option<StoredSession>>;
    async fn write_session(&self, session: &StoredSession) -> StoreResult<()>;
    async fn append_entries(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()>;
    async fn set_active_leaf(
        &self,
        session_id: &str,
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()>;
}

#[derive(Debug, Clone)]
pub struct InMemorySessionStore {
    sessions: std::sync::Arc<std::sync::Mutex<BTreeMap<String, StoredSession>>>,
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self {
            sessions: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn load_session(&self, session_id: &str) -> StoreResult<Option<StoredSession>> {
        Ok(self
            .sessions
            .lock()
            .expect("in-memory store lock poisoned")
            .get(session_id)
            .cloned())
    }

    async fn write_session(&self, session: &StoredSession) -> StoreResult<()> {
        self.sessions
            .lock()
            .expect("in-memory store lock poisoned")
            .insert(session.session_id.clone(), session.clone());
        Ok(())
    }

    async fn append_entries(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()> {
        let mut sessions = self.sessions.lock().expect("in-memory store lock poisoned");
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.to_string()))?;
        session.entries.extend(entries.iter().cloned());
        session.active_leaf_id = active_leaf_id.map(str::to_string);
        Ok(())
    }

    async fn set_active_leaf(
        &self,
        session_id: &str,
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()> {
        let mut sessions = self.sessions.lock().expect("in-memory store lock poisoned");
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StoreError::SessionNotFound(session_id.to_string()))?;
        session.active_leaf_id = active_leaf_id.map(str::to_string);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct JsonlSessionStore {
    root: PathBuf,
}

impl JsonlSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.jsonl"))
    }

    fn ensure_root(&self) -> StoreResult<()> {
        fs::create_dir_all(&self.root)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlRecord {
    Session {
        version: u32,
        session_id: String,
        active_leaf_id: Option<String>,
        metadata: BTreeMap<String, String>,
    },
    Entry {
        entry: StoredTranscriptEntry,
    },
}

#[async_trait]
impl SessionStore for JsonlSessionStore {
    async fn load_session(&self, session_id: &str) -> StoreResult<Option<StoredSession>> {
        let path = self.path_for(session_id);
        if !path.exists() {
            return Ok(None);
        }
        read_jsonl_session(&path)
    }

    async fn write_session(&self, session: &StoredSession) -> StoreResult<()> {
        self.ensure_root()?;
        write_jsonl_session(&self.path_for(&session.session_id), session)
    }

    async fn append_entries(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()> {
        let mut session = self
            .load_session(session_id)
            .await?
            .ok_or_else(|| StoreError::SessionNotFound(session_id.to_string()))?;
        session.entries.extend(entries.iter().cloned());
        session.active_leaf_id = active_leaf_id.map(str::to_string);
        self.write_session(&session).await
    }

    async fn set_active_leaf(
        &self,
        session_id: &str,
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()> {
        let mut session = self
            .load_session(session_id)
            .await?
            .ok_or_else(|| StoreError::SessionNotFound(session_id.to_string()))?;
        session.active_leaf_id = active_leaf_id.map(str::to_string);
        self.write_session(&session).await
    }
}

fn read_jsonl_session(path: &Path) -> StoreResult<Option<StoredSession>> {
    let text = fs::read_to_string(path)?;
    let mut session: Option<StoredSession> = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JsonlRecord>(line)? {
            JsonlRecord::Session {
                version: _,
                session_id,
                active_leaf_id,
                metadata,
            } => {
                if index != 0 {
                    return Err(StoreError::InvalidRecord(
                        "session header must be the first JSONL record".to_string(),
                    ));
                }
                session = Some(StoredSession {
                    session_id,
                    active_leaf_id,
                    metadata,
                    entries: Vec::new(),
                });
            }
            JsonlRecord::Entry { entry } => {
                let Some(session) = &mut session else {
                    return Err(StoreError::InvalidRecord(
                        "entry appeared before session header".to_string(),
                    ));
                };
                session.entries.push(entry);
            }
        }
    }
    Ok(session)
}

fn write_jsonl_session(path: &Path, session: &StoredSession) -> StoreResult<()> {
    let mut output = String::new();
    output.push_str(&serde_json::to_string(&JsonlRecord::Session {
        version: 1,
        session_id: session.session_id.clone(),
        active_leaf_id: session.active_leaf_id.clone(),
        metadata: session.metadata.clone(),
    })?);
    output.push('\n');
    for entry in &session.entries {
        output.push_str(&serde_json::to_string(&JsonlRecord::Entry {
            entry: entry.clone(),
        })?);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{TranscriptItem, TurnId, UserMessage};

    #[tokio::test]
    async fn memory_store_round_trips_sessions() {
        let store = InMemorySessionStore::default();
        let mut session = StoredSession::new("s1");
        session.entries.push(StoredTranscriptEntry {
            id: "e1".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::UserMessage(UserMessage::text("hi")),
        });
        session.active_leaf_id = Some("e1".to_string());

        store.write_session(&session).await.unwrap();
        assert_eq!(store.load_session("s1").await.unwrap(), Some(session));
    }

    #[tokio::test]
    async fn jsonl_store_round_trips_sessions() {
        let root = std::env::temp_dir().join(format!("agent-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = JsonlSessionStore::new(&root);
        let mut session = StoredSession::new("s2");
        session.entries.push(StoredTranscriptEntry {
            id: "e1".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
        });
        session.active_leaf_id = Some("e1".to_string());

        store.write_session(&session).await.unwrap();
        assert_eq!(store.load_session("s2").await.unwrap(), Some(session));
        let _ = fs::remove_dir_all(root);
    }
}
