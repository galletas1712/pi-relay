use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use sqlx::Row;

use super::PostgresAgentStore;
use crate::DelegationStatus;

static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(90_000);
const PREFLIGHT: &str = include_str!("../../../../migrations/concurrent-delegations-preflight.sql");
const MIGRATION: &str = include_str!("../../../../migrations/concurrent-delegations.sql");

struct TestDb {
    store: PostgresAgentStore,
    admin_url: String,
    name: String,
}

impl TestDb {
    async fn cleanup(self) {
        self.store.close().await;
        let admin = sqlx::PgPool::connect(&self.admin_url)
            .await
            .expect("connect test admin for cleanup");
        sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
            .execute(&admin)
            .await
            .expect("drop migration test database");
        admin.close().await;
    }
}

async fn test_db() -> Option<TestDb> {
    let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
    let name = format!(
        "pi_relay_migration_test_{}_{}",
        std::process::id(),
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("connect to PI_RELAY_TEST_DATABASE_URL");
    sqlx::query(&format!(r#"create database "{name}""#))
        .execute(&admin)
        .await
        .expect("create migration test database");
    admin.close().await;
    let database_url = database_url_with_name(&admin_url, &name);
    let store = PostgresAgentStore::connect(&database_url)
        .await
        .expect("connect migration test database");
    Some(TestDb {
        store,
        admin_url,
        name,
    })
}

fn database_url_with_name(base: &str, name: &str) -> String {
    let (prefix, query) = base
        .split_once('?')
        .map(|(prefix, query)| (prefix, format!("?{query}")))
        .unwrap_or((base, String::new()));
    let (root, _) = prefix.rsplit_once('/').expect("database URL path");
    format!("{root}/{name}{query}")
}

fn psql_script(script: &str) -> &str {
    script
        .strip_prefix("\\set ON_ERROR_STOP on\n")
        .expect("checked-in migration starts with psql error mode")
}

async fn install_old_schema(store: &PostgresAgentStore) -> Result<()> {
    sqlx::raw_sql(
        r#"
        create table sessions (
            id text primary key,
            project_id uuid null,
            runtime_id text not null,
            workspace_id text not null,
            workspaces jsonb not null default '[]'::jsonb,
            created_at timestamptz not null default now(),
            updated_at timestamptz not null default now(),
            active_leaf_id text null,
            system_prompt text not null,
            provider_config jsonb not null,
            metadata jsonb not null default '{}'::jsonb,
            parent_session_id text null,
            subagent_type text null,
            last_user_message_timestamp_ms bigint null,
            session_revision bigint not null default 0,
            queue_revision bigint not null default 0,
            transcript_revision bigint not null default 0,
            mcp_manifest_fingerprint text null,
            delegation_id text null
        );
        create table delegations (
            id text primary key,
            parent_session_id text not null,
            workflow text null,
            label text null,
            kind text not null,
            status text not null,
            attempt_id text not null,
            expected_subagents integer not null,
            created_at timestamptz not null default now(),
            updated_at timestamptz not null default now()
        );
        "#,
    )
    .execute(&store.pool)
    .await?;
    Ok(())
}

async fn snapshot_rows(store: &PostgresAgentStore) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query(
        r#"
        select 'delegation:' || id as key, row_to_json(d)::text as value
        from delegations d
        union all
        select 'session:' || id, row_to_json(s)::text
        from sessions s
        order by key
        "#,
    )
    .fetch_all(&store.pool)
    .await?
    .into_iter()
    .map(|row| (row.get("key"), row.get("value")))
    .collect())
}

async fn snapshot_preexisting_fields(store: &PostgresAgentStore) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query(
        r#"
        select 'delegation:' || id as key,
               jsonb_build_object(
                   'id', id,
                   'parent_session_id', parent_session_id,
                   'workflow', workflow,
                   'label', label,
                   'kind', kind,
                   'status', status,
                   'attempt_id', attempt_id,
                   'expected_subagents', expected_subagents,
                   'created_at', to_char(created_at, 'YYYY-MM-DD"T"HH24:MI:SS.USOF'),
                   'updated_at', to_char(updated_at, 'YYYY-MM-DD"T"HH24:MI:SS.USOF')
               )::text as value
        from delegations
        union all
        select 'session:' || id,
               (
                   to_jsonb(s)
                   || jsonb_build_object(
                       'metadata',
                       metadata - 'delegation_spawn_index'
                   )
               )::text
        from sessions s
        order by key
        "#,
    )
    .fetch_all(&store.pool)
    .await?
    .into_iter()
    .map(|row| (row.get("key"), row.get("value")))
    .collect())
}

async fn assert_preflight_fails_unchanged(setup: &str) -> Result<()> {
    let db = test_db()
        .await
        .context("PI_RELAY_TEST_DATABASE_URL unset")?;
    install_old_schema(&db.store).await?;
    sqlx::raw_sql(setup).execute(&db.store.pool).await?;
    let before = snapshot_rows(&db.store).await?;
    assert!(
        sqlx::raw_sql(psql_script(PREFLIGHT))
            .execute(&db.store.pool)
            .await
            .is_err(),
        "invalid old schema must fail closed"
    );
    assert_eq!(snapshot_rows(&db.store).await?, before);
    db.cleanup().await;
    Ok(())
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn concurrent_delegations_preflight_rejects_each_invalid_shape_without_writes() {
    if std::env::var("PI_RELAY_TEST_DATABASE_URL").is_err() {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    }
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('a','parent',null,null,'full','running','attempt-a',1,now(),now()),
          ('b','parent',null,null,'full','running','attempt-b',1,now(),now());
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config,
            parent_session_id,subagent_type,delegation_id,metadata
        ) values
          ('child-a','runtime-a','workspace-a','prompt-a','{}',
           'parent','full','a','{"role_name":"implementer","task":"a"}'),
          ('child-b','runtime-b','workspace-b','prompt-b','{}',
           'parent','full','b','{"role_name":"implementer","task":"b"}');
        "#,
    )
    .await
    .expect("duplicate writer preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('full','parent',null,null,'full','done','attempt',2,now(),now());
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config,
            parent_session_id,subagent_type,delegation_id,metadata
        ) values (
            'child','runtime-child','workspace-child','prompt-child','{}',
            'parent','full','full','{"role_name":"implementer","task":"build"}'
        );
        "#,
    )
    .await
    .expect("full expected count preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('full','parent',null,null,'full','done','attempt',1,now(),now());
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config,
            parent_session_id,subagent_type,delegation_id,metadata
        ) values
          ('child-a','runtime-a','workspace-a','prompt-a','{}',
           'parent','full','full','{"role_name":"implementer","task":"a"}'),
          ('child-b','runtime-b','workspace-b','prompt-b','{}',
           'parent','full','full','{"role_name":"implementer","task":"b"}');
        "#,
    )
    .await
    .expect("full linked child count preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('full','parent',null,null,'full','running','attempt',1,now(),now());
        "#,
    )
    .await
    .expect("running full without child preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('full','parent',null,null,'full','done','attempt',1,now(),now());
        "#,
    )
    .await
    .expect("success-terminal full without child preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('partial','parent',null,null,'readonly_fanout','running','attempt',2,now(),now());
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config,
            parent_session_id,subagent_type,delegation_id,metadata
        ) values (
            'child','runtime-child','workspace-child','prompt-child','{}',
            'parent','read_only','partial','{"role_name":"reviewer","task":"review"}'
        );
        "#,
    )
    .await
    .expect("partial launch preflight");
    assert_preflight_fails_unchanged(
        r#"
        insert into sessions(id,runtime_id,workspace_id,system_prompt,provider_config)
          values ('parent','runtime-parent','workspace-parent','prompt-parent','{}');
        insert into delegations values
          ('missing','parent',null,null,'full','done','attempt',1,now(),now());
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config,
            parent_session_id,subagent_type,delegation_id,metadata
        ) values (
            'child','runtime-child','workspace-child','prompt-child','{}',
            'parent','full','missing','{}'
        );
        "#,
    )
    .await
    .expect("missing durable child metadata preflight");
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn concurrent_delegations_fresh_schema_enforces_full_count_without_capping_readonly() {
    let Some(db) = test_db().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    db.store.migrate().await.expect("install fresh schema");
    db.store.migrate().await.expect("fresh schema reruns");
    sqlx::query(
        r#"
        insert into sessions(
            id,runtime_id,workspace_id,system_prompt,provider_config
        ) values ('parent','runtime-parent','workspace-parent','prompt-parent','{}')
        "#,
    )
    .execute(&db.store.pool)
    .await
    .expect("insert parent");
    assert!(
        sqlx::query(
            r#"
            insert into delegations(
                id,parent_session_id,launch_key,launch_shape,kind,status,attempt_id,
                expected_subagents
            ) values ('invalid-full','parent','invalid-full','{}','full','done','attempt',2)
            "#,
        )
        .execute(&db.store.pool)
        .await
        .is_err(),
        "fresh schema rejects a full delegation with any count other than one"
    );
    sqlx::query(
        r#"
        insert into delegations(
            id,parent_session_id,launch_key,launch_shape,kind,status,attempt_id,
            expected_subagents
        ) values ('legacy-oversized','parent','legacy-oversized','{}','readonly_fanout','done','attempt',9)
        "#,
    )
    .execute(&db.store.pool)
    .await
    .expect("fresh schema does not cap oversized read-only fan-outs");
    db.cleanup().await;
}

#[ignore = "requires PI_RELAY_TEST_DATABASE_URL; see rust/README.md"]
#[tokio::test]
async fn concurrent_delegations_migration_preserves_backfills_reruns_and_repairs_named_indexes() {
    let Some(db) = test_db().await else {
        eprintln!("SKIPPED PostgreSQL test; PI_RELAY_TEST_DATABASE_URL is not set");
        return;
    };
    install_old_schema(&db.store)
        .await
        .expect("install old schema");
    sqlx::raw_sql(
        r#"
        insert into sessions(
            id, project_id, runtime_id, workspace_id, workspaces,
            created_at, updated_at, active_leaf_id, system_prompt, provider_config,
            metadata, parent_session_id, subagent_type,
            last_user_message_timestamp_ms, session_revision, queue_revision,
            transcript_revision, mcp_manifest_fingerprint, delegation_id
        ) values (
            'parent',
            '11111111-1111-4111-8111-111111111111',
            'runtime-parent-sentinel',
            'workspace-parent-sentinel',
            '[{"name":"parent-workspace-sentinel","path":"/parent-sentinel"}]',
            '2024-01-02 03:04:05.123456+00',
            '2024-01-03 04:05:06.234567+00',
            'leaf-parent-sentinel',
            'system prompt parent sentinel',
            '{"provider":"parent-provider-sentinel","model":"parent-model-sentinel"}',
            '{"owner":"existing","nested":{"keep":[1,true,"x"]}}',
            null,
            null,
            1704164645123,
            101,
            102,
            103,
            'mcp-parent-sentinel',
            null
        );
        insert into delegations values
          ('full-old','parent','implement_review','writer','full','done','attempt-full',1,
           '2024-02-03 04:05:06.234567+00','2024-02-04 05:06:07.345678+00'),
          ('failed-before-child','parent','implement_review','spawn failure','full','failed',
           'attempt-failed',1,
           '2024-02-05 06:07:08.456789+00','2024-02-06 07:08:09.567890+00'),
          ('fanout-old','parent',null,'review','readonly_fanout','running','attempt-ro',2,
           '2024-03-04 05:06:07.456789+00','2024-03-05 06:07:08.567890+00');
        insert into sessions(
            id, project_id, runtime_id, workspace_id, workspaces,
            created_at, updated_at, active_leaf_id, system_prompt, provider_config,
            metadata, parent_session_id, subagent_type,
            last_user_message_timestamp_ms, session_revision, queue_revision,
            transcript_revision, mcp_manifest_fingerprint, delegation_id
        ) values
          (
            'full-child',
            '22222222-2222-4222-8222-222222222222',
            'runtime-full-sentinel',
            'workspace-full-sentinel',
            '[{"name":"full-workspace-sentinel","path":"/full-sentinel"}]',
            '2024-04-05 06:07:08.678901+00',
            '2024-04-06 07:08:09.789012+00',
            'leaf-full-sentinel',
            'system prompt full sentinel',
            '{"provider":"full-provider-sentinel","model":"full-model-sentinel"}',
            '{"role_name":"implementer","task":"build","unrelated":"full","nested":{"keep":1}}',
            'parent',
            'full',
            1712297228678,
            201,
            202,
            203,
            'mcp-full-sentinel',
            'full-old'
          ),
          (
            'ro-b',
            '33333333-3333-4333-8333-333333333333',
            'runtime-ro-b-sentinel',
            'workspace-ro-b-sentinel',
            '[{"name":"ro-b-workspace-sentinel","path":"/ro-b-sentinel"}]',
            '2024-05-06 07:08:10.890123+00',
            '2024-05-07 08:09:11.901234+00',
            'leaf-ro-b-sentinel',
            'system prompt ro-b sentinel',
            '{"provider":"ro-b-provider-sentinel","model":"ro-b-model-sentinel"}',
            '{"role_name":"reviewer","task":"second","unrelated":["b",2],"nested":{"keep":false}}',
            'parent',
            'read_only',
            1714979290890,
            301,
            302,
            303,
            'mcp-ro-b-sentinel',
            'fanout-old'
          ),
          (
            'ro-a',
            '44444444-4444-4444-8444-444444444444',
            'runtime-ro-a-sentinel',
            'workspace-ro-a-sentinel',
            '[{"name":"ro-a-workspace-sentinel","path":"/ro-a-sentinel"}]',
            '2024-05-06 07:08:09.789012+00',
            '2024-05-08 09:10:12.012345+00',
            'leaf-ro-a-sentinel',
            'system prompt ro-a sentinel',
            '{"provider":"ro-a-provider-sentinel","model":"ro-a-model-sentinel"}',
            '{"role_name":"reviewer","task":"first","unrelated":["a",1],"nested":{"keep":true}}',
            'parent',
            'read_only',
            1714979289789,
            401,
            402,
            403,
            'mcp-ro-a-sentinel',
            'fanout-old'
          );
        create index delegations_parent_launch_key_uq on delegations(parent_session_id);
        alter table delegations
          add constraint delegations_full_expected_subagents_one
          check (expected_subagents < 100);
        "#,
    )
    .execute(&db.store.pool)
    .await
    .expect("populate old rows and malformed named index");
    let before = snapshot_preexisting_fields(&db.store)
        .await
        .expect("snapshot every pre-existing field");

    sqlx::raw_sql(psql_script(PREFLIGHT))
        .execute(&db.store.pool)
        .await
        .expect("valid old rows pass preflight");
    sqlx::raw_sql(psql_script(MIGRATION))
        .execute(&db.store.pool)
        .await
        .expect("apply one-time migration");
    assert_eq!(
        snapshot_preexisting_fields(&db.store)
            .await
            .expect("snapshot after first apply"),
        before,
        "first apply preserves every pre-existing delegation/session field, timestamp, and metadata key"
    );
    let migrated = snapshot_rows(&db.store)
        .await
        .expect("snapshot complete migrated rows");
    sqlx::raw_sql(psql_script(MIGRATION))
        .execute(&db.store.pool)
        .await
        .expect("migration reruns");
    assert_eq!(
        snapshot_preexisting_fields(&db.store)
            .await
            .expect("snapshot after rerun"),
        before,
        "rerun preserves every pre-existing delegation/session field, timestamp, and metadata key"
    );
    assert_eq!(
        snapshot_rows(&db.store)
            .await
            .expect("snapshot complete rerun rows"),
        migrated,
        "rerun leaves intended new columns and metadata byte-equivalent"
    );
    db.store
        .migrate()
        .await
        .expect("normal startup schema inspection accepts migrated rows");
    let failed_delegation = db
        .store
        .get_delegation("failed-before-child")
        .await
        .expect("inspect migrated failed delegation")
        .expect("migrated failed delegation remains present");
    assert_eq!(failed_delegation.status, DelegationStatus::Failed);
    assert_eq!(failed_delegation.expected_subagents, 1);
    assert!(
        db.store
            .delegation_subagent_overview(&failed_delegation.id)
            .await
            .expect("inspect terminal delegation children")
            .is_empty(),
        "failed launch remains inspectable without inventing a child session"
    );
    let failed_progress = db
        .store
        .delegation_progress(&failed_delegation)
        .await
        .expect("inspect terminal delegation progress");
    assert_eq!(failed_progress.expected, 1);
    assert_eq!(failed_progress.spawned, 0);
    assert_eq!(failed_progress.terminal, 0);
    assert_eq!(failed_progress.running, 0);
    let rows = sqlx::query(
        "select id, launch_key, launch_shape::jsonb as shape from delegations order by id",
    )
    .fetch_all(&db.store.pool)
    .await
    .expect("backfilled delegations");
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let id: String = row.get("id");
        assert_eq!(row.get::<String, _>("launch_key"), format!("legacy:{id}"));
    }
    let unused_teardown_fields: Vec<(Option<String>, Option<serde_json::Value>)> =
        sqlx::query_as("select teardown_target, launch_error from delegations order by id")
            .fetch_all(&db.store.pool)
            .await
            .expect("load new teardown fields");
    assert_eq!(
        unused_teardown_fields,
        [(None, None), (None, None), (None, None)]
    );
    let fanout = rows
        .iter()
        .find(|row| row.get::<String, _>("id") == "fanout-old")
        .expect("fanout row")
        .get::<serde_json::Value, _>("shape");
    assert_eq!(fanout["tasks"][0]["prompt"], "first");
    assert_eq!(fanout["tasks"][1]["prompt"], "second");
    let failed_before_child = rows
        .iter()
        .find(|row| row.get::<String, _>("id") == "failed-before-child")
        .expect("failed full launch row")
        .get::<serde_json::Value, _>("shape");
    assert_eq!(
        failed_before_child,
        serde_json::json!({
            "kind": "terminal_only_historical_failure",
            "reason": "full_launch_failed_before_child_materialization"
        }),
        "zero-child failed full launch receives a canonical terminal-only placeholder"
    );
    let spawn_indices: Vec<String> = sqlx::query_scalar(
        "select metadata->>'delegation_spawn_index' from sessions where delegation_id='fanout-old' order by metadata->>'delegation_spawn_index'",
    )
    .fetch_all(&db.store.pool)
    .await
    .expect("spawn indices");
    assert_eq!(spawn_indices, ["0", "1"]);
    let metadata: Vec<serde_json::Value> = sqlx::query_scalar(
        "select metadata from sessions where delegation_id is not null order by id",
    )
    .fetch_all(&db.store.pool)
    .await
    .expect("load migrated child metadata");
    assert_eq!(
        metadata,
        [
            serde_json::json!({
                "role_name": "implementer",
                "task": "build",
                "unrelated": "full",
                "nested": {"keep": 1},
                "delegation_spawn_index": 0
            }),
            serde_json::json!({
                "role_name": "reviewer",
                "task": "first",
                "unrelated": ["a", 1],
                "nested": {"keep": true},
                "delegation_spawn_index": 0
            }),
            serde_json::json!({
                "role_name": "reviewer",
                "task": "second",
                "unrelated": ["b", 2],
                "nested": {"keep": false},
                "delegation_spawn_index": 1
            }),
        ],
        "migration appends only the intended spawn-index metadata key"
    );
    let launch_index_definition: String = sqlx::query_scalar(
        "select pg_get_indexdef(indexrelid) from pg_index where indexrelid='delegations_parent_launch_key_uq'::regclass",
    )
    .fetch_one(&db.store.pool)
    .await
    .expect("repaired launch index");
    assert!(launch_index_definition.contains("UNIQUE"));
    assert!(launch_index_definition.contains("launch_key"));
    let full_constraint_definition: String = sqlx::query_scalar(
        r#"
        select pg_get_constraintdef(oid)
        from pg_constraint
        where conrelid='delegations'::regclass
          and conname='delegations_full_expected_subagents_one'
        "#,
    )
    .fetch_one(&db.store.pool)
    .await
    .expect("canonical full count constraint");
    assert_eq!(
        full_constraint_definition,
        "CHECK (((kind <> 'full'::text) OR (expected_subagents = 1)))"
    );
    assert!(
        sqlx::query(
            r#"
            insert into delegations(
                id,parent_session_id,launch_key,launch_shape,kind,status,attempt_id,
                expected_subagents
            ) values ('invalid-full','parent','invalid-full','{}','full','done','attempt',2)
            "#,
        )
        .execute(&db.store.pool)
        .await
        .is_err(),
        "migrated schema rejects a full delegation with any count other than one"
    );
    sqlx::query(
        r#"
        insert into delegations(
            id,parent_session_id,launch_key,launch_shape,kind,status,attempt_id,
            expected_subagents
        ) values ('legacy-oversized','parent','legacy-oversized','{}','readonly_fanout','done','attempt',9)
        "#,
    )
    .execute(&db.store.pool)
    .await
    .expect("constraint preserves oversized legacy read-only fan-outs");
    db.cleanup().await;
}
