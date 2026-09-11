\set ON_ERROR_STOP on

-- One-time model-catalog cutover for current/executable Claude routes and
-- replay-visible provider configuration events.
--
-- Sessions hold current defaults. Only queued/consuming inputs and
-- pending/blocked/running actions can still dispatch, so terminal queue/action
-- provider_config snapshots are preserved as historical facts. Every exact
-- events.payload.provider route is rewritten because provider configuration
-- events can repaint frontend state when replayed. Transcript provider replay
-- and historical response/result payloads are intentionally immutable.
begin;

lock table sessions, queued_inputs, actions, events in share row exclusive mode;

create temporary table affected_model_route_sessions (
    session_id text primary key
) on commit drop;

insert into affected_model_route_sessions (session_id)
select session_id
from (
    select id as session_id
    from sessions
    where provider_config->>'kind' = 'claude'
      and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
    union
    select session_id
    from queued_inputs
    where status in ('queued', 'consuming')
      and provider_config->>'kind' = 'claude'
      and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
    union
    select session_id
    from actions
    where status in ('pending', 'blocked', 'running')
      and provider_config->>'kind' = 'claude'
      and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
    union
    select session_id
    from events
    where payload#>>'{provider,kind}' = 'claude'
      and payload#>>'{provider,model}' in ('claude-opus-4-8', 'claude-fable-5')
) affected;

update sessions
set provider_config = jsonb_set(
        provider_config,
        '{model}',
        to_jsonb(
            (case provider_config->>'model'
                when 'claude-opus-4-8' then 'claude-opus-5'
                when 'claude-fable-5' then 'claude-fable-5-1'
            end)::text
        ),
        false
    )
where provider_config->>'kind' = 'claude'
  and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5');

update queued_inputs
set provider_config = jsonb_set(
        provider_config,
        '{model}',
        to_jsonb(
            (case provider_config->>'model'
                when 'claude-opus-4-8' then 'claude-opus-5'
                when 'claude-fable-5' then 'claude-fable-5-1'
            end)::text
        ),
        false
    )
where provider_config->>'kind' = 'claude'
  and status in ('queued', 'consuming')
  and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5');

update actions
set provider_config = jsonb_set(
        provider_config,
        '{model}',
        to_jsonb(
            (case provider_config->>'model'
                when 'claude-opus-4-8' then 'claude-opus-5'
                when 'claude-fable-5' then 'claude-fable-5-1'
            end)::text
        ),
        false
    )
where provider_config->>'kind' = 'claude'
  and status in ('pending', 'blocked', 'running')
  and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5');

update events
set payload = jsonb_set(
        payload,
        '{provider,model}',
        to_jsonb(
            (case payload#>>'{provider,model}'
                when 'claude-opus-4-8' then 'claude-opus-5'
                when 'claude-fable-5' then 'claude-fable-5-1'
            end)::text
        ),
        false
    )
where payload#>>'{provider,kind}' = 'claude'
  and payload#>>'{provider,model}' in ('claude-opus-4-8', 'claude-fable-5');

-- Current/executable route edits and replay-visible provider event edits
-- invalidate the session projection but do not change queue membership or
-- ordering, so queue_revision remains unchanged.
update sessions
set session_revision = session_revision + 1,
    updated_at = now()
where id in (select session_id from affected_model_route_sessions);

do $$
declare
    remaining bigint;
begin
    select sum(reference_count) into remaining
    from (
        select count(*) as reference_count
        from sessions
        where provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
        union all
        select count(*)
        from queued_inputs
        where status in ('queued', 'consuming')
          and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
        union all
        select count(*)
        from actions
        where status in ('pending', 'blocked', 'running')
          and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
        union all
        select count(*)
        from events
        where payload#>>'{provider,model}' in ('claude-opus-4-8', 'claude-fable-5')
    ) references_by_location;
    if remaining <> 0 then
        raise exception 'retired current/executable or replay-visible Claude provider routes remain after migration: %', remaining;
    end if;
end $$;

commit;
