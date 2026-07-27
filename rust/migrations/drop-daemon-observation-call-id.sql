\set ON_ERROR_STOP on

-- One-time cleanup of the retired DaemonToolObservation.tool_call_id field.
--
-- The field only ever existed to key the synthetic tool call/result pair the
-- provider adapters used to manufacture for delegation wakeups. Adapters now
-- render one plain user message, so nothing reads it. The Rust type no longer
-- declares it and serde ignores unknown fields, so old rows keep deserializing
-- either way; this just stops carrying a dead key forever.
--
-- Nothing indexes or constrains these keys, and `events.payload` never embeds
-- the observation body (queued-content events write `content: []`), so only
-- the two jsonb columns below need rewriting.
begin;

-- Durable transcript items: the observation is the item itself.
update transcript_entries
   set item = item - 'tool_call_id'
 where item->>'type' = 'daemon_tool_observation';

-- Queue rows: serde tags the content as {"type": ..., "content": {...}}.
update queued_inputs
   set content = jsonb_set(content, '{content}', (content->'content') - 'tool_call_id')
 where content->>'type' = 'daemon_tool_observation';

commit;
