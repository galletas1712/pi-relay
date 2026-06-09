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
    parent_session_id text null references sessions(id) on delete set null,
    last_user_message_timestamp_ms bigint null,
    session_revision bigint not null default 0,
    queue_revision bigint not null default 0,
    transcript_revision bigint not null default 0
);

alter table sessions add column if not exists system_prompt text not null default '';

alter table sessions add column if not exists parent_session_id text null references sessions(id) on delete set null;

do $$
begin
    if to_regclass('public.session_relationships') is not null then
        update sessions s
        set parent_session_id = r.parent_session_id
        from session_relationships r
        where s.id = r.child_session_id
          and s.parent_session_id is null;

        drop table session_relationships;
    end if;
end $$;

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

create table if not exists workflow_variables (
    owner_session_id text not null references sessions(id) on delete cascade,
    workflow_id text not null,
    name text not null,
    value_json jsonb null,
    value_text text null,
    producer_session_id text null references sessions(id) on delete set null,
    producer_action_id text null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (owner_session_id, workflow_id, name)
);

alter table workflow_variables add column if not exists owner_session_id text;

update workflow_variables
set owner_session_id=coalesce(producer_session_id, 'legacy')
where owner_session_id is null;

delete from workflow_variables
where not exists (
    select 1 from sessions where sessions.id=workflow_variables.owner_session_id
);

alter table workflow_variables alter column owner_session_id set not null;

alter table workflow_variables drop constraint if exists workflow_variables_pkey;

alter table workflow_variables
    add primary key (owner_session_id, workflow_id, name);

do $$
begin
    if not exists (
        select 1
        from pg_constraint
        where conname='workflow_variables_owner_session_id_fkey'
          and conrelid='workflow_variables'::regclass
    ) then
        alter table workflow_variables
            add constraint workflow_variables_owner_session_id_fkey
            foreign key (owner_session_id)
            references sessions(id)
            on delete cascade;
    end if;
end $$;

create index if not exists workflow_variables_workflow_updated_idx
    on workflow_variables(workflow_id, updated_at, name);

create index if not exists workflow_variables_owner_workflow_updated_idx
    on workflow_variables(owner_session_id, workflow_id, updated_at, name);
"#;

pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_SQL).execute(pool).await?;
    Ok(())
}
