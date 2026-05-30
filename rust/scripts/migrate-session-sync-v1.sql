-- Manual one-shot migration for the session sync / editable queue redesign.
--
-- This script is intentionally not wired into the daemon runtime. Run it once
-- against an existing pi-relay Postgres database before deploying code that
-- expects the revision and follow-up ordering columns.

begin;

alter table sessions
    add column if not exists session_revision bigint not null default 0,
    add column if not exists queue_revision bigint not null default 0,
    add column if not exists transcript_revision bigint not null default 0;

alter table queued_inputs
    add column if not exists updated_at timestamptz not null default now(),
    add column if not exists follow_up_position integer null;

-- Abandoned consuming rows are from the pre-redesign lease path. If the input
-- was durably materialized, a later transaction would have marked it consumed.
-- Treat remaining consuming rows as queued so they can be dispatched again.
update queued_inputs
set status = 'queued',
    origin = coalesce(origin, '{}'::jsonb) - 'claim_id' - 'claimed_at',
    updated_at = now()
where status = 'consuming';

-- Preserve existing follow-up dispatch order for active queued rows.
with ranked as (
    select
        id,
        row_number() over (
            partition by session_id
            order by created_at, id
        ) - 1 as position
    from queued_inputs
    where priority = 'follow_up'
        and status = 'queued'
        and follow_up_position is null
)
update queued_inputs q
set follow_up_position = ranked.position
from ranked
where q.id = ranked.id;

commit;
