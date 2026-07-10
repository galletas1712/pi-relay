use super::*;

#[test]
fn allowlist_is_required_without_explicit_allow_all() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "transport": {"type": "stdio", "command": "example"}
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
                "transport": {"type": "stdio", "command": "example"},
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
                "transport": {
                    "type": "stdio",
                    "command": "example",
                    "env": { "PUBLIC_SETTING": "not-persisted-verbatim" },
                    "inherit_env": ["SECRET_TOKEN"]
                },
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
                "transport": {
                    "type": "stdio",
                    "command": "example",
                    "env": { "EXAMPLE_API_KEY": "literal-secret" }
                },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");

    assert!(config.validate().is_err());
}

#[test]
fn streamable_http_route_identity_excludes_token_value() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "remote": {
                "transport": {
                    "type": "streamable_http",
                    "url": "https://mcp.example.test/service",
                    "auth": {"type": "bearer_env", "env": "EXAMPLE_MCP_TOKEN"}
                },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    config.validate().expect("config validates");

    let server = &config.servers["remote"];
    let McpTransportConfig::StreamableHttp(transport) = &server.transport else {
        panic!("remote transport expected");
    };
    let first_secret = "fixture-secret-one";
    let second_secret = "fixture-secret-two";
    let fingerprint = server.semantic_fingerprint();
    let first =
        crate::http_transport::resolve_scrubber_with(transport, |_| Some(first_secret.to_string()))
            .expect("first bearer resolves")
            .expect("bearer is configured");
    assert_eq!(server.semantic_fingerprint(), fingerprint);
    let second = crate::http_transport::resolve_scrubber_with(transport, |_| {
        Some(second_secret.to_string())
    })
    .expect("second bearer resolves")
    .expect("bearer is configured");
    assert_eq!(server.semantic_fingerprint(), fingerprint);
    assert_eq!(first.scrub(first_secret), "<redacted>");
    assert_eq!(second.scrub(second_secret), "<redacted>");

    let serialized = config.semantic_fingerprint_input().to_string();
    assert!(serialized.contains("EXAMPLE_MCP_TOKEN"));
    assert!(!serialized.contains(first_secret));
    assert!(!serialized.contains(second_secret));
}

#[test]
fn streamable_http_debug_redacts_url_and_never_has_a_token_field() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "remote": {
                "transport": {
                    "type": "streamable_http",
                    "url": "https://private.example.test/secret-path",
                    "auth": {"type": "bearer_env", "env": "EXAMPLE_MCP_TOKEN"}
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");

    let debug = format!("{config:?}");
    assert!(!debug.contains("private.example.test"));
    assert!(!debug.contains("secret-path"));
    assert!(debug.contains("EXAMPLE_MCP_TOKEN"));
}

#[test]
fn streamable_http_requires_https_except_for_loopback() {
    for url in [
        "https://mcp.example.test/service",
        "http://localhost:3000/mcp",
        "http://127.0.0.1:3000/mcp",
        "http://[::1]:3000/mcp",
    ] {
        let config: McpConfig = serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {"type": "streamable_http", "url": url},
                    "allow_all_tools": true
                }
            }
        }))
        .expect("config parses");
        config.validate().expect("URL validates");
    }

    for url in [
        "http://mcp.example.test/service",
        "https://user:password@mcp.example.test/service",
        "https://mcp.example.test/service#fragment",
    ] {
        let config: McpConfig = serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {"type": "streamable_http", "url": url},
                    "allow_all_tools": true
                }
            }
        }))
        .expect("config parses");
        assert!(config.validate().is_err(), "{url}");
    }
}

#[test]
fn legacy_flat_and_tagged_stdio_retain_the_legacy_fingerprint() {
    let legacy: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "args": ["--flag"],
                "cwd": "/tmp",
                "env": {"PUBLIC_SETTING": "value"},
                "inherit_env": ["SECRET_TOKEN"],
                "parallel_calls": 2,
                "enabled_tools": ["read"],
            }
        }
    }))
    .expect("legacy flat stdio config parses");
    let tagged: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "transport": {
                    "type": "stdio",
                    "command": "example",
                    "args": ["--flag"],
                    "cwd": "/tmp",
                    "env": {"PUBLIC_SETTING": "value"},
                    "inherit_env": ["SECRET_TOKEN"]
                },
                "parallel_calls": 2,
                "enabled_tools": ["read"],
            }
        }
    }))
    .expect("tagged stdio config parses");
    let value_hash = format!("{:x}", Sha256::digest(b"value"));
    let legacy_input = serde_json::json!({
        "command": "example",
        "args": ["--flag"],
        "cwd": "/tmp",
        "env_hashes": {"PUBLIC_SETTING": value_hash},
        "inherit_env": ["SECRET_TOKEN"],
        "parallel_calls": 2,
        "allow_all_tools": false,
        "enabled_tools": ["read"],
    });
    let expected_legacy_fingerprint = crate::fingerprint_json(&legacy_input);

    assert_eq!(
        legacy.servers["example"].semantic_fingerprint(),
        expected_legacy_fingerprint
    );
    assert_eq!(
        tagged.servers["example"].semantic_fingerprint(),
        expected_legacy_fingerprint
    );
    assert_eq!(legacy.servers["example"], tagged.servers["example"]);
}

#[test]
fn mixed_legacy_and_tagged_fields_are_rejected() {
    let result = serde_json::from_value::<McpConfig>(serde_json::json!({
        "servers": {
            "example": {
                "command": "example",
                "transport": {"type": "stdio", "command": "example"},
                "allow_all_tools": true
            }
        }
    }));
    assert!(result.is_err());
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
                "transport": {
                    "type": "stdio",
                    "command": "secret-command",
                    "args": ["secret-argument"],
                    "cwd": "/secret/path",
                    "env": {"PUBLIC_SETTING": "secret-literal-value"},
                    "inherit_env": ["SECRET_TOKEN"]
                },
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
                    "transport": {
                        "type": "stdio",
                        "command": "example",
                        "env": {key: "literal-secret"}
                    },
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
                "transport": {
                    "type": "stdio",
                    "command": "example",
                    "args": ["x".repeat(MAX_ARG_BYTES + 1)]
                },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    assert!(config.validate().is_err());

    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "example": {
                "transport": {
                    "type": "stdio",
                    "command": "example",
                    "env": {"PUBLIC_SETTING": "x".repeat(MAX_ENV_VALUE_BYTES + 1)}
                },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("config parses");
    assert!(config.validate().is_err());
}
