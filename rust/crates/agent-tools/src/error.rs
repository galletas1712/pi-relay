use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command timed out")]
    Timeout,
    #[error("edit target text was not found")]
    EditTargetNotFound,
    #[error("{0}")]
    InvalidInput(String),
}

pub type ToolResult<T> = Result<T, ToolError>;
