use std::fmt;

use agent_core::{AgentInput, AgentInputError};

use crate::action::OneShotModelRequestId;
use crate::auto_compaction::OneShotModelOutput;

/// External input to a live `AgentSession`.
///
/// Core inputs continue to feed the turn FSM. One-shot completions feed
/// session-owned side work such as auto-compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    Agent(AgentInput),
    OneShotModelCompleted {
        request_id: OneShotModelRequestId,
        output: OneShotModelOutput,
    },
    OneShotModelFailed {
        request_id: OneShotModelRequestId,
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
            Self::OneShotModelCompleted { .. } | Self::OneShotModelFailed { .. } => Ok(()),
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
