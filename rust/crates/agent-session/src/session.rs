use std::collections::VecDeque;

use agent_core::{AgentAction, AgentCoreLoop, AgentInput, AgentInputError, TranscriptItem, TurnId};

use crate::action::{SessionAction, StatelessModelRequestId};
use crate::action_queue::ActionQueue;
use crate::auto_compaction::{self, AutoCompactionSettings};
use crate::event::{SessionActionKind, SessionEvent};
use crate::input::{SessionInput, SessionInputError};
use crate::model_context::ModelContext;
use crate::transcript_store::{
    CompactionPlan, CompactionSettings, TranscriptStore, TranscriptStoreError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryOperationError {
    /// The session cannot currently edit its history because its durable leaf
    /// is mid-turn and there is no active core turn that can be interrupted to
    /// close it.
    Busy,
    /// An underlying transcript-store error: entry not found, invalid summary
    /// span, not at a turn boundary, or a stale edit plan.
    Store(TranscriptStoreError),
}

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
    compaction_request_queue: VecDeque<QueuedCompactionRequest>,
    pending_compaction: Option<PendingCompaction>,
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
            compaction_request_queue: VecDeque::new(),
            pending_compaction: None,
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

    /// Queue a compaction request for the next safe model-context barrier.
    ///
    /// The request may be made while the session is busy. The session starts it
    /// when idle at a turn boundary, or when the core has requested a model
    /// call that has not yet been exposed to the harness. Auto-compaction uses
    /// the same queue.
    pub fn request_compaction(&mut self, settings: CompactionSettings) {
        self.compaction_request_queue
            .push_back(QueuedCompactionRequest {
                settings,
                source: CompactionRequestSource::Requested,
            });
        self.maybe_start_idle_compaction_request();
    }

    /// Apply a compaction plan immediately.
    ///
    /// The plan must match the current transcript shape and the current leaf
    /// must already be at a turn boundary. For compaction while a model request
    /// is being started, use `request_compaction` so the session can hold the
    /// model request at the barrier.
    pub fn compact(
        &mut self,
        plan: CompactionPlan,
        summary: impl Into<String>,
    ) -> Result<(), HistoryOperationError> {
        let summary = summary.into();
        self.transcript_store
            .validate_plan_matches(&plan)
            .map_err(HistoryOperationError::Store)?;
        self.apply_history_operation(SessionEvent::HistoryCompacted, |store| {
            store
                .apply_compaction(plan, summary)
                .map_err(HistoryOperationError::Store)
        })
    }

    /// Rewind the active transcript path to a prior turn boundary.
    ///
    /// `leaf_id = Some(id)` moves the active leaf to `id`; `leaf_id = None`
    /// resets the session to the empty root. Like `compact`, rewind is a
    /// stop-the-world edit that preserves queued user inputs while invalidating
    /// obsolete external work.
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), HistoryOperationError> {
        self.validate_rewind_target(leaf_id)
            .map_err(HistoryOperationError::Store)?;
        self.apply_history_operation(SessionEvent::HistoryRewound, |store| {
            match leaf_id {
                Some(leaf_id) => store
                    .branch_at_turn_boundary(leaf_id)
                    .map_err(HistoryOperationError::Store)?,
                None => store.reset_leaf(),
            }
            Ok(())
        })
    }

    fn validate_rewind_target(&self, leaf_id: Option<&str>) -> Result<(), TranscriptStoreError> {
        match leaf_id {
            Some(leaf_id) if !self.transcript_store.contains_entry(leaf_id) => {
                Err(TranscriptStoreError::EntryNotFound)
            }
            leaf_id if !self.transcript_store.is_turn_boundary_leaf(leaf_id) => {
                Err(TranscriptStoreError::NotTurnBoundary)
            }
            _ => Ok(()),
        }
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
            compaction_request_queue: VecDeque::new(),
            pending_compaction: None,
            next_stateless_model_request_id: StatelessModelRequestId::first(),
        }
    }

    pub fn from_transcript_store(
        transcript_store: TranscriptStore,
    ) -> Result<Self, HistoryOperationError> {
        if !transcript_store.is_turn_boundary() {
            return Err(HistoryOperationError::Store(
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
            compaction_request_queue: VecDeque::new(),
            pending_compaction: None,
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
            SessionInput::ModelStatelessCompleted { request_id, text } => {
                self.complete_stateless_model(request_id, text);
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
            self.invalidate_session_work("interrupted");
        } else if self.pending_compaction.is_some()
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
        if self.pending_compaction.is_some() {
            return;
        }
        self.core.drive();
        self.absorb_core_transcript_items();
        self.absorb_core_actions();
        self.maybe_start_idle_compaction_request();
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
    /// caller drains the observable outbox later. Session-wide cancellation is
    /// retained until the caller drains it; stale start actions can be discarded
    /// when the session invalidates work.
    ///
    /// Transcript items are absorbed into the store inside `drive`, so there is no
    /// analogous `drain_transcript_items` on the session.
    pub fn drain_actions(&mut self) -> Vec<SessionAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        self.event_outbox.drain(..).collect()
    }

    fn apply_history_operation<Output>(
        &mut self,
        event: SessionEvent,
        apply: impl FnOnce(&mut TranscriptStore) -> Result<Output, HistoryOperationError>,
    ) -> Result<Output, HistoryOperationError> {
        let queued_user_inputs = self.prepare_for_history_operation()?;
        let output = match apply(&mut self.transcript_store) {
            Ok(output) => output,
            Err(error) => {
                self.restore_queued_user_inputs(queued_user_inputs);
                return Err(error);
            }
        };
        self.rehydrate_core_from_transcript_store();
        self.restore_queued_user_inputs(queued_user_inputs);
        self.event_outbox.push_back(event);
        Ok(output)
    }

    fn prepare_for_history_operation(&mut self) -> Result<Vec<AgentInput>, HistoryOperationError> {
        let queued_user_inputs = self.core.drain_pending_inputs();
        self.invalidate_session_work("history edited");
        if !self.core.is_idle() {
            if self.core.enqueue_input(AgentInput::Interrupt).is_err() {
                self.restore_queued_user_inputs(queued_user_inputs);
                return Err(HistoryOperationError::Busy);
            }
            self.core.drive();
            self.absorb_core_transcript_items();
            self.absorb_core_actions();
        }
        self.compaction_request_queue.clear();

        if !self.transcript_store.is_turn_boundary() {
            self.restore_queued_user_inputs(queued_user_inputs);
            return Err(HistoryOperationError::Busy);
        }
        Ok(queued_user_inputs)
    }

    fn restore_queued_user_inputs(&mut self, inputs: Vec<AgentInput>) {
        for input in inputs {
            self.core
                .enqueue_input(input)
                .expect("drained user input remains valid when restored");
        }
    }

    /// Produce an unregistered `AgentSession` whose context branches from
    /// `leaf_id` (or the root when `None`). The source session is unchanged;
    /// the caller is responsible for registering the fork if desired.
    ///
    /// Fork is separate from `compact` / `rewind` because it reads the context
    /// and produces a new session rather than mutating the source in place.
    pub fn fork(&self, leaf_id: Option<&str>) -> Result<AgentSession, HistoryOperationError> {
        let transcript_store = self
            .transcript_store
            .create_branched_store_at_turn_boundary(leaf_id)
            .map_err(HistoryOperationError::Store)?;
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
                if self.maybe_start_model_barrier_compaction_request(action.clone()) {
                    return;
                }
                self.expose_agent_action(action);
            }
            AgentAction::RequestTool { .. } => self.expose_agent_action(action),
            AgentAction::CancelTurn { .. } => {
                self.invalidate_session_work("turn cancelled");
            }
        }
    }

    fn maybe_start_model_barrier_compaction_request(&mut self, held_action: AgentAction) -> bool {
        if self.maybe_start_queued_compaction_request(Some(held_action.clone())) {
            return true;
        }
        self.queue_auto_compaction_if_needed();
        self.maybe_start_queued_compaction_request(Some(held_action))
    }

    fn queue_auto_compaction_if_needed(&mut self) {
        let Some(settings) = self.auto_compaction else {
            return;
        };
        if self.pending_compaction.is_some()
            || self
                .compaction_request_queue
                .iter()
                .any(|queued| queued.source == CompactionRequestSource::Auto)
        {
            return;
        }
        if auto_compaction::prepare_auto_compaction(&self.transcript_store, settings).is_none() {
            return;
        }

        self.compaction_request_queue
            .push_back(QueuedCompactionRequest {
                settings: CompactionSettings {
                    keep_recent_tokens: settings.keep_recent_tokens,
                },
                source: CompactionRequestSource::Auto,
            });
    }

    fn maybe_start_idle_compaction_request(&mut self) {
        if !self.can_start_idle_compaction_request() {
            return;
        }
        self.maybe_start_queued_compaction_request(None);
    }

    fn can_start_idle_compaction_request(&self) -> bool {
        self.core.is_idle()
            && self.transcript_store.is_turn_boundary()
            && !self.core.has_pending_work()
            && self.action_queue.is_empty()
            && self.pending_compaction.is_none()
            && !self.compaction_request_queue.is_empty()
    }

    fn maybe_start_queued_compaction_request(&mut self, held_action: Option<AgentAction>) -> bool {
        if self.pending_compaction.is_some() {
            return false;
        }

        let mut held_action = held_action;
        while let Some(queued) = self.compaction_request_queue.pop_front() {
            let Some(plan) = self.transcript_store.prepare_compaction(queued.settings) else {
                continue;
            };
            self.start_compaction_request(plan, held_action.take(), queued.source);
            return true;
        }
        false
    }

    fn start_compaction_request(
        &mut self,
        plan: CompactionPlan,
        held_action: Option<AgentAction>,
        source: CompactionRequestSource,
    ) {
        let request_id =
            StatelessModelRequestId::take_next(&mut self.next_stateless_model_request_id);
        let request = auto_compaction::compaction_request(&plan);
        self.pending_compaction = Some(PendingCompaction {
            request_id,
            plan,
            held_action,
            source,
        });
        self.push_session_action(SessionAction::RequestModelStateless {
            request_id,
            request,
        });
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
            AgentAction::CancelTurn { .. } => SessionAction::CancelSessionWork,
        };
        self.push_session_action(session_action);
    }

    fn push_session_action(&mut self, action: SessionAction) {
        self.event_outbox.push_back(SessionEvent::ActionRequested {
            action: action.clone(),
        });
        self.action_outbox.push_back(action);
    }

    fn complete_stateless_model(&mut self, request_id: StatelessModelRequestId, summary: String) {
        let Some(pending) = self.take_matching_pending_compaction(request_id) else {
            return;
        };

        self.event_outbox.push_back(SessionEvent::ActionCompleted {
            kind: SessionActionKind::ModelStateless,
            id: request_id.0.to_string(),
        });

        if let Err(error) = self.apply_compaction_request(pending.plan, summary) {
            self.event_outbox.push_back(SessionEvent::ActionFailed {
                kind: SessionActionKind::ModelStateless,
                id: request_id.0.to_string(),
                error: format!("{error:?}"),
            });
        } else {
            self.event_outbox.push_back(SessionEvent::HistoryCompacted);
        }
        if let Some(held_action) = pending.held_action {
            self.release_held_action_after_compaction_request(
                held_action,
                pending.source == CompactionRequestSource::Requested,
            );
        } else {
            self.maybe_start_idle_compaction_request();
        }
    }

    fn fail_stateless_model(&mut self, request_id: StatelessModelRequestId, error: String) {
        let Some(pending) = self.take_matching_pending_compaction(request_id) else {
            return;
        };
        self.event_outbox.push_back(SessionEvent::ActionFailed {
            kind: SessionActionKind::ModelStateless,
            id: request_id.0.to_string(),
            error,
        });
        if let Some(held_action) = pending.held_action {
            self.expose_agent_action(held_action);
        } else {
            self.maybe_start_idle_compaction_request();
        }
    }

    fn release_held_action_after_compaction_request(
        &mut self,
        held_action: AgentAction,
        recheck_auto_compaction: bool,
    ) {
        if self.maybe_start_queued_compaction_request(Some(held_action.clone())) {
            return;
        }
        if recheck_auto_compaction {
            self.queue_auto_compaction_if_needed();
            if self.maybe_start_queued_compaction_request(Some(held_action.clone())) {
                return;
            }
        }
        self.expose_agent_action(held_action);
    }

    fn take_matching_pending_compaction(
        &mut self,
        request_id: StatelessModelRequestId,
    ) -> Option<PendingCompaction> {
        if self
            .pending_compaction
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
            return self.pending_compaction.take();
        }
        None
    }

    fn apply_compaction_request(
        &mut self,
        plan: CompactionPlan,
        summary: String,
    ) -> Result<(), HistoryOperationError> {
        self.transcript_store
            .validate_plan_fingerprint(&plan)
            .map_err(HistoryOperationError::Store)?;
        self.transcript_store
            .apply_compaction(plan, summary)
            .map_err(HistoryOperationError::Store)
    }

    fn invalidate_session_work(&mut self, invalidation_reason: &str) {
        if self.clear_stale_session_work(invalidation_reason) {
            self.push_session_action(SessionAction::CancelSessionWork);
        }
        self.compaction_request_queue
            .retain(|queued| queued.source != CompactionRequestSource::Auto);
    }

    fn clear_stale_session_work(&mut self, invalidation_reason: &str) -> bool {
        let had_tracked_work = !self.action_queue.is_empty();
        let had_pending_compaction = self.pending_compaction.is_some();
        let had_start_action = self.action_outbox.iter().any(is_start_action);
        let had_existing_cancel = self.action_outbox.iter().any(is_cancel_action);

        self.action_queue.clear();
        self.fail_and_clear_pending_compaction(invalidation_reason);
        self.action_outbox.retain(is_cancel_action);

        (had_tracked_work || had_pending_compaction || had_start_action) && !had_existing_cancel
    }

    fn fail_and_clear_pending_compaction(&mut self, error: &str) {
        let Some(pending) = self.pending_compaction.take() else {
            return;
        };
        self.event_outbox.push_back(SessionEvent::ActionFailed {
            kind: SessionActionKind::ModelStateless,
            id: pending.request_id.0.to_string(),
            error: error.to_string(),
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
        self.action_outbox.retain(is_cancel_action);
        self.compaction_request_queue.clear();
        self.pending_compaction = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedCompactionRequest {
    settings: CompactionSettings,
    source: CompactionRequestSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionRequestSource {
    Requested,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCompaction {
    request_id: StatelessModelRequestId,
    plan: CompactionPlan,
    held_action: Option<AgentAction>,
    source: CompactionRequestSource,
}

fn is_start_action(action: &SessionAction) -> bool {
    matches!(
        action,
        SessionAction::RequestModel { .. }
            | SessionAction::RequestTool { .. }
            | SessionAction::RequestModelStateless { .. }
    )
}

fn is_cancel_action(action: &SessionAction) -> bool {
    matches!(action, SessionAction::CancelSessionWork)
}

#[cfg(test)]
mod tests;
