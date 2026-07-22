use anyhow::Result;
use sqlx::PgPool;

/// Postgres is the durable source of truth for sessions.
///
/// Tables:
/// - `projects`: one row per project. Its workspace sources are the defaults
///   for new sessions.
/// - `sessions`: one row per durable session, including active transcript leaf
///   and the rendered system prompt. Sessions snapshot the project workspace
///   source metadata; project sessions get their own private workspace
///   directories addressed by their runtime-scoped `workspace_id`.
/// - `daemon_config`: reserved daemon key-value config.
/// - `transcript_entries`: append-only transcript forest. `parent_id` points
///   within the same session, while `sequence` preserves insertion order.
/// - `queued_inputs`: user inputs waiting to be consumed by a session turn.
///   Idempotency is keyed by `(session_id, client_input_id)` and
///   `provider_config` snapshots submission-time routing.
/// - `actions`: durable model/tool/compaction/cancel work records whose
///   `provider_config` snapshots turn routing for restart-safe dispatch.
/// - `events`: ordered observable event stream for websocket replay.
/// - `delegations`: bounded parent/child subagent delegation units.
const SCHEMA_SQL: &str = r#"
create table if not exists projects (
    id uuid primary key,
    runtime_id text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    name text not null,
    workspaces jsonb not null default '[]'::jsonb,
    metadata jsonb not null default '{}'::jsonb
);

create table if not exists runtimes (
    id text primary key,
    name text not null,
    last_seen_at timestamptz null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table if not exists sessions (
    id text primary key,
    project_id uuid null references projects(id),
    runtime_id text not null,
    workspace_id text not null,
    workspaces jsonb not null default '[]'::jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    active_leaf_id text null,
    system_prompt text not null,
    provider_config jsonb not null,
    metadata jsonb not null default '{}'::jsonb,
    parent_session_id text null references sessions(id) on delete set null,
    subagent_type text null,
    last_user_message_timestamp_ms bigint null,
    session_revision bigint not null default 0,
    queue_revision bigint not null default 0,
    transcript_revision bigint not null default 0
);

create index if not exists sessions_project_created_idx
    on sessions(project_id, created_at desc, id desc);

create index if not exists sessions_parent_created_idx
    on sessions(parent_session_id, created_at, id)
    where parent_session_id is not null;

create table if not exists daemon_config (
    key text primary key,
    value jsonb not null,
    updated_at timestamptz not null default now()
);

create table if not exists transcript_entries (
    session_id text not null references sessions(id) on delete cascade,
    id text not null,
    parent_id text null,
    timestamp_ms bigint not null,
    item jsonb not null,
    provider_replay jsonb not null default '[]'::jsonb,
    turn_id bigint null,
    sequence bigserial not null,
    primary key (session_id, id)
);

create index if not exists transcript_entries_session_sequence_idx
    on transcript_entries(session_id, sequence);

create table if not exists queued_inputs (
    id text primary key,
    session_id text not null references sessions(id) on delete cascade,
    priority text not null,
    content jsonb not null,
    origin jsonb null,
    status text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    follow_up_position integer null,
    client_input_id text null,
    provider_config jsonb null
);

create unique index if not exists queued_inputs_client_input_idx
    on queued_inputs(session_id, client_input_id)
    where client_input_id is not null;

create index if not exists queued_inputs_active_session_idx
    on queued_inputs(session_id)
    where status in ('queued','consuming');

create index if not exists queued_inputs_non_cancelled_session_idx
    on queued_inputs(session_id)
    where status <> 'cancelled';

create index if not exists queued_inputs_follow_up_order_idx
    on queued_inputs(session_id, follow_up_position, created_at, id)
    where priority='follow_up' and status='queued';

create table if not exists actions (
    id text primary key,
    session_id text not null references sessions(id) on delete cascade,
    turn_id bigint null,
    action_id bigint not null,
    attempt_id text not null,
    kind text not null,
    status text not null,
    payload jsonb not null,
    result jsonb null,
    provider_config jsonb null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists actions_session_status_idx
    on actions(session_id, status);

create table if not exists mcp_session_manifests (
    fingerprint text primary key,
    manifest jsonb not null,
    created_at timestamptz not null default now(),
    last_used_at timestamptz not null default now()
);

alter table sessions
    add column if not exists mcp_manifest_fingerprint text null
        references mcp_session_manifests(fingerprint);

create index if not exists sessions_mcp_manifest_idx
    on sessions(mcp_manifest_fingerprint)
    where mcp_manifest_fingerprint is not null;

-- Remove the unshipped turn-scoped MCP prototype if this feature branch was
-- run against a development database.
drop table if exists mcp_legacy_zero_turns;
drop table if exists mcp_turn_bindings;
alter table actions drop column if exists mcp_manifest_fingerprint;
alter table actions drop column if exists toolset_fingerprint;
drop table if exists mcp_catalog_manifests;

create table if not exists events (
    id bigserial primary key,
    session_id text not null references sessions(id) on delete cascade,
    type text not null,
    payload jsonb not null,
    created_at timestamptz not null default now()
);

create index if not exists events_session_id_idx on events(session_id, id);

create table if not exists delegations (
    id text primary key,
    parent_session_id text not null references sessions(id) on delete cascade,
    workflow text null,
    label text null,
    kind text not null,
    status text not null,
    attempt_id text not null,
    expected_subagents integer not null default 1
        constraint delegations_expected_subagents_positive check (expected_subagents > 0),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists delegations_parent_created_idx on delegations(parent_session_id, created_at, id);

create index if not exists delegations_parent_running_idx
    on delegations(parent_session_id)
    where status='running';

create index if not exists delegations_running_created_idx
    on delegations(created_at, id)
    where status='running';

create index if not exists delegations_completed_repair_idx
    on delegations(updated_at, id)
    where status in ('done','done_with_failures');

-- `sessions` and `delegations` reference each other, so this side of the cycle
-- is added after both canonical tables exist.
alter table sessions add column if not exists delegation_id text null references delegations(id);

alter table queued_inputs add column if not exists provider_config jsonb null;
alter table actions add column if not exists provider_config jsonb null;

create index if not exists sessions_delegation_created_idx
    on sessions(delegation_id, created_at, id)
    where delegation_id is not null;
"#;

pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_SQL).execute(pool).await?;
    Ok(())
}
