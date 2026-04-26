use agent_core::{ActionId, ToolCall, TurnId};

use crate::model_context::ModelContext;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompactionRequestId(pub u64);

impl CompactionRequestId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = *next;
        next.0 += 1;
        current
    }
}

/// Session-level work requested by `AgentSession`.
///
/// Model/tool actions are produced by `agent-core` and surfaced here with the
/// same correlation ids. `RequestModel` / `RequestCompaction` include the
/// supplied model-context snapshot, the transcript leaf that snapshot was
/// materialized from, and the latest harness-provided token count for that
/// context, if one is available. Compaction work asks the harness to call the
/// remote compaction API and return a replacement context.
///
/// `CancelSessionWork` is a session-wide invalidation barrier. A harness should
/// treat every outstanding model, tool, or compaction request for this session as
/// stale and cancel it if possible. The action is idempotent and best-effort:
/// late completions can still race in, and the session ignores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    RequestModel {
        action_id: ActionId,
        turn_id: TurnId,
        model_context: ModelContext,
        context_leaf_id: Option<String>,
        context_tokens: Option<usize>,
    },
    RequestTool {
        action_id: ActionId,
        turn_id: TurnId,
        tool_call: ToolCall,
    },
    CancelSessionWork,
    RequestCompaction {
        request_id: CompactionRequestId,
        model_context: ModelContext,
        context_leaf_id: Option<String>,
        context_tokens: Option<usize>,
    },
}

impl SessionAction {
    pub(crate) fn is_start(&self) -> bool {
        matches!(
            self,
            Self::RequestModel { .. } | Self::RequestTool { .. } | Self::RequestCompaction { .. }
        )
    }

    pub(crate) fn is_cancel(&self) -> bool {
        matches!(self, Self::CancelSessionWork)
    }
}
