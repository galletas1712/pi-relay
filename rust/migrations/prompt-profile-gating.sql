\set ON_ERROR_STOP on

-- Strip the remote-push instruction from the stored prompt of read-only
-- subagents that can still act on it.
--
-- `sessions.system_prompt` is rendered once at `session.start` and stored
-- forever, so read-only subagents started before capability gating still carry
-- "Before publishing changes, create a new descriptive branch and push that
-- branch to the configured remote." Their workspace is a disposable snapshot,
-- so acting on that sentence produces a real remote side effect that outlives
-- the session. New sessions never render it.
--
-- Only sessions that can still act are rewritten: `subagent_type = 'read_only'`
-- rows whose delegation is `running` or `cancelling`. A terminal session never
-- reads its prompt again, so rewriting it buys nothing and risks user-authored
-- bytes for no reason. On a typical deployment this matches zero rows.
--
-- The rewrite is scoped by position, not by pattern. `{{ project.agents_md }}`
-- reaches the prompt verbatim and renders strictly *before* `## Workspace`, so
-- confining `replace()` to the rendered `## Workspace` block structurally
-- excludes project instructions — including an AGENTS.md that quotes the house
-- rule, heads the file with it, or carries its own `## Workspace` heading.
-- `pg_temp.workspace_block` locates that block; the `where` filter and the
-- post-condition use the same function, so all predicates stay identical.
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

-- Everything at or after the LAST `\n## Workspace\n` heading, i.e. the rendered
-- Workspace block. Taking the last one keeps an AGENTS.md that contains the
-- same heading out of scope. The rendered block is always followed by the
-- `## Tools` section, so a heading without one did not come from the template
-- and yields '' — an empty block matches no predicate, so the row is skipped
-- rather than guessed at. Dropped with the session, on purpose: this is a
-- one-time artifact.
create function pg_temp.workspace_block(prompt text) returns text
language plpgsql immutable as $fn$
declare
    heading constant text := E'\n## Workspace\n';
    tail_offset constant integer := position(reverse(heading) in reverse(prompt));
    block text;
begin
    if tail_offset = 0 then
        return '';
    end if;
    block := substr(prompt, length(prompt) - tail_offset - length(heading) + 2);
    if position(E'\n## Tools\n' in block) = 0 then
        return '';
    end if;
    return block;
end
$fn$;

do $migration$
declare
    sentence constant text :=
'modify files in the Git workspace subdirectory directly. Before publishing changes, create a new descriptive branch and push that branch to the configured remote.';
    kept constant text := 'modify files in the Git workspace subdirectory directly.';
    rewritten integer;
    offending text[];
begin
    update sessions s
    set system_prompt =
        left(s.system_prompt,
             length(s.system_prompt) - length(pg_temp.workspace_block(s.system_prompt)))
        || replace(pg_temp.workspace_block(s.system_prompt), sentence, kept)
    where s.subagent_type = 'read_only'
      and exists (
          select 1 from delegations d
          where d.id = s.delegation_id and d.status in ('running','cancelling')
      )
      and position(sentence in pg_temp.workspace_block(s.system_prompt)) > 0;
    get diagnostics rewritten = row_count;
    raise notice 'rewrote % live read-only subagent prompt(s)', rewritten;

    -- Report rather than abort: a live read-only prompt that carries the
    -- sentence but whose rendered `## Workspace` block cannot be located is one
    -- an operator should inspect, not a reason to roll back every other row.
    select array_agg(s.id order by s.id) into offending
    from sessions s
    where s.subagent_type = 'read_only'
      and exists (
          select 1 from delegations d
          where d.id = s.delegation_id and d.status in ('running','cancelling')
      )
      and pg_temp.workspace_block(s.system_prompt) = ''
      and position(sentence in s.system_prompt) > 0;
    if offending is not null then
        raise notice 'inspect by hand, no rendered Workspace block found but the push sentence is present: %',
            array_to_string(offending, ', ');
    end if;
end
$migration$;

commit;
