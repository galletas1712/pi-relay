use std::collections::VecDeque;

#[cfg(test)]
use agent_core::UserMessage;
use agent_core::{AgentAction, AgentCoreLoop, AgentInput, AgentInputError, TranscriptItem, TurnId};
use agent_store::StoredSession;

use crate::action::{CompactionRequestId, SessionAction};
use crate::compaction_state::{CompactionBarrierModelRequest, CompactionState, RunningCompaction};
use crate::event::{SessionActionKind, SessionEvent};
use crate::external_work::ExternalWork;
use crate::input::{SessionInput, SessionInputError};
use crate::model_context::ModelContext;
use crate::transcript_store::{TranscriptStore, TranscriptStoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryOperationError {
    /// The session cannot currently edit its history because its durable leaf
    /// is mid-turn and there is no active core turn that can be interrupted to
    /// close it.
    Busy,
    /// An underlying transcript-store error: entry not found or not at a turn
    /// boundary.
    Store(TranscriptStoreError),
}

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// point at which the session's history can be safely compacted, forked,
/// rewound, or resumed after consulting external model/tool work. The
/// `TranscriptStore` is the sole owner of durable transcript items; the core
/// only buffers items produced in the current run until the session absorbs
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub(crate) core: AgentCoreLoop,
    pub(crate) transcript_store: TranscriptStore,
    external_work: ExternalWork,
    action_outbox: VecDeque<SessionAction>,
    event_outbox: VecDeque<SessionEvent>,
    context_tokens: Option<usize>,
    pending_model_context_tokens: Option<(TurnId, Option<usize>)>,
    compaction: CompactionState,
    next_compaction_request_id: CompactionRequestId,
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
            external_work: ExternalWork::new(),
            action_outbox: VecDeque::new(),
            event_outbox: VecDeque::new(),
            context_tokens: None,
            pending_model_context_tokens: None,
            compaction: CompactionState::Idle,
            next_compaction_request_id: CompactionRequestId::first(),
        }
    }

    /// Queue a remote compaction request for the next safe model-context barrier.
    ///
    /// The request may be made while the session is busy. The session starts it
    /// when idle at a turn boundary, or when the core has requested a model
    /// call that has not yet been exposed to the harness. The session does not
    /// decide when compaction is needed; the harness owns that policy and calls
    /// this method when it wants the session to compact.
    pub fn compact(&mut self) {
        self.compaction.request();
        self.maybe_start_compaction(None);
    }

    /// Rewind the active transcript path to a prior turn boundary.
    ///
    /// `leaf_id = Some(id)` moves the active leaf to `id`; `leaf_id = None`
    /// resets the session to the empty root. Like `compact`, rewind is a
    /// stop-the-world edit that preserves queued user inputs while invalidating
    /// obsolete external work.
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), HistoryOperationError> {
        match leaf_id {
            Some(leaf_id) if !self.transcript_store.contains_entry(leaf_id) => {
                return Err(HistoryOperationError::Store(
                    TranscriptStoreError::EntryNotFound,
                ));
            }
            leaf_id if !self.transcript_store.is_turn_boundary_leaf(leaf_id) => {
                return Err(HistoryOperationError::Store(
                    TranscriptStoreError::NotTurnBoundary,
                ));
            }
            _ => {}
        }

        let queued_user_inputs = self.prepare_for_history_operation()?;
        let result = match leaf_id {
            Some(leaf_id) => self
                .transcript_store
                .branch_at_turn_boundary(leaf_id)
                .map_err(HistoryOperationError::Store),
            None => {
                self.transcript_store.reset_leaf();
                Ok(())
            }
        };

        if let Err(error) = result {
            self.restore_queued_user_inputs(queued_user_inputs);
            return Err(error);
        }

        self.rehydrate_core_from_transcript_store();
        self.restore_queued_user_inputs(queued_user_inputs);
        self.event_outbox.push_back(SessionEvent::HistoryRewound);
        Ok(())
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
        let mut session = Self::new();
        session.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
        session.transcript_store = transcript_store;
        session
    }

    pub fn from_transcript_store(
        mut transcript_store: TranscriptStore,
    ) -> Result<Self, HistoryOperationError> {
        if !transcript_store.is_turn_boundary() {
            let items = transcript_store.model_context().into_transcript_items();
            let original_len = items.len();
            let recovered = ModelContext::from_transcript_items_recovering_crashed_tail(items)
                .into_transcript_items();
            if recovered.len() == original_len {
                return Err(HistoryOperationError::Store(
                    TranscriptStoreError::NotTurnBoundary,
                ));
            }
            transcript_store.append_transcript_items(recovered.into_iter().skip(original_len));
            if !transcript_store.is_turn_boundary() {
                return Err(HistoryOperationError::Store(
                    TranscriptStoreError::NotTurnBoundary,
                ));
            }
        }

        let model_context = transcript_store.model_context();
        let last_turn_id = model_context.last_turn_id();
        let mut session = Self::new();
        session.core = AgentCoreLoop::resume_at_boundary(last_turn_id);
        session.transcript_store = transcript_store;
        Ok(session)
    }

    /// Convert the durable transcript forest into a backend-neutral storage
    /// snapshot. Runtime mailboxes, pending external work, and action outboxes
    /// are intentionally excluded: resume semantics are derived from the
    /// persisted transcript path, not from volatile in-flight work.
    pub fn to_stored_session(&self, session_id: impl Into<String>) -> StoredSession {
        let mut stored = StoredSession::new(session_id);
        stored.active_leaf_id = self.transcript_store.leaf_id().map(str::to_string);
        stored.entries = self
            .transcript_store
            .entries()
            .into_iter()
            .map(Into::into)
            .collect();
        stored
    }

    /// Rehydrate a session from a backend-neutral storage snapshot.
    ///
    /// If the active branch ends mid-turn, the existing crash-tail recovery is
    /// applied before the session resumes. That keeps the same resume semantics
    /// for JSONL, future Postgres rows, or any other backend.
    pub fn from_stored_session(stored: StoredSession) -> Result<Self, HistoryOperationError> {
        let entries = stored.entries.into_iter().map(Into::into).collect();
        let transcript_store =
            TranscriptStore::from_storage_entries(entries, stored.active_leaf_id)
                .map_err(HistoryOperationError::Store)?;
        Self::from_transcript_store(transcript_store)
    }

    /// Enqueue a new input into the underlying core loop.
    ///
    /// This is the only supported way to feed the core from outside the
    /// session; the core itself is not exposed so context absorption in `drive`
    /// cannot be bypassed.
    ///
    /// `ModelCompleted` / `ModelFailed` / `ToolCompleted` resolve matching
    /// external work tracked by the session. Stale completions (no matching
    /// pending work, e.g. after an interrupt) are removed with no effect.
    pub fn enqueue_input(&mut self, input: AgentInput) -> Result<(), AgentInputError> {
        input.validate()?;
        if !self.prepare_agent_input(&input, None) {
            return Ok(());
        }
        self.core.enqueue_input(input)
    }

    pub fn enqueue_session_input(
        &mut self,
        input: impl Into<SessionInput>,
    ) -> Result<(), SessionInputError> {
        let input = input.into();
        input.validate()?;
        match input {
            SessionInput::Agent(input) => {
                self.enqueue_input(input).map_err(SessionInputError::Agent)
            }
            SessionInput::Compact => {
                self.compact();
                Ok(())
            }
            SessionInput::ModelCompleted {
                action_id,
                turn_id,
                assistant,
                context_tokens,
            } => {
                let input = AgentInput::ModelCompleted {
                    action_id,
                    turn_id,
                    assistant,
                };
                input.validate().map_err(SessionInputError::Agent)?;
                if self.prepare_agent_input(&input, context_tokens) {
                    self.core
                        .enqueue_input(input)
                        .map_err(SessionInputError::Agent)?;
                }
                Ok(())
            }
            SessionInput::ContextTokensUpdated {
                context_leaf_id,
                context_tokens,
            } => {
                if self.current_context_leaf_id() == context_leaf_id {
                    self.context_tokens = Some(context_tokens);
                }
                Ok(())
            }
            SessionInput::CompactionCompleted {
                request_id,
                replacement,
                context_tokens,
            } => {
                self.complete_compaction_request(request_id, replacement, context_tokens);
                Ok(())
            }
            SessionInput::CompactionFailed { request_id, error } => {
                self.fail_compaction_request(request_id, error);
                Ok(())
            }
        }
    }

    fn prepare_agent_input(&mut self, input: &AgentInput, context_tokens: Option<usize>) -> bool {
        if matches!(input, AgentInput::Interrupt) {
            self.invalidate_session_work("interrupted");
        }

        if is_external_completion(input) {
            if self.compaction.is_running() || !self.external_work.record_completion(input) {
                return false;
            }
            if let AgentInput::ModelCompleted { turn_id, .. } = input {
                self.pending_model_context_tokens = Some((*turn_id, context_tokens));
            }
            self.drop_completed_action_from_outbox(input);
        }
        true
    }

    /// Materialized view of the session history derived from the transcript
    /// store.
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
        if self.compaction.is_running() {
            return;
        }
        self.core.drive();
        self.absorb_core_transcript_items();
        self.absorb_core_actions();
        self.maybe_start_compaction(None);
    }

    /// Drain every queued user input (Steer then FollowUp) from the
    /// underlying core mailbox without advancing the session. Preserves the
    /// `from` and `kind` tags each input was enqueued with.
    ///
    /// Notifications (model/tool completions) and the interrupt flag are
    /// untouched. Primarily intended for tests and for caller-level
    /// introspection.
    pub fn drain_pending_inputs(&mut self) -> Vec<AgentInput> {
        self.core.drain_pending_inputs()
    }

    /// Drain pending actions the core produced during the last `drive`.
    ///
    /// Actions are recorded as external work during `drive`, so model/tool
    /// completions can clear pending work even if the caller drains the
    /// observable outbox later. Session-wide cancellation is retained until the
    /// caller drains it; stale start actions can be discarded when the session
    /// invalidates work.
    ///
    /// Transcript items are absorbed into the store inside `drive`, so there is no
    /// analogous `drain_transcript_items` on the session.
    pub fn drain_actions(&mut self) -> Vec<SessionAction> {
        self.action_outbox.drain(..).collect()
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        self.event_outbox.drain(..).collect()
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
        self.compaction.clear();

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
    /// Fork is separate from `rewind` because it reads the context and produces
    /// a new session rather than mutating the source in place.
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
            self.pending_model_context_tokens = None;
            self.external_work
                .emit_events_after_core_accepts(&items, &mut self.event_outbox);
            return;
        }
        self.context_tokens = None;
        let pending_model_context_tokens = if self
            .pending_model_context_tokens
            .as_ref()
            .is_some_and(|(turn_id, _)| last_turn_id_in_items(&items) == Some(*turn_id))
        {
            self.pending_model_context_tokens
                .take()
                .map(|(_, context_tokens)| context_tokens)
        } else {
            self.pending_model_context_tokens = None;
            None
        };
        let entry_ids = self.transcript_store.append_transcript_items(items.clone());
        for (entry_id, item) in entry_ids.into_iter().zip(items.iter().cloned()) {
            self.event_outbox
                .push_back(SessionEvent::TranscriptItemAppended { entry_id, item });
        }
        if let Some(context_tokens) = pending_model_context_tokens {
            self.context_tokens = context_tokens;
        }
        self.external_work
            .emit_events_after_core_accepts(&items, &mut self.event_outbox);
    }

    fn absorb_core_actions(&mut self) {
        let actions = self.core.drain_actions();
        if actions.is_empty() {
            return;
        }
        for action in actions {
            match &action {
                AgentAction::RequestModel { action_id, turn_id } => {
                    if self.maybe_start_compaction(Some(CompactionBarrierModelRequest::new(
                        *action_id,
                        *turn_id,
                        self.current_open_turn_suffix(),
                    ))) {
                        continue;
                    }
                    self.expose_agent_action(action);
                }
                AgentAction::RequestTool { .. } => self.expose_agent_action(action),
                AgentAction::CancelTurn { .. } => {
                    self.invalidate_session_work("turn cancelled");
                }
            }
        }
    }

    fn maybe_start_compaction(
        &mut self,
        blocked_model_request: Option<CompactionBarrierModelRequest>,
    ) -> bool {
        if !self.compaction.is_requested() {
            return false;
        }
        if blocked_model_request.is_none()
            && (!self.core.is_idle()
                || !self.transcript_store.is_turn_boundary()
                || self.core.has_pending_work()
                || !self.external_work.is_empty())
        {
            return false;
        }

        let model_context = self.transcript_store.model_context();
        if model_context.transcript_items().is_empty() {
            self.compaction.clear();
            return false;
        }
        let request_id = CompactionRequestId::take_next(&mut self.next_compaction_request_id);
        self.compaction.start(request_id, blocked_model_request);
        self.push_session_action(SessionAction::RequestCompaction {
            request_id,
            model_context,
            context_leaf_id: self.current_context_leaf_id(),
            context_tokens: self.context_tokens,
        });
        true
    }

    fn expose_agent_action(&mut self, action: AgentAction) {
        self.external_work.record_dispatched(&action);
        let session_action = match action {
            AgentAction::RequestModel { action_id, turn_id } => SessionAction::RequestModel {
                action_id,
                turn_id,
                model_context: self.model_context(),
                context_leaf_id: self.current_context_leaf_id(),
                context_tokens: self.context_tokens,
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

    fn complete_compaction_request(
        &mut self,
        request_id: CompactionRequestId,
        replacement: ModelContext,
        context_tokens: Option<usize>,
    ) {
        let Some(running) = self.take_running_compaction(request_id) else {
            return;
        };

        if let Some(error) =
            self.compaction_replacement_error(&replacement, running.blocked_model_request.as_ref())
        {
            self.event_outbox.push_back(SessionEvent::ActionFailed {
                kind: SessionActionKind::Compaction,
                id: request_id.0.to_string(),
                error,
            });
            if let Some(blocked_model_request) = running.blocked_model_request {
                self.expose_agent_action(blocked_model_request.into_agent_action());
            } else {
                self.maybe_start_compaction(None);
            }
            return;
        }

        self.event_outbox.push_back(SessionEvent::ActionCompleted {
            kind: SessionActionKind::Compaction,
            id: request_id.0.to_string(),
        });

        let had_blocked_model_request = running.blocked_model_request.is_some();
        let queued_user_inputs = if had_blocked_model_request {
            Vec::new()
        } else {
            self.core.drain_pending_inputs()
        };
        self.transcript_store.replace_active_path(&replacement);
        self.context_tokens = context_tokens;
        if !had_blocked_model_request {
            let last_turn_id = self.transcript_store.model_context().last_turn_id();
            let next_action_id = self.core.next_action_id();
            self.core =
                AgentCoreLoop::resume_at_boundary_with_next_action_id(last_turn_id, next_action_id);
            self.restore_queued_user_inputs(queued_user_inputs);
        }
        self.event_outbox.push_back(SessionEvent::HistoryCompacted);
        if let Some(blocked_model_request) = running.blocked_model_request {
            if self.maybe_start_compaction(Some(blocked_model_request.clone())) {
                return;
            }
            self.expose_agent_action(blocked_model_request.into_agent_action());
        } else {
            self.maybe_start_compaction(None);
        }
    }

    fn compaction_replacement_error(
        &self,
        replacement: &ModelContext,
        blocked_model_request: Option<&CompactionBarrierModelRequest>,
    ) -> Option<String> {
        if let Some(error) = replacement.structural_error() {
            return Some(format!("invalid compaction replacement: {error}"));
        }
        match blocked_model_request {
            Some(blocked_model_request) => {
                if replacement.is_turn_boundary() {
                    return Some(
                        "compaction replacement must keep the blocked model turn open".to_string(),
                    );
                }
                if replacement.last_turn_id() != blocked_model_request.turn_id() {
                    return Some(format!(
                        "compaction replacement last turn {:?} does not match blocked model turn {:?}",
                        replacement.last_turn_id(),
                        blocked_model_request.turn_id()
                    ));
                }
                if !replacement
                    .transcript_items()
                    .ends_with(blocked_model_request.required_turn_suffix())
                {
                    return Some(
                        "compaction replacement must preserve the blocked model turn suffix"
                            .to_string(),
                    );
                }
                None
            }
            None => {
                if !replacement.is_turn_boundary() {
                    return Some(
                        "idle compaction replacement must end at a turn boundary".to_string(),
                    );
                }
                let current_turn_id = self.transcript_store.model_context().last_turn_id();
                if replacement.last_turn_id() != current_turn_id {
                    return Some(format!(
                        "idle compaction replacement last turn {:?} does not match current turn {:?}",
                        replacement.last_turn_id(),
                        current_turn_id
                    ));
                }
                None
            }
        }
    }

    fn fail_compaction_request(&mut self, request_id: CompactionRequestId, error: String) {
        let Some(running) = self.take_running_compaction(request_id) else {
            return;
        };
        self.event_outbox.push_back(SessionEvent::ActionFailed {
            kind: SessionActionKind::Compaction,
            id: request_id.0.to_string(),
            error,
        });
        if let Some(blocked_model_request) = running.blocked_model_request {
            self.expose_agent_action(blocked_model_request.into_agent_action());
        } else {
            self.maybe_start_compaction(None);
        }
    }

    fn take_running_compaction(
        &mut self,
        request_id: CompactionRequestId,
    ) -> Option<RunningCompaction> {
        let running = self.compaction.take_running(request_id)?;
        self.action_outbox.retain(|action| {
            !matches!(
                action,
                SessionAction::RequestCompaction {
                    request_id: queued_request_id,
                    ..
                } if *queued_request_id == request_id
            )
        });
        Some(running)
    }

    fn invalidate_session_work(&mut self, invalidation_reason: &str) {
        let had_tracked_work = !self.external_work.is_empty();
        let had_active_core_work = !self.core.is_idle();
        let had_running_compaction = self.compaction.is_running();
        let had_start_action = self.action_outbox.iter().any(SessionAction::is_start);
        let had_existing_cancel = self.action_outbox.iter().any(SessionAction::is_cancel);

        self.external_work.clear();
        self.context_tokens = None;
        self.pending_model_context_tokens = None;
        if let Some(running) = self.compaction.abandon() {
            self.event_outbox.push_back(SessionEvent::ActionFailed {
                kind: SessionActionKind::Compaction,
                id: running.request_id.0.to_string(),
                error: invalidation_reason.to_string(),
            });
        }
        self.action_outbox.retain(SessionAction::is_cancel);

        if (had_active_core_work || had_tracked_work || had_running_compaction || had_start_action)
            && !had_existing_cancel
        {
            self.push_session_action(SessionAction::CancelSessionWork);
        }
    }

    fn drop_completed_action_from_outbox(&mut self, input: &AgentInput) {
        let position = self
            .action_outbox
            .iter()
            .position(|action| ExternalWork::action_matches_completion(action, input));
        if let Some(position) = position {
            self.action_outbox.remove(position);
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
        self.external_work.clear();
        self.action_outbox.retain(SessionAction::is_cancel);
        self.compaction.clear();
        self.context_tokens = None;
        self.pending_model_context_tokens = None;
    }

    fn current_context_leaf_id(&self) -> Option<String> {
        self.transcript_store.leaf_id().map(str::to_string)
    }

    fn current_open_turn_suffix(&self) -> Vec<TranscriptItem> {
        let model_context = self.transcript_store.model_context();
        let items = model_context.transcript_items();
        let Some(tail_start) = items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::TurnStarted { .. }))
        else {
            return Vec::new();
        };
        items[tail_start..].to_vec()
    }
}

fn last_turn_id_in_items(items: &[TranscriptItem]) -> Option<TurnId> {
    items.iter().rev().find_map(TranscriptItem::turn_id)
}

fn is_external_completion(input: &AgentInput) -> bool {
    matches!(
        input,
        AgentInput::ModelCompleted { .. }
            | AgentInput::ModelFailed { .. }
            | AgentInput::ToolCompleted { .. }
    )
}

#[cfg(test)]
mod tests;
