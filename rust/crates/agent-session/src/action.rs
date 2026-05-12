use agent_core::AgentInput;
use agent_vocab::{ActionId, ToolCall, ToolCallId, TurnId};

use crate::model_context::ModelContext;

/// Session-level work requested by `AgentSession`.
///
/// Model/tool actions are produced by `agent-core` and surfaced here with the
/// same correlation ids. `RequestModel` includes the supplied model-context
/// snapshot, the transcript leaf that snapshot was materialized from, and the
/// latest harness-provided token count for that context, if one is available.
///
/// `CancelSessionWork` is a session-wide invalidation barrier. A harness should
/// treat every outstanding model or tool request for this session as
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
}

impl SessionAction {
    pub(crate) fn matches_completion(&self, input: &AgentInput) -> bool {
        matches!(
            (
                CompletionTarget::from_session_action(self),
                CompletionTarget::from_input(input)
            ),
            (Some(action), Some(input)) if action == input
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionTarget {
    pub(crate) action_id: ActionId,
    pub(crate) turn_id: TurnId,
    pub(crate) tool: Option<CompletionToolTarget>,
}

impl CompletionTarget {
    pub(crate) fn from_input(input: &AgentInput) -> Option<Self> {
        match input {
            AgentInput::ModelCompleted {
                action_id, turn_id, ..
            }
            | AgentInput::ModelFailed {
                action_id, turn_id, ..
            } => Some(Self {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: None,
            }),
            AgentInput::ToolCompleted {
                action_id,
                turn_id,
                result,
            } => Some(Self {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: Some(CompletionToolTarget {
                    id: result.tool_call_id.clone(),
                    name: result.tool_name.clone(),
                }),
            }),
            AgentInput::Interrupt | AgentInput::Steer { .. } | AgentInput::FollowUp { .. } => None,
        }
    }

    pub(crate) fn from_core_action(action: &agent_core::AgentAction) -> Option<Self> {
        match action {
            agent_core::AgentAction::RequestModel { action_id, turn_id } => Some(Self {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: None,
            }),
            agent_core::AgentAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => Some(Self::from_tool_request(*action_id, *turn_id, tool_call)),
            agent_core::AgentAction::CancelTurn { .. } => None,
        }
    }

    fn from_session_action(action: &SessionAction) -> Option<Self> {
        match action {
            SessionAction::RequestModel {
                action_id, turn_id, ..
            } => Some(Self {
                action_id: *action_id,
                turn_id: *turn_id,
                tool: None,
            }),
            SessionAction::RequestTool {
                action_id,
                turn_id,
                tool_call,
            } => Some(Self::from_tool_request(*action_id, *turn_id, tool_call)),
            SessionAction::CancelSessionWork => None,
        }
    }

    fn from_tool_request(action_id: ActionId, turn_id: TurnId, tool_call: &ToolCall) -> Self {
        Self {
            action_id,
            turn_id,
            tool: Some(CompletionToolTarget {
                id: tool_call.id.clone(),
                name: tool_call.tool_name.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionToolTarget {
    pub(crate) id: ToolCallId,
    pub(crate) name: String,
}
