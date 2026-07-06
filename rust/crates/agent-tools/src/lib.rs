#![forbid(unsafe_code)]

mod context;
mod display;
mod error;
mod output;
mod registry;
mod tools;

pub use context::ToolContext;
pub use display::{tool_display, ToolDisplayInput};
pub use error::{ToolError, ToolResult};
pub use output::{limit_tool_output, limit_tool_output_with_max_tokens};
pub use registry::{
    sort_provider_tools, AgentTool, FirstPartyToolExtension, ProviderTool, ToolDescriptor,
    ToolExecution, ToolExtension, ToolRegistry,
};
pub use tools::{nonempty_domains, WebFetchArgs, WebSearchArgs, APPLY_PATCH_LARK_GRAMMAR};
