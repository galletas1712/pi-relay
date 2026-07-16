use agent_session::{HistoryOperationError, TranscriptStoreError};
use agent_store::{
    ExpectedActiveLeafMismatch, HistoryChanged, HistoryTargetNotTurnBoundary, QueueMutationError,
    RunningDelegationConflict, SourceMutationConflict,
};

use crate::types::RpcError;

pub(crate) fn map_queued_mutation_error(error: anyhow::Error) -> RpcError {
    if let Some(error) = error.downcast_ref::<QueueMutationError>() {
        return RpcError::new("input_not_found", error.to_string());
    }
    if let Some(error) = error.downcast_ref::<ExpectedActiveLeafMismatch>() {
        return RpcError::new("history_changed", error.to_string());
    }
    error.into()
}

pub(crate) fn map_source_mutation_error(error: anyhow::Error) -> RpcError {
    if let Some(error) = error.downcast_ref::<ExpectedActiveLeafMismatch>() {
        return RpcError::new("history_changed", error.to_string());
    }
    if let Some(error) = error.downcast_ref::<SourceMutationConflict>() {
        return RpcError::new("session_busy", error.to_string());
    }
    if let Some(error) = error.downcast_ref::<RunningDelegationConflict>() {
        return RpcError::new("session_busy", error.to_string());
    }
    if let Some(error) = error.downcast_ref::<HistoryChanged>() {
        return RpcError::new("history_changed", error.to_string());
    }
    if let Some(error) = error.downcast_ref::<HistoryTargetNotTurnBoundary>() {
        return RpcError::new("not_turn_boundary", error.to_string());
    }
    error.into()
}

pub(crate) fn history_error_to_rpc(error: HistoryOperationError) -> RpcError {
    match error {
        HistoryOperationError::Busy => RpcError::new("session_busy", "session history is busy"),
        HistoryOperationError::Store(TranscriptStoreError::EntryNotFound) => {
            RpcError::new("entry_not_found", "transcript entry not found")
        }
        HistoryOperationError::Store(TranscriptStoreError::NotTurnBoundary) => {
            RpcError::new("not_turn_boundary", "target is not a turn boundary")
        }
        HistoryOperationError::Store(TranscriptStoreError::DuplicateEntry) => {
            RpcError::new("invalid_transcript", "duplicate transcript entry")
        }
        HistoryOperationError::Store(TranscriptStoreError::MissingParent) => RpcError::new(
            "invalid_transcript",
            "transcript entry has a missing parent",
        ),
    }
}
