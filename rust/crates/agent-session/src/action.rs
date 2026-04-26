use agent_core::{ActionId, ToolCall, TurnId};

use crate::compaction::CompactionRequestId;
use crate::model_context::ModelContext;

/// Session-level work requested by `AgentSession`.
///
/// Model/tool actions are produced by `agent-core` and surfaced here with the
/// same correlation ids. Compaction work asks the harness to call the remote
/// compaction API with the supplied model-context snapshot and return a
/// replacement context.
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
    },
}
