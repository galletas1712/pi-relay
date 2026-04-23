use std::collections::BTreeMap;

use agent_session::AgentSession;

pub type SessionId = String;

/// Registry of live sessions keyed by id, with parent-child tracking for
/// spawned sub-agents.
///
/// Generic over the session type so it can later also hold `SessionHandle`
/// for out-of-process sessions without duplicating the identity and
/// spawn-parent bookkeeping.
#[derive(Debug)]
pub struct SessionRegistry<S = AgentSession> {
    sessions: BTreeMap<SessionId, S>,
    spawn_parents: BTreeMap<SessionId, SessionId>,
}

impl<S> Default for SessionRegistry<S> {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
            spawn_parents: BTreeMap::new(),
        }
    }
}

impl<S> SessionRegistry<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&mut self, id: impl Into<SessionId>, session: S) -> Result<(), RegistryError> {
        let id = id.into();
        if self.sessions.contains_key(&id) {
            return Err(RegistryError::SessionAlreadyExists);
        }
        self.sessions.insert(id, session);
        Ok(())
    }

    pub fn spawn_child(
        &mut self,
        id: impl Into<SessionId>,
        session: S,
        parent: impl Into<SessionId>,
    ) -> Result<(), RegistryError> {
        let id = id.into();
        let parent = parent.into();
        if !self.sessions.contains_key(&parent) {
            return Err(RegistryError::ParentNotFound);
        }
        if self.sessions.contains_key(&id) {
            return Err(RegistryError::SessionAlreadyExists);
        }
        self.spawn_parents.insert(id.clone(), parent);
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
        if !self.sessions.contains_key(id) {
            return Err(RegistryError::SessionNotFound);
        }
        // Refuse with extant children — the spawn tree must stay consistent.
        if self.spawn_parents.values().any(|parent| parent == id) {
            return Err(RegistryError::HasChildren);
        }
        self.spawn_parents.remove(id);
        Ok(self.sessions.remove(id).expect("contains_key check above"))
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    pub fn parent(&self, id: &str) -> Option<&SessionId> {
        self.spawn_parents.get(id)
    }

    pub fn children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a SessionId> + 'a {
        self.spawn_parents
            .iter()
            .filter(move |(_, p)| p.as_str() == parent)
            .map(|(child, _)| child)
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
    ParentNotFound,
    /// Session has extant children in the spawn tree; remove them first.
    HasChildren,
}

/// Errors returned by orchestrator routing primitives
/// (`AgentOrchestrator::send_message` / `send_report`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteError {
    /// The `from` session isn't in the registry.
    SenderNotFound,
    /// The `to` session isn't in the registry.
    TargetNotFound,
    /// The `to` session isn't a direct child of `from`. `send_message` only.
    NotAChild,
    /// The `from` session has no spawn parent. `send_report` only.
    NoParent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_spawn_parent_and_child_relationships() {
        let mut registry: SessionRegistry<()> = SessionRegistry::new();
        registry.spawn("root", ()).unwrap();
        registry.spawn_child("child-a", (), "root").unwrap();
        registry.spawn_child("child-b", (), "root").unwrap();

        assert_eq!(registry.parent("child-a"), Some(&"root".to_string()));
        let children: Vec<_> = registry.children("root").collect();
        assert_eq!(children.len(), 2);
        assert!(registry.children("child-a").next().is_none());

        assert!(matches!(
            registry.spawn_child("orphan", (), "missing"),
            Err(RegistryError::ParentNotFound)
        ));
    }

    #[test]
    fn registry_refuses_to_remove_a_session_with_extant_children() {
        let mut registry: SessionRegistry<()> = SessionRegistry::new();
        registry.spawn("root", ()).unwrap();
        registry.spawn_child("child", (), "root").unwrap();

        assert_eq!(registry.remove("root"), Err(RegistryError::HasChildren));
        // Root is still in the registry, and so is the child.
        assert!(registry.contains("root"));
        assert!(registry.contains("child"));

        // Remove the child first; now root can be removed.
        registry.remove("child").expect("child has no descendants");
        registry
            .remove("root")
            .expect("root now has no descendants");
    }
}
