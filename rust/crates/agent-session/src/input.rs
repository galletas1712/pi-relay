use std::fmt;

use agent_core::{AgentInput, AgentInputError};

use crate::action::StatelessModelRequestId;
use crate::auto_compaction::StatelessModelOutput;

/// External input to a live `AgentSession`.
///
/// Core inputs continue to feed the turn FSM. Stateless model completions feed
/// session-owned maintenance such as scheduled compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    Agent(AgentInput),
    ModelStatelessCompleted {
        request_id: StatelessModelRequestId,
        output: StatelessModelOutput,
    },
    ModelStatelessFailed {
        request_id: StatelessModelRequestId,
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
            Self::ModelStatelessCompleted { .. } | Self::ModelStatelessFailed { .. } => Ok(()),
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
