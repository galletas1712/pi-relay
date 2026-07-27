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
-- rows whose delegation is `running` or `cancelling`. Those are exactly the
-- children `recover_active_delegations_after_stale_mark` reloads and
-- `dispatch_ready_actions` re-dispatches on boot, off the stored prompt. A
-- terminal session never reads its prompt again, so rewriting it buys nothing
-- and risks user-authored bytes for no reason. On a typical deployment this
-- matches zero rows.
--
-- The prompt is a concatenation of template output and arbitrary user-authored
-- files: `{{ project.agents_md }}` before the rendered `## Workspace` block, and
-- the subagent role SKILL.md plus preloaded skill bodies after it. Any of them
-- may quote the sentence or carry its own `## Workspace` / `## Tools` headings,
-- so no positional rule can tell the rendered occurrence from a quoted one.
-- The rewrite therefore only touches prompts where the sentence occurs
-- *exactly once* in the whole string and that occurrence sits between a
-- `## Workspace` and a `## Tools` heading. Uniqueness makes a plain `replace()`
-- provably exact; anything ambiguous is reported for a human instead.
--
-- Nothing is *added*. The new `## Subagent contract` section and the
-- `./.pi-handoff/` promise are deliberately not injected: those sessions ran on
-- code that makes no such guarantee, and a capability claim the runtime cannot
-- honour is worse than the status quo. Removing a dangerous instruction is
-- safe; adding a false one is not.
--
-- A rerun leaves zero occurrences behind, so it matches no rows.
begin;

lock table sessions in access exclusive mode;

-- True only when the push sentence occurs exactly once in the whole prompt and
-- that occurrence is the rendered one. Dropped with the session, on purpose:
-- this is a one-time artifact.
create function pg_temp.uniquely_rendered(prompt text) returns boolean
language plpgsql immutable as $fn$
declare
    sentence constant text :=
'modify files in the Git workspace subdirectory directly. Before publishing changes, create a new descriptive branch and push that branch to the configured remote.';
    at constant integer := position(sentence in prompt);
begin
    if (length(prompt) - length(replace(prompt, sentence, ''))) / length(sentence) <> 1 then
        return false;
    end if;
    return position(E'\n## Workspace\n' in left(prompt, at - 1)) > 0
       and position(E'\n## Tools\n' in substr(prompt, at + length(sentence))) > 0;
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
    set system_prompt = replace(s.system_prompt, sentence, kept)
    where s.subagent_type = 'read_only'
      and exists (
          select 1 from delegations d
          where d.id = s.delegation_id and d.status in ('running','cancelling')
      )
      and pg_temp.uniquely_rendered(s.system_prompt);
    get diagnostics rewritten = row_count;
    raise notice 'rewrote % live read-only subagent prompt(s)', rewritten;

    -- Report rather than abort: a live read-only prompt that carries the
    -- sentence more than once, or outside a rendered block, is one an operator
    -- should read — not a reason to roll back every other row.
    select array_agg(s.id order by s.id) into offending
    from sessions s
    where s.subagent_type = 'read_only'
      and exists (
          select 1 from delegations d
          where d.id = s.delegation_id and d.status in ('running','cancelling')
      )
      and position(sentence in s.system_prompt) > 0
      and not pg_temp.uniquely_rendered(s.system_prompt);
    if offending is not null then
        raise notice 'inspect by hand, the push sentence is ambiguous (quoted, or no rendered Workspace block): %',
            array_to_string(offending, ', ');
    end if;
end
$migration$;

commit;
