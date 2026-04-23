//! Composition struct for the agent runtime.
//!
//! Currently owns a `SessionRegistry` that tracks session identity and
//! parent-child spawn relationships. The `ControlPlane` trait, model
//! provider, tool registry, usage ledger, and worklog store will join as
//! peer fields in later PRs. See `rust/docs/architecture.md` for the
//! feature roadmap and PR sequencing.

#![forbid(unsafe_code)]

mod registry;

use agent_core::AgentInput;
use agent_session::AgentSession;

pub use crate::registry::{RegistryError, RouteError, SessionId, SessionRegistry};

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

    /// Fire-and-forget: route a Steer from a parent session to one of its
    /// direct children.
    ///
    /// Enqueues `AgentInput::Steer { from: Some(from), content }` on the
    /// target's mailbox. The `from` tag rides along so the target can
    /// distinguish parent directives from human user input.
    ///
    /// Validates that `to` is a direct child of `from` in the spawn tree;
    /// routing to an unrelated or descendant session returns
    /// `RouteError::NotAChild`.
    pub fn send_message(
        &mut self,
        from: &SessionId,
        to: &SessionId,
        content: String,
    ) -> Result<(), RouteError> {
        if !self.registry.contains(from) {
            return Err(RouteError::SenderNotFound);
        }
        if !self.registry.contains(to) {
            return Err(RouteError::TargetNotFound);
        }
        match self.registry.parent(to) {
            Some(parent) if parent == from => {}
            _ => return Err(RouteError::NotAChild),
        }
        let target = self
            .registry
            .get_mut(to)
            .expect("contains check above guarantees target exists");
        target.enqueue_input(AgentInput::steer_from(from.clone(), content));
        Ok(())
    }

    /// Fire-and-forget: route a FollowUp report from a child session to its
    /// spawn parent.
    ///
    /// Enqueues `AgentInput::FollowUp { from: Some(from), content }` on the
    /// parent's mailbox. The `from` tag identifies the originating child.
    ///
    /// Validates that `from` has a registered spawn parent; an orphan
    /// sender returns `RouteError::NoParent`.
    pub fn send_report(&mut self, from: &SessionId, content: String) -> Result<(), RouteError> {
        if !self.registry.contains(from) {
            return Err(RouteError::SenderNotFound);
        }
        let parent = self
            .registry
            .parent(from)
            .ok_or(RouteError::NoParent)?
            .clone();
        let target = self
            .registry
            .get_mut(&parent)
            .expect("registered spawn parent must be in the registry");
        target.enqueue_input(AgentInput::follow_up_from(from.clone(), content));
        Ok(())
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
            .enqueue_input(AgentInput::follow_up("hello"));

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

    fn orchestrator_with_parent_and_child() -> AgentOrchestrator {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .registry_mut()
            .spawn("A", AgentSession::new())
            .expect("parent spawn");
        orchestrator
            .registry_mut()
            .spawn_child("B", AgentSession::new(), "A")
            .expect("child spawn");
        orchestrator
    }

    #[test]
    fn send_message_delivers_to_child_queue_tagged_with_sender() {
        let mut orchestrator = orchestrator_with_parent_and_child();

        orchestrator
            .send_message(&"A".to_string(), &"B".to_string(), "do X".to_string())
            .expect("A -> B is a valid parent->child route");

        let child = orchestrator
            .registry_mut()
            .get_mut("B")
            .expect("child exists");
        let drained = child.drain_pending_inputs();
        assert_eq!(
            drained,
            vec![AgentInput::Steer {
                from: Some("A".to_string()),
                content: "do X".to_string(),
            }]
        );
    }

    #[test]
    fn send_message_rejects_non_child_target() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .registry_mut()
            .spawn("A", AgentSession::new())
            .expect("A spawn");
        orchestrator
            .registry_mut()
            .spawn("C", AgentSession::new())
            .expect("C spawn");

        let err = orchestrator
            .send_message(&"A".to_string(), &"C".to_string(), "x".to_string())
            .expect_err("C is not a child of A");
        assert_eq!(err, RouteError::NotAChild);

        // Nothing queued on the unrelated session.
        let c = orchestrator.registry_mut().get_mut("C").expect("C exists");
        assert!(c.drain_pending_inputs().is_empty());
    }

    #[test]
    fn send_message_rejects_unknown_target() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .registry_mut()
            .spawn("A", AgentSession::new())
            .expect("A spawn");

        let err = orchestrator
            .send_message(&"A".to_string(), &"ghost".to_string(), "x".to_string())
            .expect_err("ghost is not registered");
        assert_eq!(err, RouteError::TargetNotFound);
    }

    #[test]
    fn send_report_delivers_to_parent_queue_tagged_with_sender() {
        let mut orchestrator = orchestrator_with_parent_and_child();

        orchestrator
            .send_report(&"B".to_string(), "found X".to_string())
            .expect("B -> A is a valid child->parent route");

        let parent = orchestrator
            .registry_mut()
            .get_mut("A")
            .expect("parent exists");
        let drained = parent.drain_pending_inputs();
        assert_eq!(
            drained,
            vec![AgentInput::FollowUp {
                from: Some("B".to_string()),
                content: "found X".to_string(),
            }]
        );
    }

    #[test]
    fn send_report_rejects_orphan_sender() {
        let mut orchestrator = AgentOrchestrator::new();
        orchestrator
            .registry_mut()
            .spawn("root", AgentSession::new())
            .expect("root spawn");

        let err = orchestrator
            .send_report(&"root".to_string(), "hello".to_string())
            .expect_err("root has no spawn parent");
        assert_eq!(err, RouteError::NoParent);
    }

    #[test]
    fn send_report_rejects_unknown_sender() {
        let mut orchestrator = AgentOrchestrator::new();

        let err = orchestrator
            .send_report(&"ghost".to_string(), "hello".to_string())
            .expect_err("ghost is not registered");
        assert_eq!(err, RouteError::SenderNotFound);
    }
}
