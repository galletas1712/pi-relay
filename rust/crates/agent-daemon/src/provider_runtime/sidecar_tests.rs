use super::*;

#[test]
fn sidecar_session_id_is_stable_short_and_descriptive() {
    let id = sidecar_session_id(
        "title",
        "session_00000000-0000-0000-0000-000000000000",
        &["turn_1"],
    );

    assert_eq!(
        id,
        sidecar_session_id(
            "title",
            "session_00000000-0000-0000-0000-000000000000",
            &["turn_1"],
        )
    );
    assert!(id.len() <= 64);
    assert!(id.starts_with("title-session_"));
}

#[test]
fn sidecar_session_id_varies_by_part() {
    let first = sidecar_session_id("web", "session", &["call_a"]);
    let second = sidecar_session_id("web", "session", &["call_b"]);

    assert_ne!(first, second);
}

#[test]
fn sidecar_session_id_falls_back_to_generic_prefix() {
    let id = sidecar_session_id("!!!", "###", &["part"]);

    assert!(id.starts_with("sidecar-"));
    assert!(id.len() <= 64);
}
