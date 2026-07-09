use super::*;

#[test]
fn allowlist_is_required_without_explicit_allow_all() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example"
            }
        }
    }))
    .expect("config parses");

    assert_eq!(
        config.validate().expect_err("config must fail").to_string(),
        "invalid MCP server example"
    );
}

#[test]
fn semantic_identity_includes_concurrency_but_not_operational_deadlines() {
    let mut config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "enabled_tools": ["read"],
                "parallel_calls": 1
            }
        }
    }))
    .expect("config parses");
    let original = config.servers["example"].semantic_fingerprint();
    config
        .servers
        .get_mut("example")
        .expect("server exists")
        .startup_timeout_ms += 1;
    assert_eq!(config.servers["example"].semantic_fingerprint(), original);
    config
        .servers
        .get_mut("example")
        .expect("server exists")
        .parallel_calls = 2;
    assert_ne!(config.servers["example"].semantic_fingerprint(), original);
}

#[test]
fn semantic_input_hashes_literal_values_and_keeps_only_inherited_names() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "env": { "PUBLIC_SETTING": "not-persisted-verbatim" },
                "inherit_env": ["SECRET_TOKEN"],
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    config.validate().expect("config validates");

    let serialized = config.semantic_fingerprint_input().to_string();
    assert!(serialized.contains("SECRET_TOKEN"));
    assert!(!serialized.contains("not-persisted-verbatim"));
}

#[test]
fn secret_like_literal_environment_values_are_rejected() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "env": { "EXAMPLE_API_KEY": "literal-secret" },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");

    assert!(config.validate().is_err());
}

#[test]
fn config_files_are_bounded_before_json_parsing() {
    let path = std::env::temp_dir().join(format!(
        "agent-mcp-config-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time advances")
            .as_nanos()
    ));
    std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1])
        .expect("oversized config writes");
    let error = McpConfig::from_path(&path).expect_err("oversized config is rejected");
    std::fs::remove_file(path).ok();
    assert!(error.to_string().contains("exceeds"));
}

#[test]
fn debug_redacts_command_arguments_paths_and_literal_values() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "secret-command",
                "args": ["secret-argument"],
                "cwd": "/secret/path",
                "env": {"PUBLIC_SETTING": "secret-literal-value"},
                "inherit_env": ["SECRET_TOKEN"],
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");

    let debug = format!("{config:?}");
    for secret in [
        "secret-command",
        "secret-argument",
        "/secret/path",
        "secret-literal-value",
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("PUBLIC_SETTING"));
    assert!(debug.contains("SECRET_TOKEN"));
}

#[test]
fn common_secret_literal_names_are_rejected() {
    for key in [
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "AUTH_HEADER",
        "COOKIE",
        "SESSION_ID",
        "GITHUB_PAT",
        "BEARER",
        "SSH_KEY",
        "DATABASE_URL",
    ] {
        let config: McpConfig = serde_json::from_value(serde_json::json!({
            "servers": {
                "example": {
                    "command": "example",
                    "env": {key: "literal-secret"},
                    "enabled_tools": ["read"]
                }
            }
        }))
        .expect("config parses");
        assert!(config.validate().is_err(), "{key}");
    }
}

#[test]
fn command_arguments_and_environment_are_bounded() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "args": ["x".repeat(MAX_ARG_BYTES + 1)],
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    assert!(config.validate().is_err());

    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "env": {"PUBLIC_SETTING": "x".repeat(MAX_ENV_VALUE_BYTES + 1)},
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    assert!(config.validate().is_err());
}
