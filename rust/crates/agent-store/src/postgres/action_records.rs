use agent_session::{SessionAction, SessionActionKind};
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{ActionKind, PostCompactionDispatchLease};

pub(super) const POST_COMPACTION_DISPATCH_KEY: &str = "post_compaction_dispatch";
const POST_COMPACTION_DISPATCH_KIND: &str = "resume_model_v1";
const POST_COMPACTION_DISPATCH_LEASE_KEY: &str = "lease";

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
            ..
        } => Ok((
            ActionKind::Model,
            action_id.0 as i64,
            Some(turn_id.0 as i64),
            model_action_payload(context_leaf_id.as_deref()),
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
pub(super) fn model_action_payload(context_leaf_id: Option<&str>) -> Value {
    json!({
        "context_leaf_id": context_leaf_id,
    })
}

pub(super) fn post_compaction_model_action_payload(
    context_leaf_id: &str,
    action_row_id: &str,
    attempt_id: &str,
) -> Value {
    json!({
        "context_leaf_id": context_leaf_id,
        POST_COMPACTION_DISPATCH_KEY: {
            "kind": POST_COMPACTION_DISPATCH_KIND,
            "action_row_id": action_row_id,
            "attempt_id": attempt_id,
            "context_leaf_id": context_leaf_id,
        },
    })
}

pub(super) fn model_action_context_leaf_id(payload: &Value) -> Option<String> {
    payload
        .get("context_leaf_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(super) fn post_compaction_dispatch_context_leaf_id(
    payload: &Value,
    action_row_id: &str,
    attempt_id: &str,
) -> Result<String> {
    let marker = payload
        .get(POST_COMPACTION_DISPATCH_KEY)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch marker is not an object"))?;
    if marker.get("kind").and_then(Value::as_str) != Some(POST_COMPACTION_DISPATCH_KIND) {
        bail!("post-compaction dispatch marker has an unsupported kind");
    }
    if marker.get("action_row_id").and_then(Value::as_str) != Some(action_row_id) {
        bail!("post-compaction dispatch marker action row does not match");
    }
    if marker.get("attempt_id").and_then(Value::as_str) != Some(attempt_id) {
        bail!("post-compaction dispatch marker attempt does not match");
    }
    let marker_leaf = marker
        .get("context_leaf_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch marker has no context leaf"))?;
    if model_action_context_leaf_id(payload).as_deref() != Some(marker_leaf) {
        bail!("post-compaction dispatch marker context leaf does not match the model payload");
    }
    Ok(marker_leaf.to_string())
}

pub(super) fn post_compaction_dispatch_lease(
    payload: &Value,
    action_row_id: &str,
    attempt_id: &str,
) -> Result<Option<PostCompactionDispatchLease>> {
    let context_leaf_id =
        post_compaction_dispatch_context_leaf_id(payload, action_row_id, attempt_id)?;
    let marker = payload
        .get(POST_COMPACTION_DISPATCH_KEY)
        .and_then(Value::as_object)
        .expect("validated post-compaction marker is an object");
    let Some(lease) = marker.get(POST_COMPACTION_DISPATCH_LEASE_KEY) else {
        return Ok(None);
    };
    let lease = lease
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch lease is not an object"))?;
    let owner_id = lease
        .get("owner_id")
        .and_then(Value::as_str)
        .filter(|owner_id| !owner_id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch lease has no owner"))?;
    let generation = lease
        .get("generation")
        .and_then(Value::as_u64)
        .filter(|generation| *generation > 0)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch lease has invalid generation"))?;
    lease
        .get("expires_at_ms")
        .and_then(Value::as_i64)
        .filter(|expires_at_ms| *expires_at_ms > 0)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch lease has invalid expiration"))?;
    Ok(Some(PostCompactionDispatchLease {
        owner_id: owner_id.to_string(),
        generation,
        context_leaf_id,
    }))
}

pub(super) fn set_post_compaction_dispatch_lease(
    payload: &mut Value,
    lease: &PostCompactionDispatchLease,
    expires_at_ms: i64,
) -> Result<()> {
    let marker = payload
        .get_mut(POST_COMPACTION_DISPATCH_KEY)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch marker is not an object"))?;
    marker.insert(
        POST_COMPACTION_DISPATCH_LEASE_KEY.to_string(),
        json!({
            "owner_id": lease.owner_id,
            "generation": lease.generation,
            "expires_at_ms": expires_at_ms,
        }),
    );
    Ok(())
}

pub(super) fn post_compaction_dispatch_lease_expires_at_ms(payload: &Value) -> Result<Option<i64>> {
    let Some(lease) = payload
        .get(POST_COMPACTION_DISPATCH_KEY)
        .and_then(Value::as_object)
        .and_then(|marker| marker.get(POST_COMPACTION_DISPATCH_LEASE_KEY))
    else {
        return Ok(None);
    };
    lease
        .as_object()
        .and_then(|lease| lease.get("expires_at_ms"))
        .and_then(Value::as_i64)
        .filter(|expires_at_ms| *expires_at_ms > 0)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("post-compaction dispatch lease has invalid expiration"))
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
        };

        let (_, _, _, payload) = action_payload(&action).expect("payload builds");

        assert_eq!(
            payload.get("context_leaf_id").and_then(Value::as_str),
            Some("entry_leaf")
        );
        assert!(payload.get("context_tokens").is_none());
        assert!(payload.get("model_context").is_none());
    }

    #[test]
    fn post_compaction_payload_is_attempt_fenced() {
        let mut payload =
            post_compaction_model_action_payload("entry_compacted", "action_1", "attempt_1");

        assert_eq!(
            post_compaction_dispatch_context_leaf_id(&payload, "action_1", "attempt_1")
                .expect("marker validates"),
            "entry_compacted"
        );
        assert!(
            post_compaction_dispatch_context_leaf_id(&payload, "action_1", "attempt_2").is_err()
        );
        assert_eq!(
            post_compaction_dispatch_lease(&payload, "action_1", "attempt_1")
                .expect("unclaimed marker validates"),
            None
        );
        let lease = PostCompactionDispatchLease {
            owner_id: "owner_1".to_string(),
            generation: 1,
            context_leaf_id: "entry_compacted".to_string(),
        };
        set_post_compaction_dispatch_lease(&mut payload, &lease, 123).expect("lease installs");
        assert_eq!(
            post_compaction_dispatch_lease(&payload, "action_1", "attempt_1")
                .expect("claimed marker validates"),
            Some(lease)
        );
        assert_eq!(
            post_compaction_dispatch_lease_expires_at_ms(&payload).expect("expiration validates"),
            Some(123)
        );
    }
}
