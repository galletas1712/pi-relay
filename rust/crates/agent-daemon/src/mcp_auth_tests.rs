use super::*;
use crate::runtime_hosts::RuntimeHostError;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn public_oauth_errors_are_fixed_and_secret_free() {
    let reflected = "code-state-verifier-token-client-secret";
    for error in [
        map_runtime_mcp_error(anyhow::Error::new(RuntimeHostError {
            code: "mcp_oauth_callback_invalid".to_string(),
            message: "The OAuth callback URL is invalid for this login".to_string(),
            data: json!({}),
        })),
        map_runtime_mcp_error(anyhow::Error::new(RuntimeHostError {
            code: "mcp_oauth_provider_error".to_string(),
            message: "The authorization server rejected the OAuth login".to_string(),
            data: json!({}),
        })),
        map_runtime_mcp_error(anyhow::Error::new(RuntimeHostError {
            code: "mcp_oauth_login_failed".to_string(),
            message: "The MCP OAuth login could not be completed".to_string(),
            data: json!({}),
        })),
        map_runtime_mcp_error(anyhow::Error::new(RuntimeHostError {
            code: "mcp_oauth_credential_store_failed".to_string(),
            message: "MCP OAuth credential storage is unavailable".to_string(),
            data: json!({}),
        })),
    ] {
        let debug = format!("{error:?}");
        assert!(!debug.contains(reflected));
        assert!(!error.code.contains(reflected));
        assert!(!error.message.contains(reflected));
        assert_eq!(error.data, json!({}));
    }
}

#[test]
fn inventory_changed_preserves_current_revision() {
    let error = map_runtime_mcp_error(anyhow::Error::new(RuntimeHostError {
        code: "mcp_inventory_changed".to_string(),
        message: "MCP inventory changed; refresh and review the selection".to_string(),
        data: json!({ "current_revision": "rev-42" }),
    }));
    assert_eq!(error.code, "mcp_inventory_changed");
    assert_eq!(error.data, json!({ "current_revision": "rev-42" }));
}

#[test]
fn malformed_params_use_fixed_per_method_errors() {
    let reflected = "secret-code-state-field";
    let errors = [
        decode_params::<RuntimeParams>(
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
