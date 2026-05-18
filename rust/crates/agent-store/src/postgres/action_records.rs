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
            context_leaf_id,
            context_tokens,
            ..
        } => Ok((
            ActionKind::Model,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            model_action_payload(context_leaf_id.as_deref(), *context_tokens),
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

/// Durable model action payloads intentionally store only a pointer to the
/// immutable transcript leaf, not the full model context. Live dispatch still
/// receives the in-memory `SessionAction::RequestModel`; restart/recovery can
/// reconstruct the context by walking `transcript_entries` from this leaf.
pub(super) fn model_action_payload(
    context_leaf_id: Option<&str>,
    context_tokens: Option<usize>,
) -> Value {
    json!({
        "context_leaf_id": context_leaf_id,
        "context_tokens": context_tokens,
    })
}

pub(super) fn model_action_context_leaf_id(payload: &Value) -> Option<String> {
    payload
        .get("context_leaf_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(super) fn model_action_context_tokens(payload: &Value) -> Option<usize> {
    payload
        .get("context_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_session::ModelContext;
    use agent_vocab::{ActionId, TurnId};

    #[test]
    fn model_action_payload_uses_context_leaf_reference_only() {
        let action = SessionAction::RequestModel {
            action_id: ActionId(7),
            turn_id: TurnId(3),
            model_context: ModelContext::from_transcript_items(Vec::new()),
            context_leaf_id: Some("entry_leaf".to_string()),
            context_tokens: Some(42),
        };

        let (_, _, _, payload) = action_payload(&action).expect("payload builds");

        assert_eq!(
            payload.get("context_leaf_id").and_then(Value::as_str),
            Some("entry_leaf")
        );
        assert_eq!(
            payload.get("context_tokens").and_then(Value::as_u64),
            Some(42)
        );
        assert!(payload.get("model_context").is_none());
    }
}
