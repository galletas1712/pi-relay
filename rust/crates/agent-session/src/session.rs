use std::collections::VecDeque;

use crate::action::SessionAction;
use crate::event::SessionEvent;
use crate::input::{SessionInput, SessionInputError};
use crate::model_context::{ModelContext, OpenTurnClosure};
use crate::outstanding_actions::OutstandingActions;
use crate::storage::StoredSession;
use crate::transcript_store::{TranscriptStore, TranscriptStoreError};
use agent_core::{AgentAction, AgentCoreLoop, AgentInput};
use agent_vocab::{ActionId, CompactionSummary, ProviderReplayItem, TranscriptItem, TurnId};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionCheckpoint {
    pub summary: String,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub continuation_suffix: Vec<crate::transcript_store::TranscriptStorageNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCompaction {
    pub new_root_id: String,
    pub active_leaf_id: String,
    pub entries: Vec<crate::transcript_store::TranscriptStorageNode>,
}

/// Session shell around the pure core loop.
///
/// `agent-core` owns deterministic state transitions. `agent-session` owns the
/// point at which the session's history can be safely forked, rewound, or
/// resumed after consulting external model/tool work. The
/// `TranscriptStore` is the sole owner of durable transcript items; the core
/// only buffers items produced in the current run until the session drains
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub(crate) core: AgentCoreLoop,
    pub(crate) transcript_store: TranscriptStore,
    outstanding_actions: OutstandingActions,
    action_outbox: VecDeque<SessionAction>,
    event_outbox: VecDeque<SessionEvent>,
    context_tokens: Option<usize>,
    pending_model_context_tokens: PendingModelContextTokens,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSession {
    pub fn new() -> Self {
        Self {
            core: AgentCoreLoop::resume_at(TurnId::default(), ActionId::first()),
            transcript_store: TranscriptStore::new(),
            outstanding_actions: OutstandingActions::default(),
            action_outbox: VecDeque::new(),
            event_outbox: VecDeque::new(),
            context_tokens: None,
            pending_model_context_tokens: PendingModelContextTokens::default(),
        }
    }

    /// Rewind the active transcript path to a prior turn boundary.
    ///
    /// `leaf_id = Some(id)` moves the active leaf to `id`; `leaf_id = None`
    /// resets the session to the empty root. Rewind is a stop-the-world edit
    /// that preserves queued user inputs while invalidating obsolete
    /// outstanding work.
    pub fn rewind(&mut self, leaf_id: Option<&str>) -> Result<(), HistoryOperationError> {
        match leaf_id {
            Some(leaf_id) if !self.transcript_store.contains_entry(leaf_id) => {
                return Err(HistoryOperationError::Store(
                    TranscriptStoreError::EntryNotFound,
                ));
            }
            leaf_id if !self.transcript_store.is_turn_boundary_at(leaf_id) => {
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
                .set_active_leaf_to_boundary(leaf_id)
                .map_err(HistoryOperationError::Store),
            None => {
                self.transcript_store.reset_active_leaf();
                Ok(())
            }
        };

        if let Err(error) = result {
            self.restore_queued_user_inputs(queued_user_inputs);
            return Err(error);
        }

        self.reset_runtime_to_active_leaf();
        self.restore_queued_user_inputs(queued_user_inputs);
        self.event_outbox.push_back(SessionEvent::HistoryRewound);
        Ok(())
    }

    pub fn from_model_context(model_context: ModelContext) -> Self {
        let model_context = model_context.close_open_turn(OpenTurnClosure::Crashed);
        let last_turn_id = model_context.last_turn_id();
        let transcript_store = TranscriptStore::from_model_context(&model_context);
        let mut session = Self::new();
        session.core = AgentCoreLoop::resume_at(last_turn_id, ActionId::first());
        session.transcript_store = transcript_store;
        session
    }

    pub(crate) fn from_transcript_store(
        mut transcript_store: TranscriptStore,
    ) -> Result<Self, HistoryOperationError> {
        Self::close_transcript_store_open_turn(&mut transcript_store, OpenTurnClosure::Crashed)?;

        let model_context = transcript_store.model_context();
        let last_turn_id = model_context.last_turn_id();
        let mut session = Self::new();
        session.core = AgentCoreLoop::resume_at(last_turn_id, ActionId::first());
        session.transcript_store = transcript_store;
        Ok(session)
    }

    pub fn from_stored_session_preserving_open_turn(
        stored: StoredSession,
    ) -> Result<Self, HistoryOperationError> {
        let entries = stored.entries.into_iter().map(Into::into).collect();
        let transcript_store =
            TranscriptStore::from_storage_entries(entries, stored.active_leaf_id)
                .map_err(HistoryOperationError::Store)?;
        let model_context = transcript_store.model_context();
        let last_turn_id = model_context.last_turn_id();
        let mut session = Self::new();
        session.core = AgentCoreLoop::resume_at(last_turn_id, ActionId::first());
        session.transcript_store = transcript_store;
        Ok(session)
    }

    /// Convert the durable transcript forest into a storage snapshot. Runtime
    /// mailboxes, outstanding requests, and action outboxes are intentionally
    /// excluded: resume semantics are derived from the persisted transcript
    /// path, not from volatile in-flight work.
    pub fn to_stored_session(&self, session_id: impl Into<String>) -> StoredSession {
        let mut stored = StoredSession::new(session_id);
        stored.active_leaf_id = self.transcript_store.active_leaf_id().map(str::to_string);
        stored.entries = self
            .transcript_store
            .entries()
            .into_iter()
            .map(Into::into)
            .collect();
        stored
    }

    /// Rehydrate a session from a storage snapshot.
    ///
    /// If the active branch ends mid-turn, the open turn is closed as crashed
    /// applied before the session resumes. That keeps resume semantics derived
    /// from the stored transcript path rather than volatile runtime state.
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
    /// outstanding work tracked by the session. Stale completions (no matching
    /// pending work, e.g. after an interrupt) are removed with no effect.
    pub fn enqueue_input(&mut self, input: AgentInput) -> Result<(), SessionInputError> {
        if matches!(input, AgentInput::ModelCompleted { .. }) {
            return Err(SessionInputError::ModelCompletionRequiresSessionInput);
        }
        if !self.accept_agent_input(&input, None) {
            return Ok(());
        }
        self.core.enqueue_input(input);
        Ok(())
    }

    pub fn enqueue_session_input(&mut self, input: SessionInput) -> Result<(), SessionInputError> {
        match input {
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
                if self.accept_agent_input(&input, context_tokens) {
                    self.core.enqueue_input(input);
                }
                Ok(())
            }
            SessionInput::ContextTokensUpdated {
                context_leaf_id,
                context_tokens,
            } => {
                if self.transcript_store.active_leaf_id().map(str::to_string) == context_leaf_id {
                    self.context_tokens = Some(context_tokens);
                }
                Ok(())
            }
        }
    }

    fn accept_agent_input(&mut self, input: &AgentInput, context_tokens: Option<usize>) -> bool {
        if matches!(input, AgentInput::Interrupt) {
            self.invalidate_session_work("interrupted");
        }

        if matches!(
            input,
            AgentInput::ModelCompleted { .. }
                | AgentInput::ModelFailed { .. }
                | AgentInput::ToolCompleted { .. }
        ) {
            if !self.outstanding_actions.accept_completion(input) {
                return false;
            }
            if let AgentInput::ModelCompleted { turn_id, .. } = input {
                self.pending_model_context_tokens =
                    PendingModelContextTokens::ModelTokenUpdatePendingAcceptance {
                        turn_id: *turn_id,
                        context_tokens,
                    };
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

    pub fn is_ready_to_continue(&self) -> bool {
        self.core.is_ready_to_continue()
    }

    pub fn context_tokens(&self) -> Option<usize> {
        self.context_tokens
    }

    pub fn transcript_store(&self) -> &TranscriptStore {
        &self.transcript_store
    }

    /// Drive the core to quiescence and append any transcript items it emitted
    /// to the session store. This is the only supported way to advance a
    /// session; the store remains the sole owner of durable history.
    pub fn drive(&mut self) {
        self.core.drive();
        self.drain_core_transcript_items();
        self.drain_core_actions();
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
    /// Actions are tracked as outstanding requests during `drive`, so model/tool
    /// completions can clear pending work even if the caller drains the
    /// observable outbox later. Session-wide cancellation is retained until the
    /// caller drains it; stale start actions can be discarded when the session
    /// invalidates work.
    ///
    /// Transcript items are drained into the store inside `drive`, so there is no
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
            self.core.enqueue_input(AgentInput::Interrupt);
            self.core.drive();
            self.drain_core_transcript_items();
            self.drain_core_actions();
        }

        if !self.transcript_store.is_turn_boundary() {
            self.restore_queued_user_inputs(queued_user_inputs);
            return Err(HistoryOperationError::Busy);
        }
        Ok(queued_user_inputs)
    }

    fn restore_queued_user_inputs(&mut self, inputs: Vec<AgentInput>) {
        for input in inputs {
            self.core.enqueue_input(input);
        }
    }

    /// Produce an unregistered `AgentSession` whose context branches from an
    /// existing transcript entry. The source session is unchanged; the caller
    /// is responsible for registering the fork if desired.
    ///
    /// Fork is separate from `rewind` because it reads the context and produces
    /// a new session rather than mutating the source in place.
    pub fn fork(&self, leaf_id: &str) -> Result<AgentSession, HistoryOperationError> {
        let transcript_store = self
            .transcript_store
            .copy_path_to_entry(leaf_id)
            .map_err(HistoryOperationError::Store)?;
        if !transcript_store.is_turn_boundary() {
            let model_context = transcript_store
                .model_context()
                .close_open_turn(OpenTurnClosure::Interrupted);
            return Ok(AgentSession::from_model_context(model_context));
        }
        AgentSession::from_transcript_store(transcript_store)
    }

    pub fn install_compaction_checkpoint(
        &mut self,
        source_session_id: impl Into<String>,
        source_leaf_id: impl Into<String>,
        tokens_before: Option<usize>,
        last_turn_id: TurnId,
        checkpoint: CompactionCheckpoint,
    ) -> InstalledCompaction {
        let root_item = TranscriptItem::CompactionSummary(CompactionSummary::new(
            source_session_id,
            source_leaf_id,
            checkpoint.summary,
            tokens_before,
            last_turn_id,
        ));
        let new_root_id = self
            .transcript_store
            .append_root_item(root_item, checkpoint.provider_replay);
        let mut entry_ids = vec![new_root_id.clone()];
        let mut suffix_parent = Some(new_root_id.clone());
        for mut suffix in checkpoint.continuation_suffix {
            suffix.parent_id = suffix_parent.clone();
            let suffix_id = suffix.id.clone();
            entry_ids.push(self.transcript_store.append_storage_node(suffix));
            suffix_parent = Some(suffix_id);
        }
        self.context_tokens = None;
        self.pending_model_context_tokens = PendingModelContextTokens::Empty;
        let active_leaf_id = self
            .transcript_store
            .active_leaf_id()
            .map(str::to_string)
            .unwrap_or_else(|| new_root_id.clone());
        let entries = entry_ids
            .iter()
            .filter_map(|id| self.transcript_store.get_entry(id).cloned())
            .collect();
        InstalledCompaction {
            new_root_id,
            active_leaf_id,
            entries,
        }
    }

    pub fn restore_compacted_runtime(
        &mut self,
        active_leaf_id: &str,
        turn_id: TurnId,
        action_id: ActionId,
    ) -> Result<(), HistoryOperationError> {
        self.transcript_store
            .set_active_leaf_to_entry(active_leaf_id)
            .map_err(HistoryOperationError::Store)?;
        self.core = AgentCoreLoop::resume_running_model(turn_id, action_id);
        self.outstanding_actions.clear();
        self.action_outbox.clear();
        let action = SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context: self.model_context(),
            context_leaf_id: Some(active_leaf_id.to_string()),
            context_tokens: None,
        };
        self.outstanding_actions.track_session_action(&action);
        self.context_tokens = None;
        self.pending_model_context_tokens = PendingModelContextTokens::Empty;
        Ok(())
    }

    /// Resume a terminal crashed/interrupted model turn from its original
    /// model-context checkpoint.
    ///
    /// The old terminal branch remains durable history. New model output will
    /// append as a sibling branch under `checkpoint_leaf_id`, so retry/continue
    /// does not duplicate the user's original message.
    pub fn resume_model_turn(
        &mut self,
        checkpoint_leaf_id: &str,
        turn_id: TurnId,
        action_id: ActionId,
        context_tokens: Option<usize>,
    ) -> Result<(), HistoryOperationError> {
        if !self.transcript_store.contains_entry(checkpoint_leaf_id) {
            return Err(HistoryOperationError::Store(
                TranscriptStoreError::EntryNotFound,
            ));
        }

        let queued_user_inputs = self.prepare_for_history_operation()?;
        let result = self
            .transcript_store
            .set_active_leaf_to_entry(checkpoint_leaf_id)
            .map_err(HistoryOperationError::Store);
        if let Err(error) = result {
            self.restore_queued_user_inputs(queued_user_inputs);
            return Err(error);
        }

        self.core = AgentCoreLoop::resume_running_model(turn_id, action_id);
        self.outstanding_actions.clear();
        self.action_outbox
            .retain(|action| matches!(action, SessionAction::CancelSessionWork));
        self.context_tokens = context_tokens;
        self.pending_model_context_tokens = PendingModelContextTokens::Empty;

        let action = SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context: self.model_context(),
            context_leaf_id: Some(checkpoint_leaf_id.to_string()),
            context_tokens,
        };
        self.outstanding_actions.track_session_action(&action);
        self.queue_session_action(action);
        self.restore_queued_user_inputs(queued_user_inputs);
        Ok(())
    }

    fn drain_core_transcript_items(&mut self) {
        let items = self.core.drain_transcript_items();
        if items.is_empty() {
            self.pending_model_context_tokens = PendingModelContextTokens::Empty;
            self.outstanding_actions
                .emit_events_after_core_accepts(&items, &mut self.event_outbox);
            return;
        }
        self.context_tokens = None;
        let entry_ids = self.transcript_store.append_transcript_items(items.clone());
        for (entry_id, item) in entry_ids.into_iter().zip(items.iter().cloned()) {
            self.event_outbox
                .push_back(SessionEvent::TranscriptItemAppended { entry_id, item });
        }
        self.pending_model_context_tokens
            .apply_if_accepted_by(&items, &mut self.context_tokens);
        self.outstanding_actions
            .emit_events_after_core_accepts(&items, &mut self.event_outbox);
    }

    fn drain_core_actions(&mut self) {
        let actions = self.core.drain_actions();
        if actions.is_empty() {
            return;
        }
        for action in actions {
            match &action {
                AgentAction::RequestModel { .. } => self.queue_core_action(action),
                AgentAction::RequestTool { .. } => self.queue_core_action(action),
                AgentAction::CancelTurn { .. } => {
                    self.invalidate_session_work("turn interrupted");
                }
            }
        }
    }

    fn queue_core_action(&mut self, action: AgentAction) {
        self.outstanding_actions.track_request(&action);
        let session_action = match action {
            AgentAction::RequestModel { action_id, turn_id } => SessionAction::RequestModel {
                action_id,
                turn_id,
                model_context: self.model_context(),
                context_leaf_id: self.transcript_store.active_leaf_id().map(str::to_string),
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
        self.queue_session_action(session_action);
    }

    fn queue_session_action(&mut self, action: SessionAction) {
        self.event_outbox.push_back(SessionEvent::ActionRequested {
            action: action.clone(),
        });
        self.action_outbox.push_back(action);
    }

    fn invalidate_session_work(&mut self, _invalidation_reason: &str) {
        let had_tracked_work = !self.outstanding_actions.is_empty();
        let had_active_core_work = !self.core.is_idle();
        let had_start_action = self.action_outbox.iter().any(|action| {
            matches!(
                action,
                SessionAction::RequestModel { .. } | SessionAction::RequestTool { .. }
            )
        });
        let had_existing_cancel = self
            .action_outbox
            .iter()
            .any(|action| matches!(action, SessionAction::CancelSessionWork));

        self.outstanding_actions.clear();
        self.context_tokens = None;
        self.pending_model_context_tokens = PendingModelContextTokens::Empty;
        self.action_outbox
            .retain(|action| matches!(action, SessionAction::CancelSessionWork));

        if (had_active_core_work || had_tracked_work || had_start_action) && !had_existing_cancel {
            self.queue_session_action(SessionAction::CancelSessionWork);
        }
    }

    fn drop_completed_action_from_outbox(&mut self, input: &AgentInput) {
        let position = self
            .action_outbox
            .iter()
            .position(|action| action.matches_completion(input));
        if let Some(position) = position {
            self.action_outbox.remove(position);
        }
    }

    fn reset_runtime_to_active_leaf(&mut self) {
        let last_turn_id = self.transcript_store.model_context().last_turn_id();
        let next_action_id = self.core.next_action_id();
        self.core = AgentCoreLoop::resume_at(last_turn_id, next_action_id);
        // Any actions tracked as pending belong to a prior run we're no
        // longer driving; reset the queue so a rehydrated session does not
        // block edits forever.
        self.outstanding_actions.clear();
        self.action_outbox
            .retain(|action| matches!(action, SessionAction::CancelSessionWork));
        self.context_tokens = None;
        self.pending_model_context_tokens = PendingModelContextTokens::Empty;
    }

    fn close_transcript_store_open_turn(
        transcript_store: &mut TranscriptStore,
        closure: OpenTurnClosure,
    ) -> Result<(), HistoryOperationError> {
        if transcript_store.is_turn_boundary() {
            return Ok(());
        }

        let items = transcript_store.model_context().into_transcript_items();
        let original_len = items.len();
        let recovered = ModelContext::from_transcript_items(items)
            .close_open_turn(closure)
            .into_transcript_items();
        if recovered.len() == original_len {
            return Err(HistoryOperationError::Store(
                TranscriptStoreError::NotTurnBoundary,
            ));
        }

        transcript_store.append_transcript_items(recovered.into_iter().skip(original_len));
        if transcript_store.is_turn_boundary() {
            Ok(())
        } else {
            Err(HistoryOperationError::Store(
                TranscriptStoreError::NotTurnBoundary,
            ))
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum PendingModelContextTokens {
    #[default]
    Empty,
    ModelTokenUpdatePendingAcceptance {
        turn_id: TurnId,
        context_tokens: Option<usize>,
    },
}

impl PendingModelContextTokens {
    fn apply_if_accepted_by(
        &mut self,
        items: &[TranscriptItem],
        current_context_tokens: &mut Option<usize>,
    ) {
        let Self::ModelTokenUpdatePendingAcceptance {
            turn_id,
            context_tokens,
        } = std::mem::take(self)
        else {
            return;
        };
        if items.iter().rev().find_map(TranscriptItem::turn_id) == Some(turn_id) {
            *current_context_tokens = context_tokens;
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
