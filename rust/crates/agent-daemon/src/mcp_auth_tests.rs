use super::*;
use pretty_assertions::assert_eq;

#[test]
fn public_oauth_errors_are_fixed_and_secret_free() {
    let reflected = "code-state-verifier-token-client-secret";
    for error in [
        map_login_error(McpOAuthLoginError::InvalidCallback),
        map_login_error(McpOAuthLoginError::Provider),
        map_login_error(McpOAuthLoginError::TokenEndpoint),
        map_store_error(OAuthCredentialStoreError::Corrupt),
    ] {
        let debug = format!("{error:?}");
        assert!(!debug.contains(reflected));
        assert!(!error.code.contains(reflected));
        assert!(!error.message.contains(reflected));
        assert_eq!(error.data, json!({}));
    }
}

#[test]
fn malformed_params_use_fixed_per_method_errors() {
    let reflected = "secret-code-state-field";
    let errors = [
        decode_params::<EmptyParams>(
            serde_json::json!({"unexpected": reflected}),
            "Invalid parameters for mcp.status",
        )
        .err()
        .expect("status params reject"),
        decode_params::<ServerParams>(
            serde_json::json!({"server": {"secret": reflected}}),
            "Invalid parameters for mcp.login",
        )
        .err()
        .expect("login params reject"),
        decode_params::<CompleteParams>(
            serde_json::json!({
                "server": "oauth",
                "login_id": "0000000000000001",
                "callback_url": {"secret": reflected},
            }),
            "Invalid parameters for mcp.complete",
        )
        .err()
        .expect("complete params reject"),
        decode_params::<LoginParams>(
            serde_json::json!({"server": "oauth", "login_id": {"secret": reflected}}),
            "Invalid parameters for mcp.cancel",
        )
        .err()
        .expect("cancel params reject"),
        decode_params::<ServerParams>(
            serde_json::json!({"server": [reflected]}),
            "Invalid parameters for mcp.logout",
        )
        .err()
        .expect("logout params reject"),
    ];
    for (error, message) in errors.into_iter().zip([
        "Invalid parameters for mcp.status",
        "Invalid parameters for mcp.login",
        "Invalid parameters for mcp.complete",
        "Invalid parameters for mcp.cancel",
        "Invalid parameters for mcp.logout",
    ]) {
        assert_eq!(
            (error.code.as_str(), error.message.as_str(), &error.data),
            ("invalid_params", message, &json!({})),
            "{message}"
        );
        assert!(!format!("{error:?}").contains(reflected));
    }
}

#[test]
fn identifiers_and_full_callback_are_bounded() {
    assert!(validate_server_id("oauth").is_ok());
    assert!(validate_server_id("").is_err());
    assert!(validate_server_id(&"s".repeat(MAX_SERVER_ID_BYTES + 1)).is_err());
    assert!(validate_server_id("server\nid").is_err());
    assert!(validate_login_id("0000000000000001").is_ok());
    assert!(validate_login_id("000000000000000G").is_err());
    assert!(validate_login_id("1").is_err());
}
