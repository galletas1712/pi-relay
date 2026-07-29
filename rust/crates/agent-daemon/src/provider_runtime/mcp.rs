use std::collections::HashMap;

use agent_mcp_types::{
    McpServerSelection, McpSessionManifest, McpSessionSelection, McpSessionSnapshot,
};
use agent_prompt::PromptProfile;
use agent_store::{McpSessionManifestBinding, SessionConfig};
use agent_vocab::ProviderKind;
use anyhow::{Context, Result};
use serde_json::Value;

use crate::state::AppState;
use crate::types::RpcError;

use super::prompt::{provider_tools_for_session, render_pi_prompt};

pub(crate) async fn author_session_mcp_and_prompt(
    state: &AppState,
    mut config: SessionConfig,
    selection: Option<McpSessionSelection>,
) -> std::result::Result<SessionConfig, RpcError> {
    config.mcp_manifest =
        if let Some(selection) = selection.filter(|selection| !selection.servers.is_empty()) {
            let manifest = state
                .runtime_hosts
                .mcp_select(
                    &config.runtime_id,
                    selection,
                    first_party_toolsets(state, super::prompt::prompt_profile(&config)),
                )
                .await
                .map_err(crate::mcp_auth::map_runtime_mcp_error)?;
            Some(McpSessionManifestBinding {
                manifest_fingerprint: manifest.manifest_fingerprint.clone(),
                manifest: serde_json::to_value(&manifest)
                    .map_err(anyhow::Error::from)
                    .map_err(RpcError::from)?,
            })
        } else {
            None
        };
    config.system_prompt = render_pi_prompt(state, &config).await?;
    Ok(config)
}

pub(crate) fn selected_mcp_tools(
    config: &SessionConfig,
) -> anyhow::Result<Vec<McpServerSelection>> {
    let snapshot = mcp_snapshot_for_session(config)?;
    let mut selected = std::collections::BTreeMap::<String, Vec<String>>::new();
    for tool in &snapshot.manifest().tools {
        selected
            .entry(tool.server_id.clone())
            .or_default()
            .push(tool.raw_name.clone());
    }
    let mut servers = selected
        .into_iter()
        .map(|(server, mut tools)| {
            tools.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            McpServerSelection { server, tools }
        })
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| left.server.encode_utf16().cmp(right.server.encode_utf16()));
    Ok(servers)
}

pub(crate) fn mcp_snapshot_for_session(config: &SessionConfig) -> Result<McpSessionSnapshot> {
    let Some(binding) = &config.mcp_manifest else {
        return Ok(McpSessionSnapshot::empty());
    };
    let manifest: McpSessionManifest = serde_json::from_value(binding.manifest.clone())
        .context("deserialize persisted MCP session manifest")?;
    if manifest.manifest_fingerprint != binding.manifest_fingerprint {
        anyhow::bail!("persisted MCP session binding fingerprint does not match its manifest");
    }
    McpSessionSnapshot::from_persisted(manifest).context("validate persisted MCP session manifest")
}

pub(crate) fn first_party_toolsets(
    state: &AppState,
    profile: PromptProfile,
) -> HashMap<ProviderKind, Vec<agent_tools::ProviderTool>> {
    [ProviderKind::OpenAi, ProviderKind::Claude]
        .into_iter()
        .map(|provider| {
            (
                provider,
                provider_tools_for_session(state, provider, profile),
            )
        })
        .collect()
}

pub(crate) fn provider_toolset_fingerprint(tools: &[agent_tools::ProviderTool]) -> String {
    agent_mcp_types::fingerprint_json(&Value::Array(
        tools.iter().map(|tool| tool.declaration.clone()).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use agent_store::SessionConfig;
    use agent_tools::ProviderTool;
    use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
    use serde_json::json;

    use super::{mcp_snapshot_for_session, provider_toolset_fingerprint};

    #[test]
    fn missing_session_binding_is_explicitly_mcp_free() {
        let config = SessionConfig {
            project_id: None,
            runtime_id: "runtime-test".to_string(),
            workspace_id: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: "prompt".to_string(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test-model".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
            mcp_manifest: None,
        };
        assert!(mcp_snapshot_for_session(&config)
            .expect("missing binding resolves")
            .manifest()
            .tools
            .is_empty());
    }

    #[test]
    fn provider_toolset_fingerprint_covers_exact_ordered_declarations() {
        let first = ProviderTool::function_json_named(
            ProviderKind::OpenAi,
            "first",
            "first",
            json!({ "type": "object" }),
        );
        let second = ProviderTool::function_json_named(
            ProviderKind::OpenAi,
            "second",
            "second",
            json!({ "type": "object" }),
        );
        assert_ne!(
            provider_toolset_fingerprint(&[first.clone(), second.clone()]),
            provider_toolset_fingerprint(&[second, first.clone()])
        );
        let changed = ProviderTool::function_json_named(
            ProviderKind::OpenAi,
            "first",
            "changed declaration",
            json!({ "type": "object" }),
        );
        assert_ne!(
            provider_toolset_fingerprint(&[first]),
            provider_toolset_fingerprint(&[changed])
        );
    }
}
