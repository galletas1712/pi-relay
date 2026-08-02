-- ONE-TIME PRIVATE WORKSPACE OWNERSHIP MIGRATION
\set ON_ERROR_STOP on
--
-- 1. STOP pi-agentd and every process that can create/delete sessions.
-- 2. TAKE AND VERIFY A POSTGRES BACKUP before running this file.
-- 3. Run the audit queries below and investigate every non-empty anomaly.
-- 4. This migration only adds/backfills durable state. It never destroys a
--    runtime workspace and never infers that an unknown directory is safe to
--    delete.
--
-- This file is rerunnable only in the stopped-daemon pre-cutover window. A
-- conflicting existing ownership row causes the transaction to fail closed
-- instead of overwriting identity.

-- Preflight inventory: retain this output with the backup/change record.
select
    case
        when parent_session_id is null and subagent_type is null then 'root_or_history_fork'
        when subagent_type='full' then 'full_shared'
        when subagent_type='read_only' then 'read_only_private'
        else 'invalid'
    end as lifecycle_class,
    count(*) as sessions
from sessions
group by 1
order by 1;

-- Parentless read-only rows require manual repair before migration.
select id, runtime_id, workspace_id, metadata
from sessions
where subagent_type='read_only' and parent_session_id is null;

-- Invalid/unknown subtype combinations require manual repair.
select id, parent_session_id, subagent_type, runtime_id, workspace_id
from sessions
where (subagent_type='full' and parent_session_id is null)
   or subagent_type not in ('full','read_only')
   or (parent_session_id is not null and subagent_type is null);

-- Duplicate private workspace identities are unsafe to infer.
select runtime_id, workspace_id, array_agg(id order by id) as session_ids
from sessions
where parent_session_id is null or subagent_type='read_only'
group by runtime_id, workspace_id
having count(*) > 1;

-- Full children must share the exact parent workspace and have no private row.
select child.id, child.runtime_id, child.workspace_id,
       parent.id as parent_id, parent.runtime_id as parent_runtime_id,
       parent.workspace_id as parent_workspace_id
from sessions child
left join sessions parent on parent.id=child.parent_session_id
where child.subagent_type='full'
  and (
      parent.id is null
      or child.runtime_id is distinct from parent.runtime_id
      or child.workspace_id is distinct from parent.workspace_id
  );

begin;

create table if not exists workspace_resources (
    workspace_id text primary key,
    owner_session_id text not null unique,
    runtime_id text not null,
    generation text not null,
    owner_kind text not null
        constraint workspace_resources_owner_kind_check
        check (owner_kind in ('root','history_fork','read_only')),
    state text not null
        constraint workspace_resources_state_check
        check (state in ('provisioning','ready','deleting','deleted')),
    cleanup_mode text null
        constraint workspace_resources_cleanup_mode_check
        check (cleanup_mode is null or cleanup_mode in ('retain_session','delete_session')),
    workspaces jsonb null,
    lease_expires_at timestamptz not null,
    retry_at timestamptz not null default now(),
    last_error text null,
    attached_at timestamptz null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint workspace_resources_state_shape_check check (
        (state='provisioning' and cleanup_mode is null and attached_at is null)
        or (state='ready' and cleanup_mode is null)
        or (state='deleting' and cleanup_mode is not null)
        or (state='deleted' and cleanup_mode='retain_session' and attached_at is not null)
    )
);

create index if not exists workspace_resources_due_idx
    on workspace_resources(state, retry_at, lease_expires_at);

-- Fail closed before backfill when session shapes are ambiguous.
do $$
begin
    if exists (
        select 1 from sessions
        where subagent_type='read_only' and parent_session_id is null
    ) then
        raise exception 'parentless read-only sessions exist; repair explicitly before migration';
    end if;
    if exists (
        select 1 from sessions
        where (subagent_type='full' and parent_session_id is null)
           or subagent_type not in ('full','read_only')
           or (parent_session_id is not null and subagent_type is null)
    ) then
        raise exception 'invalid parent/subagent session combinations exist';
    end if;
    if exists (
        select 1
        from sessions
        where parent_session_id is null or subagent_type='read_only'
        group by runtime_id, workspace_id
        having count(*) > 1
    ) then
        raise exception 'duplicate private runtime/workspace mappings exist';
    end if;
    if exists (
        select 1
        from sessions child
        left join sessions parent on parent.id=child.parent_session_id
        where child.subagent_type='full'
          and (
              parent.id is null
              or child.runtime_id is distinct from parent.runtime_id
              or child.workspace_id is distinct from parent.workspace_id
          )
    ) then
        raise exception 'full subagent does not exactly share its parent workspace';
    end if;
end
$$;

-- Main-era private sessions already exist remotely, so they are attached and
-- ready. History-fork ownership is explicit in current metadata.
insert into workspace_resources (
    workspace_id, owner_session_id, runtime_id, generation, owner_kind,
    state, cleanup_mode, workspaces, lease_expires_at, retry_at, attached_at
)
select
    s.workspace_id,
    s.id,
    s.runtime_id,
    'main-era-backfill:' || s.id,
    case
        when s.subagent_type='read_only' then 'read_only'
        when s.metadata ? 'fork' then 'history_fork'
        else 'root'
    end,
    'ready',
    null,
    s.workspaces,
    now(),
    now(),
    now()
from sessions s
where s.parent_session_id is null or s.subagent_type='read_only'
on conflict do nothing;

-- Existing rows are accepted only when every identity field matches the
-- deterministic backfill. Unknown/conflicting ownership aborts the migration.
do $$
begin
    if exists (
        select 1
        from sessions s
        left join workspace_resources r
          on r.owner_session_id=s.id
         and r.workspace_id=s.workspace_id
         and r.runtime_id=s.runtime_id
        where (s.parent_session_id is null or s.subagent_type='read_only')
          and (
              r.owner_session_id is null
              or r.generation <> 'main-era-backfill:' || s.id
              or r.owner_kind <> case
                    when s.subagent_type='read_only' then 'read_only'
                    when s.metadata ? 'fork' then 'history_fork'
                    else 'root'
                 end
              or r.state <> 'ready'
              or r.attached_at is null
          )
    ) then
        raise exception 'missing or conflicting private workspace ownership mapping';
    end if;
    if exists (
        select 1
        from sessions s
        join workspace_resources r on r.owner_session_id=s.id
        where s.subagent_type='full'
    ) then
        raise exception 'full subagent has an invalid private workspace mapping';
    end if;
end
$$;

commit;

-- Post-checks: all should return zero rows/count zero.
select s.id, s.runtime_id, s.workspace_id
from sessions s
left join workspace_resources r on r.owner_session_id=s.id
where (s.parent_session_id is null or s.subagent_type='read_only')
  and r.owner_session_id is null;

select s.id, r.workspace_id, r.owner_kind, r.state
from sessions s
join workspace_resources r on r.owner_session_id=s.id
where s.subagent_type='full';

select owner_session_id, count(*)
from workspace_resources
group by owner_session_id
having count(*) > 1;

select runtime_id, workspace_id, count(*)
from workspace_resources
group by runtime_id, workspace_id
having count(*) > 1;

select state, owner_kind, count(*)
from workspace_resources
group by state, owner_kind
order by state, owner_kind;
