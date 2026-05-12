use agent_session::{SessionAction, SessionActionKind};
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::ActionKind;

pub(super) fn action_event_matches_row(
    row_kind: ActionKind,
    row_action_id: i64,
    event_kind: &SessionActionKind,
    event_id: &str,
) -> bool {
    let event_kind = match event_kind {
        SessionActionKind::Model => ActionKind::Model,
        SessionActionKind::Tool => ActionKind::Tool,
    };
    row_kind == event_kind && event_id.parse::<i64>().ok() == Some(row_action_id)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ActionKey {
    kind: ActionKind,
    action_id: i64,
}

impl ActionKey {
    pub(super) fn new(kind: ActionKind, action_id: i64) -> Self {
        Self { kind, action_id }
    }
}

pub(super) fn action_payload(
    action: &SessionAction,
) -> Result<(ActionKind, i64, Option<i64>, Value)> {
    match action {
        SessionAction::RequestModel {
            action_id,
            turn_id,
            model_context,
            context_leaf_id,
            context_tokens,
        } => Ok((
            ActionKind::Model,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            json!({
                "model_context": model_context.transcript_items(),
                "context_leaf_id": context_leaf_id,
                "context_tokens": context_tokens,
            }),
        )),
        SessionAction::RequestTool {
            action_id,
            turn_id,
            tool_call,
        } => Ok((
            ActionKind::Tool,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            serde_json::to_value(tool_call)?,
        )),
        SessionAction::CancelSessionWork => {
            bail!("cancel actions do not have persisted action rows")
        }
    }
}
