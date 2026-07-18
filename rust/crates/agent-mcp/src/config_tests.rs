use super::*;

fn write_mcp_config(suffix: &str, contents: &str) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new()
        .prefix("agent-mcp-config-")
        .suffix(suffix)
        .tempfile()
        .expect("create MCP config");
    std::fs::write(file.path(), contents).expect("write MCP config");
    file
}

#[test]
fn toml_file_loader_accepts_tagged_and_legacy_stdio_configs() {
    let tagged = write_mcp_config(
        ".toml",
        r#"
[servers.tagged]
enabled_tools = ["read"]
parallel_calls = 2

[servers.tagged.transport]
type = "stdio"
command = "example"
args = ["--flag"]
cwd = "/tmp"
inherit_env = ["EXAMPLE_TOKEN"]
"#,
    );
    let tagged = McpConfig::from_path(tagged.path()).expect("tagged TOML config parses");
    tagged.validate().expect("tagged TOML config validates");

    let legacy = write_mcp_config(
        ".toml",
        r#"
[servers.legacy]
command = "example"
args = ["--flag"]
cwd = "/tmp"
inherit_env = ["EXAMPLE_TOKEN"]
enabled_tools = ["read"]
parallel_calls = 2
"#,
    );
    let legacy = McpConfig::from_path(legacy.path()).expect("legacy TOML config parses");
    legacy.validate().expect("legacy TOML config validates");

    assert!(matches!(
        tagged.servers["tagged"].transport,
        McpTransportConfig::Stdio(_)
    ));
    assert!(matches!(
        legacy.servers["legacy"].transport,
        McpTransportConfig::Stdio(_)
    ));
}

#[test]
fn toml_file_loader_accepts_streamable_http_bearer_and_oauth_configs() {
    let config = write_mcp_config(
        ".toml",
        r#"
[servers.bearer]
enabled_tools = ["search"]

[servers.bearer.transport]
type = "streamable_http"
url = "https://mcp.example.test/bearer"

[servers.bearer.transport.auth]
type = "bearer_env"
env = "EXAMPLE_MCP_TOKEN"

[servers.oauth]
allow_all_tools = true

[servers.oauth.transport]
type = "streamable_http"
url = "https://mcp.example.test/oauth"

[servers.oauth.transport.auth]
type = "oauth"
client_id = "public-client"
scopes = ["read", "search"]
resource = "https://api.example.test/audience"
callback_port = 8765
callback_timeout_ms = 300000
"#,
    );

    let config = McpConfig::from_path(config.path()).expect("HTTP TOML config parses");
    config.validate().expect("HTTP TOML config validates");
    assert!(matches!(
        config.servers["bearer"].transport,
        McpTransportConfig::StreamableHttp(_)
    ));
    assert!(matches!(
        config.servers["oauth"].transport,
        McpTransportConfig::StreamableHttp(_)
    ));
}

#[test]
fn toml_file_loader_rejects_malformed_toml_and_json() {
    let malformed = write_mcp_config(
        ".toml",
        r#"
[servers.example
command = "example"
"#,
    );
    let error = McpConfig::from_path(malformed.path()).expect_err("malformed TOML is rejected");
    assert!(format!("{error:#}").contains("parse MCP config"));

    let json = write_mcp_config(
        ".json",
        r#"{"servers":{"example":{"command":"example","allow_all_tools":true}}}"#,
    );
    let error = McpConfig::from_path(json.path()).expect_err("JSON is rejected by TOML loader");
    assert!(format!("{error:#}").contains("parse MCP config"));
}

#[test]
fn toml_file_loader_is_strict_at_root_transport_and_auth_boundaries() {
    for contents in [
        r#"
unexpected = true
"#,
        r#"
[servers.example]
allow_all_tools = true
unexpected = true

[servers.example.transport]
type = "stdio"
command = "example"
"#,
        r#"
[servers.example]
allow_all_tools = true

[servers.example.transport]
type = "stdio"
command = "example"
unexpected = true
"#,
        r#"
[servers.example]
allow_all_tools = true

[servers.example.transport]
type = "streamable_http"
url = "https://mcp.example.test/service"

[servers.example.transport.auth]
type = "bearer_env"
env = "EXAMPLE_MCP_TOKEN"
unexpected = true
"#,
    ] {
        let config = write_mcp_config(".toml", contents);
        assert!(
            McpConfig::from_path(config.path()).is_err(),
            "unknown fields must be rejected: {contents}"
        );
    }
}

#[test]
fn toml_file_loader_ignores_filename_extension() {
    let config = write_mcp_config(
        ".json",
        r#"
[servers.example]
allow_all_tools = true

[servers.example.transport]
type = "stdio"
command = "example"
"#,
    );

    McpConfig::from_path(config.path()).expect("TOML config parses despite .json extension");
}

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
fn oauth_callback_settings_are_operational() {
    fn config(port: u16, timeout: u64) -> McpConfig {
        serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {
                        "type": "streamable_http",
                        "url": "https://mcp.example.test/service",
                        "auth": {
                            "type": "oauth",
                            "callback_port": port,
                            "callback_timeout_ms": timeout
                        }
                    },
                    "allow_all_tools": true
                }
            }
        }))
        .expect("OAuth config")
    }

    let original = config(3210, 10_000);
    let different_timeout = config(3210, 20_000);
    let different_port = config(3211, 10_000);
    for config in [&original, &different_timeout, &different_port] {
        config.validate().expect("valid config");
    }
    assert_eq!(
        original.servers["remote"].semantic_fingerprint(),
        different_timeout.servers["remote"].semantic_fingerprint()
    );
    assert_eq!(
        original.servers["remote"].semantic_fingerprint(),
        different_port.servers["remote"].semantic_fingerprint()
    );
}

#[test]
fn oauth_config_is_codex_shaped_and_uses_streamable_http_url_rules() {
    for url in [
        "https://mcp.example.test/service?tenant=one",
        "http://localhost:3000/mcp?tenant=one",
        "http://127.0.0.1:3000/mcp?tenant=one",
        "http://[::1]:3000/mcp?tenant=one",
    ] {
        let config: McpConfig = serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {
                        "type": "streamable_http",
                        "url": url,
                        "auth": {
                            "type": "oauth",
                            "client_id": "public-client",
                            "scopes": ["read", "search"],
                            "resource": "https://api.example.test/a?tenant=one",
                        }
                    },
                    "allow_all_tools": true
                }
            }
        }))
        .expect("OAuth config parses");
        config.validate().expect("OAuth config validates");
    }

    for auth in [
        serde_json::json!({"type": "oauth", "callback_port": 0}),
        serde_json::json!({"type": "oauth", "callback_timeout_ms": 600001}),
    ] {
        let config: McpConfig = serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {
                        "type": "streamable_http",
                        "url": "https://mcp.example.test/service",
                        "auth": auth
                    },
                    "allow_all_tools": true
                }
            }
        }))
        .expect("invalid OAuth config still parses");
        assert!(config.validate().is_err());
    }

    for abandoned_field in [
        serde_json::json!({"registration": {"type": "dynamic"}}),
        serde_json::json!({"client_secret_env": "CLIENT_SECRET"}),
        serde_json::json!({"allowed_scopes": ["read"]}),
        serde_json::json!({"initial_scopes": ["read"]}),
        serde_json::json!({"authorization_server_issuer": "https://auth.example.test"}),
        serde_json::json!({"trusted_authorization_origins": ["https://auth.example.test"]}),
    ] {
        let mut auth = serde_json::json!({"type": "oauth"});
        auth.as_object_mut()
            .expect("auth object")
            .extend(abandoned_field.as_object().expect("field object").clone());
        assert!(serde_json::from_value::<McpConfig>(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {
                        "type": "streamable_http",
                        "url": "https://mcp.example.test/service",
                        "auth": auth
                    },
                    "allow_all_tools": true
                }
            }
        }))
        .is_err());
    }
}

#[test]
fn oauth_fingerprint_contains_semantic_config_and_no_operational_state() {
    fn config(callback_port: u16, timeout: u64) -> McpConfig {
        serde_json::from_value(serde_json::json!({
            "servers": {
                "remote": {
                    "transport": {
                        "type": "streamable_http",
                        "url": "https://mcp.example.test/service?tenant=one",
                        "auth": {
                            "type": "oauth",
                            "client_id": "public-client",
                            "scopes": ["read", "search"],
                            "resource": "https://api.example.test/a?tenant=one",
                            "callback_port": callback_port,
                            "callback_timeout_ms": timeout,
                        }
                    },
                    "enabled_tools": ["read"]
                }
            }
        }))
        .expect("OAuth config parses")
    }

    let first = config(3210, 10_000);
    let operationally_different = config(3211, 20_000);
    first.validate().expect("first config validates");
    operationally_different
        .validate()
        .expect("second config validates");
    assert_eq!(
        first.servers["remote"].semantic_fingerprint(),
        operationally_different.servers["remote"].semantic_fingerprint()
    );

    let input = first.semantic_fingerprint_input().to_string();
    for expected in [
        "\"type\":\"oauth\"",
        "public-client",
        "https://mcp.example.test/service?tenant=one",
        "https://api.example.test/a?tenant=one",
        "\"scopes\":[\"read\",\"search\"]",
    ] {
        assert!(input.contains(expected), "{expected}");
    }
    for forbidden in [
        "callback_port",
        "callback_timeout_ms",
        "access_token",
        "refresh_token",
        "code_verifier",
        "metadata_cache",
    ] {
        assert!(!input.contains(forbidden), "{forbidden}");
    }

    let server = &first.servers["remote"];
    let catalog = crate::catalog::build_inventory_catalog(
        &BTreeMap::from([("remote".to_string(), server.semantic_fingerprint())]),
        vec![crate::catalog::DiscoveredTool {
            server_id: "remote".to_string(),
            server_config_fingerprint: server.semantic_fingerprint(),
            raw_name: "read".to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        &BTreeSet::new(),
    )
    .expect("manifest builds");
    let manifest = serde_json::to_string(&catalog).expect("manifest serializes");
    for forbidden in [
        "public-client",
        "mcp.example.test",
        "api.example.test",
        "\"search\"",
    ] {
        assert!(!manifest.contains(forbidden), "{forbidden}");
    }

    let debug = format!("{first:?}");
    for redacted in ["public-client", "mcp.example.test", "api.example.test"] {
        assert!(!debug.contains(redacted), "{redacted}");
    }
}

#[test]
fn oauth_fingerprint_has_a_fixed_regression_fixture() {
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "remote": {
                "transport": {
                    "type": "streamable_http",
                    "url": "https://mcp.example.test/service?tenant=one",
                    "auth": {
                        "type": "oauth",
                        "client_id": "public-client",
                        "scopes": ["read", "search"],
                        "resource": "https://api.example.test/a?tenant=one",
                    }
                },
                "enabled_tools": ["read"]
            }
        }
    }))
    .expect("OAuth config");

    assert_eq!(
        config.servers["remote"].semantic_fingerprint(),
        "58da63fd45549f1134a6eb0b603a854e931125f60de442b524b71c095e0f8785"
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
    assert_eq!(
        expected_legacy_fingerprint,
        "94e1bcd5e733490f8908db743a74a804f7dd02e11cd3bcbf403546ca9be0fb0c"
    );
    assert_eq!(legacy.servers["example"], tagged.servers["example"]);
}

#[test]
fn bearer_env_fingerprint_retains_the_pre_oauth_fixture() {
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
    .expect("bearer config parses");

    assert_eq!(
        config.servers["remote"].semantic_fingerprint(),
        "8c0d8822a2dae49040d310f2488ae41e667201a428a9862eb062e2f89b4cf6a3"
    );
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
fn config_files_are_bounded_before_toml_parsing() {
    let path = std::env::temp_dir().join(format!(
        "agent-mcp-config-{}-{}.toml",
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
