\set ON_ERROR_STOP on

-- One-time cutover to a single terminal delegation wakeup.
--
-- Partial (per-child) parent wakeups used the three-segment client_input_id
-- `delegation-steer:{delegation_id}:{attempt_id}:{subagent_id}`. The terminal
-- wakeup uses the two-segment `delegation-steer:{delegation_id}:{attempt_id}`.
-- Delegation ids, attempt ids, and session ids never contain ':', so the
-- anchored regex below distinguishes the two shapes exactly.
--
-- Rows already `consumed` are left alone: they reached the parent transcript
-- and are historical fact. Only still-deliverable rows are cancelled; the
-- delegations they belong to are still `running` and still owe their terminal
-- wakeup, so no parent is stranded.
begin;

lock table queued_inputs in share row exclusive mode;

create temporary table migrated_partial_wakeups on commit drop as
update queued_inputs
set status = 'cancelled',
    follow_up_position = null,
    updated_at = now(),
    origin = coalesce(origin, '{}'::jsonb)
        || jsonb_build_object(
               'cancelled_at', now()::text,
               'cancelled_reason', 'partial_delegation_wakeups_removed'
           )
where priority = 'steer'
  and status in ('queued', 'consuming')
  and content->>'type' = 'daemon_tool_observation'
  and client_input_id ~ '^delegation-steer:[^:]+:[^:]+:[^:]+$'
returning id, session_id;

-- Bump the affected parents so the first reconnect after the upgrade refetches
-- their queue projection instead of serving a cached row that no longer exists.
update sessions s
set session_revision = s.session_revision + 1,
    queue_revision = s.queue_revision + 1,
    updated_at = now()
where s.id in (select distinct session_id from migrated_partial_wakeups);

do $$
declare
    remaining bigint;
begin
    select count(*) into remaining
    from queued_inputs
    where priority = 'steer'
      and status in ('queued', 'consuming')
      and client_input_id ~ '^delegation-steer:[^:]+:[^:]+:[^:]+$';
    if remaining <> 0 then
        raise exception 'partial delegation wakeups remain after migration: %', remaining;
    end if;
end $$;

commit;
