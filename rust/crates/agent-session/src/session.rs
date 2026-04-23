use std::collections::VecDeque;

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
    action_outbox: VecDeque<AgentAction>,
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
            action_outbox: VecDeque::new(),
        }
    }

    pub fn from_records(records: Vec<TranscriptRecord>) -> Self {
        Self::from_transcript(Transcript::from_records_recovering_crashed_tail(records))
    }

    pub fn from_transcript(transcript: Transcript) -> Self {
        let last_turn_id = transcript.last_turn_id();
        let context = Context::from_transcript(&transcript);
        Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            context,
            action_queue: ActionQueue::new(),
            action_outbox: VecDeque::new(),
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
            action_outbox: VecDeque::new(),
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
        self.drop_completed_action_from_outbox(&input);
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
        self.absorb_core_actions();
    }

    /// Drain every queued user input (Steer then FollowUp) from the
    /// underlying core mailbox without advancing the session. Preserves the
    /// `from` tag each input was enqueued with.
    ///
    /// Notifications (model/tool completions) and the interrupt flag are
    /// untouched. Primarily intended for tests and for orchestrator-level
    /// introspection of routing.
    pub fn drain_pending_inputs(&mut self) -> Vec<AgentInput> {
        self.core.drain_pending_inputs()
    }

    /// Drain pending actions the core produced during the last `drive`.
    ///
    /// Actions are recorded in the session's internal action queue during
    /// `drive`, so `can_edit_history` does not depend on when callers drain
    /// the observable outbox.
    ///
    /// Records are absorbed into the context inside `drive`, so there is no
    /// analogous `drain_records` on the session.
    pub fn drain_actions(&mut self) -> Vec<AgentAction> {
        self.action_outbox.drain(..).collect()
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

    /// Apply a `ContextEdit` operation (`Compact`, `Rewind`,
    /// `ReplaceTranscript`) to this session's context.
    ///
    /// The quiescence check runs once here; the op's `apply` then mutates the
    /// context directly. On success the core loop is rehydrated from the new
    /// context so the next `drive` resumes from the correct turn id.
    pub fn edit<E: ContextEdit>(
        &mut self,
        pending: PendingWork,
        edit: E,
    ) -> Result<E::Output, HistoryEditError> {
        if !self.can_edit_history(pending) {
            return Err(HistoryEditError::Busy);
        }
        let output = edit.apply(&mut self.context)?;
        self.rehydrate_core_from_context();
        Ok(output)
    }

    /// Produce an unregistered `AgentSession` whose context branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    ///
    /// Fork stays as a direct method rather than a `ContextEdit` impl because
    /// it reads the context and produces a new session rather than mutating
    /// the source in place.
    pub fn fork(
        &mut self,
        pending: PendingWork,
        leaf_id: Option<&str>,
    ) -> Result<AgentSession, HistoryEditError> {
        if !self.can_edit_history(pending) {
            return Err(HistoryEditError::Busy);
        }
        let context = self
            .context
            .create_branched_context_at_turn_boundary(leaf_id)
            .map_err(HistoryEditError::Context)?;
        AgentSession::from_context(context)
    }

    fn absorb_core_records(&mut self) {
        let records = self.core.drain_records();
        if records.is_empty() {
            return;
        }
        self.context.append_transcript_records(records);
    }

    fn absorb_core_actions(&mut self) {
        let actions = self.core.drain_actions();
        if actions.is_empty() {
            return;
        }
        self.action_queue.record_drained(&actions);
        for action in actions {
            if let AgentAction::CancelTurn { turn_id } = action {
                self.action_outbox.retain(|queued| {
                    !matches!(
                        queued,
                        AgentAction::RequestModel {
                            turn_id: queued_turn_id,
                        } | AgentAction::RequestTool {
                            turn_id: queued_turn_id,
                            ..
                        } if *queued_turn_id == turn_id
                    )
                });
            }
            self.action_outbox.push_back(action);
        }
    }

    fn drop_completed_action_from_outbox(&mut self, input: &AgentInput) {
        let position = self
            .action_outbox
            .iter()
            .position(|action| match (action, input) {
                (
                    AgentAction::RequestModel {
                        turn_id: action_turn_id,
                    },
                    AgentInput::ModelCompleted {
                        turn_id: input_turn_id,
                        ..
                    },
                ) => action_turn_id == input_turn_id,
                (
                    AgentAction::RequestTool {
                        turn_id: action_turn_id,
                        tool_call,
                    },
                    AgentInput::ToolCompleted {
                        turn_id: input_turn_id,
                        result,
                    },
                ) => {
                    action_turn_id == input_turn_id
                        && tool_call.id == result.tool_call_id
                        && tool_call.tool_name == result.tool_name
                }
                _ => false,
            });
        if let Some(position) = position {
            self.action_outbox.remove(position);
        }
    }

    pub(crate) fn rehydrate_core_from_context(&mut self) {
        let last_turn_id = self.context.transcript().last_turn_id();
        self.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
        // Any actions tracked as pending belong to a prior run we're no
        // longer driving; reset the queue so a rehydrated session does not
        // block edits forever.
        self.action_queue.clear();
        self.action_outbox.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::compaction::compaction_summary;
    use crate::context::rewind::branch_summary;
    use crate::context::{Compact, CompactionSettings, ReplaceTranscript, Rewind};
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
    fn transcript_replacement_is_only_allowed_at_turn_boundary() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::follow_up("hello"));

        let busy = session
            .edit(
                PendingWork::NONE,
                ReplaceTranscript {
                    replacement: finished_transcript("compact"),
                },
            )
            .expect_err("running sessions cannot edit history");
        assert_eq!(busy, HistoryEditError::Busy);

        let mut session = AgentSession::from_transcript(finished_transcript("hello"));

        let old = session
            .edit(
                PendingWork::NONE,
                ReplaceTranscript {
                    replacement: finished_transcript("compact"),
                },
            )
            .expect("idle session can swap transcript");

        assert_eq!(old.last_turn_id(), TurnId(1));
        assert_eq!(
            session.transcript().records()[1],
            TranscriptRecord::UserMessage("compact".to_string())
        );
    }

    #[test]
    fn rewind_and_fork_only_accept_turn_finished_entries() {
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second".to_string()),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));
        let mid_turn_id = session.context().entries()[1].id.clone();
        let turn_one_end_id = session.context().entries()[2].id.clone();

        assert_eq!(
            session.edit(
                PendingWork::NONE,
                Rewind {
                    leaf_id: Some(mid_turn_id.clone())
                }
            ),
            Err(HistoryEditError::Context(ContextError::NotTurnBoundary))
        );
        assert_eq!(
            session
                .fork(PendingWork::NONE, Some(&mid_turn_id))
                .map(|_| ()),
            Err(HistoryEditError::Context(ContextError::NotTurnBoundary))
        );

        session
            .edit(
                PendingWork::NONE,
                Rewind {
                    leaf_id: Some(turn_one_end_id.clone()),
                },
            )
            .expect("turn end is a valid rewind point");
        assert_eq!(session.transcript().last_turn_id(), TurnId(1));

        let fork = session
            .fork(PendingWork::NONE, Some(&turn_one_end_id))
            .expect("turn end is a valid fork point");
        assert_eq!(fork.transcript().last_turn_id(), TurnId(1));
    }

    #[test]
    fn compact_op_compacts_via_context_edit_dispatch() {
        let mut session = AgentSession::from_transcript(Transcript::from_records(vec![
            TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
            TranscriptRecord::UserMessage("first".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("ok".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            TranscriptRecord::TurnStarted { turn_id: TurnId(2) },
            TranscriptRecord::UserMessage("second".to_string()),
            TranscriptRecord::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("ok2".to_string())],
            }),
            TranscriptRecord::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ]));

        let plan = session
            .context()
            .prepare_compaction(CompactionSettings {
                keep_recent_tokens: 1,
            })
            .expect("old turn should be compactable");
        session
            .edit(
                PendingWork::NONE,
                Compact {
                    plan,
                    summary: "s".to_string(),
                },
            )
            .expect("compact should apply");
        assert_eq!(session.transcript().latest_compaction_summary(), Some("s"));
    }

    #[test]
    fn can_edit_history_requires_idle_core_empty_queues_and_no_pending_work() {
        let mut session = AgentSession::new();

        session.enqueue_input(AgentInput::follow_up("hello"));
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

        session.enqueue_input(AgentInput::follow_up("hello"));
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
    fn live_transcript_keeps_open_turns_open() {
        let mut session = AgentSession::new();

        session.enqueue_input(AgentInput::follow_up("hello"));
        session.drive();

        assert_eq!(
            session.transcript().records(),
            &[
                TranscriptRecord::TurnStarted { turn_id: TurnId(1) },
                TranscriptRecord::UserMessage("hello".to_string()),
            ]
        );
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
        session.enqueue_input(AgentInput::follow_up("hi"));
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
    fn late_action_drain_does_not_leave_completed_request_pending() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::follow_up("hi"));
        session.drive();

        session.enqueue_input(AgentInput::ModelCompleted {
            turn_id: TurnId(1),
            assistant: AssistantMessage { items: Vec::new() },
        });
        session.drive();

        let late_actions = session.drain_actions();
        assert!(late_actions.is_empty());
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn session_blocks_edit_history_while_tool_actions_in_flight() {
        let mut session = AgentSession::new();
        session.enqueue_input(AgentInput::follow_up("go"));
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
        session.enqueue_input(AgentInput::follow_up("hi"));
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
