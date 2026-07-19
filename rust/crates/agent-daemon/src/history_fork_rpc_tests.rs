use super::*;

#[tokio::test]
async fn creates_top_level_root_child_and_publishes_complete_provenance() {
    let Some(env) = test_env().await else {
        eprintln!("skipping; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("PI.md"),
        env.cwd.path().join("PI.md"),
    )
    .expect("copy PI template");
    let project_id = Uuid::new_v4();
    let local_source = env.cwd.path().join("fork-local-source");
    std::fs::create_dir_all(&local_source).expect("create local workspace source");
    std::fs::write(local_source.join("tracked.txt"), "workspace")
        .expect("write local workspace source");
    let project_workspaces = vec![agent_store::ProjectWorkspace::local(
        "docs",
        local_source.to_string_lossy(),
    )];
    env.state
        .repo
        .create_project(
            project_id,
            "runtime-test",
            "fork RPC",
            &project_workspaces,
            json!({}),
        )
        .await
        .expect("create project");
    let selected_workspaces = crate::workspace_selection::WorkspaceSelection::All
        .resolve(&project_workspaces)
        .expect("select project workspaces");
    let (workspace_id, workspaces) = env
        .state
        .runtime_hosts
        .materialize_session(
            TEST_RUNTIME_ID,
            project_id,
            &project_workspaces,
            &selected_workspaces,
        )
        .await
        .expect("materialize managed source cwd");
    let mut config = session_config(
        &env,
        project_id,
        json!({
            "title": "source",
            "auto_title_disabled": true,
            "archived": true,
            "subagent": true,
            "subagent_type": "full",
            "subagent_parent_idle_notification_key": "inherited-key",
            "compaction": {
                "config": { "auto_enabled": true },
                "auto_state": { "suppressed": true },
            },
        }),
    );
    config.workspace_id = workspace_id;
    config.workspaces = workspaces;
    let mcp_snapshot = agent_mcp::McpSessionSnapshot::empty();
    config.mcp_manifest = Some(McpSessionManifestBinding {
        manifest_fingerprint: mcp_snapshot.manifest_fingerprint().to_string(),
        manifest: serde_json::to_value(mcp_snapshot.manifest()).expect("MCP manifest serializes"),
    });
    let entries = vec![
        TranscriptStorageNode {
            id: "fork-rpc-start".to_string(),
            parent_id: None,
            timestamp_ms: 1,
            item: TranscriptItem::TurnStarted { turn_id: TurnId(1) },
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "fork-rpc-user".to_string(),
            parent_id: Some("fork-rpc-start".to_string()),
            timestamp_ms: 2,
            item: TranscriptItem::UserMessage(UserMessage::text("source message")),
            provider_replay: Vec::new(),
        },
        TranscriptStorageNode {
            id: "fork-rpc-finish".to_string(),
            parent_id: Some("fork-rpc-user".to_string()),
            timestamp_ms: 3,
            item: TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
            provider_replay: Vec::new(),
        },
    ];
    env.state
        .repo
        .start_session_outputs(
            "fork-rpc-source",
            &config,
            &entries,
            Some("fork-rpc-finish"),
            &[],
            &[],
            InputPriority::FollowUp,
            &UserMessage::text("source message"),
            None,
        )
        .await
        .expect("create source session");
    let revision = env
        .state
        .repo
        .session_snapshot("fork-rpc-source")
        .await
        .expect("source snapshot")
        .transcript_revision;
    let mut events = env.state.events.subscribe();

    let result = public_rpc(
        &env.state,
        "history.fork",
        json!({
            "session_id": "fork-rpc-source",
            "leaf_id": null,
            "expected_active_leaf_id": "fork-rpc-finish",
            "expected_transcript_revision": revision,
        }),
    )
    .await
    .expect("fork RPC succeeds");
    let child_session_id = result["session_id"].as_str().expect("child id");
    assert!(child_session_id.starts_with("session_"));
    assert_ne!(child_session_id, "fork-rpc-source");
    assert_eq!(result["source_session_id"], "fork-rpc-source");
    assert_eq!(result["source_leaf_id"], serde_json::Value::Null);
    let created = events.recv().await.expect("published session.created");
    assert_eq!(created.event, EventType::SessionCreated);
    assert_eq!(created.session_id, child_session_id);
    assert_eq!(
        created.data,
        json!({
            "session_id": child_session_id,
            "project_id": project_id,
            "provider": config.provider,
            "active_leaf_id": null,
            "source_session_id": "fork-rpc-source",
            "source_leaf_id": null,
            "session_revision": result["session_revision"],
            "queue_revision": result["queue_revision"],
            "transcript_revision": result["transcript_revision"],
        })
    );

    let child = public_rpc(
        &env.state,
        "session.get",
        json!({ "session_id": child_session_id, "include_entries": true }),
    )
    .await
    .expect("cold child load succeeds");
    assert_eq!(child["project_id"], json!(project_id));
    assert_eq!(child["provider"], json!(config.provider));
    assert_eq!(child["active_leaf_id"], serde_json::Value::Null);
    assert_eq!(child["parent_session_id"], serde_json::Value::Null);
    assert_eq!(child["metadata"]["title"], "source");
    assert_eq!(child["metadata"]["auto_title_disabled"], true);
    assert_eq!(child["metadata"]["prompt_profile"], "parent");
    assert_eq!(
        child["metadata"]["fork"],
        json!({
            "source_session_id": "fork-rpc-source",
            "source_leaf_id": null,
        })
    );
    assert_eq!(
        child["metadata"]["compaction"]["config"],
        json!({ "auto_enabled": true })
    );
    assert!(child["metadata"].get("archived").is_none());
    assert!(child["metadata"]["compaction"].get("auto_state").is_none());
    assert!(child["metadata"].get("subagent").is_none());
    assert!(child["metadata"]
        .get("subagent_parent_idle_notification_key")
        .is_none());
    assert_eq!(child["pending_actions"], json!([]));
    assert_eq!(child["queued_inputs"], json!([]));
    assert_eq!(child["entries"].as_array().expect("child entries").len(), 3);
    let child_config = env
        .state
        .repo
        .load_session_config(child_session_id)
        .await
        .expect("child config loads");
    assert_eq!(child_config.project_id, Some(project_id));
    assert_eq!(
        serde_json::to_value(&child_config.provider).expect("child provider serializes"),
        serde_json::to_value(&config.provider).expect("source provider serializes")
    );
    assert_eq!(child_config.workspaces, config.workspaces);
    assert_ne!(child_config.workspace_id, config.workspace_id);
    assert_eq!(child_config.mcp_manifest, config.mcp_manifest);
    assert!(!child_config.system_prompt.is_empty());
    assert_ne!(child_config.system_prompt, config.system_prompt);
    assert_eq!(
        child_config.system_prompt,
        crate::provider_runtime::render_pi_prompt(&env.state, &child_config)
            .expect("child prompt rerenders")
    );
    env.cleanup().await;
}
