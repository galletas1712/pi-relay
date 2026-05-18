use std::fmt;

use agent_vocab::{ActionId, AssistantMessage, ProviderReplayItem, TurnId};

/// External input to a live `AgentSession`.
///
/// Session-level model completions carry provider sidecars that the pure core
/// loop does not understand. Plain core inputs use `AgentSession::enqueue_input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
    },
    ModelMaxOutputTokens {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
        provider_replay: Vec<ProviderReplayItem>,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInputError {
    ModelCompletionRequiresSessionInput,
}

impl fmt::Display for SessionInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelCompletionRequiresSessionInput => {
                f.write_str("model completions must use SessionInput::ModelCompleted")
            }
        }
    }
}

impl std::error::Error for SessionInputError {}
