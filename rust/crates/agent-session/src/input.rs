use std::fmt;

use agent_core::{ActionId, AgentInput, AgentInputError, AssistantMessage, TurnId};

use crate::action::CompactionRequestId;
use crate::model_context::ModelContext;

/// External input to a live `AgentSession`.
///
/// Core inputs continue to feed the turn FSM. Session-level model completions
/// and direct updates can refresh the harness-provided context token count.
/// Compaction completions replace the active model context with output returned
/// by the remote compaction API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    Agent(AgentInput),
    Compact,
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
    CompactionCompleted {
        request_id: CompactionRequestId,
        replacement: ModelContext,
        context_tokens: Option<usize>,
    },
    CompactionFailed {
        request_id: CompactionRequestId,
        error: String,
    },
}

impl From<AgentInput> for SessionInput {
    fn from(input: AgentInput) -> Self {
        Self::Agent(input)
    }
}

impl SessionInput {
    pub fn validate(&self) -> Result<(), SessionInputError> {
        match self {
            Self::Agent(input) => input.validate().map_err(SessionInputError::Agent),
            Self::Compact
            | Self::ModelCompleted { .. }
            | Self::ContextTokensUpdated { .. }
            | Self::CompactionCompleted { .. }
            | Self::CompactionFailed { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInputError {
    Agent(AgentInputError),
}

impl fmt::Display for SessionInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(f, "invalid agent input: {error}"),
        }
    }
}

impl std::error::Error for SessionInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
        }
    }
}
