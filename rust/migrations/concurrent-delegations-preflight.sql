\set ON_ERROR_STOP on

do $$
begin
    if exists (
        select 1
        from delegations
        where status = 'running' and kind = 'full'
        group by parent_session_id
        having count(*) > 1
    ) then
        raise exception
            'preflight failed: a parent has duplicate running full delegations';
    end if;
    if exists (
        select 1
        from delegations d
        where d.kind = 'full'
          and d.expected_subagents <> 1
    ) then
        raise exception
            'preflight failed: every full delegation must expect exactly one child';
    end if;
    if exists (
        select 1
        from delegations d
        where d.kind = 'full'
          and (select count(*) from sessions s where s.delegation_id=d.id) <> 1
          and not (
              d.status = 'failed'
              and (select count(*) from sessions s where s.delegation_id=d.id) = 0
          )
    ) then
        raise exception
            'preflight failed: every full delegation must link exactly one child except a failed launch with no materialized child';
    end if;
    if exists (
        select 1
        from delegations d
        where d.status = 'running'
          and (select count(*) from sessions s where s.delegation_id=d.id)
              <> d.expected_subagents
    ) then
        raise exception
            'preflight failed: a running legacy delegation is partially materialized; preserve and inspect it because missing child prompts cannot be reconstructed';
    end if;
    if exists (
        select 1
        from sessions
        where delegation_id is not null
          and (
              nullif(btrim(metadata->>'role_name'), '') is null
              or nullif(btrim(metadata->>'task'), '') is null
          )
    ) then
        raise exception
            'preflight failed: a delegation child is missing durable role/task metadata';
    end if;
end
$$;
