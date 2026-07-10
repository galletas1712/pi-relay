use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_tools::{ProviderTool, ToolExecution};
use agent_vocab::ProviderKind;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const MANIFEST_VERSION: u32 = 2;
pub(crate) const MAX_TOOLS: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_SCHEMA_BYTES: usize = 24 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODEL_DECLARATION_BYTES: usize = 32 * 1024;
const MAX_PROVIDER_MCP_DECLARATIONS_BYTES: usize = 512 * 1024;
const MAX_PROVIDER_NAME_BYTES: usize = 64;
const HASH_SUFFIX_HEX_BYTES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredTool {
    pub(crate) server_id: String,
    pub(crate) server_config_fingerprint: String,
    pub(crate) raw_name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpManifestTool {
    pub server_id: String,
    pub server_config_fingerprint: String,
    pub raw_name: String,
    pub exposed_name: String,
    pub description: String,
    pub input_schema: Value,
    pub contract_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSessionManifest {
    pub version: u32,
    /// Revision of the complete configured inventory used to create this
    /// selection. It is semantic and deliberately excludes health.
    pub inventory_revision: String,
    /// Revisions of the selected servers' complete operator-allowed catalogs.
    pub server_revisions: BTreeMap<String, String>,
    /// Content address of this exact selected MCP-only manifest.
    pub manifest_fingerprint: String,
    pub tools: Vec<McpManifestTool>,
    /// Exact MCP-only provider declarations in deterministic order.
    pub openai_tools: Vec<ProviderTool>,
    pub anthropic_tools: Vec<ProviderTool>,
}

impl McpSessionManifest {
    pub fn provider_tools(&self, provider: ProviderKind) -> &[ProviderTool] {
        match provider {
            ProviderKind::OpenAi => &self.openai_tools,
            ProviderKind::Claude => &self.anthropic_tools,
        }
    }

    pub fn tool(&self, exposed_name: &str) -> Option<&McpManifestTool> {
        self.tools
            .iter()
            .find(|tool| tool.exposed_name == exposed_name)
    }
}

#[derive(Debug, Clone)]
pub struct McpSessionSnapshot {
    manifest: Arc<McpSessionManifest>,
}

impl McpSessionSnapshot {
    pub(crate) fn new(mut manifest: McpSessionManifest) -> Result<Self> {
        validate_persisted_manifest(&manifest, /*check_fingerprint*/ false)?;
        manifest.manifest_fingerprint = manifest_fingerprint(&manifest);
        Ok(Self {
            manifest: Arc::new(manifest),
        })
    }

    pub fn empty() -> Self {
        Self::new(McpSessionManifest {
            version: MANIFEST_VERSION,
            inventory_revision: fingerprint_json(&serde_json::json!({ "servers": {} })),
            server_revisions: BTreeMap::new(),
            manifest_fingerprint: String::new(),
            tools: Vec::new(),
            openai_tools: Vec::new(),
            anthropic_tools: Vec::new(),
        })
        .expect("the static empty MCP manifest is valid")
    }

    pub fn from_persisted(manifest: McpSessionManifest) -> Result<Self> {
        validate_persisted_manifest(&manifest, /*check_fingerprint*/ true)?;
        Ok(Self {
            manifest: Arc::new(manifest),
        })
    }

    pub fn manifest(&self) -> &McpSessionManifest {
        &self.manifest
    }

    pub fn manifest_arc(&self) -> Arc<McpSessionManifest> {
        self.manifest.clone()
    }

    pub fn inventory_revision(&self) -> &str {
        &self.manifest.inventory_revision
    }

    pub fn manifest_fingerprint(&self) -> &str {
        &self.manifest.manifest_fingerprint
    }

    pub fn provider_tools(&self, provider: ProviderKind) -> Vec<ProviderTool> {
        self.manifest.provider_tools(provider).to_vec()
    }
}

pub(crate) fn build_inventory_catalog(
    server_config_fingerprints: &BTreeMap<String, String>,
    discovered: Vec<DiscoveredTool>,
    builtin_names: &BTreeSet<String>,
) -> Result<McpSessionManifest> {
    if discovered.len() > MAX_TOOLS {
        bail!("MCP inventory has more than {MAX_TOOLS} tools");
    }
    let mut candidates = discovered
        .into_iter()
        .map(canonical_candidate)
        .collect::<Result<Vec<_>>>()?;
    candidates.sort_by(|left, right| {
        left.server_id
            .cmp(&right.server_id)
            .then_with(|| left.raw_name.cmp(&right.raw_name))
            .then_with(|| left.contract_fingerprint.cmp(&right.contract_fingerprint))
    });
    if candidates.windows(2).any(|pair| {
        pair[0].server_id == pair[1].server_id
            && pair[0].raw_name == pair[1].raw_name
            && pair[0].contract_fingerprint != pair[1].contract_fingerprint
    }) {
        bail!("MCP inventory contains conflicting duplicate raw tool names");
    }
    candidates.dedup_by(|left, right| {
        left.server_id == right.server_id
            && left.raw_name == right.raw_name
            && left.contract_fingerprint == right.contract_fingerprint
    });

    let mut preliminary_counts = BTreeMap::<String, usize>::new();
    for candidate in &candidates {
        *preliminary_counts
            .entry(preliminary_name(&candidate.server_id, &candidate.raw_name))
            .or_default() += 1;
    }
    let mut used = BTreeSet::new();
    for candidate in &mut candidates {
        let preliminary = preliminary_name(&candidate.server_id, &candidate.raw_name);
        let needs_hash = preliminary.len() > MAX_PROVIDER_NAME_BYTES
            || preliminary_counts
                .get(&preliminary)
                .copied()
                .unwrap_or_default()
                > 1;
        candidate.exposed_name = if needs_hash {
            name_with_hash(&preliminary, &candidate.contract_fingerprint)
        } else {
            preliminary
        };
        if builtin_names.contains(&candidate.exposed_name) {
            bail!(
                "MCP tool {} collides with a first-party tool",
                candidate.exposed_name
            );
        }
        if !used.insert(candidate.exposed_name.clone()) {
            bail!(
                "MCP exposed-name collision remains for {}",
                candidate.exposed_name
            );
        }
    }
    candidates.sort_by(tool_order);

    let server_revisions = server_config_fingerprints
        .iter()
        .map(|(server_id, config_fingerprint)| {
            let contracts = candidates
                .iter()
                .filter(|tool| tool.server_id == *server_id)
                .map(|tool| {
                    serde_json::json!({
                        "raw_name": tool.raw_name,
                        "contract_fingerprint": tool.contract_fingerprint,
                    })
                })
                .collect::<Vec<_>>();
            (
                server_id.clone(),
                fingerprint_json(&serde_json::json!({
                    "server": server_id,
                    "config_fingerprint": config_fingerprint,
                    "contracts": contracts,
                })),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let inventory_revision = fingerprint_json(&serde_json::json!({
        "version": MANIFEST_VERSION,
        "servers": server_revisions,
    }));
    let openai_tools = candidates
        .iter()
        .map(|tool| provider_tool(tool, ProviderKind::OpenAi))
        .collect();
    let anthropic_tools = candidates
        .iter()
        .map(|tool| provider_tool(tool, ProviderKind::Claude))
        .collect();
    let manifest = McpSessionManifest {
        version: MANIFEST_VERSION,
        inventory_revision,
        server_revisions,
        manifest_fingerprint: String::new(),
        tools: candidates,
        openai_tools,
        anthropic_tools,
    };
    validate_catalog_bytes(&manifest)?;
    Ok(manifest)
}

pub(crate) fn select_manifest(
    inventory: &McpSessionManifest,
    selection: &BTreeMap<String, BTreeSet<String>>,
) -> Result<McpSessionManifest> {
    let selected_server_revisions = selection
        .keys()
        .map(|server| {
            inventory
                .server_revisions
                .get(server)
                .cloned()
                .map(|revision| (server.clone(), revision))
                .ok_or_else(|| anyhow::anyhow!("unknown MCP server {server}"))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let tools = inventory
        .tools
        .iter()
        .filter(|tool| {
            selection
                .get(&tool.server_id)
                .is_some_and(|names| names.contains(&tool.raw_name))
        })
        .cloned()
        .collect::<Vec<_>>();
    for (server, names) in selection {
        for name in names {
            if !tools
                .iter()
                .any(|tool| tool.server_id == *server && tool.raw_name == *name)
            {
                bail!("unknown MCP tool {server}/{name}");
            }
        }
    }
    let openai_tools = tools
        .iter()
        .map(|tool| provider_tool(tool, ProviderKind::OpenAi))
        .collect();
    let anthropic_tools = tools
        .iter()
        .map(|tool| provider_tool(tool, ProviderKind::Claude))
        .collect();
    Ok(McpSessionManifest {
        version: MANIFEST_VERSION,
        inventory_revision: inventory.inventory_revision.clone(),
        server_revisions: selected_server_revisions,
        manifest_fingerprint: String::new(),
        tools,
        openai_tools,
        anthropic_tools,
    })
}

pub(crate) fn declaration_token_estimate(tool: &ProviderTool) -> Result<usize> {
    let bytes = serde_json::to_vec(&canonical_json(&tool.declaration))?.len();
    Ok(bytes.div_ceil(4))
}

fn validate_catalog_bytes(manifest: &McpSessionManifest) -> Result<()> {
    let bytes = serde_json::to_vec(&canonical_json(&serde_json::json!({
        "version": manifest.version,
        "inventory_revision": manifest.inventory_revision,
        "server_revisions": manifest.server_revisions,
        "tools": manifest.tools,
        "openai_tools": manifest.openai_tools,
        "anthropic_tools": manifest.anthropic_tools,
    })))?;
    if bytes.len() > MAX_CATALOG_BYTES {
        bail!("MCP inventory exceeds {MAX_CATALOG_BYTES} bytes");
    }
    validate_mcp_declaration_bounds(manifest)
}

fn validate_mcp_declaration_bounds(manifest: &McpSessionManifest) -> Result<()> {
    for declarations in [&manifest.openai_tools, &manifest.anthropic_tools] {
        let mut total = 0_usize;
        for tool in declarations {
            let bytes = serde_json::to_vec(&canonical_json(&tool.declaration))?.len();
            if bytes > MAX_MODEL_DECLARATION_BYTES {
                bail!(
                    "MCP provider declaration {} exceeds {MAX_MODEL_DECLARATION_BYTES} bytes",
                    tool.name
                );
            }
            total = total.saturating_add(bytes);
        }
        if total > MAX_PROVIDER_MCP_DECLARATIONS_BYTES {
            bail!("MCP provider declarations exceed {MAX_PROVIDER_MCP_DECLARATIONS_BYTES} bytes");
        }
    }
    Ok(())
}

fn validate_persisted_manifest(
    manifest: &McpSessionManifest,
    check_fingerprint: bool,
) -> Result<()> {
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported persisted MCP manifest version {}",
            manifest.version
        );
    }
    if manifest.inventory_revision.is_empty()
        || manifest.tools.len() > MAX_TOOLS
        || manifest
            .server_revisions
            .iter()
            .any(|(server, revision)| server.is_empty() || revision.is_empty())
    {
        bail!("persisted MCP manifest contains invalid inventory metadata");
    }
    let mut exposed_names = BTreeSet::new();
    let mut raw_names = BTreeSet::new();
    let mut prior_sort_key = None;
    for tool in &manifest.tools {
        if !manifest.server_revisions.contains_key(&tool.server_id) {
            bail!("persisted MCP tool has no selected server revision");
        }
        let canonical = canonical_candidate(DiscoveredTool {
            server_id: tool.server_id.clone(),
            server_config_fingerprint: tool.server_config_fingerprint.clone(),
            raw_name: tool.raw_name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })?;
        if canonical.contract_fingerprint != tool.contract_fingerprint
            || canonical.input_schema != tool.input_schema
        {
            bail!("persisted MCP tool contract fingerprint does not match");
        }
        if tool.exposed_name.is_empty()
            || tool.exposed_name.len() > MAX_PROVIDER_NAME_BYTES
            || !tool.exposed_name.is_ascii()
            || !tool
                .exposed_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || !exposed_names.insert(tool.exposed_name.as_str())
            || !raw_names.insert((tool.server_id.as_str(), tool.raw_name.as_str()))
        {
            bail!("persisted MCP tool identity is invalid or duplicated");
        }
        let sort_key = tool_sort_key(tool);
        if prior_sort_key.is_some_and(|prior| prior >= sort_key) {
            bail!("persisted MCP tools are not in deterministic order");
        }
        prior_sort_key = Some(sort_key);
    }
    if manifest
        .server_revisions
        .keys()
        .any(|server| !manifest.tools.iter().any(|tool| tool.server_id == *server))
    {
        bail!("persisted MCP manifest contains an empty selected server");
    }
    validate_provider_pairings(manifest, ProviderKind::OpenAi)?;
    validate_provider_pairings(manifest, ProviderKind::Claude)?;
    validate_catalog_bytes(manifest)?;
    if check_fingerprint && manifest_fingerprint(manifest) != manifest.manifest_fingerprint {
        bail!("persisted MCP manifest fingerprint does not match");
    }
    Ok(())
}

fn validate_provider_pairings(manifest: &McpSessionManifest, provider: ProviderKind) -> Result<()> {
    let expected = manifest
        .tools
        .iter()
        .map(|tool| provider_tool(tool, provider))
        .collect::<Vec<_>>();
    if manifest.provider_tools(provider) != expected {
        bail!("persisted MCP provider declarations do not match the manifest");
    }
    Ok(())
}

fn manifest_fingerprint(manifest: &McpSessionManifest) -> String {
    fingerprint_json(&serde_json::json!({
        "version": manifest.version,
        "inventory_revision": manifest.inventory_revision,
        "server_revisions": manifest.server_revisions,
        "tools": manifest.tools,
        "openai_tools": manifest.openai_tools,
        "anthropic_tools": manifest.anthropic_tools,
    }))
}

pub(crate) fn canonical_candidate(tool: DiscoveredTool) -> Result<McpManifestTool> {
    if tool.server_id.is_empty() || tool.raw_name.is_empty() || tool.raw_name.len() > 256 {
        bail!("MCP server and tool names must be nonempty and tool names at most 256 bytes");
    }
    if tool.description.len() > MAX_DESCRIPTION_BYTES {
        bail!("MCP tool {} description is too large", tool.raw_name);
    }
    if json_depth(&tool.input_schema, 0) > MAX_SCHEMA_DEPTH {
        bail!("MCP tool {} schema is too deep", tool.raw_name);
    }
    let input_schema = canonical_json(&tool.input_schema);
    if serde_json::to_vec(&input_schema)?.len() > MAX_SCHEMA_BYTES {
        bail!("MCP tool {} schema is too large", tool.raw_name);
    }
    let contract_fingerprint = fingerprint_json(&serde_json::json!({
        "server_id": tool.server_id,
        "raw_name": tool.raw_name,
        "description": tool.description,
        "input_schema": input_schema,
    }));
    Ok(McpManifestTool {
        server_id: tool.server_id,
        server_config_fingerprint: tool.server_config_fingerprint,
        raw_name: tool.raw_name,
        exposed_name: String::new(),
        description: tool.description,
        input_schema,
        contract_fingerprint,
    })
}

fn provider_tool(tool: &McpManifestTool, provider: ProviderKind) -> ProviderTool {
    let mut provider_tool = ProviderTool::function_json_named(
        provider,
        tool.exposed_name.clone(),
        tool.description.clone(),
        tool.input_schema.clone(),
    );
    provider_tool.canonical_name = tool.exposed_name.clone();
    provider_tool.execution = ToolExecution::LocalJson;
    provider_tool
}

fn tool_order(left: &McpManifestTool, right: &McpManifestTool) -> std::cmp::Ordering {
    tool_sort_key(left).cmp(&tool_sort_key(right))
}

fn tool_sort_key(tool: &McpManifestTool) -> (&str, &str, &str, &str) {
    (
        &tool.exposed_name,
        &tool.server_id,
        &tool.raw_name,
        &tool.contract_fingerprint,
    )
}

fn preliminary_name(server_id: &str, raw_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        sanitized_component(server_id),
        sanitized_component(raw_name)
    )
}

fn sanitized_component(value: &str) -> String {
    let mut result = String::new();
    let mut replacing = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            result.push(char::from(byte));
            replacing = false;
        } else if !replacing {
            result.push('_');
            replacing = true;
        }
    }
    if result.is_empty() {
        "_".to_string()
    } else {
        result
    }
}

fn name_with_hash(base: &str, fingerprint: &str) -> String {
    let suffix = format!("__{}", &fingerprint[..HASH_SUFFIX_HEX_BYTES]);
    let prefix_bytes = MAX_PROVIDER_NAME_BYTES - suffix.len();
    format!("{}{suffix}", &base[..base.len().min(prefix_bytes)])
}

fn json_depth(value: &Value, depth: usize) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| json_depth(value, depth + 1))
            .max()
            .unwrap_or(depth),
        Value::Object(values) => values
            .values()
            .map(|value| json_depth(value, depth + 1))
            .max()
            .unwrap_or(depth),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => depth,
    }
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let sorted = values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            let mut object = Map::new();
            for (key, value) in sorted {
                object.insert(key, value);
            }
            Value::Object(object)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

pub fn fingerprint_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(&canonical_json(value))
        .expect("serde_json::Value serialization cannot fail");
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
