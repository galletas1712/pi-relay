\set ON_ERROR_STOP on

-- Strip the remote-push instruction from every stored read-only subagent
-- prompt.
--
-- `sessions.system_prompt` is rendered once at `session.start` and stored
-- forever, so read-only subagents started before capability gating still carry
-- "Before publishing changes, create a new descriptive branch and push that
-- branch to the configured remote." Their workspace is a disposable snapshot,
-- so acting on that sentence produces a real remote side effect that outlives
-- the session. New sessions never render it.
--
-- The sentence is static `PI.md` template text on a single line with no nested
-- conditional or loop, so the rendered bytes are exactly the template bytes and
-- an exact-string `replace()` is deterministic. It is only removed from rows
-- whose `subagent_type` is `read_only`, so a parent or full subagent that
-- legitimately publishes branches is untouched.
--
-- Nothing is *added*. The new `## Subagent contract` section and the
-- `./.pi-handoff/` promise are deliberately not injected: those sessions ran on
-- code that makes no such guarantee, and a capability claim the runtime cannot
-- honour is worse than the status quo. Removing a dangerous instruction is
-- safe; adding a false one is not.
--
-- The `where` guard keys off the removed sentence, so a rerun matches no rows.
begin;

lock table sessions in access exclusive mode;

do $migration$
declare
    rewritten integer;
begin
    update sessions
    set system_prompt = replace(
        system_prompt,
' Before publishing changes, create a new descriptive branch and push that branch to the configured remote.',
''
    ),
        updated_at = now()
    where subagent_type = 'read_only'
      and system_prompt like '%push that branch to the configured remote%';
    get diagnostics rewritten = row_count;
    raise notice 'rewrote % read-only subagent prompt(s)', rewritten;

    if exists (
        select 1 from sessions
        where subagent_type = 'read_only'
          and system_prompt like '%push that branch to the configured remote%'
    ) then
        raise exception 'a read-only subagent prompt still instructs a remote push';
    end if;
end
$migration$;

commit;
