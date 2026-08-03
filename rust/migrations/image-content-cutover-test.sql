\set ON_ERROR_STOP on

select current_database() = :'expected_database'
   and exists (
       select 1 from public.pi_relay_cutover_test_sentinel where token=:'sentinel'
   ) as verified
\gset
\if :verified
\else
  \echo 'Refusing fixture setup: generated database identity or sentinel mismatch'
  \quit 3
\endif

insert into public.sessions (
  id, runtime_id, workspace_id, workspaces, system_prompt, provider_config
) values ('cutover-session', 'test-runtime', 'test-workspace', '[]', 'test', '{}');

insert into public.transcript_entries (
  session_id, id, parent_id, timestamp_ms, item, provider_replay
) values
  (
    'cutover-session', 'legacy-user', null, 1,
    '{
      "type":"user_message",
      "content":[
        {"type":"text","text":"before"},
        {"type":"image","image":{"mime_type":"image/png","source":{"kind":"url","value":"https://example.test/a.png?x=%22exact%22#frag"}}},
        {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg=="}}},
        {"type":"text","text":"after"}
      ],
      "future_metadata":{"exact":["transcript",7]}
    }',
    '[{"opaque":{"image":{"source":{"kind":"base64","data":"do-not-touch"}}}}]'
  ),
  (
    'cutover-session', 'legacy-tool', 'legacy-user', 2,
    '{"type":"tool_result","tool_call_id":"call-1","tool_name":"Read","output":"old output","status":"Success"}',
    '[]'
  );

insert into public.actions (
  id, session_id, action_id, attempt_id, kind, status, payload, result
) values
  (
    'legacy-action', 'cutover-session', 1, 'attempt-1', 'tool', 'completed', '{}',
    '{
      "tool_call_id":"call-2","tool_name":"ReadImage","status":"Success",
      "content":[
        {"type":"text","text":"before"},
        {"type":"image","image":{"mime_type":"image/png","source":{"kind":"base64","value":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg=="}}}
      ]
    }'
  ),
  (
    'interrupted-action', 'cutover-session', 2, 'attempt-2', 'tool', 'interrupted', '{}',
    '{"reason":"session interrupted"}'
  ),
  (
    'cancelled-action', 'cutover-session', 3, 'attempt-3', 'tool', 'interrupted', '{}',
    '{"reason":"delegation cancelled"}'
  ),
  (
    'control-action', 'cutover-session', 4, 'attempt-4', 'tool', 'interrupted', '{}',
    '{"reason":"combined subagent control","control_input_id":"input-control"}'
  ),
  (
    'error-action', 'cutover-session', 5, 'attempt-5', 'tool', 'error', '{}',
    '{"error":"tool failed before producing content"}'
  ),
  (
    'error-result-action', 'cutover-session', 6, 'attempt-6', 'tool', 'error', '{}',
    '{
      "tool_call_id":"call-error","tool_name":"Bash","status":"Error",
      "content":[{"type":"text","text":"command failed"}]
    }'
  );

insert into public.queued_inputs (
  id, session_id, priority, content, status, follow_up_position
) values (
  'legacy-queue', 'cutover-session', 'follow_up',
  '{
    "type":"user_message",
    "content":{"content":[
      {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg=="}}}
    ],"future_metadata":{"exact":["queue",9]}}
  }',
  'queued', 1
);

with historical(item) as (
  values ('{
    "type":"user_message",
    "content":[
      {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg=="}}}
    ],
    "future_metadata":{"exact":["event-item",10]}
  }'::jsonb)
)
insert into public.events (session_id, type, payload)
select
  'cutover-session', 'transcript.appended',
  jsonb_build_object(
    'item', item,
    'entry', jsonb_build_object(
      'id', 'copy',
      'parent_id', 'legacy-tool',
      'timestamp_ms', 3,
      'sequence', 3,
      'item', item,
      'future_entry_metadata', jsonb_build_object('keep', true)
    ),
    'opaque', jsonb_build_object(
      'content', jsonb_build_array(
        jsonb_build_object(
          'type', 'image',
          'image', jsonb_build_object(
            'source', jsonb_build_object('kind', 'blob', 'value', 'unchanged')
          )
        )
      )
    )
  )
from historical;
