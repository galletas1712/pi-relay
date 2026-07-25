\set ON_ERROR_STOP on

-- Run only after backups, successful preflight, and stopping the old control
-- plane/runtime. The exclusive locks make this one-time migration atomic and
-- let it safely rebuild invalid artifacts from interrupted prior attempts.
begin;

lock table delegations in access exclusive mode;
lock table sessions in access exclusive mode;

alter table delegations add column if not exists launch_key text;
alter table delegations add column if not exists launch_shape text;
alter table delegations add column if not exists teardown_target text;
alter table delegations add column if not exists launch_error jsonb;

update delegations
set launch_key = coalesce(launch_key, 'legacy:' || id);

with indexed as (
    select id,
           row_number() over (
               partition by delegation_id
               order by created_at, id
           ) - 1 as spawn_index
    from sessions s
    where delegation_id is not null
      and not exists (
          select 1
          from sessions existing
          where existing.delegation_id=s.delegation_id
            and existing.metadata ? 'delegation_spawn_index'
      )
)
update sessions s
set metadata=s.metadata
    || jsonb_build_object('delegation_spawn_index', indexed.spawn_index)
from indexed
where s.id=indexed.id;

with child_specs as (
    select d.id,
           case
               when d.kind = 'full'
                    and d.status = 'failed'
                    and first_child.metadata is null
               then jsonb_build_object(
                   'kind', 'terminal_only_historical_failure',
                   'reason', 'full_launch_failed_before_child_materialization'
               )
               else case d.kind
               when 'full' then jsonb_build_object(
                   'kind', 'full',
                   'role', first_child.metadata->>'role_name',
                   'prompt', first_child.metadata->>'task',
                   'workflow', d.workflow,
                   'label', d.label
               )
               when 'readonly_fanout' then jsonb_build_object(
                   'kind', 'readonly_fanout',
                   'tasks', coalesce(children.tasks, '[]'::jsonb),
                   'workflow', d.workflow,
                   'label', d.label
               )
               end
           end as launch_shape
    from delegations d
    left join lateral (
        select metadata
        from sessions
        where delegation_id=d.id
        order by (metadata->>'delegation_spawn_index')::integer
        limit 1
    ) first_child on true
    left join lateral (
        select jsonb_agg(
            jsonb_build_object(
                'role', metadata->>'role_name',
                'prompt', metadata->>'task'
            )
            order by (metadata->>'delegation_spawn_index')::integer
        ) as tasks
        from sessions
        where delegation_id=d.id
    ) children on true
)
update delegations d
set launch_shape=child_specs.launch_shape::text
from child_specs
where d.id=child_specs.id
  and (
      d.launch_shape is null
      or coalesce((d.launch_shape::jsonb)->>'legacy', 'false')='true'
  );

alter table delegations alter column launch_key set not null;
alter table delegations alter column launch_shape set not null;

alter table delegations
    drop constraint if exists delegations_full_expected_subagents_one;
alter table delegations
    add constraint delegations_full_expected_subagents_one
        check (kind <> 'full' or expected_subagents = 1);

drop index if exists delegations_parent_launch_key_uq;
create unique index delegations_parent_launch_key_uq
    on delegations(parent_session_id, launch_key);

drop index if exists sessions_delegation_spawn_index_uq;
create unique index sessions_delegation_spawn_index_uq
    on sessions(delegation_id, (metadata->>'delegation_spawn_index'))
    where delegation_id is not null and metadata ? 'delegation_spawn_index';

drop index if exists delegations_parent_running_idx;
create index delegations_parent_running_idx
    on delegations(parent_session_id)
    where status in ('running','cancelling');

drop index if exists delegations_running_created_idx;
create index delegations_running_created_idx
    on delegations(created_at, id)
    where status in ('running','cancelling');

drop index if exists delegations_parent_running_full_uq;
create unique index delegations_parent_running_full_uq
    on delegations(parent_session_id)
    where status in ('running','cancelling') and kind='full';

do $$
declare
    required_index text;
begin
    foreach required_index in array array[
        'delegations_parent_launch_key_uq',
        'sessions_delegation_spawn_index_uq',
        'delegations_parent_running_idx',
        'delegations_running_created_idx',
        'delegations_parent_running_full_uq'
    ]
    loop
        if not exists (
            select 1
            from pg_class c
            join pg_index i on i.indexrelid = c.oid
            where c.relname = required_index and i.indisvalid and i.indisready
        ) then
            raise exception 'required index % is absent or invalid', required_index;
        end if;
    end loop;
end
$$;

commit;
