\set ON_ERROR_STOP on

select current_database() = :'expected_database'
   and exists (
       select 1 from public.pi_relay_cutover_test_sentinel where token=:'sentinel'
   ) as verified
\gset
\if :verified
\else
  \echo 'Refusing fixture assertion: generated database identity or sentinel mismatch'
  \quit 3
\endif

do $$
declare
  artifact text;
begin
  select item#>>'{content,2,artifact_id}' into artifact
  from public.transcript_entries where id='legacy-user';
  if artifact !~ '^sha256:[0-9a-f]{64}$' then
    raise exception 'legacy user image did not become an artifact ref';
  end if;
  if (select count(*) from public.image_artifacts) <> 1 then
    raise exception 'identical bytes were not deduplicated';
  end if;
  if (select item#>>'{content,1,text}' from public.transcript_entries where id='legacy-user')
     is distinct from
     '[remote image preserved as URL: https://example.test/a.png?x=%22exact%22#frag]' then
    raise exception 'URL was not preserved exactly in ordered text';
  end if;
  if (select item->>'output' from public.transcript_entries where id='legacy-tool') is not null
     or (select item#>>'{content,0,text}' from public.transcript_entries where id='legacy-tool')
        is distinct from 'old output' then
    raise exception 'legacy tool output was not migrated';
  end if;
  if (select result#>>'{content,1,artifact_id}' from public.actions where id='legacy-action')
     is distinct from artifact then
    raise exception 'action image did not reuse artifact ref';
  end if;
  if (select item->'future_metadata' from public.transcript_entries where id='legacy-user')
     is distinct from '{"exact":["transcript",7]}'::jsonb
     or (select content#>'{content,future_metadata}' from public.queued_inputs where id='legacy-queue')
        is distinct from '{"exact":["queue",9]}'::jsonb then
    raise exception 'unknown enclosing metadata was not preserved';
  end if;
  if (select result from public.actions where id='interrupted-action')
     is distinct from '{"reason":"session interrupted"}'::jsonb
     or (select result from public.actions where id='cancelled-action')
        is distinct from '{"reason":"delegation cancelled"}'::jsonb
     or (select result from public.actions where id='control-action')
        is distinct from
           '{"reason":"combined subagent control","control_input_id":"input-control"}'::jsonb
     or (select result from public.actions where id='error-action')
        is distinct from '{"error":"tool failed before producing content"}'::jsonb
     or (select result from public.actions where id='error-result-action')
        is distinct from
           '{"tool_call_id":"call-error","tool_name":"Bash","status":"Error","content":[{"type":"text","text":"command failed"}]}'::jsonb then
    raise exception 'sanctioned tool action bookkeeping changed';
  end if;
  if (select content#>>'{content,content,0,artifact_id}' from public.queued_inputs where id='legacy-queue')
     is distinct from artifact then
    raise exception 'queue image did not reuse artifact ref';
  end if;
  if (select payload#>>'{entry,item,content,0,artifact_id}' from public.events limit 1)
     is distinct from artifact then
    raise exception 'event transcript copy did not reuse artifact ref';
  end if;
  if (select payload ? 'item' from public.events where type='transcript.appended') then
    raise exception 'historical top-level event item was not removed';
  end if;
  if (select payload#>'{entry,item,future_metadata}' from public.events where type='transcript.appended')
     is distinct from '{"exact":["event-item",10]}'::jsonb
     or (select payload#>'{entry,future_entry_metadata}' from public.events where type='transcript.appended')
        is distinct from '{"keep":true}'::jsonb then
    raise exception 'event item or entry metadata was not preserved';
  end if;
  if exists (
    select 1
    from public.transcript_entries
    where item @? '$.content[*] ? (@.type == "image" && exists(@.image))'
  ) or exists (
    select 1
    from public.actions
    where id='legacy-action'
      and result @? '$.content[*] ? (@.type == "image" && exists(@.image))'
  ) or exists (
    select 1
    from public.queued_inputs
    where id='legacy-queue'
      and content @? '$.content.content[*] ? (@.type == "image" && exists(@.image))'
  ) or exists (
    select 1
    from public.events
    where type='transcript.appended'
      and payload @? '$.entry.item.content[*] ? (@.type == "image" && exists(@.image))'
  ) then
    raise exception 'owned durable content still contains a legacy image shape';
  end if;
  if (select provider_replay from public.transcript_entries where id='legacy-user')
     is distinct from
     '[{"opaque":{"image":{"source":{"kind":"base64","data":"do-not-touch"}}}}]'::jsonb then
    raise exception 'provider replay changed';
  end if;
end
$$;
