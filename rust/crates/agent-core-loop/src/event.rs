use crate::ids::{Epoch, ToolCallId};
use crate::message::{ToolCall, ToolResultStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopAction {
    TurnStarted {
        epoch: Epoch,
    },
    ToolCallStarted {
        epoch: Epoch,
        call_id: ToolCallId,
        tool_name: String,
    },
    ToolCallFinished {
        epoch: Epoch,
        call_id: ToolCallId,
        tool_name: String,
        status: ToolResultStatus,
    },
    Interrupted {
        epoch: Epoch,
    },
    TurnFinished {
        epoch: Epoch,
    },
    RequestModel {
        epoch: Epoch,
    },
    RequestTool {
        epoch: Epoch,
        tool_call: ToolCall,
    },
    CancelActive {
        epoch: Epoch,
    },
}
