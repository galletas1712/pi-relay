//! Composition struct for the agent runtime.
//!
//! Currently owns a `SessionRegistry` that tracks session identity and
//! parent-child spawn relationships. The `ControlPlane` trait, model
//! provider, tool registry, usage ledger, and worklog store will join as
//! peer fields in later PRs. See `rust/docs/architecture.md` for the
//! feature roadmap and PR sequencing.

#![forbid(unsafe_code)]

mod registry;

use agent_session::AgentSession;

pub use crate::registry::{RegistryError, SessionId, SessionRegistry};

/// Composition struct for the agent runtime.
///
/// Today this owns only the session registry. As `ModelProvider`,
/// `ToolRegistry`, `UsageLedger`, and `AgentWorklogStore` land, they join
/// here as peer fields.
#[derive(Debug, Default)]
pub struct AgentOrchestrator {
    registry: SessionRegistry<AgentSession>,
}

impl AgentOrchestrator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn registry(&self) -> &SessionRegistry<AgentSession> {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut SessionRegistry<AgentSession> {
        &mut self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AgentInput, AssistantMessage, TranscriptRecord, TurnId, TurnOutcome};
    use agent_session::{
        CompactionSettings, ContextError, HistoryEditError, PendingWork, Transcript,
    };

    #[test]
    fn orchestrator_routes_input_to_sessions() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .registry_mut()
            .spawn("root", AgentSession::new())
            .expect("new session should be inserted");

        orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .enqueue_input(AgentInput::FollowUp("hello".to_string()));

        assert!(orchestrator
            .registry()
            .get("root")
            .expect("session should exist")
            .has_pending_work());
    }

    #[test]
    fn orchestrator_delegates_transcript_replacement_to_session_history_edit() {
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
            .registry_mut()
            .spawn("root", AgentSession::new())
            .expect("new session should be inserted");
        orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .edit_history(PendingWork::NONE)
            .expect("idle empty session is quiescent")
            .replace_transcript(transcript)
            .expect("idle empty session can replace transcript");

        assert_eq!(
            orchestrator
                .registry()
                .get("root")
                .expect("session should exist")
                .transcript()
                .last_turn_id(),
            TurnId(1)
        );
    }

    #[test]
    fn orchestrator_delegates_rewind_fork_and_compaction_to_session_history_edits() {
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
        let mid_turn_id = session.context().entries()[1].id.clone();
        let turn_one_end_id = session.context().entries()[3].id.clone();

        orchestrator
            .registry_mut()
            .spawn("root", session)
            .expect("new session should be inserted");

        let rewind_err = orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .edit_history(PendingWork::NONE)
            .expect("session is quiescent")
            .rewind(Some(&mid_turn_id));
        assert!(matches!(
            rewind_err,
            Err(HistoryEditError::Context(ContextError::NotTurnBoundary))
        ));

        let fork = orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .edit_history(PendingWork::NONE)
            .expect("session is quiescent")
            .fork(Some(&turn_one_end_id))
            .expect("turn boundary fork should succeed");
        orchestrator
            .registry_mut()
            .spawn_child("fork", fork, "root")
            .expect("fork should insert under root");
        assert_eq!(
            orchestrator
                .registry()
                .get("fork")
                .expect("fork should exist")
                .transcript()
                .last_turn_id(),
            TurnId(1)
        );
        assert_eq!(
            orchestrator.registry().parent("fork"),
            Some(&"root".to_string())
        );

        let plan = orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .edit_history(PendingWork::NONE)
            .expect("session is quiescent")
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");
        orchestrator
            .registry_mut()
            .get_mut("root")
            .expect("session should exist")
            .edit_history(PendingWork::NONE)
            .expect("session is quiescent")
            .compact(&plan, "summary")
            .expect("root can compact at turn boundary");
        assert_eq!(
            orchestrator
                .registry()
                .get("root")
                .expect("root should exist")
                .transcript()
                .latest_compaction_summary(),
            Some("summary")
        );
    }
}
