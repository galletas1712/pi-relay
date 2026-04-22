#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use agent_core::{AgentAction, AgentInput, Transcript};
use agent_session::{AgentSession, ExternalWork, SessionBoundaryError};

/// Thin multi-session coordinator.
///
/// The orchestrator should own cross-agent policy: spawning, routing messages,
/// worklog maintenance, background work, and child-agent reporting. Execution
/// details stay in `agent-session`; deterministic transitions stay in
/// `agent-core`.
#[derive(Debug, Default)]
pub struct AgentOrchestrator {
    sessions: BTreeMap<String, AgentSession>,
}

impl AgentOrchestrator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn_session(
        &mut self,
        id: impl Into<String>,
        session: AgentSession,
    ) -> Result<(), OrchestratorError> {
        let id = id.into();
        if self.sessions.contains_key(&id) {
            return Err(OrchestratorError::SessionAlreadyExists);
        }

        self.sessions.insert(id, session);
        Ok(())
    }

    pub fn session(&self, id: &str) -> Result<&AgentSession, OrchestratorError> {
        self.sessions
            .get(id)
            .ok_or(OrchestratorError::SessionNotFound)
    }

    pub fn session_mut(&mut self, id: &str) -> Result<&mut AgentSession, OrchestratorError> {
        self.sessions
            .get_mut(id)
            .ok_or(OrchestratorError::SessionNotFound)
    }

    pub fn send(
        &mut self,
        id: &str,
        input: AgentInput,
    ) -> Result<Vec<AgentAction>, OrchestratorError> {
        Ok(self.session_mut(id)?.handle_input(input))
    }

    pub fn replace_session_transcript(
        &mut self,
        id: &str,
        replacement: Transcript,
        external_work: ExternalWork,
    ) -> Result<Transcript, OrchestratorError> {
        self.session_mut(id)?
            .replace_transcript_at_boundary(replacement, external_work)
            .map_err(OrchestratorError::Boundary)
    }

    pub fn remove_session(&mut self, id: &str) -> Result<AgentSession, OrchestratorError> {
        self.sessions
            .remove(id)
            .ok_or(OrchestratorError::SessionNotFound)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestratorError {
    SessionAlreadyExists,
    SessionNotFound,
    Boundary(SessionBoundaryError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{TranscriptRecord, TurnId, TurnOutcome};

    #[test]
    fn orchestrator_routes_input_to_sessions() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .spawn_session("root", AgentSession::new())
            .expect("new session should be inserted");

        let actions = orchestrator
            .send("root", AgentInput::FollowUp("hello".to_string()))
            .expect("session should exist");

        assert_eq!(
            actions,
            vec![AgentAction::RequestModel { turn_id: TurnId(1) }]
        );
    }

    #[test]
    fn orchestrator_delegates_transcript_replacement_to_session_boundary() {
        let mut orchestrator = AgentOrchestrator::new();
        let transcript = Transcript::from_records_raw(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("compacted".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]);

        orchestrator
            .spawn_session("root", AgentSession::new())
            .expect("new session should be inserted");
        orchestrator
            .replace_session_transcript("root", transcript, ExternalWork::NONE)
            .expect("idle empty session can replace transcript");

        assert_eq!(
            orchestrator
                .session("root")
                .expect("session should exist")
                .transcript()
                .last_turn_id(),
            TurnId(1)
        );
    }
}
