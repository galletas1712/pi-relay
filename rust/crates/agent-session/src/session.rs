use agent_core::{AgentAction, AgentCoreLoop, AgentInput, TranscriptRecord, TurnId};

use crate::action_queue::ActionQueue;
use crate::context::{Context, ContextEdit, ContextError, HistoryEditError, PendingWork};
use crate::transcript::Transcript;

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// point at which the session's history can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work. The
/// `Context` is the sole owner of durable records; the core only buffers
/// records produced in the current run until the session absorbs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub(crate) core: AgentCoreLoop,
    pub(crate) context: Context,
    action_queue: ActionQueue,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSession {
    pub fn new() -> Self {
        Self {
            core: AgentCoreLoop::resume_at_boundary(TurnId::default()),
            context: Context::new(),
            action_queue: ActionQueue::new(),
        }
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self::from_transcript(Transcript::from_records(records))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        let last_turn_id = transcript.last_turn_id();
        let context = Context::from_transcript(&transcript);
        Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            context,
            action_queue: ActionQueue::new(),
        }
    }

    pub fn from_context(context: Context) -> Result<Self, HistoryEditError> {
        if !context.is_turn_boundary() {
            return Err(HistoryEditError::Context(ContextError::NotTurnBoundary));
        }

        let transcript = context.transcript();
        let last_turn_id = transcript.last_turn_id();
        Ok(Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            context,
            action_queue: ActionQueue::new(),
        })
    }

    /// Enqueue a new input into the underlying core loop.
    ///
    /// This is the only supported way to feed the core from outside the
    /// session; the core itself is not exposed so context absorption in `drive`
    /// cannot be bypassed.
    ///
    /// `ModelCompleted` / `ToolCompleted` clear the matching entry from the
    /// session's internal action queue. Stale completions (no matching
    /// pending entry, e.g. after an interrupt) are removed with no effect.
    pub fn enqueue_input(&mut self, input: AgentInput) {
        self.action_queue.record_input(&input);
        self.core.enqueue_input(input);
    }

    /// The most recent turn id observed by the core loop.
    pub fn last_turn_id(&self) -> TurnId {
        self.core.last_turn_id()
    }

    /// True when the core loop is between turns and has no in-flight work.
    pub fn is_idle(&self) -> bool {
        self.core.is_idle()
    }

    /// True when the core loop's mailbox still has queued inputs.
    pub fn has_pending_work(&self) -> bool {
        self.core.has_pending_work()
    }

    /// Materialized view of the session history derived from the context.
    /// With a compaction present, the latest summary is inlined ahead of the
    /// kept suffix so downstream callers see a single ordered record stream.
    pub fn transcript(&self) -> Transcript {
        self.context.transcript()
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Drive the core to quiescence and append any records it emitted to the
    /// session context. This is the only supported way to advance a session;
    /// the context remains the sole owner of durable history.
    pub fn drive(&mut self) {
        self.core.drive();
        self.absorb_core_records();
    }

    /// Drain pending actions the core produced during the last `drive`.
    ///
    /// Each drained `RequestModel` / `RequestTool` is recorded in the
    /// session's internal action queue so `can_edit_history` can block
    /// until the matching completion comes back via `enqueue_input`.
    /// `CancelTurn` clears every pending action for the cancelled turn — the
    /// orchestrator will not deliver completions for cancelled work.
    ///
    /// Records are absorbed into the context inside `drive`, so there is no
    /// analogous `drain_records` on the session.
    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        let actions = self.core.drain_actions();
        self.action_queue.record_drained(&actions);
        actions
    }

    /// True when the session's history can be edited: core idle, context at a
    /// turn boundary, mailbox empty, no in-flight drained actions, and no
    /// caller-tracked background work.
    pub fn can_edit_history(&self, pending: PendingWork) -> bool {
        self.core.is_idle()
            && self.context.is_turn_boundary()
            && !self.core.has_pending_work()
            && self.action_queue.is_empty()
            && pending.is_empty()
    }

    /// Validate the precondition once; return a view that permits editing the
    /// session's history.
    ///
    /// The returned `ContextEdit` is the only surface for compact, rewind,
    /// fork, and replace_transcript — the guard ran once here, so individual
    /// ops do not repeat it.
    pub fn edit_history(
        &mut self,
        pending: PendingWork,
    ) -> Result<ContextEdit<'_>, HistoryEditError> {
        if self.can_edit_history(pending) {
            Ok(ContextEdit::new(self))
        } else {
            Err(HistoryEditError::Busy)
        }
    }

    fn absorb_core_records(&mut self) {
        let records = self.core.drain_records();
        if records.is_empty() {
            return;
        }
        self.context.append_transcript_records(records);
    }

    pub(crate) fn rehydrate_core_from_context(&mut self) {
        let last_turn_id = self.context.transcript().last_turn_id();
        self.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
        // Any actions tracked as pending belong to a prior run we're no
        // longer driving; reset the queue so a rehydrated session does not
        // block edits forever.
        self.action_queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{branch_summary, compaction_summary};
    use agent_core::{
        AssistantItem, AssistantMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus,
        TurnId, TurnOutcome,
    };

    fn finished_transcript(input: &str) -> Transcript {
        Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage(input.to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ])
    }

    #[test]
    fn can_edit_history_requires_idle_core_empty_queues_and_no_pending_work() {
        let mut session = AgentSession::new();

        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));
        assert!(!session.can_edit_history(PendingWork::NONE));
        assert!(!session.can_edit_history(PendingWork {
            background_tasks: 1,
        }));
    }

    #[test]
    fn context_tracks_core_turn_records() {
        let session = AgentSession::from_transcript(finished_transcript("hello"));

        assert_eq!(session.context().entries().len(), 3);
        assert!(session.context().is_turn_boundary());
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));
    }

    #[test]
    fn drive_absorbs_core_records_into_the_session_context() {
        let mut session = AgentSession::new();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        };

        session.enqueue_input(AgentInput::FollowUp("hello".to_string()));
        session.drive();
        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: assistant.clone(),
        });
        session.drive();

        assert_eq!(
            session.transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::AssistantMessage(assistant),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        // Driving again absorbs nothing new; the core buffer was drained.
        session.drive();
        assert_eq!(session.transcript().records().len(), 4);
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
            TranscriptRecord::UserMessage("hello".to_string()),
        ];

        let session = AgentSession::from_records(transcript);

        assert_eq!(
            session.transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(7) },
                TranscriptRecord::UserMessage("hello".to_string()),
                TranscriptRecord::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(7));
    }

    #[test]
    fn rehydrating_a_graceful_boundary_restores_idle_state() {
        let transcript = vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("hello".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let session = AgentSession::from_records(transcript.clone());

        assert_eq!(session.transcript().records(), transcript.as_slice());
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(2));
    }

    #[test]
    fn session_blocks_edit_history_until_drained_model_action_completes() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("hi".to_string()));
        session.drive();
        let actions = session.drain_actions();
        assert!(matches!(
            actions.as_slice(),
            [AgentAction::RequestModel { .. }]
        ));
        assert!(!session.can_edit_history(PendingWork::NONE));

        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });
        session.drive();
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn session_blocks_edit_history_while_tool_actions_in_flight() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("go".to_string()));
        session.drive();
        session.drain_actions();

        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "bash".to_string(),
            args_json: "{}".to_string(),
        };
        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            },
        });
        session.drive();
        session.drain_actions();
        assert!(!session.can_edit_history(PendingWork::NONE));

        session.enqueue_input(AgentInput::ToolCompleted {
            turn_id: TurnId(1),
            result: ToolResultMessage {
                tool_call_id: ToolCallId(1),
                tool_name: "bash".to_string(),
                output: "ok".to_string(),
                status: ToolResultStatus::Success,
            },
        });
        session.drive();
        // A second model request fires after the tool completes; the session
        // is still waiting on that completion, so edits remain blocked.
        assert!(!session.can_edit_history(PendingWork::NONE));
        session.drain_actions();
        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });
        session.drive();
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn can_edit_history_walks_past_multiple_custom_entries() {
        // A context whose leaf is a run of back-to-back Custom entries after
        // a TurnFinished is still at a turn boundary; can_edit_history must
        // see through the Custom tail to the underlying TurnFinished.
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("hi".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ]));
        session.context.append_custom(branch_summary("a", None));
        session
            .context
            .append_custom(compaction_summary("b", "does-not-matter", 0));
        session.context.append_custom(branch_summary("c", None));

        assert!(session.context().is_turn_boundary());
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn cancel_turn_clears_pending_actions_for_that_turn() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::FollowUp("hi".to_string()));
        session.drive();
        session.drain_actions();

        session.enqueue_input(AgentInput::Interrupt);
        session.drive();
        let actions = session.drain_actions();
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::CancelTurn { .. })));
        assert!(session.can_edit_history(PendingWork::NONE));
    }
}
