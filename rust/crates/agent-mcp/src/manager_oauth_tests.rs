use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::ProviderKind;
use pretty_assertions::assert_eq;
use tokio::net::TcpListener;

use super::*;

#[tokio::test]
async fn oauth_route_is_immediately_login_required_without_blocking_healthy_routes() {
    let oauth_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind OAuth observation listener");
    let oauth_address = oauth_listener.local_addr().expect("OAuth address");
    let config: McpConfig = serde_json::from_value(serde_json::json!({
        "servers": {
            "stdio": {
                "transport": {
                    "type": "stdio",
                    "command": env!("AGENT_MCP_FAKE_SERVER"),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "oauth": {
                "transport": {
                    "type": "streamable_http",
                    "url": format!("https://{oauth_address}/mcp"),
                    "auth": {"type": "oauth"}
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("mixed config parses");

    let manager = tokio::time::timeout(Duration::from_secs(2), McpManager::start(config))
        .await
        .expect("OAuth does not block startup")
        .expect("manager starts");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), oauth_listener.accept())
            .await
            .is_err()
    );

    let first_party = first_party();
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party)
        .await
        .expect("mixed inventory remains coherent");
    assert_eq!(
        inventory
            .servers
            .iter()
            .map(|server| (server.server.as_str(), server.health))
            .collect::<Vec<_>>(),
        vec![
            ("oauth", McpHealth::Unavailable),
            ("stdio", McpHealth::Healthy),
        ]
    );

    let snapshot = manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision,
                servers: vec![McpServerSelection {
                    server: "stdio".to_string(),
                    tools: vec!["read".to_string()],
                }],
            },
            &first_party,
        )
        .await
        .expect("healthy route selects");
    assert_eq!(
        snapshot
            .manifest()
            .tools
            .iter()
            .map(|tool| (tool.server_id.as_str(), tool.raw_name.as_str()))
            .collect::<Vec<_>>(),
        vec![("stdio", "read")]
    );
    for provider in [ProviderKind::OpenAi, ProviderKind::Claude] {
        assert!(snapshot
            .provider_tools(provider)
            .iter()
            .all(|tool| !tool.name.contains("oauth")));
    }

    let prompt = agent_prompt::render_prompt(
        include_str!("../../../../PI.md"),
        &agent_prompt::PromptContext {
            profile: agent_prompt::PromptProfile::Parent,
            cwd: PathBuf::from("/unused"),
            has_project: false,
            workspaces: Vec::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            subagent_roles: Vec::new(),
            mcp_servers: vec![agent_prompt::PromptMcpServer {
                server: "stdio".to_string(),
                tools: vec!["read".to_string()],
            }],
        },
    );
    let selected_prompt_section = prompt
        .split_once("### Selected MCP tools")
        .expect("selected MCP prompt section exists")
        .1
        .split_once("## Subagent delegation")
        .expect("selected MCP prompt section is bounded")
        .0;
    assert!(selected_prompt_section.contains("- stdio:"));
    assert!(!selected_prompt_section.contains("oauth"));

    manager.shutdown().await;
}

fn first_party() -> HashMap<ProviderKind, Vec<ProviderTool>> {
    let registry = ToolRegistry::with_builtin_tools();
    [ProviderKind::OpenAi, ProviderKind::Claude]
        .into_iter()
        .map(|provider| (provider, registry.provider_tools_for_provider(provider)))
        .collect()
}
