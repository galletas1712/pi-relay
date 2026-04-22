#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use agent_core::AgentInput;
use agent_session::{
    AgentSession, CompactionPlan, CompactionSettings, ExternalWork, SessionBoundaryError,
    SessionLog, Transcript,
};

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

    pub fn enqueue_input(&mut self, id: &str, input: AgentInput) -> Result<(), OrchestratorError> {
        self.session_mut(id)?.core_mut().enqueue_input(input);
        Ok(())
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

    pub fn prepare_session_compaction(
        &self,
        id: &str,
        settings: CompactionSettings,
        external_work: ExternalWork,
    ) -> Result<Option<CompactionPlan>, OrchestratorError> {
        self.session(id)?
            .prepare_compaction(settings, external_work)
            .map_err(OrchestratorError::Boundary)
    }

    pub fn compact_session_at_boundary(
        &mut self,
        id: &str,
        plan: &CompactionPlan,
        summary: impl Into<String>,
        external_work: ExternalWork,
    ) -> Result<(), OrchestratorError> {
        self.session_mut(id)?
            .compact_at_boundary(plan, summary, external_work)
            .map_err(OrchestratorError::Boundary)
    }

    pub fn rewind_session_to_turn_boundary(
        &mut self,
        id: &str,
        leaf_id: Option<&str>,
        external_work: ExternalWork,
    ) -> Result<(), OrchestratorError> {
        self.session_mut(id)?
            .rewind_to_turn_boundary(leaf_id, external_work)
            .map_err(OrchestratorError::Boundary)
    }

    pub fn fork_session_at_turn_boundary(
        &mut self,
        source_id: &str,
        new_id: impl Into<String>,
        leaf_id: Option<&str>,
        external_work: ExternalWork,
    ) -> Result<(), OrchestratorError> {
        let new_id = new_id.into();
        if self.sessions.contains_key(&new_id) {
            return Err(OrchestratorError::SessionAlreadyExists);
        }

        let fork = self
            .session(source_id)?
            .fork_at_turn_boundary(leaf_id, external_work)
            .map_err(OrchestratorError::Boundary)?;
        self.sessions.insert(new_id, fork);
        Ok(())
    }

    pub fn replace_session_from_log(
        &mut self,
        id: &str,
        log: SessionLog,
    ) -> Result<(), OrchestratorError> {
        let session = AgentSession::from_session_log(log).map_err(OrchestratorError::Boundary)?;
        *self.session_mut(id)? = session;
        Ok(())
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
    use agent_core::{AssistantMessage, TranscriptRecord, TurnId, TurnOutcome};

    #[test]
    fn orchestrator_routes_input_to_sessions() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .spawn_session("root", AgentSession::new())
            .expect("new session should be inserted");

        orchestrator
            .enqueue_input("root", AgentInput::FollowUp("hello".to_string()))
            .expect("session should exist");

        assert_eq!(
            orchestrator
                .session("root")
                .expect("session should exist")
                .core()
                .mailbox
                .follow_up_len(),
            1
        );
    }

    #[test]
    fn orchestrator_delegates_transcript_replacement_to_session_boundary() {
        let mut orchestrator = AgentOrchestrator::new();
        let transcript = Transcript::from_records(vec![
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

    #[test]
    fn orchestrator_delegates_rewind_fork_and_compaction_to_session_boundaries() {
        let mut orchestrator = AgentOrchestrator::new();
        let session = AgentSession::from_transcript(Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first user message".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage { items: Vec::new() }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second user message".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage { items: Vec::new() }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));
        let mid_turn_id = session.session_log().entries()[1].id.clone();
        let turn_one_end_id = session.session_log().entries()[3].id.clone();

        orchestrator
            .spawn_session("root", session)
            .expect("new session should be inserted");
        assert!(matches!(
            orchestrator.rewind_session_to_turn_boundary(
                "root",
                Some(&mid_turn_id),
                ExternalWork::NONE
            ),
            Err(OrchestratorError::Boundary(SessionBoundaryError::Log(
                agent_session::SessionLogError::NotTurnBoundary
            )))
        ));

        orchestrator
            .fork_session_at_turn_boundary(
                "root",
                "fork",
                Some(&turn_one_end_id),
                ExternalWork::NONE,
            )
            .expect("turn boundary fork should be inserted");
        assert_eq!(
            orchestrator
                .session("fork")
                .expect("fork should exist")
                .transcript()
                .last_turn_id(),
            TurnId(1)
        );

        let plan = orchestrator
            .prepare_session_compaction(
                "root",
                CompactionSettings {
                    keep_recent_tokens: 1,
                },
                ExternalWork::NONE,
            )
            .expect("session should exist")
            .expect("old turn should be compactable");
        orchestrator
            .compact_session_at_boundary("root", &plan, "summary", ExternalWork::NONE)
            .expect("root can compact at boundary");
        assert_eq!(
            orchestrator
                .session("root")
                .expect("root should exist")
                .model_context()
                .compaction
                .as_ref()
                .map(|entry| entry.summary.as_str()),
            Some("summary")
        );
    }
}
