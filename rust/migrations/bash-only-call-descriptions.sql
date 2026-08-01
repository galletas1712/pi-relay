\set ON_ERROR_STOP on

-- One-time cleanup for sessions written while #330 applied call descriptions
-- to every first-party tool. Run this against a stopped daemon, after taking
-- a backup. The script is idempotent and preserves Bash, MCP, and unknown
-- tool arguments.

begin;

lock table sessions, transcript_entries, queued_inputs, actions, events
    in share row exclusive mode;

create temporary table migration_changed_sessions (
    session_id text primary key,
    transcript_changed boolean not null default false,
    queue_changed boolean not null default false,
    action_event_changed boolean not null default false,
    prompt_changed boolean not null default false
) on commit drop;

create or replace function pg_temp.strip_apply_patch_header(input_text text)
returns text
language plpgsql
as $$
declare
    newline_position integer;
begin
    if input_text is null
       or left(input_text, length('call_description: ')) <> 'call_description: '
    then
        return input_text;
    end if;

    newline_position := position(E'\n' in input_text);
    if newline_position = 0 then
        return input_text;
    end if;
    return substring(input_text from newline_position + 1);
end
$$;

create or replace function pg_temp.strip_json_description(args_text text)
returns text
language plpgsql
as $$
declare
    args jsonb;
begin
    if args_text is null then
        return null;
    end if;

    begin
        args := args_text::jsonb;
    exception when others then
        return args_text;
    end;

    if jsonb_typeof(args) <> 'object' or not (args ? 'call_description') then
        return args_text;
    end if;
    return (args - 'call_description'::text)::text;
end
$$;

create or replace function pg_temp.strip_canonical_tool_call(tool_call jsonb)
returns jsonb
language plpgsql
as $$
declare
    tool_name text;
    args_text text;
    args jsonb;
    input_text text;
    stripped_input text;
    stripped_args text;
    changed boolean := false;
begin
    if jsonb_typeof(tool_call) <> 'object' then
        return tool_call;
    end if;

    tool_name := tool_call->>'tool_name';
    if tool_name is null
       or tool_name not in (
           'Edit',
           'apply_patch',
           'str_replace_based_edit_tool',
           'WebSearch',
           'WebFetch',
           'web_search',
           'web_fetch',
           'LoadSkill',
           'delegate_writing_task',
           'delegate_readonly_tasks',
           'inspect_delegation',
           'cancel_delegation',
           'steer_subagent',
           'interrupt_subagent'
       )
    then
        return tool_call;
    end if;

    args_text := tool_call->>'args_json';
    if args_text is null then
        return tool_call;
    end if;

    stripped_args := pg_temp.strip_json_description(args_text);
    if tool_name <> 'Edit' then
        if stripped_args is not distinct from args_text then
            return tool_call;
        end if;
        return jsonb_set(
            tool_call,
            '{args_json}',
            to_jsonb(stripped_args),
            true
        );
    end if;

    begin
        args := args_text::jsonb;
    exception when others then
        return tool_call;
    end;
    if jsonb_typeof(args) <> 'object' then
        return tool_call;
    end if;

    if args ? 'call_description' then
        args := args - 'call_description'::text;
        changed := true;
    end if;
    if jsonb_typeof(args->'input') = 'string' then
        input_text := args->>'input';
        stripped_input := pg_temp.strip_apply_patch_header(input_text);
        if stripped_input is distinct from input_text then
            args := jsonb_set(args, '{input}', to_jsonb(stripped_input), true);
            changed := true;
        end if;
    end if;

    if not changed then
        return tool_call;
    end if;
    return jsonb_set(tool_call, '{args_json}', to_jsonb(args::text), true);
end
$$;

create or replace function pg_temp.strip_assistant_items(items jsonb)
returns jsonb
language plpgsql
as $$
declare
    cleaned jsonb;
begin
    if jsonb_typeof(items) <> 'array' then
        return items;
    end if;

    select coalesce(
        jsonb_agg(
            case
                when parts.part->>'type' = 'tool_call'
                    then pg_temp.strip_canonical_tool_call(parts.part)
                else parts.part
            end
            order by parts.ordinal
        ),
        '[]'::jsonb
    )
    into cleaned
    from jsonb_array_elements(items) with ordinality as parts(part, ordinal);
    return cleaned;
end
$$;

create or replace function pg_temp.strip_transcript_item(item jsonb)
returns jsonb
language plpgsql
as $$
declare
    cleaned jsonb;
begin
    if jsonb_typeof(item) <> 'object' then
        return item;
    end if;

    cleaned := pg_temp.strip_canonical_tool_call(item);
    if cleaned is distinct from item then
        return cleaned;
    end if;

    case item->>'type'
        when 'assistant_message' then
            if jsonb_typeof(item->'items') = 'array' then
                return jsonb_set(
                    item,
                    '{items}',
                    pg_temp.strip_assistant_items(item->'items'),
                    true
                );
            end if;
        when 'tool_call_started' then
            if jsonb_typeof(item->'tool_call') = 'object' then
                return jsonb_set(
                    item,
                    '{tool_call}',
                    pg_temp.strip_canonical_tool_call(item->'tool_call'),
                    true
                );
            end if;
        else
            null;
    end case;
    return item;
end
$$;

create or replace function pg_temp.strip_queued_content(content jsonb)
returns jsonb
language plpgsql
as $$
begin
    if jsonb_typeof(content) = 'object'
       and content->>'type' = 'daemon_tool_observation'
       and jsonb_typeof(content->'content') = 'object'
    then
        return jsonb_set(
            content,
            '{content}',
            pg_temp.strip_canonical_tool_call(content->'content'),
            true
        );
    end if;
    return content;
end
$$;

create or replace function pg_temp.strip_event_payload(payload jsonb)
returns jsonb
language plpgsql
as $$
declare
    cleaned jsonb;
begin
    if jsonb_typeof(payload) <> 'object' then
        return payload;
    end if;
    cleaned := payload;

    if cleaned ? 'tool_name' and cleaned ? 'args_json' then
        cleaned := pg_temp.strip_canonical_tool_call(cleaned);
    end if;
    if jsonb_typeof(cleaned->'item') = 'object' then
        cleaned := jsonb_set(
            cleaned,
            '{item}',
            pg_temp.strip_transcript_item(cleaned->'item'),
            true
        );
    end if;
    if jsonb_typeof(cleaned->'entry') = 'object'
       and jsonb_typeof(cleaned->'entry'->'item') = 'object'
    then
        cleaned := jsonb_set(
            cleaned,
            '{entry,item}',
            pg_temp.strip_transcript_item(cleaned->'entry'->'item'),
            true
        );
    end if;
    if jsonb_typeof(cleaned->'assistant') = 'object'
       and jsonb_typeof(cleaned->'assistant'->'items') = 'array'
    then
        cleaned := jsonb_set(
            cleaned,
            '{assistant,items}',
            pg_temp.strip_assistant_items(cleaned->'assistant'->'items'),
            true
        );
    end if;
    if jsonb_typeof(cleaned->'payload') = 'object'
       and cleaned->'payload' ? 'tool_name'
    then
        cleaned := jsonb_set(
            cleaned,
            '{payload}',
            pg_temp.strip_canonical_tool_call(cleaned->'payload'),
            true
        );
    end if;
    return cleaned;
end
$$;

create or replace function pg_temp.strip_provider_replay_item(replay_item jsonb)
returns jsonb
language plpgsql
as $$
declare
    provider text;
    raw_text text;
    raw jsonb;
    arguments text;
    stripped text;
    input_text text;
    stripped_input text;
    changed boolean := false;
begin
    if jsonb_typeof(replay_item) <> 'object' then
        return replay_item;
    end if;
    provider := replay_item->>'provider';
    raw_text := replay_item->>'raw_json';
    if raw_text is null then
        return replay_item;
    end if;

    begin
        raw := raw_text::jsonb;
    exception when others then
        return replay_item;
    end;
    if jsonb_typeof(raw) <> 'object' then
        return replay_item;
    end if;

    if provider = 'openai'
       and raw->>'type' = 'custom_tool_call'
       and raw->>'name' = 'apply_patch'
       and jsonb_typeof(raw->'input') = 'string'
    then
        input_text := raw->>'input';
        stripped_input := pg_temp.strip_apply_patch_header(input_text);
        if stripped_input is distinct from input_text then
            raw := jsonb_set(raw, '{input}', to_jsonb(stripped_input), true);
            changed := true;
        end if;
    elsif provider = 'openai'
          and raw->>'type' = 'function_call'
          and raw->>'name' in (
              'Edit',
              'WebSearch',
              'WebFetch',
              'web_search',
              'web_fetch',
              'LoadSkill',
              'delegate_writing_task',
              'delegate_readonly_tasks',
              'inspect_delegation',
              'cancel_delegation',
              'steer_subagent',
              'interrupt_subagent'
          )
          and jsonb_typeof(raw->'arguments') = 'string'
    then
        arguments := raw->>'arguments';
        stripped := pg_temp.strip_json_description(arguments);
        if stripped is distinct from arguments then
            raw := jsonb_set(raw, '{arguments}', to_jsonb(stripped), true);
            changed := true;
        end if;
    elsif provider = 'claude'
          and raw->>'type' = 'tool_use'
          and raw->>'name' in (
              'Edit',
              'str_replace_based_edit_tool',
              'WebSearch',
              'WebFetch',
              'web_search',
              'web_fetch',
              'LoadSkill',
              'delegate_writing_task',
              'delegate_readonly_tasks',
              'inspect_delegation',
              'cancel_delegation',
              'steer_subagent',
              'interrupt_subagent'
          )
          and jsonb_typeof(raw->'input') = 'object'
          and raw->'input' ? 'call_description'
    then
        raw := jsonb_set(
            raw,
            '{input}',
            (raw->'input') - 'call_description'::text,
            true
        );
        changed := true;
    end if;

    if not changed then
        return replay_item;
    end if;
    return jsonb_set(replay_item, '{raw_json}', to_jsonb(raw::text), true);
end
$$;

create or replace function pg_temp.strip_provider_replay(replay jsonb)
returns jsonb
language plpgsql
as $$
declare
    cleaned jsonb;
begin
    if jsonb_typeof(replay) <> 'array' then
        return replay;
    end if;

    select coalesce(
        jsonb_agg(
            pg_temp.strip_provider_replay_item(parts.part)
            order by parts.ordinal
        ),
        '[]'::jsonb
    )
    into cleaned
    from jsonb_array_elements(replay) with ordinality as parts(part, ordinal);
    return cleaned;
end
$$;

with changed as (
    update transcript_entries as entries
    set item = pg_temp.strip_transcript_item(entries.item),
        provider_replay = pg_temp.strip_provider_replay(entries.provider_replay)
    where entries.item is distinct from pg_temp.strip_transcript_item(entries.item)
       or entries.provider_replay is distinct from
          pg_temp.strip_provider_replay(entries.provider_replay)
    returning entries.session_id
)
insert into migration_changed_sessions (session_id, transcript_changed)
select distinct session_id, true
from changed
on conflict (session_id) do update
set transcript_changed = true;

with changed as (
    update queued_inputs as inputs
    set content = pg_temp.strip_queued_content(inputs.content)
    where inputs.content is distinct from pg_temp.strip_queued_content(inputs.content)
    returning inputs.session_id
)
insert into migration_changed_sessions (session_id, queue_changed)
select distinct session_id, true
from changed
on conflict (session_id) do update
set queue_changed = true;

with changed as (
    update actions as actions
    set payload = pg_temp.strip_canonical_tool_call(actions.payload)
    where actions.kind = 'tool'
      and actions.payload is distinct from
          pg_temp.strip_canonical_tool_call(actions.payload)
    returning actions.session_id
)
insert into migration_changed_sessions (session_id, action_event_changed)
select distinct session_id, true
from changed
on conflict (session_id) do update
set action_event_changed = true;

with changed as (
    update events as events
    set payload = pg_temp.strip_event_payload(events.payload)
    where events.payload is distinct from pg_temp.strip_event_payload(events.payload)
    returning events.session_id
)
insert into migration_changed_sessions (session_id, action_event_changed)
select distinct session_id, true
from changed
on conflict (session_id) do update
set action_event_changed = true;

with changed as (
    update sessions
    set system_prompt = replace(
            system_prompt,
            'Every pi-relay-owned tool call must include `call_description`: one short, single-line sentence explaining that exact invocation. Externally defined MCP tools retain their server-owned contracts and are excluded for now.',
            'Canonical `Bash` tool calls must include `call_description`: one short, single-line sentence explaining that exact invocation. Other first-party tools and MCP tools keep their own argument contracts.'
        ),
        updated_at = now()
    where system_prompt is distinct from replace(
        system_prompt,
        'Every pi-relay-owned tool call must include `call_description`: one short, single-line sentence explaining that exact invocation. Externally defined MCP tools retain their server-owned contracts and are excluded for now.',
        'Canonical `Bash` tool calls must include `call_description`: one short, single-line sentence explaining that exact invocation. Other first-party tools and MCP tools keep their own argument contracts.'
    )
    returning id as session_id
)
insert into migration_changed_sessions (session_id, prompt_changed)
select distinct session_id, true
from changed
on conflict (session_id) do update
set prompt_changed = true;

update sessions as sessions
set session_revision = sessions.session_revision
        + (
            case when changed.transcript_changed then 1 else 0 end
            + case when changed.queue_changed then 1 else 0 end
            + case when changed.action_event_changed then 1 else 0 end
            + case when changed.prompt_changed then 1 else 0 end
        )::bigint,
    queue_revision = sessions.queue_revision
        + case when changed.queue_changed then 1 else 0 end,
    transcript_revision = sessions.transcript_revision
        + case when changed.transcript_changed then 1 else 0 end,
    updated_at = now()
from migration_changed_sessions as changed
where sessions.id = changed.session_id;

commit;
