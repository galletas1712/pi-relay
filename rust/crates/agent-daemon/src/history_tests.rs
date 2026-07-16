use serde_json::json;

use super::{parse_history_target, SwitchOnlyParams};
use crate::codec::from_params;

#[test]
fn switch_and_fork_reject_malformed_shared_targets() {
    let malformed = [
        json!({ "session_id": "s1" }),
        json!({ "session_id": "s1", "leaf_id": 7 }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "expected_active_leaf_id": 7,
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "expected_transcript_revision": "7",
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "expected_transcript_revision": null,
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "active_branch_entry_ids": [7],
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "active_branch_entry_ids": null,
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "unexpected": true,
        }),
    ];

    for params in malformed {
        assert_eq!(
            parse_history_target(&params, &["return_active_branch", "missing_body_ids"])
                .expect_err("switch rejects malformed target")
                .code,
            "invalid_params"
        );
        assert_eq!(
            parse_history_target(&params, &[])
                .expect_err("fork rejects malformed target")
                .code,
            "invalid_params"
        );
    }
}

#[test]
fn switch_rejects_malformed_operation_fields() {
    for params in [
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "return_active_branch": "true",
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "return_active_branch": null,
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "missing_body_ids": [7],
        }),
        json!({
            "session_id": "s1",
            "leaf_id": null,
            "missing_body_ids": null,
        }),
    ] {
        parse_history_target(&params, &["return_active_branch", "missing_body_ids"])
            .expect("common target parses");
        assert_eq!(
            from_params::<SwitchOnlyParams>(params)
                .expect_err("switch-only field rejects")
                .code,
            "invalid_params"
        );
    }
}

#[test]
fn target_parser_preserves_missing_and_explicit_empty_fences() {
    let omitted = parse_history_target(&json!({ "session_id": "s1", "leaf_id": null }), &[])
        .expect("omitted target fences parse");
    let explicit = parse_history_target(
        &json!({
            "session_id": "s1",
            "leaf_id": null,
            "expected_active_leaf_id": null,
            "active_branch_entry_ids": [],
        }),
        &[],
    )
    .expect("explicit target fences parse");

    assert_eq!(omitted.expected_active_leaf_id, None);
    assert_eq!(omitted.active_branch_entry_ids, None);
    assert_eq!(explicit.expected_active_leaf_id, Some(None));
    assert_eq!(explicit.active_branch_entry_ids, Some(Vec::new()));
}
