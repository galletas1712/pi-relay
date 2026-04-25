use std::collections::VecDeque;

use agent_core::{AgentAction, AgentCoreLoop, AgentInput, AgentInputError, TranscriptItem, TurnId};

use crate::action::{SessionAction, StatelessModelRequestId};
use crate::action_queue::ActionQueue;
use crate::auto_compaction::{
    self, AutoCompactionSettings, PendingStatelessModel, PendingStatelessModelKind,
    StatelessModelOutput,
};
use crate::event::{HistoryEditKind, SessionActionKind, SessionEvent};
use crate::input::{SessionInput, SessionInputError};
use crate::model_context::ModelContext;
use crate::transcript_store::{
    compaction_summary, HistoryEdit, HistoryEditError, PendingWork, SummarizeSpan, TranscriptStore,
    TranscriptStoreError,
};

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// point at which the session's history can be safely replaced, forked,
/// rewound, or resumed after consulting external model/tool work. The
/// `TranscriptStore` is the sole owner of durable transcript items; the core
/// only buffers items produced in the current run until the session absorbs
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub(crate) core: AgentCoreLoop,
    pub(crate) transcript_store: TranscriptStore,
    action_queue: ActionQueue,
    action_outbox: VecDeque<SessionAction>,
    event_outbox: VecDeque<SessionEvent>,
    auto_compaction: Option<AutoCompactionSettings>,
    pending_stateless_model: Option<PendingStatelessModel>,
    next_stateless_model_request_id: StatelessModelRequestId,
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
            transcript_store: TranscriptStore::new(),
            action_queue: ActionQueue::new(),
            action_outbox: VecDeque::new(),
            event_outbox: VecDeque::new(),
            auto_compaction: None,
            pending_stateless_model: None,
            next_stateless_model_request_id: StatelessModelRequestId::first(),
        }
    }

    pub fn with_auto_compaction(mut self, settings: AutoCompactionSettings) -> Self {
        self.auto_compaction = Some(settings);
        self
    }

    pub fn set_auto_compaction(&mut self, settings: Option<AutoCompactionSettings>) {
        self.auto_compaction = settings;
    }

    pub fn auto_compaction(&self) -> Option<AutoCompactionSettings> {
        self.auto_compaction
    }

    pub fn from_transcript_items(items: Vec<TranscriptItem>) -> Self {
        Self::from_model_context(ModelContext::from_transcript_items_recovering_crashed_tail(
            items,
        ))
    }

    pub fn from_model_context(model_context: ModelContext) -> Self {
        let model_context = if model_context.is_turn_boundary() {
            model_context
        } else {
            ModelContext::from_transcript_items_recovering_crashed_tail(
                model_context.into_transcript_items(),
            )
        };
        let last_turn_id = model_context.last_turn_id();
        let transcript_store = TranscriptStore::from_model_context(&model_context);
        Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            transcript_store,
            action_queue: ActionQueue::new(),
            action_outbox: VecDeque::new(),
            event_outbox: VecDeque::new(),
            auto_compaction: None,
            pending_stateless_model: None,
            next_stateless_model_request_id: StatelessModelRequestId::first(),
        }
    }

    pub fn from_transcript_store(
        transcript_store: TranscriptStore,
    ) -> Result<Self, HistoryEditError> {
        if !transcript_store.is_turn_boundary() {
            return Err(HistoryEditError::Store(
                TranscriptStoreError::NotTurnBoundary,
            ));
        }

        let model_context = transcript_store.model_context();
        let last_turn_id = model_context.last_turn_id();
        Ok(Self {
            core: AgentCoreLoop::resume_at_boundary(last_turn_id),
            transcript_store,
            action_queue: ActionQueue::new(),
            action_outbox: VecDeque::new(),
            event_outbox: VecDeque::new(),
            auto_compaction: None,
            pending_stateless_model: None,
            next_stateless_model_request_id: StatelessModelRequestId::first(),
        })
    }

    /// Enqueue a new input into the underlying core loop.
    ///
    /// This is the only supported way to feed the core from outside the
    /// session; the core itself is not exposed so context absorption in `drive`
    /// cannot be bypassed.
    ///
    /// `ModelCompleted` / `ModelFailed` / `ToolCompleted` clear the matching
    /// entry from the session's internal action queue. Stale completions (no
    /// matching pending entry, e.g. after an interrupt) are removed with no
    /// effect.
    pub fn enqueue_input(&mut self, input: AgentInput) -> Result<(), AgentInputError> {
        self.enqueue_agent_input(input)
    }

    pub fn enqueue_session_input(
        &mut self,
        input: impl Into<SessionInput>,
    ) -> Result<(), SessionInputError> {
        let input = input.into();
        input.validate()?;
        match input {
            SessionInput::Agent(input) => self
                .enqueue_agent_input(input)
                .map_err(SessionInputError::Agent),
            SessionInput::ModelStatelessCompleted { request_id, output } => {
                self.complete_stateless_model(request_id, output);
                Ok(())
            }
            SessionInput::ModelStatelessFailed { request_id, error } => {
                self.fail_stateless_model(request_id, error);
                Ok(())
            }
        }
    }

    fn enqueue_agent_input(&mut self, input: AgentInput) -> Result<(), AgentInputError> {
        input.validate()?;
        if matches!(input, AgentInput::Interrupt) {
            self.clear_pending_stateless_model("interrupted");
        } else if self.pending_stateless_model.is_some()
            && matches!(
                input,
                AgentInput::ModelCompleted { .. }
                    | AgentInput::ModelFailed { .. }
                    | AgentInput::ToolCompleted { .. }
            )
        {
            return Ok(());
        }

        if self.action_queue.record_input(&input) {
            self.push_agent_completion_event(&input);
        }
        self.drop_completed_action_from_outbox(&input);
        self.core.enqueue_input(input)
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

    /// Materialized view of the session history derived from the transcript
    /// store.
    /// With a compaction present, the latest summary is inlined ahead of the
    /// kept suffix so downstream callers see a single ordered transcript-item
    /// stream.
    pub fn model_context(&self) -> ModelContext {
        self.transcript_store.model_context()
    }

    pub fn transcript_store(&self) -> &TranscriptStore {
        &self.transcript_store
    }

    /// Drive the core to quiescence and append any transcript items it emitted
    /// to the session store. This is the only supported way to advance a
    /// session; the store remains the sole owner of durable history.
    pub fn drive(&mut self) {
        self.core.drive();
        self.absorb_core_transcript_items();
        self.absorb_core_actions();
    }

    /// Drain every queued user input (Steer then FollowUp) from the
    /// underlying core mailbox without advancing the session. Preserves the
    /// `from` and `kind` tags each input was enqueued with.
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
    /// `drive`, so model/tool completions can clear pending work even if the
    /// caller drains the observable outbox later. History edits still require
    /// the observable outbox itself to be empty, so callers cannot drop an
    /// undrained cancellation action by editing first.
    ///
    /// Transcript items are absorbed into the store inside `drive`, so there is no
    /// analogous `drain_transcript_items` on the session.
    pub fn drain_actions(&mut self) -> Vec<SessionAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        self.event_outbox.drain(..).collect()
    }

    /// True when the session's history can be edited: core idle, context at a
    /// turn boundary, mailbox empty, no in-flight drained actions, and no
    /// undrained observable actions or caller-tracked background work.
    pub fn can_edit_history(&self, pending: PendingWork) -> bool {
        self.core.is_idle()
            && self.transcript_store.is_turn_boundary()
            && !self.core.has_pending_work()
            && self.action_queue.is_empty()
            && self.action_outbox.is_empty()
            && self.pending_stateless_model.is_none()
            && pending.is_empty()
    }

    /// Apply a `HistoryEdit` operation (`Compact`, `Rewind`,
    /// `ReplaceModelContext`) to this session's context.
    ///
    /// The quiescence check runs once here; the op's `apply` then mutates the
    /// context directly. On success the core loop is rehydrated from the new
    /// context so the next `drive` resumes from the correct turn id.
    pub fn edit<E: HistoryEdit>(
        &mut self,
        pending: PendingWork,
        edit: E,
    ) -> Result<E::Output, HistoryEditError> {
        if !self.can_edit_history(pending) {
            return Err(HistoryEditError::Busy);
        }
        let output = edit.apply(&mut self.transcript_store)?;
        self.rehydrate_core_from_transcript_store();
        self.event_outbox.push_back(SessionEvent::HistoryEdited {
            kind: HistoryEditKind::HistoryEdit,
        });
        Ok(output)
    }

    /// Produce an unregistered `AgentSession` whose context branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    ///
    /// Fork stays as a direct method rather than a `HistoryEdit` impl because
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
        let transcript_store = self
            .transcript_store
            .create_branched_store_at_turn_boundary(leaf_id)
            .map_err(HistoryEditError::Store)?;
        AgentSession::from_transcript_store(transcript_store)
    }

    fn absorb_core_transcript_items(&mut self) {
        let items = self.core.drain_transcript_items();
        if items.is_empty() {
            return;
        }
        let entry_ids = self.transcript_store.append_transcript_items(items.clone());
        for (entry_id, item) in entry_ids.into_iter().zip(items) {
            self.event_outbox
                .push_back(SessionEvent::TranscriptItemAppended { entry_id, item });
        }
    }

    fn absorb_core_actions(&mut self) {
        let actions = self.core.drain_actions();
        if actions.is_empty() {
            return;
        }
        for action in actions {
            self.handle_core_action(action);
        }
    }

    fn handle_core_action(&mut self, action: AgentAction) {
        match action {
            AgentAction::RequestModel { .. } => {
                if self.maybe_start_auto_compaction(action.clone()) {
                    return;
                }
                self.expose_agent_action(action);
            }
            AgentAction::RequestTool { .. } => self.expose_agent_action(action),
            AgentAction::CancelTurn { turn_id } => {
                self.clear_pending_stateless_model_for_turn(turn_id);
                self.remove_actions_for_turn(turn_id);
                self.expose_agent_action(AgentAction::CancelTurn { turn_id });
            }
        }
    }

    fn maybe_start_auto_compaction(&mut self, held_model_action: AgentAction) -> bool {
        if self.pending_stateless_model.is_some() {
            return false;
        }
        let Some(settings) = self.auto_compaction else {
            return false;
        };
        let Some(plan) = auto_compaction::prepare_auto_compaction(&self.transcript_store, settings)
        else {
            return false;
        };

        let request_id =
            StatelessModelRequestId::take_next(&mut self.next_stateless_model_request_id);
        let request = auto_compaction::compaction_request(&plan);
        self.pending_stateless_model = Some(PendingStatelessModel {
            request_id,
            kind: PendingStatelessModelKind::Compaction {
                plan,
                held_model_action,
            },
        });
        self.push_session_action(SessionAction::RequestModelStateless {
            request_id,
            request,
        });
        true
    }

    fn expose_agent_action(&mut self, action: AgentAction) {
        self.action_queue
            .record_drained(std::slice::from_ref(&action));
        let session_action = match action {
            AgentAction::RequestModel { action_id, turn_id } => SessionAction::RequestModel {
                action_id,
                turn_id,
                model_context: self.model_context(),
            },
            AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => SessionAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            },
            AgentAction::CancelTurn { turn_id } => SessionAction::CancelTurn { turn_id },
        };
        self.push_session_action(session_action);
    }

    fn push_session_action(&mut self, action: SessionAction) {
        self.event_outbox.push_back(SessionEvent::ActionRequested {
            action: action.clone(),
        });
        self.action_outbox.push_back(action);
    }

    fn complete_stateless_model(
        &mut self,
        request_id: StatelessModelRequestId,
        output: StatelessModelOutput,
    ) {
        let Some(pending) = self.take_matching_stateless_model(request_id) else {
            return;
        };

        self.event_outbox.push_back(SessionEvent::ActionCompleted {
            kind: SessionActionKind::ModelStateless,
            id: request_id.0.to_string(),
        });

        match pending.kind {
            PendingStatelessModelKind::Compaction {
                plan,
                held_model_action,
            } => {
                let StatelessModelOutput::Text(summary) = output;
                if let Err(error) = self.apply_pending_compaction(plan, summary) {
                    self.event_outbox.push_back(SessionEvent::ActionFailed {
                        kind: SessionActionKind::ModelStateless,
                        id: request_id.0.to_string(),
                        error: format!("{error:?}"),
                    });
                } else {
                    self.event_outbox.push_back(SessionEvent::HistoryEdited {
                        kind: HistoryEditKind::Compact,
                    });
                }
                self.expose_agent_action(held_model_action);
            }
        }
    }

    fn fail_stateless_model(&mut self, request_id: StatelessModelRequestId, error: String) {
        let Some(pending) = self.take_matching_stateless_model(request_id) else {
            return;
        };
        self.event_outbox.push_back(SessionEvent::ActionFailed {
            kind: SessionActionKind::ModelStateless,
            id: request_id.0.to_string(),
            error,
        });
        match pending.kind {
            PendingStatelessModelKind::Compaction {
                held_model_action, ..
            } => self.expose_agent_action(held_model_action),
        }
    }

    fn take_matching_stateless_model(
        &mut self,
        request_id: StatelessModelRequestId,
    ) -> Option<PendingStatelessModel> {
        if self
            .pending_stateless_model
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.action_outbox.retain(|action| {
                !matches!(
                    action,
                    SessionAction::RequestModelStateless {
                        request_id: queued_request_id,
                        ..
                    } if *queued_request_id == request_id
                )
            });
            return self.pending_stateless_model.take();
        }
        None
    }

    fn apply_pending_compaction(
        &mut self,
        plan: crate::transcript_store::CompactionPlan,
        summary: String,
    ) -> Result<(), HistoryEditError> {
        let first_kept_entry_id = plan.first_kept_entry_id.clone();
        let tokens_before = plan.tokens_before;
        SummarizeSpan {
            plan: plan.summary_span,
            summary: compaction_summary(summary, first_kept_entry_id, tokens_before),
        }
        .apply(&mut self.transcript_store)
    }

    fn clear_pending_stateless_model(&mut self, error: &str) {
        let Some(pending) = self.pending_stateless_model.take() else {
            return;
        };
        let request_id = pending.request_id;
        self.action_outbox.retain(|action| {
            !matches!(
                action,
                SessionAction::RequestModelStateless {
                    request_id: queued_request_id,
                    ..
                } if *queued_request_id == request_id
            )
        });
        self.event_outbox.push_back(SessionEvent::ActionFailed {
            kind: SessionActionKind::ModelStateless,
            id: request_id.0.to_string(),
            error: error.to_string(),
        });
    }

    fn clear_pending_stateless_model_for_turn(&mut self, turn_id: TurnId) {
        let clear = self
            .pending_stateless_model
            .as_ref()
            .is_some_and(|pending| {
                matches!(
                    &pending.kind,
                    PendingStatelessModelKind::Compaction {
                        held_model_action: AgentAction::RequestModel {
                            turn_id: held_turn_id,
                            ..
                        },
                        ..
                    } if *held_turn_id == turn_id
                )
            });
        if clear {
            self.clear_pending_stateless_model("turn cancelled");
        }
    }

    fn remove_actions_for_turn(&mut self, turn_id: TurnId) {
        self.action_outbox.retain(|queued| {
            !matches!(
                queued,
                SessionAction::RequestModel {
                    turn_id: queued_turn_id,
                    ..
                } | SessionAction::RequestTool {
                    turn_id: queued_turn_id,
                    ..
                } if *queued_turn_id == turn_id
            )
        });
    }

    fn drop_completed_action_from_outbox(&mut self, input: &AgentInput) {
        let position = self
            .action_outbox
            .iter()
            .position(|action| match (action, input) {
                (
                    SessionAction::RequestModel {
                        action_id: action_action_id,
                        turn_id: action_turn_id,
                        ..
                    },
                    AgentInput::ModelCompleted {
                        action_id: input_action_id,
                        turn_id: input_turn_id,
                        ..
                    }
                    | AgentInput::ModelFailed {
                        action_id: input_action_id,
                        turn_id: input_turn_id,
                        ..
                    },
                ) => action_action_id == input_action_id && action_turn_id == input_turn_id,
                (
                    SessionAction::RequestTool {
                        action_id: action_action_id,
                        turn_id: action_turn_id,
                        tool_call,
                    },
                    AgentInput::ToolCompleted {
                        action_id: input_action_id,
                        turn_id: input_turn_id,
                        result,
                    },
                ) => {
                    action_action_id == input_action_id
                        && action_turn_id == input_turn_id
                        && tool_call.id == result.tool_call_id
                        && tool_call.tool_name == result.tool_name
                }
                _ => false,
            });
        if let Some(position) = position {
            self.action_outbox.remove(position);
        }
    }

    fn push_agent_completion_event(&mut self, input: &AgentInput) {
        match input {
            AgentInput::ModelCompleted { action_id, .. } => {
                self.event_outbox.push_back(SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Model,
                    id: action_id.0.to_string(),
                });
            }
            AgentInput::ModelFailed {
                action_id, error, ..
            } => {
                self.event_outbox.push_back(SessionEvent::ActionFailed {
                    kind: SessionActionKind::Model,
                    id: action_id.0.to_string(),
                    error: error.clone(),
                });
            }
            AgentInput::ToolCompleted { action_id, .. } => {
                self.event_outbox.push_back(SessionEvent::ActionCompleted {
                    kind: SessionActionKind::Tool,
                    id: action_id.0.to_string(),
                });
            }
            _ => {}
        }
    }

    pub(crate) fn rehydrate_core_from_transcript_store(&mut self) {
        let last_turn_id = self.transcript_store.model_context().last_turn_id();
        let next_action_id = self.core.next_action_id();
        self.core =
            AgentCoreLoop::resume_at_boundary_with_next_action_id(last_turn_id, next_action_id);
        // Any actions tracked as pending belong to a prior run we're no
        // longer driving; reset the queue so a rehydrated session does not
        // block edits forever.
        self.action_queue.clear();
        self.action_outbox.clear();
        self.pending_stateless_model = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_compaction::StatelessModelOutput;
    use crate::transcript_store::{Compact, CompactionSettings, ReplaceModelContext, Rewind};
    use agent_core::{
        ActionId, AssistantItem, AssistantMessage, InjectedMessage, ToolCall, ToolCallId,
        ToolResultMessage, ToolResultStatus, TurnId, TurnOutcome,
    };

    fn finished_model_context(input: &str) -> ModelContext {
        ModelContext::from_transcript_items(vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            TranscriptItem::UserMessage(input.to_string()),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ])
    }

    fn finished_turn(turn_id: u64, user: &str, assistant: &str) -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::TurnStarted {
                turn_id: TurnId(turn_id),
            },
            TranscriptItem::UserMessage(user.to_string()),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(assistant.to_string())],
            }),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(turn_id),
                outcome: TurnOutcome::Graceful,
            },
        ]
    }

    fn assert_single_request_model(
        actions: Vec<SessionAction>,
        expected_action_id: ActionId,
        expected_turn_id: TurnId,
    ) -> ModelContext {
        let [SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context,
        }] = actions.as_slice()
        else {
            panic!("expected one RequestModel action, got {actions:?}");
        };
        assert_eq!(
            (*action_id, *turn_id),
            (expected_action_id, expected_turn_id)
        );
        model_context.clone()
    }

    fn session_with_compactable_history() -> AgentSession {
        let mut items = Vec::new();
        items.extend(finished_turn(
            1,
            "first user message with enough text to count",
            "first assistant message with enough text to count",
        ));
        items.extend(finished_turn(
            2,
            "second user message with enough text to count",
            "second assistant message with enough text to count",
        ));
        AgentSession::from_model_context(ModelContext::from_transcript_items(items))
            .with_auto_compaction(AutoCompactionSettings::new(1, 1))
    }

    #[test]
    fn model_context_replacement_is_only_allowed_at_turn_boundary() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("plain follow-up is valid");

        let busy = session
            .edit(
                PendingWork::NONE,
                ReplaceModelContext {
                    replacement: finished_model_context("compact"),
                },
            )
            .expect_err("running sessions cannot edit history");
        assert_eq!(busy, HistoryEditError::Busy);

        let mut session = AgentSession::from_model_context(finished_model_context("hello"));

        let old = session
            .edit(
                PendingWork::NONE,
                ReplaceModelContext {
                    replacement: finished_model_context("compact"),
                },
            )
            .expect("idle session can swap model_context");

        assert_eq!(old.last_turn_id(), TurnId(1));
        assert_eq!(
            session.model_context().transcript_items()[1],
            TranscriptItem::UserMessage("compact".to_string())
        );
    }

    #[test]
    fn rewind_and_fork_only_accept_turn_finished_entries() {
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("first".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage("second".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                },
            ]));
        let mid_turn_id = session.transcript_store().entries()[1].id.clone();
        let turn_one_end_id = session.transcript_store().entries()[2].id.clone();

        assert_eq!(
            session.edit(
                PendingWork::NONE,
                Rewind {
                    leaf_id: Some(mid_turn_id.clone())
                }
            ),
            Err(HistoryEditError::Store(
                TranscriptStoreError::NotTurnBoundary
            ))
        );
        assert_eq!(
            session
                .fork(PendingWork::NONE, Some(&mid_turn_id))
                .map(|_| ()),
            Err(HistoryEditError::Store(
                TranscriptStoreError::NotTurnBoundary
            ))
        );

        session
            .edit(
                PendingWork::NONE,
                Rewind {
                    leaf_id: Some(turn_one_end_id.clone()),
                },
            )
            .expect("turn end is a valid rewind point");
        assert_eq!(session.model_context().last_turn_id(), TurnId(1));

        let fork = session
            .fork(PendingWork::NONE, Some(&turn_one_end_id))
            .expect("turn end is a valid fork point");
        assert_eq!(fork.model_context().last_turn_id(), TurnId(1));
    }

    #[test]
    fn compact_op_compacts_via_context_edit_dispatch() {
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("first".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("ok".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage("second".to_string()),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("ok2".to_string())],
                }),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(2),
                    outcome: TurnOutcome::Graceful,
                },
            ]));

        let plan = session
            .transcript_store()
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
        assert_eq!(
            session.model_context().latest_compaction_summary(),
            Some("s")
        );
    }

    #[test]
    fn can_edit_history_requires_idle_core_empty_queues_and_no_pending_work() {
        let mut session = AgentSession::new();

        session
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("plain follow-up is valid");
        assert!(!session.can_edit_history(PendingWork::NONE));
        assert!(!session.can_edit_history(PendingWork {
            background_tasks: 1,
        }));
    }

    #[test]
    fn context_tracks_core_turn_items() {
        let session = AgentSession::from_model_context(finished_model_context("hello"));

        assert_eq!(session.transcript_store().entries().len(), 3);
        assert!(session.transcript_store().is_turn_boundary());
        assert_eq!(session.model_context().last_turn_id(), TurnId(1));
    }

    #[test]
    fn drive_absorbs_core_items_into_the_session_context() {
        let mut session = AgentSession::new();
        let assistant = AssistantMessage {
            items: vec![AssistantItem::Text("hi".to_string())],
        };

        session
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("plain follow-up is valid");
        session.drive();
        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: assistant.clone(),
            })
            .expect("matching model completion is valid");
        session.drive();

        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hello".to_string()),
                TranscriptItem::AssistantMessage(assistant),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]
        );
        // Driving again absorbs nothing new; the core buffer was drained.
        session.drive();
        assert_eq!(session.model_context().transcript_items().len(), 4);
    }

    #[test]
    fn live_transcript_keeps_open_turns_open() {
        let mut session = AgentSession::new();

        session
            .enqueue_input(AgentInput::follow_up("hello"))
            .expect("plain follow-up is valid");
        session.drive();

        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hello".to_string()),
            ]
        );
    }

    #[test]
    fn rehydrating_an_incomplete_transcript_patches_a_crashed_finish() {
        let model_context = vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(7) },
            TranscriptItem::UserMessage("hello".to_string()),
        ];

        let session = AgentSession::from_transcript_items(model_context);

        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(7) },
                TranscriptItem::UserMessage("hello".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(7));
    }

    #[test]
    fn from_model_context_recovers_an_open_tail_as_crashed() {
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(7) },
                TranscriptItem::UserMessage("hello".to_string()),
            ]));

        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(7) },
                TranscriptItem::UserMessage("hello".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(7),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(session.is_idle());

        session
            .enqueue_input(AgentInput::follow_up("next"))
            .expect("plain follow-up is valid");
        session.drive();
        assert!(matches!(
            session.model_context().transcript_items().last(),
            Some(TranscriptItem::UserMessage(text)) if text == "next"
        ));
    }

    #[test]
    fn rehydrating_a_graceful_boundary_restores_idle_state() {
        let model_context = vec![
            TranscriptItem::TurnStarted { turn_id: TurnId(2) },
            TranscriptItem::UserMessage("hello".to_string()),
            TranscriptItem::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            },
        ];

        let session = AgentSession::from_transcript_items(model_context.clone());

        assert_eq!(
            session.model_context().transcript_items(),
            model_context.as_slice()
        );
        assert!(session.is_idle());
        assert_eq!(session.last_turn_id(), TurnId(2));
    }

    #[test]
    fn session_blocks_edit_history_until_drained_model_action_completes() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("hi"))
            .expect("plain follow-up is valid");
        session.drive();
        let actions = session.drain_actions();
        assert!(matches!(
            actions.as_slice(),
            [SessionAction::RequestModel { .. }]
        ));
        assert!(!session.can_edit_history(PendingWork::NONE));

        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: AssistantMessage { items: Vec::new() },
            })
            .expect("matching model completion is valid");
        session.drive();
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn model_failure_marks_turn_crashed_and_unblocks_history_edits() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("hi"))
            .expect("plain follow-up is valid");
        session.drive();
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(1));

        session
            .enqueue_input(AgentInput::ModelFailed {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                error: "provider failed".to_string(),
            })
            .expect("matching model failure is valid");
        session.drive();

        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hi".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Crashed,
                },
            ]
        );
        assert!(session.can_edit_history(PendingWork::NONE));
        assert!(session.drain_events().iter().any(|event| matches!(
            event,
            SessionEvent::ActionFailed {
                kind: SessionActionKind::Model,
                error,
                ..
            } if error == "provider failed"
        )));
    }

    #[test]
    fn auto_compaction_requests_stateless_model_before_releasing_model_request() {
        let mut session = session_with_compactable_history();
        session
            .enqueue_input(AgentInput::follow_up("third user message"))
            .expect("plain follow-up is valid");
        session.drive();

        let actions = session.drain_actions();
        let [SessionAction::RequestModelStateless {
            request_id,
            request,
        }] = actions.as_slice()
        else {
            panic!("expected stateless model compaction request, got {actions:?}");
        };
        assert!(request.input.iter().any(|block| {
            matches!(
                block,
                crate::auto_compaction::ModelContentBlock::Text { text }
                    if text.contains("first user message")
            )
        }));
        assert_eq!(session.model_context().latest_compaction_summary(), None);
        assert!(matches!(
            session.model_context().transcript_items().last(),
            Some(TranscriptItem::UserMessage(text)) if text == "third user message"
        ));
        assert!(!session.can_edit_history(PendingWork::NONE));

        let events = session.drain_events();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::TranscriptItemAppended {
                item: TranscriptItem::TurnStarted { turn_id: TurnId(3) },
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::ActionRequested {
                action: SessionAction::RequestModelStateless { .. }
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            SessionEvent::TranscriptItemAppended {
                item: TranscriptItem::Injected(_),
                ..
            }
        )));

        session
            .enqueue_session_input(SessionInput::ModelStatelessCompleted {
                request_id: *request_id,
                output: StatelessModelOutput::Text("summary text".to_string()),
            })
            .expect("stateless model completion should be accepted");

        let request_model_context =
            assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
        assert_eq!(request_model_context, session.model_context());
        assert_eq!(
            session.model_context().latest_compaction_summary(),
            Some("summary text")
        );
        assert!(matches!(
            session.model_context().transcript_items().last(),
            Some(TranscriptItem::UserMessage(text)) if text == "third user message"
        ));

        let events = session.drain_events();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::ActionCompleted {
                kind: SessionActionKind::ModelStateless,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::HistoryEdited {
                kind: HistoryEditKind::Compact
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::ActionRequested {
                action: SessionAction::RequestModel {
                    action_id: ActionId(1),
                    turn_id: TurnId(3),
                    ..
                }
            }
        )));
    }

    #[test]
    fn failed_stateless_model_compaction_releases_model_request_without_editing_context() {
        let mut session = session_with_compactable_history();
        session
            .enqueue_input(AgentInput::follow_up("third user message"))
            .expect("plain follow-up is valid");
        session.drive();
        let actions = session.drain_actions();
        let [SessionAction::RequestModelStateless { request_id, .. }] = actions.as_slice() else {
            panic!("expected stateless model compaction request, got {actions:?}");
        };

        session
            .enqueue_session_input(SessionInput::ModelStatelessFailed {
                request_id: *request_id,
                error: "no summary".to_string(),
            })
            .expect("stateless model failure should be accepted");

        let request_model_context =
            assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(3));
        assert_eq!(request_model_context, session.model_context());
        assert_eq!(session.model_context().latest_compaction_summary(), None);
        assert!(session.drain_events().iter().any(|event| matches!(
            event,
            SessionEvent::ActionFailed {
                kind: SessionActionKind::ModelStateless,
                error,
                ..
            } if error == "no summary"
        )));
    }

    #[test]
    fn stale_stateless_model_completion_does_not_unblock_pending_compaction() {
        let mut session = session_with_compactable_history();
        session
            .enqueue_input(AgentInput::follow_up("third user message"))
            .expect("plain follow-up is valid");
        session.drive();
        let actions = session.drain_actions();
        let [SessionAction::RequestModelStateless { request_id, .. }] = actions.as_slice() else {
            panic!("expected stateless model compaction request, got {actions:?}");
        };

        session
            .enqueue_session_input(SessionInput::ModelStatelessCompleted {
                request_id: StatelessModelRequestId(99),
                output: StatelessModelOutput::Text("wrong".to_string()),
            })
            .expect("stale stateless model completion should be accepted and ignored");
        assert!(session.drain_actions().is_empty());
        assert_eq!(session.model_context().latest_compaction_summary(), None);
        assert!(!session.can_edit_history(PendingWork::NONE));

        session
            .enqueue_session_input(SessionInput::ModelStatelessCompleted {
                request_id: *request_id,
                output: StatelessModelOutput::Text("right".to_string()),
            })
            .expect("matching stateless model completion should be accepted");
        assert!(matches!(
            session.drain_actions().as_slice(),
            [SessionAction::RequestModel { .. }]
        ));
        assert_eq!(
            session.model_context().latest_compaction_summary(),
            Some("right")
        );
    }

    #[test]
    fn late_action_drain_does_not_leave_completed_request_pending() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("hi"))
            .expect("plain follow-up is valid");
        session.drive();

        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: AssistantMessage { items: Vec::new() },
            })
            .expect("matching model completion is valid");
        session.drive();

        let late_actions = session.drain_actions();
        assert!(late_actions.is_empty());
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn stale_completion_after_history_edit_cannot_attach_to_reused_turn_id() {
        let mut session = AgentSession::from_model_context(finished_model_context("first"));
        let turn_one_end_id = session.transcript_store().entries()[2].id.clone();

        session
            .enqueue_input(AgentInput::follow_up("old second"))
            .expect("plain follow-up is valid");
        session.drive();
        assert_single_request_model(session.drain_actions(), ActionId(1), TurnId(2));
        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(2),
                assistant: AssistantMessage {
                    items: vec![AssistantItem::Text("old response".to_string())],
                },
            })
            .expect("matching model completion is valid");
        session.drive();
        assert!(session.can_edit_history(PendingWork::NONE));

        session
            .edit(
                PendingWork::NONE,
                Rewind {
                    leaf_id: Some(turn_one_end_id),
                },
            )
            .expect("completed history can rewind to turn one");
        session
            .enqueue_input(AgentInput::follow_up("new second"))
            .expect("plain follow-up is valid");
        session.drive();
        assert_single_request_model(session.drain_actions(), ActionId(2), TurnId(2));

        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(2),
                assistant: AssistantMessage {
                    items: vec![AssistantItem::Text("stale old response".to_string())],
                },
            })
            .expect("well-formed stale completion is valid input");
        session.drive();
        assert_eq!(
            session.model_context().transcript_items(),
            &[
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("first".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
                TranscriptItem::TurnStarted { turn_id: TurnId(2) },
                TranscriptItem::UserMessage("new second".to_string()),
            ]
        );
        assert!(!session.can_edit_history(PendingWork::NONE));

        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(2),
                assistant: AssistantMessage {
                    items: vec![AssistantItem::Text("new response".to_string())],
                },
            })
            .expect("matching model completion is valid");
        session.drive();
        assert_eq!(
            session.model_context().transcript_items().last(),
            Some(&TranscriptItem::TurnFinished {
                turn_id: TurnId(2),
                outcome: TurnOutcome::Graceful,
            })
        );
    }

    #[test]
    fn invalid_origin_tags_are_rejected_without_mutating_session_state() {
        let mut session = AgentSession::new();
        let result = session.enqueue_input(AgentInput::Steer {
            from: Some("parent".to_string()),
            kind: None,
            content: "half tagged".to_string(),
        });

        assert_eq!(result, Err(AgentInputError::UnpairedOriginTags));
        session.drive();
        assert!(session.model_context().transcript_items().is_empty());
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn session_blocks_edit_history_while_tool_actions_in_flight() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("go"))
            .expect("plain follow-up is valid");
        session.drive();
        session.drain_actions();

        let tool_call = ToolCall {
            id: ToolCallId(1),
            tool_name: "bash".to_string(),
            args_json: "{}".to_string(),
        };
        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(1),
                turn_id: TurnId(1),
                assistant: AssistantMessage {
                    items: vec![AssistantItem::ToolCall(tool_call.clone())],
                },
            })
            .expect("matching model completion is valid");
        session.drive();
        session.drain_actions();
        assert!(!session.can_edit_history(PendingWork::NONE));

        session
            .enqueue_input(AgentInput::ToolCompleted {
                action_id: ActionId(2),
                turn_id: TurnId(1),
                result: ToolResultMessage {
                    tool_call_id: ToolCallId(1),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    status: ToolResultStatus::Success,
                },
            })
            .expect("matching tool completion is valid");
        session.drive();
        // A second model request fires after the tool completes; the session
        // is still waiting on that completion, so edits remain blocked.
        assert!(!session.can_edit_history(PendingWork::NONE));
        session.drain_actions();
        session
            .enqueue_input(AgentInput::ModelCompleted {
                action_id: ActionId(3),
                turn_id: TurnId(1),
                assistant: AssistantMessage { items: Vec::new() },
            })
            .expect("matching model completion is valid");
        session.drive();
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn can_edit_history_walks_past_multiple_injected_entries() {
        // A context whose leaf is a run of back-to-back injected entries after
        // a TurnFinished is still at a turn boundary; can_edit_history must
        // see through the injected tail to the underlying TurnFinished.
        let mut session =
            AgentSession::from_model_context(ModelContext::from_transcript_items(vec![
                TranscriptItem::TurnStarted { turn_id: TurnId(1) },
                TranscriptItem::UserMessage("hi".to_string()),
                TranscriptItem::TurnFinished {
                    turn_id: TurnId(1),
                    outcome: TurnOutcome::Graceful,
                },
            ]));
        session
            .transcript_store
            .append_injected(InjectedMessage::new("note", "a"));
        session
            .transcript_store
            .append_injected(compaction_summary("b", "does-not-matter", 0));
        session
            .transcript_store
            .append_injected(InjectedMessage::new("note", "c"));

        assert!(session.transcript_store().is_turn_boundary());
        assert!(session.can_edit_history(PendingWork::NONE));
    }

    #[test]
    fn cancel_turn_clears_pending_actions_for_that_turn() {
        let mut session = AgentSession::new();
        session
            .enqueue_input(AgentInput::follow_up("hi"))
            .expect("plain follow-up is valid");
        session.drive();
        session.drain_actions();

        session
            .enqueue_input(AgentInput::Interrupt)
            .expect("interrupt is valid");
        session.drive();
        assert!(!session.can_edit_history(PendingWork::NONE));
        let actions = session.drain_actions();
        assert!(actions
            .iter()
            .any(|a| matches!(a, SessionAction::CancelTurn { .. })));
        assert!(session.can_edit_history(PendingWork::NONE));
    }
}
