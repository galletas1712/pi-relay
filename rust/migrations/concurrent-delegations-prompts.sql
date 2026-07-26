\set ON_ERROR_STOP on

-- Remove the pre-concurrency single-delegation rules from every top-level
-- `sessions.system_prompt` that still carries them.
--
-- `sessions.system_prompt` is rendered once at `session.start` and stored
-- forever, but tool descriptions and JSON schemas are rebuilt from the registry
-- on every model request. Without this rewrite an old session would carry the
-- single-delegation-per-turn prose alongside concurrent-capable tool schemas.
--
-- Every replaced line is static `PI.md` template text inside the
-- `capabilities.can_delegate` block, with no nested conditional or loop, so the
-- rendered bytes are exactly the template bytes and an exact-string `replace()`
-- is deterministic.
--
-- Subagent prompts rendered before parent/subagent prompt profiles were split do
-- embed the parent delegation block. They are deliberately left alone: subagents
-- have no delegation tools, so concurrency prose would be wrong for them.
--
-- The `where` guard keys off the old first bullet, so a rerun matches no rows.
begin;

lock table sessions in access exclusive mode;

do $migration$
declare
    rewritten integer;
    stale_fragment text;
begin
    update sessions
    set system_prompt = replace(
        replace(
            replace(
                replace(
                    system_prompt,
'- Launch at most one delegation per turn, then end your turn. Do not poll or loop —
  you will be notified.',
'- You may launch several independent delegations in one turn, including one full
  writer and read-only fan-outs together. At most one full delegation may be
  active; fan-outs may reserve up to eight read-only slots across the parent.
  Slots remain reserved until their whole delegation is terminal. After
  launching the useful batch, end your turn. Do not poll or loop — you will be
  notified.'
                ),
'  snapshot is still `running`, decide only for that current running delegation:
  steer a running/steerable subagent, cancel the delegation, or end your turn
  and wait. Do not start an unrelated delegation from a running partial wakeup.',
'  snapshot is still `running`, apply it only to its explicit `delegation_id`.
  Other delegations may still be active and wakeups may arrive out of launch
  order. You may steer or cancel that delegation, wait, or launch additional
  independent read-only work.'
            ),
'  the whole current delegation, not as a substitute for exact-child interrupt.',
'  the named delegation, not as a substitute for exact-child interrupt.'
        ),
'  snapshot), with your own judgment (skip, launch fresh work, escalate, stop).',
'  snapshot), with your own judgment (skip, launch fresh work, escalate, stop).
  Sequential gates remain sequential: for example, final review of an
  implementation starts only after that writer''s terminal wakeup, even though
  unrelated research may overlap it.'
    ),
        updated_at = now()
    where system_prompt like '%- Launch at most one delegation per turn%'
      and parent_session_id is null;
    get diagnostics rewritten = row_count;
    raise notice 'rewrote % session prompt(s)', rewritten;

    foreach stale_fragment in array array[
        '- Launch at most one delegation per turn',
        'decide only for that current running delegation',
        'Do not start an unrelated delegation from a running partial wakeup',
        'the whole current delegation, not as a substitute'
    ]
    loop
        if exists (
            select 1 from sessions
            where system_prompt like '%' || stale_fragment || '%'
              and parent_session_id is null
        ) then
            raise exception 'session prompt still carries pre-concurrency text: %', stale_fragment;
        end if;
    end loop;
end
$migration$;

commit;
