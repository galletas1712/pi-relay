use std::fmt;

use agent_vocab::{ActionId, AssistantMessage, TurnId};

/// External input to a live `AgentSession`.
///
/// Session-level model completions can refresh the harness-provided context
/// token count. Plain core inputs use `AgentSession::enqueue_input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    ModelCompleted {
        action_id: ActionId,
        turn_id: TurnId,
        assistant: AssistantMessage,
        context_tokens: Option<usize>,
    },
    ContextTokensUpdated {
        context_leaf_id: Option<String>,
        context_tokens: usize,
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
