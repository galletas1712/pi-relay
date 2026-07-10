use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_tools::ToolRegistry;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::McpTransportConfig;

fn first_party() -> HashMap<ProviderKind, Vec<ProviderTool>> {
    let registry = ToolRegistry::with_builtin_tools();
    [ProviderKind::OpenAi, ProviderKind::Claude]
        .into_iter()
        .map(|provider| (provider, registry.provider_tools_for_provider(provider)))
        .collect()
}

#[tokio::test]
async fn stdio_ignores_unsolicited_list_changed_without_negotiated_capability() {
    let marker = temp_path("mcp-unsolicited-list-changed");
    let manager = McpManager::start(one_server_config("unsolicited_list_changed", Some(&marker)))
        .await
        .expect("manager starts");
    let before = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let snapshot = select_all(&manager).await;
    let tool = snapshot.manifest().tools[0].exposed_name.clone();
    manager
        .call(&snapshot, &tool, json!({"value": "notify"}))
        .await
        .expect("unsolicited notification does not fence the call");
    wait_for_marker(&marker, "NOTIFICATION_SENT").await;

    let client = manager
        .servers
        .read()
        .await
        .get("fixture")
        .and_then(|server| server.client.clone())
        .expect("stdio client remains available");
    assert_eq!(client.tools_revision(), 0);
    assert!(!client.tools_uncertain());
    let after = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory remains coherent");
    assert_eq!(after.revision, before.revision);
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

async fn select_all(manager: &McpManager) -> McpSessionSnapshot {
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let selection = McpSessionSelection {
        inventory_revision: inventory.revision,
        servers: inventory
            .servers
            .into_iter()
            .filter(|server| server.health == McpHealth::Healthy && !server.tools.is_empty())
            .map(|server| McpServerSelection {
                server: server.server,
                tools: server.tools.into_iter().map(|tool| tool.raw_name).collect(),
            })
            .collect(),
    };
    manager
        .select(&selection, &first_party())
        .await
        .expect("selection binds")
}

async fn wait_for_marker(path: &std::path::Path, marker: &str) {
    for _ in 0..200 {
        if std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(marker)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture marker {marker:?} was not written");
}

#[cfg(unix)]
fn marker_pids(contents: &str, label: &str) -> Vec<u32> {
    contents
        .lines()
        .filter_map(|line| {
            let (line_label, pid) = line.split_once(' ')?;
            (line_label == label).then(|| pid.parse().ok()).flatten()
        })
        .collect()
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    for _ in 0..200 {
        if !std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process {pid} remained alive");
}

#[tokio::test]
async fn selection_rejects_stale_unknown_and_duplicate_identities() {
    let manager = McpManager::start(one_server_config("normal", None))
        .await
        .expect("manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let stale = McpSessionSelection {
        inventory_revision: "stale".to_string(),
        servers: Vec::new(),
    };
    assert!(matches!(
        manager.select(&stale, &first_party()).await,
        Err(McpManagerError::InventoryChanged { .. })
    ));
    let empty = manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision.clone(),
                servers: Vec::new(),
            },
            &first_party(),
        )
        .await
        .expect("empty selection binds");
    assert_eq!(empty.manifest().tools, Vec::new());
    let unknown = McpSessionSelection {
        inventory_revision: inventory.revision.clone(),
        servers: vec![McpServerSelection {
            server: "fixture".to_string(),
            tools: vec!["unknown".to_string()],
        }],
    };
    assert!(matches!(
        manager.select(&unknown, &first_party()).await,
        Err(McpManagerError::SelectionInvalid { .. })
    ));
    let duplicate = McpSessionSelection {
        inventory_revision: inventory.revision.clone(),
        servers: vec![McpServerSelection {
            server: "fixture".to_string(),
            tools: vec!["read".to_string(), "read".to_string()],
        }],
    };
    assert!(matches!(
        manager.select(&duplicate, &first_party()).await,
        Err(McpManagerError::SelectionInvalid { .. })
    ));
    let unsorted = McpSessionSelection {
        inventory_revision: inventory.revision,
        servers: vec![McpServerSelection {
            server: "fixture".to_string(),
            tools: vec!["fail".to_string(), "echo".to_string()],
        }],
    };
    assert!(matches!(
        manager.select(&unsorted, &first_party()).await,
        Err(McpManagerError::SelectionInvalid { .. })
    ));
    manager.shutdown().await;
}

#[tokio::test]
async fn explicit_subset_is_frozen_and_inventory_estimate_is_provider_specific() {
    let manager = McpManager::start(normal_config(None))
        .await
        .expect("manager starts");
    let inventory = manager
        .inventory(ProviderKind::Claude, &first_party())
        .await
        .expect("inventory loads");
    assert!(inventory.servers[0]
        .tools
        .iter()
        .all(|tool| tool.context_token_estimate > 0));
    let selection = McpSessionSelection {
        inventory_revision: inventory.revision,
        servers: vec![McpServerSelection {
            server: "fixture".to_string(),
            tools: vec!["echo".to_string()],
        }],
    };
    let snapshot = manager
        .select(&selection, &first_party())
        .await
        .expect("selection binds");
    assert_eq!(
        snapshot
            .manifest()
            .tools
            .iter()
            .map(|tool| tool.raw_name.as_str())
            .collect::<Vec<_>>(),
        vec!["echo"]
    );
    assert_eq!(snapshot.provider_tools(ProviderKind::OpenAi).len(), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn selected_unavailable_gates_while_unselected_unavailable_does_not() {
    let config: McpConfig = serde_json::from_value(json!({
        "servers": {
            "healthy": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "down": {
                "transport": {"type": "stdio", "command": "/definitely/missing/pi-relay-mcp"},
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");
    let manager = McpManager::start(config).await.expect("manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let selected_healthy = McpSessionSelection {
        inventory_revision: inventory.revision.clone(),
        servers: vec![McpServerSelection {
            server: "healthy".to_string(),
            tools: vec!["read".to_string()],
        }],
    };
    manager
        .select(&selected_healthy, &first_party())
        .await
        .expect("unselected unavailable server does not gate");
    let selected_down = McpSessionSelection {
        inventory_revision: inventory.revision,
        servers: vec![McpServerSelection {
            server: "down".to_string(),
            tools: vec!["read".to_string()],
        }],
    };
    assert!(matches!(
        manager.select(&selected_down, &first_party()).await,
        Err(McpManagerError::Unavailable { server }) if server == "down"
    ));
    manager.shutdown().await;
}

#[tokio::test]
async fn unselected_ordinary_outage_retains_a_coherent_catalog() {
    let marker = temp_path("mcp-unselected-ordinary-outage");
    let config: McpConfig = serde_json::from_value(json!({
        "servers": {
            "a": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "b": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {
                        "MCP_FIXTURE_MODE": "exit_then_fail",
                        "MCP_FIXTURE_MARKER": marker
                    }
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");
    let manager = McpManager::start(config).await.expect("manager starts");
    let exiting_client = manager
        .servers
        .read()
        .await
        .get("b")
        .and_then(|server| server.client.clone())
        .expect("B starts with a client");
    let before = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("initial inventory loads");
    assert_eq!(
        before
            .servers
            .iter()
            .find(|server| server.server == "b")
            .expect("B is in the initial inventory")
            .health,
        McpHealth::Healthy
    );
    std::fs::write(&marker, "EXIT_REQUESTED\n").expect("B exit is requested");
    tokio::time::timeout(Duration::from_secs(2), exiting_client.wait_for_closed())
        .await
        .expect("B process death is observed");

    let unavailable = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("ordinary outage retains a coherent inventory");
    assert_eq!(unavailable.revision, before.revision);
    assert_eq!(
        unavailable
            .servers
            .iter()
            .find(|server| server.server == "b")
            .expect("B remains in inventory")
            .health,
        McpHealth::Unavailable
    );
    manager
        .select(
            &McpSessionSelection {
                inventory_revision: unavailable.revision,
                servers: vec![McpServerSelection {
                    server: "a".to_string(),
                    tools: vec!["read".to_string()],
                }],
            },
            &first_party(),
        )
        .await
        .expect("unselected ordinary outage does not gate A");

    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn failed_unselected_list_changed_refresh_fences_inventory_until_recovery() {
    let marker = temp_path("mcp-unselected-list-changed-failure");
    let config: McpConfig = serde_json::from_value(json!({
        "servers": {
            "a": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "b": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {
                        "MCP_FIXTURE_MODE": "notification_refresh_failure",
                        "MCP_FIXTURE_MARKER": marker
                    }
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");
    let manager = McpManager::start(config).await.expect("manager starts");
    let before = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("initial inventory loads");
    let b_snapshot = manager
        .select(
            &McpSessionSelection {
                inventory_revision: before.revision.clone(),
                servers: vec![McpServerSelection {
                    server: "b".to_string(),
                    tools: vec!["read".to_string()],
                }],
            },
            &first_party(),
        )
        .await
        .expect("B selection freezes");
    let b_tool = b_snapshot.manifest().tools[0].exposed_name.clone();
    manager
        .call(&b_snapshot, &b_tool, json!({"value": "notify"}))
        .await
        .expect("B call sends list_changed");
    wait_for_marker(&marker, "NOTIFICATION_SENT").await;

    let incomplete_revision = match manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
    {
        Err(McpManagerError::InventoryChanged { current_revision }) => current_revision,
        _ => panic!("failed stale refresh must not publish an inventory"),
    };
    assert_ne!(incomplete_revision, before.revision);
    {
        let servers = manager.servers.read().await;
        let b = servers.get("b").expect("B remains configured");
        assert!(!b.catalog_coherent);
        assert!(b.client.is_none());
    }

    let selection_for_a = |inventory_revision| McpSessionSelection {
        inventory_revision,
        servers: vec![McpServerSelection {
            server: "a".to_string(),
            tools: vec!["read".to_string()],
        }],
    };
    assert!(matches!(
        manager
            .select(&selection_for_a(before.revision.clone()), &first_party())
            .await,
        Err(McpManagerError::InventoryChanged { .. })
    ));
    assert!(matches!(
        manager
            .select(
                &selection_for_a(incomplete_revision.clone()),
                &first_party()
            )
            .await,
        Err(McpManagerError::InventoryChanged { .. })
    ));

    {
        use std::io::Write;

        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&marker)
                .expect("fixture marker opens"),
            "RECOVER"
        )
        .expect("fixture recovery marker writes");
    }
    let recovered = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("coherent inventory publishes after recovery");
    assert_ne!(recovered.revision, before.revision);
    assert_ne!(recovered.revision, incomplete_revision);
    assert_eq!(
        recovered
            .servers
            .iter()
            .find(|server| server.server == "b")
            .expect("B returns to inventory")
            .health,
        McpHealth::Healthy
    );
    manager
        .select(&selection_for_a(recovered.revision), &first_party())
        .await
        .expect("coherent recovered revision binds A");

    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn unselected_list_changed_invalidates_the_global_inventory_revision() {
    let marker = temp_path("mcp-unselected-list-changed");
    let config: McpConfig = serde_json::from_value(json!({
        "servers": {
            "a": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "b": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {
                        "MCP_FIXTURE_MODE": "notification_race",
                        "MCP_FIXTURE_MARKER": marker
                    }
                },
                "allow_all_tools": true,
                "call_timeout_ms": 1_000
            }
        }
    }))
    .expect("config parses");
    let manager = McpManager::start(config).await.expect("manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let b_snapshot = manager
        .select(
            &McpSessionSelection {
                inventory_revision: inventory.revision.clone(),
                servers: vec![McpServerSelection {
                    server: "b".to_string(),
                    tools: vec!["echo".to_string()],
                }],
            },
            &first_party(),
        )
        .await
        .expect("B selection freezes");
    let b_tool = b_snapshot.manifest().tools[0].exposed_name.clone();
    let call = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .call(&b_snapshot, &b_tool, json!({"value": "notify"}))
                .await
        })
    };
    wait_for_marker(&marker, "NOTIFICATION_SENT").await;
    for _ in 0..200 {
        let stale_and_admitted = {
            let servers = manager.servers.read().await;
            let b = servers.get("b").expect("B remains configured");
            b.client.as_ref().is_some_and(|client| {
                !client.tools_uncertain() && client.tools_revision() != b.catalog_tools_revision
            })
        };
        if stale_and_admitted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let stale_a_selection = McpSessionSelection {
        inventory_revision: inventory.revision,
        servers: vec![McpServerSelection {
            server: "a".to_string(),
            tools: vec!["read".to_string()],
        }],
    };
    assert!(matches!(
        manager.select(&stale_a_selection, &first_party()).await,
        Err(McpManagerError::InventoryChanged { .. })
    ));

    let _ = call.await.expect("B call task joins");
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn parallel_calls_use_the_configured_client_semaphore() {
    let mut config = one_server_config("parallel", None);
    let server = config.servers.get_mut("fixture").expect("fixture config");
    server.parallel_calls = 2;
    server.call_timeout_ms = 1_000;
    let manager = McpManager::start(config).await.expect("manager starts");
    let snapshot = select_all(&manager).await;
    let tool = snapshot.manifest().tools.first().expect("tool advertised");
    let started = tokio::time::Instant::now();
    let calls = tokio::join!(
        manager.call(&snapshot, &tool.exposed_name, json!({"value": "one"})),
        manager.call(&snapshot, &tool.exposed_name, json!({"value": "two"}))
    );
    assert!(calls.0.is_ok());
    assert!(calls.1.is_ok());
    assert!(started.elapsed() < Duration::from_millis(280));
    manager.shutdown().await;
}

#[tokio::test]
async fn list_changed_changes_inventory_but_not_a_frozen_snapshot() {
    let marker = temp_path("mcp-list-changed-race");
    let mut config = one_server_config("notification_race", Some(&marker));
    config
        .servers
        .get_mut("fixture")
        .expect("fixture config")
        .call_timeout_ms = 1_000;
    let manager = McpManager::start(config).await.expect("manager starts");
    let before = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory loads");
    let snapshot = select_all(&manager).await;
    let frozen_manifest = snapshot.manifest().clone();
    let tool = snapshot.manifest().tools.first().expect("tool advertised");
    let first = {
        let manager = manager.clone();
        let snapshot = snapshot.clone();
        let name = tool.exposed_name.clone();
        tokio::spawn(async move {
            manager
                .call(&snapshot, &name, json!({"value": "first"}))
                .await
        })
    };
    wait_for_marker(&marker, "NOTIFICATION_SENT").await;
    let second = manager
        .call(&snapshot, &tool.exposed_name, json!({"value": "second"}))
        .await;
    assert!(first.await.expect("first task joins").is_ok());
    assert!(matches!(second, Err(McpCallError::ContractChanged { .. })));
    let after = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory refreshes");
    assert_ne!(before.revision, after.revision);
    assert_eq!(snapshot.manifest(), &frozen_manifest);
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn startup_timeout_kills_the_process_tree() {
    let marker = temp_path("mcp-startup-timeout");
    let mut config = one_server_config("startup_timeout_descendant", Some(&marker));
    config
        .servers
        .get_mut("fixture")
        .expect("fixture config")
        .startup_timeout_ms = 100;
    let manager = McpManager::start(config).await.expect("manager starts");
    let inventory = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory remains observable");
    assert_eq!(inventory.servers[0].health, McpHealth::Unavailable);
    let contents = std::fs::read_to_string(&marker).expect("fixture marker exists");
    #[cfg(unix)]
    {
        for pid in marker_pids(&contents, "START") {
            wait_for_process_exit(pid).await;
        }
        let descendants = marker_pids(&contents, "DESCENDANT");
        assert!(!descendants.is_empty());
        for pid in descendants {
            wait_for_process_exit(pid).await;
        }
    }
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn slow_reconnect_uses_the_total_call_deadline_without_replaying() {
    assert_reconnect_call_deadline("slow_reconnect", "RECONNECT_WAITING").await;
}

#[tokio::test]
async fn stalled_startup_cleanup_stays_outside_the_total_call_deadline() {
    assert_reconnect_call_deadline("stalled_startup", "RECONNECT_LIST_WAITING").await;
}

async fn assert_reconnect_call_deadline(mode: &str, waiting_marker: &str) {
    const CALL_TIMEOUT: Duration = Duration::from_millis(250);
    const DEADLINE_ALLOWANCE: Duration = Duration::from_millis(250);

    let marker = temp_path(&format!("mcp-{mode}"));
    let mut config = one_server_config(mode, Some(&marker));
    config
        .servers
        .get_mut("fixture")
        .expect("fixture config")
        .call_timeout_ms = CALL_TIMEOUT.as_millis() as u64;
    let manager = McpManager::start(config).await.expect("manager starts");
    let snapshot = select_all(&manager).await;
    let tool = snapshot.manifest().tools.first().expect("tool advertised");
    let initial_client = manager
        .servers
        .read()
        .await
        .get("fixture")
        .and_then(|server| server.client.clone())
        .expect("initial client is present");
    {
        use std::io::Write;

        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&marker)
                .expect("fixture marker opens"),
            "EXIT_REQUESTED"
        )
        .expect("fixture exit marker writes");
    }
    wait_for_marker(&marker, "EXITED").await;
    tokio::time::timeout(Duration::from_secs(2), initial_client.wait_for_closed())
        .await
        .expect("initial client exit is observed");

    let started = tokio::time::Instant::now();
    let result = manager
        .call(
            &snapshot,
            &tool.exposed_name,
            json!({"value": "never-issued"}),
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(
        result,
        Err(McpCallError::Timeout {
            tool: tool.exposed_name.clone(),
        })
    );
    assert!(
        elapsed >= CALL_TIMEOUT.saturating_sub(Duration::from_millis(25)),
        "call returned before its reconnect deadline: {elapsed:?}"
    );
    assert!(
        elapsed <= CALL_TIMEOUT + DEADLINE_ALLOWANCE,
        "call exceeded its reconnect deadline allowance: {elapsed:?}"
    );
    wait_for_marker(&marker, waiting_marker).await;
    {
        let servers = manager.servers.read().await;
        let server = servers.get("fixture").expect("fixture remains configured");
        assert_eq!(server.health, McpHealth::Unavailable);
        assert!(server.client.is_none());
    }

    let recovered = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("a newer reconnect publishes a coherent catalog");
    assert_eq!(recovered.servers[0].health, McpHealth::Healthy);
    let (recovered_generation, recovered_client, recovered_tools) = {
        let servers = manager.servers.read().await;
        let server = servers.get("fixture").expect("fixture remains configured");
        (
            server.refresh.generation,
            server.client.clone().expect("newer client is published"),
            server.tools.clone(),
        )
    };
    tokio::time::sleep(DEADLINE_ALLOWANCE).await;
    {
        let servers = manager.servers.read().await;
        let server = servers.get("fixture").expect("fixture remains configured");
        assert_eq!(server.refresh.generation, recovered_generation);
        assert_eq!(server.tools, recovered_tools);
        assert!(server
            .client
            .as_ref()
            .is_some_and(|client| Arc::ptr_eq(client, &recovered_client)));
    }
    let contents = std::fs::read_to_string(&marker).expect("fixture marker exists");
    assert_eq!(contents.matches("CALL").count(), 0);

    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn blocked_writer_does_not_extend_deadline_or_replay() {
    let marker = temp_path("mcp-backpressure");
    let manager = McpManager::start(one_server_config("cancel_backpressure", Some(&marker)))
        .await
        .expect("manager starts");
    let snapshot = select_all(&manager).await;
    let tool = snapshot.manifest().tools.first().expect("tool advertised");
    wait_for_marker(&marker, "READY_TO_BLOCK").await;
    let started = tokio::time::Instant::now();
    assert!(matches!(
        manager
            .call(
                &snapshot,
                &tool.exposed_name,
                json!({"value": "x".repeat(200_000)}),
            )
            .await,
        Err(McpCallError::Timeout { .. })
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let contents = std::fs::read_to_string(&marker).expect("fixture marker exists");
    assert_eq!(contents.matches("CALL").count(), 0);
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn exact_route_survives_operational_and_unrelated_config_changes() {
    let config: McpConfig = serde_json::from_value(json!({
        "servers": {
            "target": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple"}
                },
                "allow_all_tools": true
            },
            "unrelated": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": {"MCP_FIXTURE_MODE": "simple", "PUBLIC_SETTING": "one"}
                },
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");
    let first = McpManager::start(config.clone())
        .await
        .expect("first manager starts");
    let snapshot = select_all(&first).await;
    first.shutdown().await;
    let mut changed = config;
    changed
        .servers
        .get_mut("target")
        .expect("target exists")
        .startup_timeout_ms += 1;
    let McpTransportConfig::Stdio(unrelated) = &mut changed
        .servers
        .get_mut("unrelated")
        .expect("unrelated exists")
        .transport
    else {
        panic!("fixture uses stdio");
    };
    unrelated
        .env
        .insert("PUBLIC_SETTING".to_string(), "two".to_string());
    let second = McpManager::start(changed)
        .await
        .expect("second manager starts");
    let target = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.server_id == "target")
        .expect("target tool selected");
    assert!(second
        .call(&snapshot, &target.exposed_name, json!({}))
        .await
        .is_ok());
    second.shutdown().await;
}

#[tokio::test]
async fn exact_route_classifies_changed_revoked_and_unavailable() {
    let first = McpManager::start(one_server_config("simple", None))
        .await
        .expect("first manager starts");
    let snapshot = select_all(&first).await;
    first.shutdown().await;
    let tool = snapshot.manifest().tools.first().expect("tool selected");

    let mut changed_config = one_server_config("simple", None);
    let McpTransportConfig::Stdio(changed_stdio) = &mut changed_config
        .servers
        .get_mut("fixture")
        .expect("fixture exists")
        .transport
    else {
        panic!("fixture uses stdio");
    };
    changed_stdio
        .env
        .insert("PUBLIC_SETTING".to_string(), "different".to_string());
    let changed = McpManager::start(changed_config)
        .await
        .expect("changed manager starts");
    assert!(matches!(
        changed.call(&snapshot, &tool.exposed_name, json!({})).await,
        Err(McpCallError::ContractChanged { .. })
    ));
    changed.shutdown().await;

    let mut filtered_config = one_server_config("simple", None);
    let filtered_server = filtered_config
        .servers
        .get_mut("fixture")
        .expect("fixture exists");
    filtered_server.allow_all_tools = false;
    filtered_server
        .enabled_tools
        .insert("different".to_string());
    let filtered = McpManager::start(filtered_config)
        .await
        .expect("filtered manager starts");
    assert!(matches!(
        filtered
            .call(&snapshot, &tool.exposed_name, json!({}))
            .await,
        Err(McpCallError::Revoked { .. })
    ));
    filtered.shutdown().await;

    let unavailable: McpConfig = serde_json::from_value(json!({
        "servers": {
            "fixture": {
                "transport": {"type": "stdio", "command": "/definitely/missing/pi-relay-mcp"},
                "allow_all_tools": true
            }
        }
    }))
    .expect("config parses");
    let unavailable = McpManager::start(unavailable)
        .await
        .expect("unavailable manager starts");
    assert!(matches!(
        unavailable
            .call(&snapshot, &tool.exposed_name, json!({}))
            .await,
        Err(McpCallError::ServerUnavailable { .. })
    ));
    unavailable.shutdown().await;
}

#[tokio::test]
async fn timeout_sends_cancellation_and_never_replays() {
    let marker = temp_path("mcp-timeout");
    let manager = McpManager::start(one_server_config("timeout", Some(&marker)))
        .await
        .expect("manager starts");
    let snapshot = select_all(&manager).await;
    let tool = snapshot.manifest().tools.first().expect("tool advertised");
    assert!(matches!(
        manager
            .call(&snapshot, &tool.exposed_name, json!({"value": "wait"}))
            .await,
        Err(McpCallError::Timeout { .. })
    ));
    for _ in 0..50 {
        if std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .contains("CANCEL")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let contents = std::fs::read_to_string(&marker).expect("fixture recorded call");
    assert_eq!(contents.matches("CALL").count(), 1);
    assert_eq!(contents.matches("CANCEL").count(), 1);
    manager.shutdown().await;
    std::fs::remove_file(marker).ok();
}

#[tokio::test]
async fn stdio_client_initializes_pages_calls_refreshes_and_cleans_up() {
    let marker = temp_path("mcp-cleanup");
    let manager = McpManager::start(normal_config(Some(&marker)))
        .await
        .expect("manager starts");
    let snapshot = select_all(&manager).await;
    assert_eq!(
        snapshot
            .manifest()
            .tools
            .iter()
            .map(|tool| tool.raw_name.as_str())
            .collect::<Vec<_>>(),
        vec!["echo", "fail"]
    );
    let echo = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.raw_name == "echo")
        .expect("echo selected");
    assert_eq!(
        manager
            .call(&snapshot, &echo.exposed_name, json!({"value": "hello"}))
            .await
            .expect("call succeeds"),
        McpCallOutput {
            output: "hello\n[structured content]\n{\"a\":2,\"z\":1}".to_string(),
            is_error: false,
        }
    );
    assert!(matches!(
        manager
            .call(&snapshot, &echo.exposed_name, json!({"value": "old"}))
            .await,
        Err(McpCallError::ContractChanged { .. })
    ));
    let fail = snapshot
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.raw_name == "fail")
        .expect("fail selected");
    assert_eq!(
        manager
            .call(&snapshot, &fail.exposed_name, json!({}))
            .await
            .expect("MCP isError is a normal result"),
        McpCallOutput {
            output: "expected failure".to_string(),
            is_error: true,
        }
    );
    let refreshed = manager
        .inventory(ProviderKind::OpenAi, &first_party())
        .await
        .expect("inventory refreshes");
    assert_ne!(refreshed.revision, snapshot.inventory_revision());
    assert_eq!(
        snapshot.manifest().tools[0].description,
        "Echo arguments v1"
    );
    manager.shutdown().await;
    assert!(matches!(
        manager.call(&snapshot, &echo.exposed_name, json!({})).await,
        Err(McpCallError::ServerUnavailable { .. })
    ));
    std::fs::remove_file(marker).ok();
}

fn normal_config(marker: Option<&std::path::Path>) -> McpConfig {
    let mut env = serde_json::Map::from_iter([(
        "MCP_FIXTURE_MODE".to_string(),
        Value::String("normal".to_string()),
    )]);
    if let Some(marker) = marker {
        env.insert(
            "MCP_FIXTURE_MARKER".to_string(),
            Value::String(marker.to_string_lossy().into_owned()),
        );
    }
    serde_json::from_value(json!({
        "servers": {
            "fixture": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": env
                },
                "enabled_tools": ["echo", "fail"]
            }
        }
    }))
    .expect("config parses")
}

fn one_server_config(mode: &str, marker: Option<&std::path::Path>) -> McpConfig {
    let mut env = serde_json::Map::from_iter([(
        "MCP_FIXTURE_MODE".to_string(),
        Value::String(mode.to_string()),
    )]);
    if let Some(marker) = marker {
        env.insert(
            "MCP_FIXTURE_MARKER".to_string(),
            Value::String(marker.to_string_lossy().into_owned()),
        );
    }
    serde_json::from_value(json!({
        "servers": {
            "fixture": {
                "transport": {
                    "type": "stdio",
                    "command": fake_server(),
                    "env": env
                },
                "allow_all_tools": true,
                "call_timeout_ms": 100
            }
        }
    }))
    .expect("config parses")
}

fn fake_server() -> String {
    env!("AGENT_MCP_FAKE_SERVER").to_string()
}

fn temp_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time advances")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}
