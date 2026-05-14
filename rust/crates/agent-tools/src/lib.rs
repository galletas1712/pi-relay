#![forbid(unsafe_code)]

mod context;
mod display;
mod error;
mod output;
mod registry;
mod tools;

pub use context::ToolContext;
pub use display::{tool_display, tool_pretty_name, ToolDisplayInput};
pub use error::{ToolError, ToolResult};
pub use output::limit_tool_output;
pub use registry::{builtin_tool_definition, AgentTool, ToolListing, ToolRegistry};
