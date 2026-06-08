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
///   directories under `outer_cwd`.
/// - `daemon_config`: reserved daemon key-value config.
/// - `transcript_entries`: append-only transcript forest. `parent_id` points
///   within the same session, while `sequence` preserves insertion order.
/// - `queued_inputs`: user inputs waiting to be consumed by a session turn.
///   Idempotency is keyed by `(session_id, client_input_id)`.
/// - `actions`: durable model/tool/compaction/cancel work records.
/// - `events`: ordered observable event stream for websocket replay.
/// - `session_relationships`: provenance/control graph between normal session
///   rows for subagents and related top-level sessions.
/// - `workflow_variables`: durable workflow/session-scoped context bus values.
const SCHEMA_SQL: &str = r#"
create table if not exists projects (
    id uuid primary key,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    name text not null,
    workspaces jsonb not null default '[]'::jsonb,
    metadata jsonb not null default '{}'::jsonb
);

create table if not exists sessions (
    id text primary key,
    project_id uuid null references projects(id),
    outer_cwd text not null,
    workspaces jsonb not null default '[]'::jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    active_leaf_id text null,
    system_prompt text not null,
    provider_config jsonb not null,
    metadata jsonb not null default '{}'::jsonb,
    session_revision bigint not null default 0,
    queue_revision bigint not null default 0,
    transcript_revision bigint not null default 0
);

alter table sessions add column if not exists system_prompt text not null default '';

create index if not exists sessions_project_created_idx
    on sessions(project_id, created_at desc, id desc);

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
    client_input_id text null
);

create unique index if not exists queued_inputs_client_input_idx
    on queued_inputs(session_id, client_input_id)
    where client_input_id is not null;

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
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists actions_session_status_idx
    on actions(session_id, status);

create table if not exists events (
    id bigserial primary key,
    session_id text not null references sessions(id) on delete cascade,
    type text not null,
    payload jsonb not null,
    created_at timestamptz not null default now()
);

create index if not exists events_session_id_idx on events(session_id, id);

create table if not exists session_relationships (
    id text primary key,
    source_session_id text not null references sessions(id) on delete cascade,
    target_session_id text not null references sessions(id) on delete cascade,
    root_session_id text not null references sessions(id) on delete cascade,
    kind text not null,
    control_mode text not null,
    visibility text not null,
    role_name text null,
    role_workspace text null,
    display_name text null,
    task text not null,
    spawned_from_leaf_id text null,
    spawned_from_action_row_id text null,
    workflow_id text null,
    result_variable text null,
    status text not null,
    filesystem_mode text null,
    baseline_cwd text null,
    metadata jsonb not null default '{}'::jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create unique index if not exists session_relationships_target_idx
    on session_relationships(target_session_id);

create index if not exists session_relationships_source_kind_idx
    on session_relationships(source_session_id, kind, created_at, id);

create index if not exists session_relationships_root_idx
    on session_relationships(root_session_id, created_at, id);

create table if not exists workflow_variables (
    workflow_id text not null,
    name text not null,
    value_json jsonb null,
    value_text text null,
    producer_session_id text null references sessions(id) on delete set null,
    producer_action_id text null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (workflow_id, name)
);

create index if not exists workflow_variables_workflow_updated_idx
    on workflow_variables(workflow_id, updated_at, name);
"#;

pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_SQL).execute(pool).await?;
    Ok(())
}
