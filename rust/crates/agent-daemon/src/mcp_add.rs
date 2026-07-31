use std::collections::{BTreeMap, BTreeSet};

use agent_mcp_types::{McpServerSelection, McpSessionSelection};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::provider_runtime::{author_session_mcp_and_prompt, selected_mcp_tools};
use crate::runtime::{
    clear_event_buffer_after_commit, map_source_mutation_error, publish_events, SessionDriver,
};
use crate::state::AppState;
use crate::types::RpcError;

pub(crate) async fn add(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let params: AddMcpParams = from_params(params)?;
    let driver = SessionDriver::acquire(state, &params.session_id).await;
    let current = load_root_session_config(state, &params.session_id).await?;
    driver.ensure_idle_for_source_mutation().await?;
    if state
        .repo
        .parent_has_running_delegation(&params.session_id)
        .await?
    {
        return Err(RpcError::new(
            "session_busy",
            "adding MCP tools requires all running delegations to finish",
        ));
    }

    if current.session_revision != params.session_revision {
        return Err(RpcError::new(
            "session_changed",
            "session configuration changed before MCP tools were added",
        ));
    }
    let current = current.config;
    let existing = selected_mcp_tools(&current).map_err(|error| {
        RpcError::new(
            "corrupt_mcp_manifest",
            format!("stored MCP manifest failed validation: {error:#}"),
        )
    })?;
    let old_fingerprint = current
        .mcp_manifest
        .as_ref()
        .map(|binding| binding.manifest_fingerprint.clone());
    let selection = additive_selection(params.inventory_revision, existing, params.servers)?;
    let authored = author_session_mcp_and_prompt(state, current, Some(selection)).await?;
    let binding = authored
        .mcp_manifest
        .as_ref()
        .expect("a nonempty additive selection authors an MCP binding");
    let result = state
        .repo
        .add_session_mcp(
            &params.session_id,
            params.session_revision,
            old_fingerprint.as_deref(),
            binding,
            &authored.system_prompt,
        )
        .await
        .map_err(map_source_mutation_error)?;
    publish_events(state, result.events);
    clear_event_buffer_after_commit(state, &params.session_id, "MCP tool addition").await;
    Ok(json!({
        "session_id": params.session_id,
        "manifest_fingerprint": binding.manifest_fingerprint,
        "session_revision": result.session_revision,
        "queue_revision": result.queue_revision,
        "transcript_revision": result.transcript_revision,
    }))
}

pub(crate) async fn load_root_session_config(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<agent_store::VersionedSessionConfig, RpcError> {
    let session = state
        .repo
        .load_versioned_session_config(session_id)
        .await
        .map_err(map_source_mutation_error)?;
    require_root_session(session.parent_session_id.as_deref(), session.subagent_type)?;
    Ok(session)
}

fn require_root_session(
    parent_session_id: Option<&str>,
    subagent_type: Option<agent_store::SubagentType>,
) -> std::result::Result<(), RpcError> {
    if parent_session_id.is_some() || subagent_type.is_some() {
        return Err(RpcError::new(
            "root_session_required",
            "MCP tools can only be managed on top-level sessions",
        ));
    }
    Ok(())
}

fn additive_selection(
    inventory_revision: String,
    existing: Vec<McpServerSelection>,
    additions: Vec<McpServerSelection>,
) -> std::result::Result<McpSessionSelection, RpcError> {
    if additions.is_empty() {
        return Err(RpcError::new(
            "mcp_selection_invalid",
            "select at least one new MCP tool",
        ));
    }
    let mut selected = BTreeMap::<String, BTreeSet<String>>::new();
    for server in existing {
        selected
            .entry(server.server)
            .or_default()
            .extend(server.tools);
    }
    let mut addition_servers = BTreeSet::new();
    for server in additions {
        if !addition_servers.insert(server.server.clone()) {
            return Err(RpcError::new(
                "mcp_selection_invalid",
                format!("duplicate MCP server {}", server.server),
            ));
        }
        if server.tools.is_empty() {
            return Err(RpcError::new(
                "mcp_selection_invalid",
                "added MCP server tool lists must be nonempty",
            ));
        }
        let tools = selected.entry(server.server.clone()).or_default();
        for tool in server.tools {
            if !tools.insert(tool.clone()) {
                return Err(RpcError::new(
                    "mcp_selection_invalid",
                    format!(
                        "MCP tool {}/{} is already selected or duplicated",
                        server.server, tool
                    ),
                ));
            }
        }
    }
    let mut servers = selected
        .into_iter()
        .map(|(server, tools)| {
            let mut tools = tools.into_iter().collect::<Vec<_>>();
            tools.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            McpServerSelection { server, tools }
        })
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| left.server.encode_utf16().cmp(right.server.encode_utf16()));
    Ok(McpSessionSelection {
        inventory_revision,
        servers,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddMcpParams {
    session_id: String,
    session_revision: i64,
    inventory_revision: String,
    servers: Vec<McpServerSelection>,
}

#[cfg(test)]
mod tests {
    use agent_store::SubagentType;

    use super::*;

    #[test]
    fn additive_selection_unions_old_tools_and_rejects_duplicates() {
        let selection = additive_selection(
            "inventory".to_string(),
            vec![McpServerSelection {
                server: "alpha".to_string(),
                tools: vec!["read".to_string()],
            }],
            vec![
                McpServerSelection {
                    server: "zeta".to_string(),
                    tools: vec!["write".to_string()],
                },
                McpServerSelection {
                    server: "alpha".to_string(),
                    tools: vec!["search".to_string()],
                },
            ],
        )
        .expect("selection unions");
        assert_eq!(
            selection.servers,
            vec![
                McpServerSelection {
                    server: "alpha".to_string(),
                    tools: vec!["read".to_string(), "search".to_string()],
                },
                McpServerSelection {
                    server: "zeta".to_string(),
                    tools: vec!["write".to_string()],
                },
            ]
        );
        let duplicate = additive_selection(
            "inventory".to_string(),
            selection.servers,
            vec![McpServerSelection {
                server: "alpha".to_string(),
                tools: vec!["read".to_string()],
            }],
        )
        .expect_err("existing identity is rejected");
        assert_eq!(duplicate.code, "mcp_selection_invalid");
        let duplicate_server = additive_selection(
            "inventory".to_string(),
            Vec::new(),
            vec![
                McpServerSelection {
                    server: "alpha".to_string(),
                    tools: vec!["read".to_string()],
                },
                McpServerSelection {
                    server: "alpha".to_string(),
                    tools: vec!["search".to_string()],
                },
            ],
        )
        .expect_err("duplicate server is rejected");
        assert_eq!(duplicate_server.code, "mcp_selection_invalid");
    }

    #[test]
    fn root_preflight_rejects_durable_subagent_marker_without_parent() {
        let error = require_root_session(None, Some(SubagentType::Full))
            .expect_err("subagent marker is rejected");
        assert_eq!(error.code, "root_session_required");
    }
}
