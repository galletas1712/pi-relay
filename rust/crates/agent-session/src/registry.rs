use std::collections::BTreeMap;

use crate::AgentSession;

pub type SessionId = String;

/// Registry of live sessions keyed by id.
///
/// This is intentionally in-memory process state, not durable storage. It lets
/// a CLI/control plane keep several `AgentSession` values open, switch between
/// them, and insert forks as independent sessions.
#[derive(Debug)]
pub struct SessionRegistry<S = AgentSession> {
    sessions: BTreeMap<SessionId, S>,
}

impl<S> Default for SessionRegistry<S> {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
        }
    }
}

impl<S> SessionRegistry<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: impl Into<SessionId>, session: S) -> Result<(), RegistryError> {
        let id = id.into();
        if self.sessions.contains_key(&id) {
            return Err(RegistryError::SessionAlreadyExists);
        }
        self.sessions.insert(id, session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<&S, RegistryError> {
        self.sessions.get(id).ok_or(RegistryError::SessionNotFound)
    }

    pub fn get_mut(&mut self, id: &str) -> Result<&mut S, RegistryError> {
        self.sessions
            .get_mut(id)
            .ok_or(RegistryError::SessionNotFound)
    }

    pub fn remove(&mut self, id: &str) -> Result<S, RegistryError> {
        self.sessions
            .remove(id)
            .ok_or(RegistryError::SessionNotFound)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &SessionId> + '_ {
        self.sessions.keys()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    SessionAlreadyExists,
    SessionNotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_independent_sessions() {
        let mut registry: SessionRegistry<()> = SessionRegistry::new();
        registry.insert("a", ()).unwrap();
        registry.insert("b", ()).unwrap();

        assert!(registry.contains("a"));
        assert!(registry.contains("b"));
        assert_eq!(registry.ids().cloned().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(
            registry.insert("a", ()),
            Err(RegistryError::SessionAlreadyExists)
        );
    }

    #[test]
    fn registry_removes_sessions_without_tree_constraints() {
        let mut registry: SessionRegistry<()> = SessionRegistry::new();
        registry.insert("root", ()).unwrap();

        registry.remove("root").unwrap();
        assert!(!registry.contains("root"));
        assert_eq!(registry.remove("root"), Err(RegistryError::SessionNotFound));
    }

    #[test]
    fn default_registry_holds_agent_sessions() {
        let mut registry = SessionRegistry::new();
        registry.insert("main", AgentSession::new()).unwrap();

        registry
            .get_mut("main")
            .unwrap()
            .enqueue_input(crate::AgentInput::follow_up("hello"))
            .unwrap();

        assert_eq!(
            registry.get_mut("main").unwrap().drain_pending_inputs(),
            vec![crate::AgentInput::follow_up("hello")]
        );
    }
}
