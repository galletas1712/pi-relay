use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn tool(server_id: &str, raw_name: &str, description: &str, schema: Value) -> DiscoveredTool {
    DiscoveredTool {
        server_id: server_id.to_string(),
        server_config_fingerprint: format!("{server_id}-config"),
        raw_name: raw_name.to_string(),
        description: description.to_string(),
        input_schema: schema,
    }
}

fn catalog(tools: Vec<DiscoveredTool>) -> McpSessionManifest {
    let configs = tools
        .iter()
        .map(|tool| {
            (
                tool.server_id.clone(),
                tool.server_config_fingerprint.clone(),
            )
        })
        .collect();
    build_inventory_catalog(&configs, tools, &BTreeSet::new()).expect("catalog builds")
}

#[test]
fn inventory_revision_and_names_are_deterministic() {
    let left = catalog(vec![
        tool(
            "z",
            "read",
            "read",
            json!({"properties":{"b":{"type":"number"},"a":{"type":"string"}},"type":"object"}),
        ),
        tool("a", "write", "write", json!({"type":"object"})),
    ]);
    let right = catalog(vec![
        tool("a", "write", "write", json!({"type":"object"})),
        tool(
            "z",
            "read",
            "read",
            json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"number"}}}),
        ),
    ]);

    assert_eq!(left, right);
}

#[test]
fn server_revision_is_independent_of_unrelated_servers_and_changes_with_contract() {
    let original = catalog(vec![
        tool("a", "read", "read", json!({"type":"object"})),
        tool("b", "write", "write", json!({"type":"object"})),
    ]);
    let unrelated_changed = catalog(vec![
        tool("a", "read", "read", json!({"type":"object"})),
        tool("b", "write", "changed", json!({"type":"object"})),
    ]);
    let contract_changed = catalog(vec![
        tool("a", "read", "changed", json!({"type":"object"})),
        tool("b", "write", "write", json!({"type":"object"})),
    ]);

    assert_eq!(
        original.server_revisions["a"],
        unrelated_changed.server_revisions["a"]
    );
    assert_ne!(
        original.server_revisions["a"],
        contract_changed.server_revisions["a"]
    );
    assert_ne!(
        original.inventory_revision,
        unrelated_changed.inventory_revision
    );
}

#[test]
fn selected_subset_preserves_inventory_names_and_contains_mcp_only_declarations() {
    let inventory = catalog(vec![
        tool("same server", "read/file", "one", json!({"type":"object"})),
        tool("same_server", "read_file", "two", json!({"type":"object"})),
    ]);
    let selected_tool = inventory
        .tools
        .iter()
        .find(|tool| tool.server_id == "same server")
        .expect("tool exists")
        .clone();
    let selected = select_manifest(
        &inventory,
        &BTreeMap::from([(
            "same server".to_string(),
            BTreeSet::from(["read/file".to_string()]),
        )]),
    )
    .expect("selection succeeds");
    let snapshot = McpSessionSnapshot::new(selected).expect("snapshot builds");

    assert_eq!(snapshot.manifest().tools, vec![selected_tool.clone()]);
    assert_eq!(
        snapshot.provider_tools(ProviderKind::OpenAi)[0].declaration,
        json!({
            "type": "function",
            "name": selected_tool.exposed_name,
            "description": "one",
            "parameters": {"type":"object"},
        })
    );
    assert_eq!(snapshot.provider_tools(ProviderKind::OpenAi).len(), 1);
}

#[test]
fn persisted_manifests_revalidate_version_contract_declarations_and_fingerprint() {
    let inventory = catalog(vec![tool(
        "server",
        "read",
        "read",
        json!({"type":"object"}),
    )]);
    let selected = select_manifest(
        &inventory,
        &BTreeMap::from([("server".to_string(), BTreeSet::from(["read".to_string()]))]),
    )
    .expect("selection succeeds");
    let snapshot = McpSessionSnapshot::new(selected).expect("snapshot builds");

    let mut invalid_version = snapshot.manifest().clone();
    invalid_version.version += 1;
    assert!(McpSessionSnapshot::from_persisted(invalid_version).is_err());

    let mut invalid_contract = snapshot.manifest().clone();
    invalid_contract.tools[0].description = "changed".to_string();
    assert!(McpSessionSnapshot::from_persisted(invalid_contract).is_err());

    let mut invalid_declaration = snapshot.manifest().clone();
    invalid_declaration.openai_tools[0].description = "changed".to_string();
    assert!(McpSessionSnapshot::from_persisted(invalid_declaration).is_err());
}

#[test]
fn conflicting_raw_names_builtin_collisions_and_bounds_are_rejected() {
    let exact = tool("server", "read", "same", json!({"type":"object"}));
    assert_eq!(catalog(vec![exact.clone(), exact]).tools.len(), 1);
    let configs = BTreeMap::from([("server".to_string(), "config".to_string())]);
    assert!(build_inventory_catalog(
        &configs,
        vec![
            tool("server", "read", "one", json!({"type":"object"})),
            tool("server", "read", "two", json!({"type":"object"})),
        ],
        &BTreeSet::new(),
    )
    .is_err());
    assert!(build_inventory_catalog(
        &configs,
        vec![tool("server", "tool", "test", json!({"type":"object"}))],
        &BTreeSet::from(["mcp__server__tool".to_string()]),
    )
    .is_err());
    assert!(build_inventory_catalog(
        &configs,
        vec![tool(
            "server",
            "read",
            &"x".repeat(MAX_DESCRIPTION_BYTES + 1),
            json!({"type":"object"}),
        )],
        &BTreeSet::new(),
    )
    .is_err());
}

#[test]
fn empty_snapshot_is_explicit_and_valid() {
    let snapshot = McpSessionSnapshot::empty();
    assert!(snapshot.manifest().tools.is_empty());
    assert!(!snapshot.inventory_revision().is_empty());
    assert!(!snapshot.manifest_fingerprint().is_empty());
    McpSessionSnapshot::from_persisted(snapshot.manifest().clone())
        .expect("empty manifest rehydrates");
}
