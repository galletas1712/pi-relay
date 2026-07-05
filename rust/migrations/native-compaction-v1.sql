\set ON_ERROR_STOP on
\set QUIET 1

-- pi-relay native-compaction-v1 one-time migration.
--
-- Required invocation variables:
--   -v accept_native_only=1
--   -v dry_run=1   (exercise the exact patch, report, then ROLLBACK)
--   -v dry_run=0   (exercise the exact patch, report, then COMMIT)
--
-- Run pg_dump first. Stop every pi-agentd and old writer before this script.
-- The selected database/search_path must resolve the pi-relay tables in one
-- ordinary schema. Ambiguous historical forms abort the whole transaction.

\if :{?accept_native_only}
\else
  DO $$
  BEGIN
      RAISE EXCEPTION USING MESSAGE =
          '{"migration_version":"native_compaction_v1","category":"missing_psql_variable","detail":"missing -v accept_native_only=1"}';
  END
  $$;
\endif
\if :{?dry_run}
\else
  DO $$
  BEGIN
      RAISE EXCEPTION USING MESSAGE =
          '{"migration_version":"native_compaction_v1","category":"missing_psql_variable","detail":"missing -v dry_run=1 or -v dry_run=0"}';
  END
  $$;
\endif

BEGIN ISOLATION LEVEL SERIALIZABLE;
SET LOCAL lock_timeout = '10s';
SET LOCAL statement_timeout = '0';

SELECT set_config('pi_relay.accept_native_only', :'accept_native_only', true)
    AS accepted_native_only \gset
SELECT set_config('pi_relay.native_compaction_dry_run', :'dry_run', true)
    AS native_compaction_dry_run \gset

DO $$
BEGIN
    IF current_setting('pi_relay.accept_native_only') <> '1' THEN
        RAISE EXCEPTION USING
            MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'native_only_acknowledgement_required',
                'detail', 'rerun with -v accept_native_only=1'
            )::text;
    END IF;
    IF current_setting('pi_relay.native_compaction_dry_run') NOT IN ('0', '1') THEN
        RAISE EXCEPTION USING
            MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'invalid_dry_run_value',
                'detail', 'dry_run must be 1 or 0'
            )::text;
    END IF;
END
$$;

DO $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            current_database() || chr(31) || coalesce(current_schema(), '') ||
            chr(31) || 'pi-relay-native-compaction-v1',
            0
        )
    );
END
$$;

-- Concise schema floor: these are the relations/columns this patch reads or
-- writes. PostgreSQL constraints and casts remain the authoritative validators.
DO $$
DECLARE
    relation_name text;
    column_spec text[];
    found_type text;
BEGIN
    IF current_schema() IS NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            '{"migration_version":"native_compaction_v1","category":"unsupported_schema_floor","detail":"current_schema() is null"}';
    END IF;

    FOREACH relation_name IN ARRAY ARRAY[
        'projects', 'sessions', 'daemon_config', 'transcript_entries',
        'queued_inputs', 'actions', 'events', 'delegations'
    ] LOOP
        IF to_regclass(format('%I.%I', current_schema(), relation_name)) IS NULL
           OR (
               SELECT c.relkind <> 'r'
               FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE n.nspname = current_schema() AND c.relname = relation_name
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'unsupported_schema_floor',
                'relation', format('%I.%I', current_schema(), relation_name),
                'detail', 'required ordinary table is missing'
            )::text;
        END IF;
    END LOOP;

    FOREACH column_spec SLICE 1 IN ARRAY ARRAY[
        ['projects','id','uuid'], ['projects','created_at','timestamp with time zone'],
        ['projects','updated_at','timestamp with time zone'], ['projects','name','text'],
        ['projects','workspaces','jsonb'], ['projects','metadata','jsonb'],
        ['sessions','id','text'], ['sessions','project_id','uuid'],
        ['sessions','outer_cwd','text'], ['sessions','workspaces','jsonb'],
        ['sessions','created_at','timestamp with time zone'],
        ['sessions','updated_at','timestamp with time zone'],
        ['sessions','active_leaf_id','text'], ['sessions','system_prompt','text'],
        ['sessions','provider_config','jsonb'], ['sessions','metadata','jsonb'],
        ['sessions','parent_session_id','text'],
        ['sessions','last_user_message_timestamp_ms','bigint'],
        ['sessions','session_revision','bigint'], ['sessions','queue_revision','bigint'],
        ['sessions','transcript_revision','bigint'], ['sessions','subagent_type','text'],
        ['sessions','delegation_id','text'],
        ['daemon_config','key','text'], ['daemon_config','value','jsonb'],
        ['daemon_config','updated_at','timestamp with time zone'],
        ['transcript_entries','session_id','text'], ['transcript_entries','id','text'],
        ['transcript_entries','parent_id','text'], ['transcript_entries','timestamp_ms','bigint'],
        ['transcript_entries','item','jsonb'], ['transcript_entries','turn_id','bigint'],
        ['transcript_entries','sequence','bigint'],
        ['queued_inputs','id','text'], ['queued_inputs','session_id','text'],
        ['queued_inputs','priority','text'], ['queued_inputs','content','jsonb'],
        ['queued_inputs','origin','jsonb'], ['queued_inputs','status','text'],
        ['queued_inputs','created_at','timestamp with time zone'],
        ['queued_inputs','updated_at','timestamp with time zone'],
        ['queued_inputs','follow_up_position','integer'],
        ['queued_inputs','client_input_id','text'],
        ['actions','id','text'], ['actions','session_id','text'], ['actions','attempt_id','text'],
        ['actions','turn_id','bigint'], ['actions','action_id','bigint'],
        ['actions','kind','text'], ['actions','status','text'],
        ['actions','payload','jsonb'], ['actions','result','jsonb'],
        ['actions','created_at','timestamp with time zone'],
        ['actions','updated_at','timestamp with time zone'],
        ['events','id','bigint'], ['events','session_id','text'], ['events','type','text'],
        ['events','payload','jsonb'], ['events','created_at','timestamp with time zone'],
        ['delegations','id','text'], ['delegations','parent_session_id','text'],
        ['delegations','workflow','text'], ['delegations','label','text'],
        ['delegations','kind','text'], ['delegations','status','text'],
        ['delegations','attempt_id','text'],
        ['delegations','created_at','timestamp with time zone'],
        ['delegations','updated_at','timestamp with time zone'],
        ['delegations','expected_subagents','integer']
    ] LOOP
        SELECT format_type(a.atttypid, a.atttypmod)
        INTO found_type
        FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = current_schema()
          AND c.relname = column_spec[1]
          AND a.attname = column_spec[2]
          AND a.attnum > 0
          AND NOT a.attisdropped;
        IF found_type IS DISTINCT FROM column_spec[3] THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'unsupported_schema_floor',
                'column', concat(column_spec[1], '.', column_spec[2]),
                'expected_type', column_spec[3],
                'actual_type', found_type
            )::text;
        END IF;
    END LOOP;

    IF to_regclass(format('%I.pi_native_compaction_v1_runs', current_schema())) IS NOT NULL
       OR to_regclass(format('%I.pi_native_compaction_v1_audit', current_schema())) IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
            'migration_version', 'native_compaction_v1',
            'category', 'obsolete_migration_ledger',
            'detail', 'remove the rejected Rust-migrator ledger tables before running this patch'
        )::text;
    END IF;
END
$$;

LOCK TABLE
    projects, sessions, daemon_config, transcript_entries,
    queued_inputs, actions, events, delegations
IN ACCESS EXCLUSIVE MODE;

CREATE TEMP TABLE _nc_context (
    schema_name text NOT NULL,
    replay_column_existed boolean NOT NULL,
    transcript_sequence text NOT NULL,
    transcript_original_next bigint NOT NULL,
    transcript_planned_next bigint,
    events_sequence text NOT NULL,
    events_original_next bigint NOT NULL,
    events_planned_next bigint,
    before_checksum text,
    after_checksum text
) ON COMMIT DROP;

DO $$
DECLARE
    transcript_sequence text;
    events_sequence text;
    transcript_last bigint;
    transcript_called boolean;
    events_last bigint;
    events_called boolean;
    replay_existed boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
        FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = current_schema()
          AND c.relname = 'transcript_entries'
          AND a.attname = 'provider_replay'
          AND a.attnum > 0
          AND NOT a.attisdropped
    ) INTO replay_existed;

    transcript_sequence := pg_get_serial_sequence(
        format('%I.%I', current_schema(), 'transcript_entries'), 'sequence'
    );
    events_sequence := pg_get_serial_sequence(
        format('%I.%I', current_schema(), 'events'), 'id'
    );
    IF transcript_sequence IS NULL OR events_sequence IS NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            '{"migration_version":"native_compaction_v1","category":"unsupported_schema_floor","detail":"required owned sequences are missing"}';
    END IF;

    EXECUTE format('SELECT last_value, is_called FROM %s', transcript_sequence::regclass)
        INTO transcript_last, transcript_called;
    EXECUTE format('SELECT last_value, is_called FROM %s', events_sequence::regclass)
        INTO events_last, events_called;

    INSERT INTO _nc_context (
        schema_name, replay_column_existed,
        transcript_sequence, transcript_original_next,
        events_sequence, events_original_next
    ) VALUES (
        current_schema(), replay_existed,
        transcript_sequence,
        CASE WHEN transcript_called THEN transcript_last + 1 ELSE transcript_last END,
        events_sequence,
        CASE WHEN events_called THEN events_last + 1 ELSE events_last END
    );
EXCEPTION WHEN numeric_value_out_of_range THEN
    RAISE EXCEPTION USING MESSAGE =
        '{"migration_version":"native_compaction_v1","category":"sequence_overflow","detail":"sequence next value overflows bigint"}';
END
$$;

CREATE TEMP TABLE _nc_original (
    table_name text NOT NULL,
    row_id text NOT NULL,
    session_id text,
    row_data jsonb NOT NULL,
    PRIMARY KEY (table_name, row_id)
) ON COMMIT DROP;

INSERT INTO _nc_original
SELECT 'projects', id::text, NULL, to_jsonb(p) FROM projects p
UNION ALL
SELECT 'sessions', id, id, to_jsonb(s) FROM sessions s
UNION ALL
SELECT 'daemon_config', key, NULL, to_jsonb(c) FROM daemon_config c
UNION ALL
SELECT 'transcript_entries', session_id || ':' || id, session_id, to_jsonb(t)
FROM transcript_entries t
UNION ALL
SELECT 'queued_inputs', id, session_id, to_jsonb(q) FROM queued_inputs q
UNION ALL
SELECT 'actions', id, session_id, to_jsonb(a) FROM actions a
UNION ALL
SELECT 'events', id::text, session_id, to_jsonb(e) FROM events e
UNION ALL
SELECT 'delegations', id, parent_session_id, to_jsonb(d) FROM delegations d;

UPDATE _nc_context
SET before_checksum = (
    SELECT md5(coalesce(string_agg(
        table_name || chr(31) || row_id || chr(31) || row_data::text,
        chr(1) ORDER BY table_name, row_id
    ), '') || chr(2) ||
    (SELECT transcript_original_next::text || ':' || events_original_next::text FROM _nc_context))
    FROM _nc_original
);

CREATE TEMP TABLE _nc_categories (
    category text NOT NULL,
    table_name text NOT NULL,
    row_id text NOT NULL,
    session_id text,
    PRIMARY KEY (category, table_name, row_id)
) ON COMMIT DROP;

-- Applying with unfinished work would race or synthesize intent. Operators
-- must drain/terminalize it using the old release. Selected-subagent controls
-- in an interrupt phase block independently of queue status: the migration
-- never guesses whether a malformed/partially settled control is safe.
DO $$
DECLARE
    detail jsonb;
BEGIN
    SELECT jsonb_build_object(
        'actions', coalesce((
            SELECT jsonb_agg(id ORDER BY id)
            FROM actions WHERE status IN ('pending', 'blocked', 'running')
        ), '[]'::jsonb),
        'queued_inputs', coalesce((
            SELECT jsonb_agg(id ORDER BY id)
            FROM queued_inputs WHERE status IN ('queued', 'consuming')
        ), '[]'::jsonb),
        'delegations', coalesce((
            SELECT jsonb_agg(id ORDER BY id)
            FROM delegations WHERE status = 'running'
        ), '[]'::jsonb),
        'subagent_controls', coalesce((
            SELECT jsonb_agg(id ORDER BY id)
            FROM queued_inputs
            WHERE origin->>'control_kind' IN (
                      'scoped_subagent_steer',
                      'scoped_subagent_interrupt'
                  )
              AND origin->>'control_phase' IN (
                      'pending_interrupt',
                      'interrupt_applied'
                  )
        ), '[]'::jsonb),
        'dispatch_actions', coalesce((
            SELECT jsonb_agg(id ORDER BY id)
            FROM actions WHERE payload ? 'post_compaction_dispatch'
        ), '[]'::jsonb)
    ) INTO detail;
    IF detail <> '{"actions":[],"queued_inputs":[],"delegations":[],"subagent_controls":[],"dispatch_actions":[]}'::jsonb THEN
        RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
            'migration_version', 'native_compaction_v1',
            'category', 'live_work_blocked',
            'detail', detail,
            'remediation', 'stop writers and drain or terminalize work with the old release'
        )::text;
    END IF;
END
$$;

-- The sidecar-less historical variant is accepted.
INSERT INTO _nc_categories
SELECT 'provider_replay_column_added', 'schema', 'transcript_entries.provider_replay', NULL
FROM _nc_context WHERE NOT replay_column_existed;

ALTER TABLE transcript_entries
    ADD COLUMN IF NOT EXISTS provider_replay jsonb NOT NULL DEFAULT '[]'::jsonb;

DO $$
DECLARE
    replay_type text;
    replay_default text;
    replay_not_null boolean;
BEGIN
    SELECT format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull
    INTO replay_type, replay_default, replay_not_null
    FROM pg_attribute a
    JOIN pg_class c ON c.oid = a.attrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE n.nspname = current_schema()
      AND c.relname = 'transcript_entries'
      AND a.attname = 'provider_replay'
      AND a.attnum > 0
      AND NOT a.attisdropped;
    IF replay_type <> 'jsonb' OR NOT replay_not_null
       OR replay_default IS DISTINCT FROM '''[]''::jsonb' THEN
        RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
            'migration_version', 'native_compaction_v1',
            'category', 'unsupported_provider_replay_column',
            'type', replay_type,
            'not_null', replay_not_null,
            'default', replay_default
        )::text;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION pg_temp.nc_canonical_provider(tag text)
RETURNS text LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT CASE tag
        WHEN 'openai' THEN 'openai'
        WHEN 'codex' THEN 'openai'
        WHEN 'claude' THEN 'claude'
        WHEN 'anthropic' THEN 'claude'
    END
$$;

CREATE OR REPLACE FUNCTION pg_temp.nc_try_jsonb(raw text)
RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    RETURN raw::jsonb;
EXCEPTION WHEN others THEN
    RETURN NULL;
END
$$;

CREATE OR REPLACE FUNCTION pg_temp.nc_canonical_replay(replay jsonb)
RETURNS jsonb LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(jsonb_agg(
        CASE
            WHEN jsonb_typeof(value) = 'object'
             AND pg_temp.nc_canonical_provider(value->>'provider') IS NOT NULL
            THEN jsonb_set(
                value, '{provider}',
                to_jsonb(pg_temp.nc_canonical_provider(value->>'provider')), false
            )
            ELSE value
        END
        ORDER BY ord
    ), '[]'::jsonb)
    FROM jsonb_array_elements(replay) WITH ORDINALITY AS entry(value, ord)
$$;

CREATE OR REPLACE FUNCTION pg_temp.nc_jsonb_suffix(value jsonb, suffix_length integer)
RETURNS jsonb LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(jsonb_agg(element ORDER BY ord), '[]'::jsonb)
    FROM jsonb_array_elements(value) WITH ORDINALITY AS item(element, ord)
    WHERE ord > jsonb_array_length(value) - suffix_length
$$;

CREATE OR REPLACE FUNCTION pg_temp.nc_embedded_replay(item jsonb)
RETURNS jsonb LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT coalesce(jsonb_agg(
        jsonb_build_object(
            'provider', pg_temp.nc_canonical_provider(element->>'provider'),
            'raw_json', element->>'raw_json'
        )
        ORDER BY ord
    ), '[]'::jsonb)
    FROM jsonb_array_elements(
        CASE
            WHEN item->>'type' = 'assistant_message'
             AND jsonb_typeof(item->'items') = 'array'
            THEN item->'items'
            ELSE '[]'::jsonb
        END
    ) WITH ORDINALITY AS item_element(element, ord)
    WHERE element->>'type' = 'provider_replay_record'
$$;

-- Validate and canonicalize provider configuration.
DO $$
DECLARE
    row record;
BEGIN
    FOR row IN SELECT id, provider_config FROM sessions ORDER BY id LOOP
        IF jsonb_typeof(row.provider_config) IS DISTINCT FROM 'object'
           OR jsonb_typeof(row.provider_config->'kind') IS DISTINCT FROM 'string'
           OR pg_temp.nc_canonical_provider(row.provider_config->>'kind') IS NULL
           OR jsonb_typeof(row.provider_config->'model') IS DISTINCT FROM 'string'
           OR coalesce(row.provider_config->>'model', '') = ''
           OR (
               row.provider_config ? 'reasoning_effort'
               AND (
                   jsonb_typeof(row.provider_config->'reasoning_effort')
                       IS DISTINCT FROM 'string'
                   OR coalesce(row.provider_config->>'reasoning_effort','') NOT IN
                       ('none','minimal','low','medium','high','xhigh','max')
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_provider_config',
                'session_id', row.id
            )::text;
        END IF;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'provider_tag_canonicalized', 'sessions', id, id
FROM sessions
WHERE provider_config->>'kind' IN ('anthropic', 'codex');

UPDATE sessions
SET provider_config = jsonb_set(
    provider_config, '{kind}',
    to_jsonb(pg_temp.nc_canonical_provider(provider_config->>'kind')), false
)
WHERE provider_config->>'kind' IN ('anthropic', 'codex');

-- Policy conversion: known direct fields move into compaction.config; equal
-- direct/nested values dedupe; conflicts and malformed known values abort.
DO $$
DECLARE
    row record;
    field text;
    direct jsonb;
    nested jsonb;
BEGIN
    FOR row IN SELECT id, provider_config, metadata FROM sessions ORDER BY id LOOP
        IF jsonb_typeof(row.metadata) IS DISTINCT FROM 'object' THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_session_metadata',
                'session_id', row.id
            )::text;
        END IF;
        IF row.metadata ? 'compaction'
           AND jsonb_typeof(row.metadata->'compaction') IS DISTINCT FROM 'object' THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_compaction_policy',
                'session_id', row.id,
                'field', 'compaction'
            )::text;
        END IF;
        IF row.metadata#>'{compaction}' ? 'config'
           AND jsonb_typeof(row.metadata#>'{compaction,config}') IS DISTINCT FROM 'object' THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_compaction_policy',
                'session_id', row.id,
                'field', 'compaction.config'
            )::text;
        END IF;

        FOREACH field IN ARRAY ARRAY[
            'auto_enabled', 'context_window', 'auto_limit_tokens',
            'max_consecutive_failures', 'remote_mode',
            'anthropic_native_compaction'
        ] LOOP
            direct := row.metadata#>ARRAY['compaction', field];
            nested := row.metadata#>ARRAY['compaction', 'config', field];
            IF direct IS NOT NULL AND nested IS NOT NULL AND direct <> nested THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version', 'native_compaction_v1',
                    'category', 'conflicting_compaction_policy',
                    'session_id', row.id,
                    'field', field,
                    'direct', direct,
                    'nested', nested
                )::text;
            END IF;
            IF coalesce(direct, nested) IS NOT NULL THEN
                direct := coalesce(direct, nested);
                IF (field = 'auto_enabled'
                    AND jsonb_typeof(direct) IS DISTINCT FROM 'boolean')
                   OR (
                       field IN ('context_window','auto_limit_tokens','max_consecutive_failures')
                       AND (
                           jsonb_typeof(direct) IS DISTINCT FROM 'number'
                           OR direct::text !~ '^[0-9]+$'
                       )
                   )
                   OR (
                       field = 'remote_mode'
                       AND (
                           jsonb_typeof(direct) IS DISTINCT FROM 'string'
                           OR direct#>>'{}' NOT IN ('never','auto','always')
                       )
                   )
                   OR (
                       field = 'anthropic_native_compaction'
                       AND direct <> 'null'::jsonb
                       AND (
                           jsonb_typeof(direct) IS DISTINCT FROM 'string'
                           OR direct#>>'{}' <> 'compact_20260112'
                       )
                   ) THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version', 'native_compaction_v1',
                        'category', 'malformed_compaction_policy',
                        'session_id', row.id,
                        'field', field,
                        'value', direct
                    )::text;
                END IF;
            END IF;
        END LOOP;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'implicit_claude_local', 'sessions', id, id
FROM sessions
WHERE provider_config->>'kind' = 'claude'
  AND metadata#>'{compaction,config,remote_mode}' IS NULL
  AND metadata#>'{compaction,remote_mode}' IS NULL;

INSERT INTO _nc_categories
SELECT 'explicit_remote_mode_' || coalesce(
    metadata#>>'{compaction,config,remote_mode}',
    metadata#>>'{compaction,remote_mode}'
), 'sessions', id, id
FROM sessions
WHERE coalesce(
    metadata#>'{compaction,config,remote_mode}',
    metadata#>'{compaction,remote_mode}'
) IS NOT NULL;

INSERT INTO _nc_categories
SELECT 'retired_claude_enrollment', 'sessions', id, id
FROM sessions
WHERE coalesce(
    metadata#>'{compaction,config,anthropic_native_compaction}',
    metadata#>'{compaction,anthropic_native_compaction}'
) IS NOT NULL;

INSERT INTO _nc_categories
SELECT 'policy_layout_converted', 'sessions', id, id
FROM sessions
WHERE EXISTS (
    SELECT 1
    FROM unnest(ARRAY[
        'auto_enabled','context_window','auto_limit_tokens',
        'max_consecutive_failures','remote_mode','anthropic_native_compaction'
    ]) field
    WHERE metadata#>ARRAY['compaction', field] IS NOT NULL
);

INSERT INTO _nc_categories
SELECT 'retired_selector_removed', 'sessions', id, id
FROM sessions
WHERE coalesce(
    metadata#>'{compaction,config,remote_mode}',
    metadata#>'{compaction,remote_mode}',
    metadata#>'{compaction,config,anthropic_native_compaction}',
    metadata#>'{compaction,anthropic_native_compaction}'
) IS NOT NULL;

CREATE OR REPLACE FUNCTION pg_temp.nc_policy_after(metadata jsonb)
RETURNS jsonb LANGUAGE plpgsql IMMUTABLE STRICT AS $$
DECLARE
    compaction jsonb;
    config jsonb;
    field text;
    had_config boolean;
    had_direct boolean := false;
BEGIN
    IF NOT metadata ? 'compaction' THEN
        RETURN metadata;
    END IF;
    compaction := metadata->'compaction';
    had_config := compaction ? 'config';
    config := coalesce(compaction->'config', '{}'::jsonb);
    FOREACH field IN ARRAY ARRAY[
        'auto_enabled','context_window','auto_limit_tokens',
        'max_consecutive_failures','remote_mode','anthropic_native_compaction'
    ] LOOP
        IF compaction ? field THEN
            had_direct := true;
            IF NOT config ? field THEN
                config := config || jsonb_build_object(field, compaction->field);
            END IF;
            compaction := compaction - field;
        END IF;
    END LOOP;
    config := config - 'remote_mode' - 'anthropic_native_compaction';
    IF had_config OR had_direct THEN
        compaction := jsonb_set(compaction, '{config}', config, true);
    END IF;
    RETURN jsonb_set(metadata, '{compaction}', compaction, false);
END
$$;

UPDATE sessions
SET metadata = pg_temp.nc_policy_after(metadata)
WHERE metadata IS DISTINCT FROM pg_temp.nc_policy_after(metadata);

-- Canonicalize sidecar wrapper tags without touching opaque raw_json bytes.
DO $$
DECLARE
    row record;
    item jsonb;
BEGIN
    FOR row IN
        SELECT transcript.session_id, transcript.id,
               transcript.item->>'type' AS item_type,
               transcript.provider_replay
        FROM transcript_entries transcript
        ORDER BY transcript.session_id, transcript.sequence, transcript.id
    LOOP
        IF jsonb_typeof(row.provider_replay) IS DISTINCT FROM 'array'
           AND row.item_type <> 'compaction_summary' THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_provider_replay',
                'row_id', row.session_id || ':' || row.id,
                'detail', 'provider_replay is not an array'
            )::text;
        END IF;
        -- A finished semantic checkpoint can safely discard invalid replay
        -- during bootstrap. Ordinary entries cannot.
        IF row.item_type <> 'compaction_summary' THEN
            FOR item IN SELECT value FROM jsonb_array_elements(row.provider_replay) LOOP
                IF jsonb_typeof(item) IS DISTINCT FROM 'object'
                   OR jsonb_typeof(item->'provider') IS DISTINCT FROM 'string'
                   OR pg_temp.nc_canonical_provider(item->>'provider') IS NULL
                   OR jsonb_typeof(item->'raw_json') IS DISTINCT FROM 'string'
                   OR pg_temp.nc_try_jsonb(item->>'raw_json') IS NULL THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version', 'native_compaction_v1',
                        'category', 'malformed_provider_replay',
                        'row_id', row.session_id || ':' || row.id
                    )::text;
                END IF;
            END LOOP;
        END IF;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'replay_tag_canonicalized', 'transcript_entries', session_id || ':' || id, session_id
FROM transcript_entries
WHERE CASE
    WHEN jsonb_typeof(provider_replay) = 'array' THEN EXISTS (
        SELECT 1 FROM jsonb_array_elements(provider_replay) entry
        WHERE entry->>'provider' IN ('anthropic','codex')
    )
    ELSE false
END;

UPDATE transcript_entries
SET provider_replay = pg_temp.nc_canonical_replay(provider_replay)
WHERE jsonb_typeof(provider_replay) = 'array'
  AND provider_replay IS DISTINCT FROM pg_temp.nc_canonical_replay(provider_replay);

-- Lift old embedded replay as one ordered sequence. Duplicates are retained.
DO $$
DECLARE
    row record;
    element jsonb;
    raw jsonb;
    embedded jsonb;
BEGIN
    FOR row IN
        SELECT session_id, id, item, provider_replay
        FROM transcript_entries
        WHERE item->>'type' = 'assistant_message'
          AND jsonb_typeof(item->'items') = 'array'
          AND EXISTS (
              SELECT 1 FROM jsonb_array_elements(item->'items') value
              WHERE value->>'type' = 'provider_replay_record'
          )
        ORDER BY session_id, sequence, id
    LOOP
        FOR element IN
            SELECT value FROM jsonb_array_elements(row.item->'items') value
            WHERE value->>'type' = 'provider_replay_record'
        LOOP
            raw := pg_temp.nc_try_jsonb(element->>'raw_json');
            IF jsonb_typeof(element) IS DISTINCT FROM 'object'
               OR pg_temp.nc_canonical_provider(element->>'provider') IS NULL
               OR jsonb_typeof(element->'record_type') IS DISTINCT FROM 'string'
               OR jsonb_typeof(element->'raw_json') IS DISTINCT FROM 'string'
               OR jsonb_typeof(raw) IS DISTINCT FROM 'object'
               OR coalesce(raw->>'type', '') = ''
               OR raw->>'type' <> element->>'record_type' THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version', 'native_compaction_v1',
                    'category', 'malformed_embedded_provider_replay',
                    'row_id', row.session_id || ':' || row.id
                )::text;
            END IF;
        END LOOP;
        embedded := pg_temp.nc_embedded_replay(row.item);
        IF jsonb_array_length(row.provider_replay) > 0
           AND (
               jsonb_array_length(row.provider_replay) < jsonb_array_length(embedded)
               OR pg_temp.nc_jsonb_suffix(
                   row.provider_replay, jsonb_array_length(embedded)
               ) <> embedded
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'ambiguous_embedded_replay_lift',
                'row_id', row.session_id || ':' || row.id
            )::text;
        END IF;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'embedded_replay_lifted', 'transcript_entries', session_id || ':' || id, session_id
FROM transcript_entries
WHERE item->>'type' = 'assistant_message'
  AND jsonb_typeof(item->'items') = 'array'
  AND EXISTS (
      SELECT 1 FROM jsonb_array_elements(item->'items') value
      WHERE value->>'type' = 'provider_replay_record'
  );

INSERT INTO _nc_categories
SELECT 'redacted_placeholder_removed', 'transcript_entries', session_id || ':' || id, session_id
FROM transcript_entries
WHERE item->>'type' = 'assistant_message'
  AND jsonb_typeof(item->'items') = 'array'
  AND EXISTS (
      SELECT 1 FROM jsonb_array_elements(item->'items') value
      WHERE value->>'type' = 'thinking_redacted'
  );

WITH split AS (
    SELECT t.session_id, t.id,
           pg_temp.nc_embedded_replay(t.item) AS embedded,
           coalesce(jsonb_agg(element ORDER BY ord) FILTER (
               WHERE element->>'type' NOT IN
                   ('provider_replay_record', 'thinking_redacted')
           ), '[]'::jsonb) AS visible
    FROM transcript_entries t
    CROSS JOIN LATERAL jsonb_array_elements(t.item->'items')
        WITH ORDINALITY AS item_element(element, ord)
    WHERE t.item->>'type' = 'assistant_message'
      AND jsonb_typeof(t.item->'items') = 'array'
      AND EXISTS (
          SELECT 1 FROM jsonb_array_elements(t.item->'items') value
          WHERE value->>'type' IN ('provider_replay_record','thinking_redacted')
      )
    GROUP BY t.session_id, t.id, t.item
)
UPDATE transcript_entries t
SET provider_replay = CASE
        WHEN jsonb_array_length(t.provider_replay) = 0
        THEN t.provider_replay || split.embedded
        ELSE t.provider_replay
    END,
    item = jsonb_set(t.item, '{items}', split.visible, false)
FROM split
WHERE t.session_id = split.session_id AND t.id = split.id;

-- The old injected item has one unambiguous ordinary semantic representation.
DO $$
DECLARE
    row record;
BEGIN
    FOR row IN
        SELECT session_id, id, item FROM transcript_entries
        WHERE item->>'type' = 'injected'
    LOOP
        IF jsonb_typeof(row.item->'content') IS DISTINCT FROM 'string'
           OR jsonb_typeof(row.item->'kind') IS DISTINCT FROM 'string'
           OR jsonb_typeof(row.item->'metadata') IS DISTINCT FROM 'object' THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'malformed_injected_item',
                'row_id', row.session_id || ':' || row.id
            )::text;
        END IF;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'injected_item_converted', 'transcript_entries', session_id || ':' || id, session_id
FROM transcript_entries WHERE item->>'type' = 'injected';

UPDATE transcript_entries
SET item = jsonb_build_object(
        'type', 'user_message',
        'content', jsonb_build_array(jsonb_build_object(
            'type', 'text', 'text', item->>'content'
        )),
        'native_compaction_v1_source', jsonb_build_object(
            'legacy_kind', item->'kind',
            'legacy_metadata', item->'metadata'
        )
    ),
    turn_id = NULL
WHERE item->>'type' = 'injected';

-- Physical/logical graph validation is shared by preflight and final verify.
CREATE OR REPLACE FUNCTION pg_temp.nc_assert_topology()
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    issue jsonb;
BEGIN
    SELECT jsonb_build_object(
        'category', 'duplicate_transcript_sequence',
        'sequence', sequence,
        'row_ids', jsonb_agg(session_id || ':' || id ORDER BY session_id, id)
    ) INTO issue
    FROM transcript_entries
    GROUP BY sequence
    HAVING count(*) > 1
    ORDER BY sequence
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category', 'invalid_physical_topology',
        'row_id', child.session_id || ':' || child.id,
        'parent_id', child.parent_id
    ) INTO issue
    FROM transcript_entries child
    LEFT JOIN transcript_entries parent
      ON parent.session_id = child.session_id AND parent.id = child.parent_id
    WHERE child.parent_id IS NOT NULL
      AND (parent.id IS NULL OR parent.sequence >= child.sequence)
    ORDER BY child.session_id, child.sequence, child.id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    WITH RECURSIVE walk AS (
        SELECT session_id, id AS start_id, id AS current_id,
               ARRAY[id]::text[] AS visited, false AS cycle
        FROM transcript_entries
        UNION ALL
        SELECT walk.session_id, walk.start_id, parent.id,
               walk.visited || parent.id,
               parent.id = ANY(walk.visited)
        FROM walk
        JOIN transcript_entries current
          ON current.session_id = walk.session_id AND current.id = walk.current_id
        JOIN transcript_entries parent
          ON parent.session_id = current.session_id AND parent.id = current.parent_id
        WHERE current.parent_id IS NOT NULL AND NOT walk.cycle
    )
    SELECT jsonb_build_object(
        'category', 'physical_transcript_cycle',
        'row_id', session_id || ':' || start_id
    ) INTO issue
    FROM walk WHERE cycle
    ORDER BY session_id, start_id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category', 'invalid_logical_predecessor',
        'row_id', child.session_id || ':' || child.id,
        'predecessor_id', child.item->>'source_leaf_id'
    ) INTO issue
    FROM transcript_entries child
    LEFT JOIN transcript_entries predecessor
      ON predecessor.session_id = child.session_id
     AND predecessor.id = child.item->>'source_leaf_id'
    WHERE child.item->>'type' = 'compaction_summary'
      AND (
          child.item->>'source_session_id' IS DISTINCT FROM child.session_id
          OR coalesce(child.item->>'source_leaf_id','') = ''
          OR predecessor.id IS NULL
      )
    ORDER BY child.session_id, child.sequence, child.id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    WITH RECURSIVE logical_walk AS (
        SELECT session_id, id AS start_id, id AS current_id,
               ARRAY[id]::text[] AS visited, false AS cycle
        FROM transcript_entries
        UNION ALL
        SELECT walk.session_id, walk.start_id, predecessor.id,
               walk.visited || predecessor.id,
               predecessor.id = ANY(walk.visited)
        FROM logical_walk walk
        JOIN transcript_entries current
          ON current.session_id = walk.session_id AND current.id = walk.current_id
        JOIN transcript_entries predecessor
          ON predecessor.session_id = current.session_id
         AND predecessor.id = CASE
             WHEN current.item->>'type' = 'compaction_summary'
             THEN current.item->>'source_leaf_id'
             ELSE current.parent_id
         END
        WHERE CASE
            WHEN current.item->>'type' = 'compaction_summary'
            THEN current.item->>'source_leaf_id'
            ELSE current.parent_id
        END IS NOT NULL
          AND NOT walk.cycle
    )
    SELECT jsonb_build_object(
        'category', 'logical_transcript_cycle',
        'row_id', session_id || ':' || start_id
    ) INTO issue
    FROM logical_walk WHERE cycle
    ORDER BY session_id, start_id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category', 'invalid_logical_predecessor_order',
        'row_id', child.session_id || ':' || child.id,
        'predecessor_id', predecessor.id
    ) INTO issue
    FROM transcript_entries child
    JOIN transcript_entries predecessor
      ON predecessor.session_id = child.session_id
     AND predecessor.id = child.item->>'source_leaf_id'
    WHERE child.item->>'type' = 'compaction_summary'
      AND predecessor.sequence >= child.sequence
    ORDER BY child.session_id, child.sequence, child.id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category', 'missing_active_leaf',
        'session_id', s.id,
        'active_leaf_id', s.active_leaf_id
    ) INTO issue
    FROM sessions s
    LEFT JOIN transcript_entries t
      ON t.session_id = s.id AND t.id = s.active_leaf_id
    WHERE (
        s.active_leaf_id IS NOT NULL
        AND t.id IS NULL
    ) OR (
        s.active_leaf_id IS NULL
        AND EXISTS (
            SELECT 1 FROM transcript_entries history
            WHERE history.session_id = s.id
        )
    )
    ORDER BY s.id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category', 'active_leaf_not_terminal',
        'session_id', s.id,
        'active_leaf_id', s.active_leaf_id
    ) INTO issue
    FROM sessions s
    WHERE s.active_leaf_id IS NOT NULL
      AND EXISTS (
          SELECT 1
          FROM transcript_entries child
          WHERE child.session_id = s.id
            AND child.parent_id = s.active_leaf_id
      )
    ORDER BY s.id
    LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

END
$$;

DO $$ BEGIN PERFORM pg_temp.nc_assert_topology(); END $$;

CREATE OR REPLACE FUNCTION pg_temp.nc_distance(
    target_session text, ancestor text, leaf text
) RETURNS integer LANGUAGE plpgsql STABLE AS $$
DECLARE
    current_id text := leaf;
    parent text;
    distance integer := 0;
    visited text[] := ARRAY[]::text[];
BEGIN
    LOOP
        IF current_id = ancestor THEN
            RETURN distance;
        END IF;
        IF current_id IS NULL OR current_id = ANY(visited) THEN
            RETURN NULL;
        END IF;
        visited := visited || current_id;
        SELECT parent_id INTO parent
        FROM transcript_entries
        WHERE session_id = target_session AND id = current_id;
        IF NOT FOUND THEN
            RETURN NULL;
        END IF;
        current_id := parent;
        distance := distance + 1;
    END LOOP;
END
$$;

-- Completed compaction records are topology facts and must agree before any
-- checkpoint rewrite.
DO $$
DECLARE
    row record;
    root_id text;
    leaf_id text;
    source_id text;
    expected_count integer;
    action_row record;
BEGIN
    FOR row IN
        SELECT id, session_id, result
        FROM actions
        WHERE kind = 'compaction' AND status = 'completed'
        ORDER BY id
    LOOP
        root_id := row.result->>'new_root_id';
        leaf_id := row.result->>'active_leaf_id';
        source_id := row.result->>'source_leaf_id';
        expected_count := pg_temp.nc_distance(row.session_id, root_id, leaf_id);
        IF jsonb_typeof(row.result) IS DISTINCT FROM 'object'
           OR coalesce(root_id,'') = '' OR coalesce(leaf_id,'') = ''
           OR row.result->>'source_session_id' IS DISTINCT FROM row.session_id
           OR NOT EXISTS (
               SELECT 1 FROM transcript_entries
               WHERE session_id = row.session_id AND id = root_id AND parent_id IS NULL
           )
           OR expected_count IS NULL
           OR NOT EXISTS (
               SELECT 1 FROM transcript_entries
               WHERE session_id = row.session_id AND id = source_id
           )
           OR (
               row.result ? 'continuation_suffix_items'
               AND (
                   jsonb_typeof(row.result->'continuation_suffix_items')
                       IS DISTINCT FROM 'number'
                   OR (row.result->>'continuation_suffix_items')::integer <> expected_count
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'invalid_completed_compaction_reference',
                'action_id', row.id
            )::text;
        END IF;
    END LOOP;

    FOR row IN
        SELECT id, session_id, type, payload
        FROM events
        WHERE type IN ('history.compacted','compaction.completed')
        ORDER BY id
    LOOP
        root_id := row.payload->>'new_root_id';
        leaf_id := row.payload->>'active_leaf_id';
        IF coalesce(root_id,'') = '' OR coalesce(leaf_id,'') = ''
           OR NOT EXISTS (
               SELECT 1 FROM transcript_entries
               WHERE session_id = row.session_id AND id = root_id AND parent_id IS NULL
           )
           OR pg_temp.nc_distance(row.session_id, root_id, leaf_id) IS NULL THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'invalid_completed_compaction_reference',
                'event_id', row.id
            )::text;
        END IF;
        IF row.type = 'history.compacted'
           AND (
               row.payload->>'source_session_id' IS DISTINCT FROM row.session_id
               OR NOT EXISTS (
                   SELECT 1 FROM transcript_entries
                   WHERE session_id = row.session_id
                     AND id = row.payload->>'source_leaf_id'
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version', 'native_compaction_v1',
                'category', 'invalid_completed_compaction_source',
                'event_id', row.id
            )::text;
        END IF;
        IF row.type = 'compaction.completed' THEN
            SELECT id, session_id, result INTO action_row
            FROM actions
            WHERE id = row.payload->>'action_row_id'
              AND session_id = row.session_id
              AND kind = 'compaction'
              AND status = 'completed';
            IF NOT FOUND
               OR action_row.result->>'new_root_id' IS DISTINCT FROM root_id
               OR action_row.result->>'active_leaf_id' IS DISTINCT FROM leaf_id THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version', 'native_compaction_v1',
                    'category', 'invalid_compaction_event_action_link',
                    'event_id', row.id
                )::text;
            END IF;
        END IF;
    END LOOP;
END
$$;

-- Topology-proven root -> leaf auto-state derivation.
CREATE TEMP TABLE _nc_auto_candidates (
    session_id text NOT NULL,
    root_id text NOT NULL,
    leaf_id text NOT NULL,
    PRIMARY KEY (session_id, root_id, leaf_id)
) ON COMMIT DROP;

INSERT INTO _nc_auto_candidates
SELECT session_id, result->>'new_root_id', result->>'active_leaf_id'
FROM actions
WHERE kind = 'compaction' AND status = 'completed'
  AND result ? 'new_root_id' AND result ? 'active_leaf_id'
ON CONFLICT DO NOTHING;

INSERT INTO _nc_auto_candidates
SELECT session_id, payload->>'new_root_id', payload->>'active_leaf_id'
FROM events
WHERE type IN ('history.compacted','compaction.completed')
  AND payload ? 'new_root_id' AND payload ? 'active_leaf_id'
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE _nc_auto_plan (
    session_id text PRIMARY KEY,
    root_id text NOT NULL,
    leaf_id text NOT NULL
) ON COMMIT DROP;

DO $$
DECLARE
    row record;
    leaves text[];
    derived text;
    existing text;
BEGIN
    FOR row IN
        SELECT id, metadata#>>'{compaction,auto_state,last_success_root_id}' AS root_id
        FROM sessions
        WHERE metadata#>'{compaction,auto_state,last_success_root_id}' IS NOT NULL
        ORDER BY id
    LOOP
        IF jsonb_typeof((
            SELECT metadata#>'{compaction,auto_state,last_success_root_id}'
            FROM sessions WHERE id = row.id
        )) NOT IN ('string','null') THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_compaction_auto_state',
                'session_id',row.id
            )::text;
        END IF;
        IF row.root_id IS NULL THEN
            CONTINUE;
        END IF;
        IF NOT EXISTS (
            SELECT 1 FROM transcript_entries
            WHERE session_id = row.id AND id = row.root_id
        ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','ambiguous_last_success_root',
                'session_id',row.id,
                'detail','root does not exist'
            )::text;
        END IF;
        SELECT array_agg(leaf_id ORDER BY leaf_id) INTO leaves
        FROM _nc_auto_candidates
        WHERE session_id = row.id AND root_id = row.root_id;
        IF coalesce(cardinality(leaves),0) = 0 THEN
            IF EXISTS (
                SELECT 1 FROM transcript_entries
                WHERE session_id = row.id AND parent_id = row.root_id
            ) THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version','native_compaction_v1',
                    'category','ambiguous_last_success_root',
                    'session_id',row.id,
                    'detail','checkpoint has descendants but no completed leaf fence'
                )::text;
            END IF;
            derived := row.root_id;
        ELSIF cardinality(leaves) = 1 THEN
            derived := leaves[1];
        ELSE
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','ambiguous_last_success_root',
                'session_id',row.id,
                'leaves',to_jsonb(leaves)
            )::text;
        END IF;
        IF pg_temp.nc_distance(row.id, row.root_id, derived) IS NULL THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','ambiguous_last_success_root',
                'session_id',row.id,
                'detail','derived leaf is not a descendant'
            )::text;
        END IF;
        SELECT metadata#>>'{compaction,auto_state,last_success_leaf_id}'
        INTO existing FROM sessions WHERE id = row.id;
        IF existing IS NOT NULL AND existing <> derived THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','conflicting_compaction_auto_state',
                'session_id',row.id
            )::text;
        END IF;
        INSERT INTO _nc_auto_plan VALUES (row.id, row.root_id, derived);
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'auto_state_root_derived', 'sessions', session_id, session_id
FROM _nc_auto_plan;

UPDATE sessions s
SET metadata = jsonb_set(
    s.metadata,
    '{compaction,auto_state}',
    ((s.metadata#>'{compaction,auto_state}') - 'last_success_root_id'::text)
      || jsonb_build_object('last_success_leaf_id', plan.leaf_id),
    false
)
FROM _nc_auto_plan plan
WHERE s.id = plan.session_id;

-- Current replay predicates for checkpoint summaries. Ordinary replay is also
-- checked below using its adapter-owned outer taxonomies.
CREATE OR REPLACE FUNCTION pg_temp.nc_valid_summary_replay(
    provider text, replay jsonb
) RETURNS boolean LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT CASE provider
        WHEN 'openai' THEN CASE
            WHEN jsonb_typeof(replay) IS DISTINCT FROM 'array' THEN false
            ELSE jsonb_array_length(replay) > 0
              AND NOT EXISTS (
                  SELECT 1 FROM jsonb_array_elements(replay) entry
                  WHERE entry->>'provider' <> 'openai'
                     OR jsonb_typeof(pg_temp.nc_try_jsonb(entry->>'raw_json'))
                         IS DISTINCT FROM 'object'
                     OR coalesce(pg_temp.nc_try_jsonb(entry->>'raw_json')->>'type','') = ''
              )
              AND (
                  SELECT count(*)
                  FROM jsonb_array_elements(replay) entry
                  WHERE pg_temp.nc_try_jsonb(entry->>'raw_json')->>'type'
                      IN ('compaction','compaction_summary')
              ) = 1
        END
        WHEN 'claude' THEN CASE
            WHEN jsonb_typeof(replay) IS DISTINCT FROM 'array' THEN false
            ELSE jsonb_array_length(replay) = 1
              AND replay->0->>'provider' = 'claude'
              AND pg_temp.nc_try_jsonb(replay->0->>'raw_json')->>'type' = 'compaction'
              AND jsonb_typeof(pg_temp.nc_try_jsonb(replay->0->>'raw_json')->'content') = 'string'
              AND coalesce(pg_temp.nc_try_jsonb(replay->0->>'raw_json')->>'content','') <> ''
              AND (
                  NOT (pg_temp.nc_try_jsonb(replay->0->>'raw_json') ? 'encrypted_content')
                  OR pg_temp.nc_try_jsonb(replay->0->>'raw_json')->'encrypted_content' = 'null'::jsonb
                  OR jsonb_typeof(
                      pg_temp.nc_try_jsonb(replay->0->>'raw_json')->'encrypted_content'
                  ) = 'string'
              )
        END
        ELSE false
    END
$$;

DO $$
DECLARE
    row record;
BEGIN
    FOR row IN
        SELECT t.session_id, t.id, t.item, t.turn_id
        FROM transcript_entries t
        WHERE t.item->>'type' = 'compaction_summary'
        ORDER BY t.session_id, t.sequence, t.id
    LOOP
        IF jsonb_typeof(row.item) IS DISTINCT FROM 'object'
           OR row.item->>'source_session_id' IS DISTINCT FROM row.session_id
           OR coalesce(row.item->>'source_leaf_id','') = ''
           OR jsonb_typeof(row.item->'summary') IS DISTINCT FROM 'string'
           OR btrim(row.item->>'summary') = ''
           OR jsonb_typeof(row.item->'last_turn_id') IS DISTINCT FROM 'number'
           OR (row.item->>'last_turn_id') !~ '^[0-9]+$'
           OR row.turn_id IS DISTINCT FROM (row.item->>'last_turn_id')::bigint
           OR NOT row.item ? 'tokens_before'
           OR (
               row.item->'tokens_before' <> 'null'::jsonb
               AND (
                   jsonb_typeof(row.item->'tokens_before') IS DISTINCT FROM 'number'
                   OR (row.item->>'tokens_before') !~ '^[0-9]+$'
               )
           )
           OR (
               row.item ? 'turn_started_at_ms'
               AND row.item->'turn_started_at_ms' <> 'null'::jsonb
               AND (
                   jsonb_typeof(row.item->'turn_started_at_ms') IS DISTINCT FROM 'number'
                   OR (row.item->>'turn_started_at_ms') !~ '^[0-9]+$'
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','unsafe_compaction_summary',
                'row_id',row.session_id || ':' || row.id,
                'detail','summary semantic fields are malformed'
            )::text;
        END IF;
    END LOOP;
END
$$;

CREATE TEMP TABLE _nc_bootstrap (
    session_id text NOT NULL,
    old_root_id text NOT NULL,
    start_id text NOT NULL,
    semantic_id text NOT NULL,
    source_leaf_id text NOT NULL,
    summary_text text NOT NULL,
    tokens_before jsonb NOT NULL,
    last_turn_id bigint NOT NULL,
    start_timestamp_ms bigint NOT NULL,
    old_timestamp_ms bigint NOT NULL,
    old_sequence bigint NOT NULL,
    original_item jsonb NOT NULL,
    PRIMARY KEY (session_id, old_root_id)
) ON COMMIT DROP;

INSERT INTO _nc_bootstrap
SELECT t.session_id,
       t.id,
       'entry_native_compaction_v1_' ||
           md5('native_compaction_v1' || chr(31) || t.session_id || chr(31) || t.id || chr(31) || 'turn_started'),
       'entry_native_compaction_v1_' ||
           md5('native_compaction_v1' || chr(31) || t.session_id || chr(31) || t.id || chr(31) || 'semantic_summary'),
       t.item->>'source_leaf_id',
       t.item->>'summary',
       t.item->'tokens_before',
       (t.item->>'last_turn_id')::bigint,
       coalesce((t.item->>'turn_started_at_ms')::bigint, t.timestamp_ms),
       t.timestamp_ms,
       t.sequence,
       t.item
FROM transcript_entries t
JOIN sessions s ON s.id = t.session_id
WHERE t.item->>'type' = 'compaction_summary'
  AND NOT pg_temp.nc_valid_summary_replay(
      s.provider_config->>'kind', t.provider_replay
  );

INSERT INTO _nc_categories
SELECT 'valid_native_summary', 'transcript_entries', t.session_id || ':' || t.id, t.session_id
FROM transcript_entries t
JOIN sessions s ON s.id = t.session_id
WHERE t.item->>'type' = 'compaction_summary'
  AND pg_temp.nc_valid_summary_replay(s.provider_config->>'kind', t.provider_replay);

DO $$
DECLARE
    row record;
    child record;
BEGIN
    IF EXISTS (SELECT 1 FROM _nc_bootstrap)
       AND EXISTS (
           SELECT 1 FROM transcript_entries
           WHERE sequence < 0
              OR sequence > (9223372036854775807::numeric - 2) / 4
       ) THEN
        RAISE EXCEPTION USING MESSAGE =
            '{"migration_version":"native_compaction_v1","category":"unsafe_transcript_sequence","detail":"sequence spacing would overflow bigint"}';
    END IF;

    FOR row IN SELECT * FROM _nc_bootstrap ORDER BY session_id, old_sequence LOOP
        IF NOT EXISTS (
            SELECT 1 FROM transcript_entries
            WHERE session_id = row.session_id
              AND id = row.old_root_id
              AND parent_id IS NULL
        ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','unsafe_compaction_summary',
                'row_id',row.session_id || ':' || row.old_root_id,
                'detail','legacy summary is not a physical root'
            )::text;
        END IF;
        IF EXISTS (
            SELECT 1 FROM transcript_entries
            WHERE session_id = row.session_id
              AND id IN (row.start_id, row.semantic_id)
        ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','deterministic_id_collision',
                'row_id',row.session_id || ':' || row.old_root_id
            )::text;
        END IF;
        FOR child IN
            SELECT id, item
            FROM transcript_entries
            WHERE session_id = row.session_id AND parent_id = row.old_root_id
            ORDER BY sequence, id
        LOOP
            IF child.item->>'type' = 'turn_started' THEN
                IF jsonb_typeof(child.item->'turn_id') IS DISTINCT FROM 'number'
                   OR child.item->>'turn_id' !~ '^[0-9]+$'
                   OR (child.item->>'turn_id')::bigint <> row.last_turn_id + 1 THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version','native_compaction_v1',
                        'category','ambiguous_compaction_topology',
                        'row_id',row.session_id || ':' || row.old_root_id,
                        'child_id',child.id,
                        'detail','same-turn or unexpected TurnStarted child must be remediated with the old release'
                    )::text;
                END IF;
            ELSIF child.item->>'type' <> 'user_message' THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version','native_compaction_v1',
                    'category','ambiguous_compaction_topology',
                    'row_id',row.session_id || ':' || row.old_root_id,
                    'child_id',child.id,
                    'detail','mid-turn suffix is not an ordinary user message'
                )::text;
            END IF;
        END LOOP;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'semantic_summary_bootstrap', 'transcript_entries',
       session_id || ':' || old_root_id, session_id
FROM _nc_bootstrap;

INSERT INTO _nc_categories
SELECT 'transcript_sequence_spaced', 'transcript_entries',
       session_id || ':' || id, session_id
FROM transcript_entries
WHERE EXISTS (SELECT 1 FROM _nc_bootstrap);

UPDATE transcript_entries
SET sequence = sequence * 4 + 2
WHERE EXISTS (SELECT 1 FROM _nc_bootstrap);

INSERT INTO transcript_entries (
    session_id, id, parent_id, timestamp_ms, item,
    provider_replay, turn_id, sequence
)
SELECT session_id, start_id, NULL, start_timestamp_ms,
       jsonb_build_object('type','turn_started','turn_id',last_turn_id),
       '[]'::jsonb, last_turn_id, old_sequence * 4
FROM _nc_bootstrap
UNION ALL
SELECT session_id, semantic_id, start_id, old_timestamp_ms,
       jsonb_build_object(
           'type','user_message',
           'content',jsonb_build_array(jsonb_build_object(
               'type','text',
               'text','The conversation history before this point was compacted into this summary:' ||
                      E'\n\n' || summary_text
           )),
           'native_compaction_v1_source',jsonb_build_object(
               'source_session_id',session_id,
               'source_leaf_id',source_leaf_id,
               'tokens_before',tokens_before
           ),
           'native_compaction_v1_original_summary',original_item
       ),
       '[]'::jsonb, NULL, old_sequence * 4 + 1
FROM _nc_bootstrap;

INSERT INTO _nc_categories
SELECT 'semantic_summary_row_inserted', 'transcript_entries',
       session_id || ':' || start_id, session_id
FROM _nc_bootstrap
UNION ALL
SELECT 'semantic_summary_row_inserted', 'transcript_entries',
       session_id || ':' || semantic_id, session_id
FROM _nc_bootstrap;

UPDATE transcript_entries t
SET parent_id = bootstrap.semantic_id,
    item = jsonb_build_object(
        'type','turn_finished',
        'turn_id',bootstrap.last_turn_id,
        'outcome','Graceful'
    ),
    provider_replay = '[]'::jsonb,
    turn_id = bootstrap.last_turn_id
FROM _nc_bootstrap bootstrap
WHERE t.session_id = bootstrap.session_id
  AND t.id = bootstrap.old_root_id;

INSERT INTO _nc_categories
SELECT 'mid_turn_suffix_reparented', 'transcript_entries',
       child.session_id || ':' || child.id, child.session_id
FROM transcript_entries child
JOIN _nc_bootstrap bootstrap
  ON bootstrap.session_id = child.session_id
 AND bootstrap.old_root_id = child.parent_id
WHERE child.item->>'type' = 'user_message';

UPDATE transcript_entries child
SET parent_id = bootstrap.semantic_id
FROM _nc_bootstrap bootstrap
WHERE child.session_id = bootstrap.session_id
  AND child.parent_id = bootstrap.old_root_id
  AND child.item->>'type' = 'user_message';

-- Rewrite every completed topology fact that named a bootstrapped root.
INSERT INTO _nc_categories
SELECT 'completed_action_reference_rewritten', 'actions', action.id, action.session_id
FROM actions action
JOIN _nc_bootstrap bootstrap
  ON bootstrap.session_id = action.session_id
 AND action.result->>'new_root_id' = bootstrap.old_root_id
WHERE action.kind = 'compaction' AND action.status = 'completed';

UPDATE actions action
SET result = (
    action.result - 'remote' - 'execution_mode' - 'provider'
) || jsonb_build_object(
    'new_root_id', bootstrap.start_id,
    'active_leaf_id', action.result->>'active_leaf_id',
    'source_session_id', bootstrap.session_id,
    'source_leaf_id', bootstrap.source_leaf_id,
    'summary_kind', 'generic',
    'provider_replay_items', 0,
    'continuation_suffix_items', pg_temp.nc_distance(
        action.session_id, bootstrap.start_id, action.result->>'active_leaf_id'
    )
)
FROM _nc_bootstrap bootstrap
WHERE action.session_id = bootstrap.session_id
  AND action.kind = 'compaction'
  AND action.status = 'completed'
  AND action.result->>'new_root_id' = bootstrap.old_root_id;

INSERT INTO _nc_categories
SELECT 'completed_event_reference_rewritten', 'events', event.id::text, event.session_id
FROM events event
JOIN _nc_bootstrap bootstrap
  ON bootstrap.session_id = event.session_id
 AND event.payload->>'new_root_id' = bootstrap.old_root_id
WHERE event.type IN ('history.compacted','compaction.completed');

UPDATE events event
SET payload = (
    event.payload - 'remote' - 'execution_mode' - 'provider'
) || jsonb_build_object(
    'new_root_id', bootstrap.start_id,
    'active_leaf_id', event.payload->>'active_leaf_id',
    'source_session_id', bootstrap.session_id,
    'source_leaf_id', bootstrap.source_leaf_id,
    'summary_kind', 'generic'
)
FROM _nc_bootstrap bootstrap
WHERE event.session_id = bootstrap.session_id
  AND event.type IN ('history.compacted','compaction.completed')
  AND event.payload->>'new_root_id' = bootstrap.old_root_id;

-- Remove obsolete provenance. Historical local rows deliberately lose their
-- provider selector so the approved UI falls back to generic compaction text.
DO $$
DECLARE
    row record;
BEGIN
    FOR row IN
        SELECT 'actions' AS table_name, id AS row_id, result AS value
        FROM actions
        WHERE kind = 'compaction' AND status = 'completed'
          AND (result ? 'remote' OR result ? 'execution_mode')
        UNION ALL
        SELECT 'events', id::text, payload
        FROM events
        WHERE type IN ('history.compacted','compaction.completed')
          AND (payload ? 'remote' OR payload ? 'execution_mode')
    LOOP
        IF (row.value ? 'remote'
            AND jsonb_typeof(row.value->'remote') IS DISTINCT FROM 'boolean')
           OR (
               row.value ? 'execution_mode'
               AND row.value->>'execution_mode' NOT IN ('local','provider_native')
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_execution_provenance',
                'table',row.table_name,
                'row_id',row.row_id
            )::text;
        END IF;
    END LOOP;
END
$$;

INSERT INTO _nc_categories
SELECT 'obsolete_provenance_removed', 'actions', id, session_id
FROM actions
WHERE kind = 'compaction' AND status = 'completed'
  AND (result ? 'remote' OR result ? 'execution_mode')
UNION ALL
SELECT 'obsolete_provenance_removed', 'events', id::text, session_id
FROM events
WHERE type IN ('history.compacted','compaction.completed')
  AND (payload ? 'remote' OR payload ? 'execution_mode')
ON CONFLICT DO NOTHING;

UPDATE actions
SET result = CASE
    WHEN result->'remote' = 'false'::jsonb OR result->>'execution_mode' = 'local'
    THEN result - 'remote' - 'execution_mode' - 'provider'
    ELSE result - 'remote' - 'execution_mode'
END
WHERE kind = 'compaction' AND status = 'completed'
  AND (result ? 'remote' OR result ? 'execution_mode');

UPDATE events
SET payload = CASE
    WHEN payload->'remote' = 'false'::jsonb OR payload->>'execution_mode' = 'local'
    THEN payload - 'remote' - 'execution_mode' - 'provider'
    ELSE payload - 'remote' - 'execution_mode'
END
WHERE type IN ('history.compacted','compaction.completed')
  AND (payload ? 'remote' OR payload ? 'execution_mode');

-- Structural item/replay verification.
DO $$
DECLARE
    row record;
    replay jsonb;
    raw jsonb;
    provider text;
    item_type text;
BEGIN
    FOR row IN
        SELECT t.*, s.provider_config->>'kind' AS session_provider
        FROM transcript_entries t
        JOIN sessions s ON s.id = t.session_id
        ORDER BY t.session_id, t.sequence, t.id
    LOOP
        item_type := row.item->>'type';
        IF jsonb_typeof(row.item) IS DISTINCT FROM 'object'
           OR coalesce(item_type, '') NOT IN (
               'turn_started','user_message','assistant_message',
               'tool_call_started','tool_result','turn_finished',
               'compaction_summary','daemon_tool_observation'
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','unsupported_transcript_item',
                'row_id',row.session_id || ':' || row.id,
                'item_type',item_type
            )::text;
        END IF;
        IF (item_type IN ('turn_started','tool_call_started','turn_finished')
            AND (
                jsonb_typeof(row.item->'turn_id') IS DISTINCT FROM 'number'
                OR row.item->>'turn_id' !~ '^[0-9]+$'
                OR row.turn_id IS DISTINCT FROM (row.item->>'turn_id')::bigint
            ))
           OR (item_type = 'compaction_summary'
               AND row.turn_id IS DISTINCT FROM (row.item->>'last_turn_id')::bigint)
           OR (item_type IN (
               'user_message','assistant_message','tool_result','daemon_tool_observation'
           ) AND row.turn_id IS NOT NULL) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','transcript_turn_id_mismatch',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;

        IF item_type = 'user_message'
           AND (
               jsonb_typeof(row.item->'content') IS DISTINCT FROM 'array'
               OR EXISTS (
                   SELECT 1 FROM jsonb_array_elements(row.item->'content') block
                   WHERE coalesce(block->>'type','') NOT IN ('text','image')
                      OR (
                          block->>'type' = 'text'
                          AND jsonb_typeof(block->'text') IS DISTINCT FROM 'string'
                      )
                      OR (
                          block->>'type' = 'image'
                          AND (
                              jsonb_typeof(block->'image') IS DISTINCT FROM 'object'
                              OR jsonb_typeof(block#>'{image,mime_type}')
                                  IS DISTINCT FROM 'string'
                              OR jsonb_typeof(block#>'{image,source}')
                                  IS DISTINCT FROM 'object'
                              OR coalesce(block#>>'{image,source,kind}','')
                                  NOT IN ('base64','url')
                              OR jsonb_typeof(block#>'{image,source,value}')
                                  IS DISTINCT FROM 'string'
                          )
                      )
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_user_message',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;
        IF item_type = 'assistant_message'
           AND (
               jsonb_typeof(row.item->'items') IS DISTINCT FROM 'array'
               OR EXISTS (
                   SELECT 1 FROM jsonb_array_elements(row.item->'items') part
                   WHERE coalesce(part->>'type','') NOT IN ('text','tool_call')
                      OR (part->>'type' = 'text'
                          AND jsonb_typeof(part->'text') IS DISTINCT FROM 'string')
                      OR (
                          part->>'type' = 'tool_call'
                          AND (
                              coalesce(part->>'id','') = ''
                              OR coalesce(part->>'tool_name','') = ''
                              OR jsonb_typeof(part->'args_json')
                                  IS DISTINCT FROM 'string'
                              OR pg_temp.nc_try_jsonb(part->>'args_json') IS NULL
                          )
                      )
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_assistant_message',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;
        IF item_type = 'tool_call_started'
           AND (
               jsonb_typeof(row.item->'tool_call') IS DISTINCT FROM 'object'
               OR coalesce(row.item#>>'{tool_call,id}','') = ''
               OR coalesce(row.item#>>'{tool_call,tool_name}','') = ''
               OR jsonb_typeof(row.item#>'{tool_call,args_json}')
                   IS DISTINCT FROM 'string'
               OR pg_temp.nc_try_jsonb(row.item#>>'{tool_call,args_json}') IS NULL
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_tool_call_started',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;
        IF item_type = 'tool_result'
           AND (
               coalesce(row.item->>'tool_call_id','') = ''
               OR coalesce(row.item->>'tool_name','') = ''
               OR jsonb_typeof(row.item->'output') IS DISTINCT FROM 'string'
               OR coalesce(row.item->>'status','') NOT IN (
                   'Success','Error','Interrupted','Crashed'
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_tool_result',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;
        IF item_type = 'turn_finished'
           AND coalesce(row.item->>'outcome','') NOT IN (
               'Graceful','Interrupted','Crashed'
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_turn_finished',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;
        IF item_type = 'daemon_tool_observation'
           AND (
               coalesce(row.item->>'tool_call_id','') = ''
               OR coalesce(row.item->>'tool_name','') = ''
               OR jsonb_typeof(row.item->'args_json') IS DISTINCT FROM 'string'
               OR pg_temp.nc_try_jsonb(row.item->>'args_json') IS NULL
               OR NOT row.item ? 'result_json'
               OR coalesce(row.item->>'status','') NOT IN (
                   'Success','Error','Interrupted','Crashed'
               )
               OR (
                   row.item ? 'summary'
                   AND row.item->'summary' <> 'null'::jsonb
                   AND jsonb_typeof(row.item->'summary') IS DISTINCT FROM 'string'
               )
           ) THEN
            RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                'migration_version','native_compaction_v1',
                'category','malformed_daemon_tool_observation',
                'row_id',row.session_id || ':' || row.id
            )::text;
        END IF;

        IF item_type = 'compaction_summary' THEN
            IF row.parent_id IS NOT NULL
               OR NOT pg_temp.nc_valid_summary_replay(
                   row.session_provider, row.provider_replay
               ) THEN
                RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                    'migration_version','native_compaction_v1',
                    'category','unresolved_compaction_summary',
                    'row_id',row.session_id || ':' || row.id
                )::text;
            END IF;
        ELSE
            FOR replay IN SELECT value FROM jsonb_array_elements(row.provider_replay) LOOP
                provider := replay->>'provider';
                raw := pg_temp.nc_try_jsonb(replay->>'raw_json');
                IF provider <> row.session_provider
                   OR jsonb_typeof(raw) IS DISTINCT FROM 'object'
                   OR coalesce(raw->>'type','') = '' THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version','native_compaction_v1',
                        'category','malformed_provider_replay',
                        'row_id',row.session_id || ':' || row.id
                    )::text;
                END IF;
                IF provider = 'openai'
                   AND (
                       raw->>'type' NOT IN (
                       'message','agent_message','function_call','custom_tool_call',
                       'reasoning','web_search_call','file_search_call',
                       'code_interpreter_call','image_generation_call','mcp_call',
                       'mcp_list_tools','tool_search_output','additional_tools',
                       'compaction','context_compaction','function_call_output',
                       'custom_tool_call_output','local_shell_call_output',
                       'shell_call_output','apply_patch_call_output',
                       'computer_call_output','mcp_approval_response'
                       )
                       AND NOT (
                           raw->>'type' = 'tool_search_call'
                           AND raw->>'execution' = 'server'
                       )
                   ) THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version','native_compaction_v1',
                        'category','unsupported_openai_replay_type',
                        'row_id',row.session_id || ':' || row.id,
                        'replay_type',raw->>'type'
                    )::text;
                END IF;
                IF provider = 'claude' AND raw->>'type' = 'compaction'
                   AND NOT (
                       jsonb_typeof(raw->'content') = 'string'
                       AND coalesce(raw->>'content','') <> ''
                       AND (
                           NOT raw ? 'encrypted_content'
                           OR raw->'encrypted_content' = 'null'::jsonb
                           OR jsonb_typeof(raw->'encrypted_content') = 'string'
                       )
                   ) THEN
                    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
                        'migration_version','native_compaction_v1',
                        'category','malformed_claude_compaction_replay',
                        'row_id',row.session_id || ':' || row.id
                    )::text;
                END IF;
            END LOOP;
        END IF;
    END LOOP;
END
$$;

-- Full-forest turn and tool grammar. CompactionSummary is either a boundary
-- before the next TurnStarted or an open-turn anchor before a user suffix.
CREATE OR REPLACE FUNCTION pg_temp.nc_assert_turn_grammar()
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    leaf record;
    entry record;
    open_turn bigint;
    from_summary boolean;
    pending jsonb;
    part jsonb;
    call_id text;
    call_name text;
BEGIN
    FOR leaf IN
        SELECT t.session_id, t.id
        FROM transcript_entries t
        WHERE NOT EXISTS (
            SELECT 1 FROM transcript_entries child
            WHERE child.session_id = t.session_id AND child.parent_id = t.id
        )
        ORDER BY t.session_id, t.sequence, t.id
    LOOP
        open_turn := NULL;
        from_summary := false;
        pending := '{}'::jsonb;
        FOR entry IN
            WITH RECURSIVE path AS (
                SELECT t.*, 0 AS depth
                FROM transcript_entries t
                WHERE t.session_id = leaf.session_id AND t.id = leaf.id
                UNION ALL
                SELECT parent.*, child.depth + 1
                FROM transcript_entries parent
                JOIN path child
                  ON parent.session_id = child.session_id
                 AND parent.id = child.parent_id
            )
            SELECT * FROM path ORDER BY depth DESC
        LOOP
            CASE entry.item->>'type'
            WHEN 'turn_started' THEN
                IF open_turn IS NOT NULL AND NOT (
                    from_summary
                    AND (entry.item->>'turn_id')::bigint = open_turn + 1
                ) THEN
                    RAISE EXCEPTION 'nested or out-of-order TurnStarted';
                END IF;
                open_turn := (entry.item->>'turn_id')::bigint;
                from_summary := false;
                pending := '{}'::jsonb;
            WHEN 'compaction_summary' THEN
                IF open_turn IS NOT NULL OR entry.parent_id IS NOT NULL THEN
                    RAISE EXCEPTION 'CompactionSummary is not a root boundary';
                END IF;
                open_turn := (entry.item->>'last_turn_id')::bigint;
                from_summary := true;
                pending := '{}'::jsonb;
            WHEN 'user_message', 'daemon_tool_observation' THEN
                IF open_turn IS NULL THEN
                    RAISE EXCEPTION 'ordinary input outside a turn';
                END IF;
                from_summary := false;
            WHEN 'assistant_message' THEN
                IF open_turn IS NULL THEN
                    RAISE EXCEPTION 'assistant message outside a turn';
                END IF;
                FOR part IN
                    SELECT value FROM jsonb_array_elements(entry.item->'items')
                    WHERE value->>'type' = 'tool_call'
                LOOP
                    call_id := part->>'id';
                    call_name := part->>'tool_name';
                    IF pending ? call_id THEN
                        RAISE EXCEPTION 'duplicate unresolved tool call %', call_id;
                    END IF;
                    pending := pending || jsonb_build_object(
                        call_id, jsonb_build_object('name',call_name,'started',false)
                    );
                END LOOP;
                from_summary := false;
            WHEN 'tool_call_started' THEN
                call_id := entry.item#>>'{tool_call,id}';
                call_name := entry.item#>>'{tool_call,tool_name}';
                IF open_turn IS NULL
                   OR (entry.item->>'turn_id')::bigint <> open_turn
                   OR NOT pending ? call_id
                   OR pending#>>ARRAY[call_id,'name'] IS DISTINCT FROM call_name
                   OR (pending#>>ARRAY[call_id,'started'])::boolean THEN
                    RAISE EXCEPTION 'tool start does not match assistant call';
                END IF;
                pending := jsonb_set(pending, ARRAY[call_id,'started'], 'true'::jsonb, false);
                from_summary := false;
            WHEN 'tool_result' THEN
                call_id := entry.item->>'tool_call_id';
                call_name := entry.item->>'tool_name';
                IF open_turn IS NULL
                   OR NOT pending ? call_id
                   OR pending#>>ARRAY[call_id,'name'] IS DISTINCT FROM call_name
                   OR NOT (pending#>>ARRAY[call_id,'started'])::boolean THEN
                    RAISE EXCEPTION 'tool result does not match started call';
                END IF;
                pending := pending - call_id;
                from_summary := false;
            WHEN 'turn_finished' THEN
                IF open_turn IS NULL
                   OR (entry.item->>'turn_id')::bigint <> open_turn
                   OR pending <> '{}'::jsonb THEN
                    RAISE EXCEPTION 'TurnFinished has wrong turn or unfinished tools';
                END IF;
                open_turn := NULL;
                from_summary := false;
            ELSE
                RAISE EXCEPTION 'unknown transcript item';
            END CASE;
        END LOOP;
        IF open_turn IS NOT NULL AND NOT from_summary THEN
            RAISE EXCEPTION 'non-terminal transcript leaf without live work';
        END IF;
    END LOOP;
EXCEPTION WHEN raise_exception THEN
    RAISE EXCEPTION USING MESSAGE = jsonb_build_object(
        'migration_version','native_compaction_v1',
        'category','invalid_turn_grammar',
        'session_id',leaf.session_id,
        'leaf_id',leaf.id,
        'detail',SQLERRM
    )::text;
END
$$;

DO $$
BEGIN
    PERFORM pg_temp.nc_assert_topology();
    PERFORM pg_temp.nc_assert_turn_grammar();
END
$$;

-- Final current-shape invariants.
DO $$
DECLARE
    issue jsonb;
BEGIN
    SELECT jsonb_build_object(
        'category','unmigrated_provider_alias',
        'session_id',id
    ) INTO issue
    FROM sessions
    WHERE provider_config->>'kind' IN ('anthropic','codex')
    ORDER BY id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category','unmigrated_replay_alias',
        'row_id',t.session_id || ':' || t.id
    ) INTO issue
    FROM transcript_entries t
    WHERE EXISTS (
        SELECT 1 FROM jsonb_array_elements(t.provider_replay) replay
        WHERE replay->>'provider' IN ('anthropic','codex')
    )
    ORDER BY t.session_id, t.sequence, t.id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category','unmigrated_compaction_policy',
        'session_id',id
    ) INTO issue
    FROM sessions
    WHERE EXISTS (
        SELECT 1 FROM unnest(ARRAY[
            'auto_enabled','context_window','auto_limit_tokens',
            'max_consecutive_failures','remote_mode','anthropic_native_compaction'
        ]) field
        WHERE metadata#>ARRAY['compaction',field] IS NOT NULL
    )
       OR metadata#>'{compaction,config,remote_mode}' IS NOT NULL
       OR metadata#>'{compaction,config,anthropic_native_compaction}' IS NOT NULL
       OR metadata#>'{compaction,auto_state,last_success_root_id}' IS NOT NULL
    ORDER BY id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category','stale_completed_reference',
        'action_id',a.id
    ) INTO issue
    FROM actions a
    WHERE a.kind = 'compaction' AND a.status = 'completed'
      AND (
          NOT EXISTS (
              SELECT 1 FROM transcript_entries t
              WHERE t.session_id = a.session_id
                AND t.id = a.result->>'new_root_id'
                AND t.parent_id IS NULL
          )
          OR pg_temp.nc_distance(
              a.session_id,
              a.result->>'new_root_id',
              a.result->>'active_leaf_id'
          ) IS NULL
          OR (
              a.result ? 'continuation_suffix_items'
              AND (a.result->>'continuation_suffix_items')::integer <>
                  pg_temp.nc_distance(
                      a.session_id,
                      a.result->>'new_root_id',
                      a.result->>'active_leaf_id'
                  )
          )
      )
    ORDER BY a.id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category','stale_completed_event_reference',
        'event_id',e.id
    ) INTO issue
    FROM events e
    WHERE e.type IN ('history.compacted','compaction.completed')
      AND (
          NOT EXISTS (
              SELECT 1 FROM transcript_entries root
              WHERE root.session_id = e.session_id
                AND root.id = e.payload->>'new_root_id'
                AND root.parent_id IS NULL
          )
          OR pg_temp.nc_distance(
              e.session_id,
              e.payload->>'new_root_id',
              e.payload->>'active_leaf_id'
          ) IS NULL
          OR (
              e.type = 'history.compacted'
              AND (
                  e.payload->>'source_session_id' IS DISTINCT FROM e.session_id
                  OR NOT EXISTS (
                      SELECT 1 FROM transcript_entries source
                      WHERE source.session_id = e.session_id
                        AND source.id = e.payload->>'source_leaf_id'
                  )
              )
          )
          OR (
              e.type = 'compaction.completed'
              AND NOT EXISTS (
                  SELECT 1 FROM actions a
                  WHERE a.id = e.payload->>'action_row_id'
                    AND a.session_id = e.session_id
                    AND a.kind = 'compaction'
                    AND a.status = 'completed'
                    AND a.result->>'new_root_id' = e.payload->>'new_root_id'
                    AND a.result->>'active_leaf_id' = e.payload->>'active_leaf_id'
              )
          )
      )
    ORDER BY e.id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;

    SELECT jsonb_build_object(
        'category','stale_migration_source_reference',
        'row_id',t.session_id || ':' || t.id
    ) INTO issue
    FROM transcript_entries t
    WHERE t.item ? 'native_compaction_v1_source'
      AND (
          t.item#>>'{native_compaction_v1_source,source_session_id}'
              IS DISTINCT FROM t.session_id
          OR NOT EXISTS (
              SELECT 1 FROM transcript_entries source
              WHERE source.session_id = t.session_id
                AND source.id =
                    t.item#>>'{native_compaction_v1_source,source_leaf_id}'
          )
      )
    ORDER BY t.session_id, t.sequence, t.id LIMIT 1;
    IF issue IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            (jsonb_build_object('migration_version','native_compaction_v1') || issue)::text;
    END IF;
END
$$;

-- Preserve sequence semantic next values. ALTER SEQUENCE ... RESTART is
-- transactional, unlike setval, so dry-run really rolls it back.
DO $$
DECLARE
    original_next bigint;
    planned_next bigint;
    sequence_name text;
BEGIN
    SELECT transcript_original_next, transcript_sequence
    INTO original_next, sequence_name
    FROM _nc_context;
    SELECT greatest(
        original_next,
        coalesce((SELECT max(sequence) + 1 FROM transcript_entries), 1)
    ) INTO planned_next;
    UPDATE _nc_context SET transcript_planned_next = planned_next;
    IF planned_next > original_next THEN
        EXECUTE format(
            'ALTER SEQUENCE %s RESTART WITH %s',
            sequence_name::regclass,
            planned_next
        );
    END IF;

    SELECT events_original_next, events_sequence
    INTO original_next, sequence_name
    FROM _nc_context;
    SELECT greatest(
        original_next,
        coalesce((SELECT max(id) + 1 FROM events), 1)
    ) INTO planned_next;
    UPDATE _nc_context SET events_planned_next = planned_next;
    IF planned_next > original_next THEN
        EXECUTE format(
            'ALTER SEQUENCE %s RESTART WITH %s',
            sequence_name::regclass,
            planned_next
        );
    END IF;
EXCEPTION WHEN numeric_value_out_of_range THEN
    RAISE EXCEPTION USING MESSAGE =
        '{"migration_version":"native_compaction_v1","category":"sequence_overflow","detail":"planned next value overflows bigint"}';
END
$$;

UPDATE _nc_context
SET after_checksum = (
    WITH current_rows AS (
        SELECT 'projects' AS table_name, id::text AS row_id, to_jsonb(p) AS row_data
        FROM projects p
        UNION ALL
        SELECT 'sessions', id, to_jsonb(s)
        FROM sessions s
        UNION ALL
        SELECT 'daemon_config', key, to_jsonb(c) FROM daemon_config c
        UNION ALL
        SELECT 'transcript_entries', session_id || ':' || id, to_jsonb(t)
        FROM transcript_entries t
        UNION ALL
        SELECT 'queued_inputs', id, to_jsonb(q) FROM queued_inputs q
        UNION ALL
        SELECT 'actions', id, to_jsonb(a) FROM actions a
        UNION ALL
        SELECT 'events', id::text, to_jsonb(e) FROM events e
        UNION ALL
        SELECT 'delegations', id, to_jsonb(d) FROM delegations d
    )
    SELECT md5(coalesce(string_agg(
        table_name || chr(31) || row_id || chr(31) || row_data::text,
        chr(1) ORDER BY table_name, row_id
    ), '') || chr(2) ||
    (SELECT transcript_planned_next::text || ':' || events_planned_next::text FROM _nc_context))
    FROM current_rows
);

CREATE TEMP VIEW _nc_current_rows AS
SELECT 'projects' AS table_name, id::text AS row_id, NULL::text AS session_id,
       to_jsonb(p) AS row_data
FROM projects p
UNION ALL
SELECT 'sessions', id, id, to_jsonb(s)
FROM sessions s
UNION ALL
SELECT 'daemon_config', key, NULL::text, to_jsonb(c) FROM daemon_config c
UNION ALL
SELECT 'transcript_entries', session_id || ':' || id, session_id, to_jsonb(t)
FROM transcript_entries t
UNION ALL
SELECT 'queued_inputs', id, session_id, to_jsonb(q) FROM queued_inputs q
UNION ALL
SELECT 'actions', id, session_id, to_jsonb(a) FROM actions a
UNION ALL
SELECT 'events', id::text, session_id, to_jsonb(e) FROM events e
UNION ALL
SELECT 'delegations', id, parent_session_id, to_jsonb(d) FROM delegations d;

\set QUIET 0
SELECT jsonb_build_object(
    'migration_version', 'native_compaction_v1',
    'schema', context.schema_name,
    'mode', CASE
        WHEN current_setting('pi_relay.native_compaction_dry_run') = '1'
        THEN 'dry_run'
        ELSE 'apply'
    END,
    'status', CASE
        WHEN EXISTS (
            SELECT 1
            FROM (
                SELECT coalesce(original.table_name,current.table_name) AS table_name,
                       coalesce(original.row_id,current.row_id) AS row_id,
                       original.row_data AS before_row,
                       current.row_data AS after_row
                FROM _nc_original original
                FULL JOIN _nc_current_rows current
                  ON current.table_name = original.table_name
                 AND current.row_id = original.row_id
            ) changed
            WHERE before_row IS DISTINCT FROM after_row
        ) OR NOT context.replay_column_existed
        THEN CASE
            WHEN current_setting('pi_relay.native_compaction_dry_run') = '1'
            THEN 'ready'
            ELSE 'applied'
        END
        ELSE CASE
            WHEN current_setting('pi_relay.native_compaction_dry_run') = '1'
            THEN 'clean'
            ELSE 'no_op'
        END
    END,
    'accepted_native_only', true,
    'before_checksum', context.before_checksum,
    'after_checksum', context.after_checksum,
    'sequences', jsonb_build_array(
        jsonb_build_object(
            'sequence',context.transcript_sequence,
            'original_next_value',context.transcript_original_next,
            'planned_next_value',context.transcript_planned_next
        ),
        jsonb_build_object(
            'sequence',context.events_sequence,
            'original_next_value',context.events_original_next,
            'planned_next_value',context.events_planned_next
        )
    ),
    'categories', coalesce((
        SELECT jsonb_object_agg(category, detail ORDER BY category)
        FROM (
            SELECT category, jsonb_build_object(
                'count',count(*),
                'row_ids',jsonb_agg(row_id ORDER BY table_name,row_id),
                'session_ids',coalesce(jsonb_agg(DISTINCT session_id ORDER BY session_id)
                    FILTER (WHERE session_id IS NOT NULL),'[]'::jsonb)
            ) AS detail
            FROM _nc_categories
            GROUP BY category
        ) categories
    ), '{}'::jsonb),
    'rows', coalesce((
        SELECT jsonb_agg(jsonb_build_object(
            'table',changed.table_name,
            'row_id',changed.row_id,
            'session_id',changed.session_id,
            'operation',CASE
                WHEN changed.before_row IS NULL THEN 'insert'
                WHEN changed.after_row IS NULL THEN 'delete'
                ELSE 'update'
            END,
            'categories',coalesce((
                SELECT jsonb_agg(category ORDER BY category)
                FROM _nc_categories category
                WHERE category.table_name = changed.table_name
                  AND category.row_id = changed.row_id
            ),'[]'::jsonb),
            'before_checksum',CASE
                WHEN changed.before_row IS NULL THEN NULL
                ELSE md5(changed.before_row::text)
            END,
            'after_checksum',CASE
                WHEN changed.after_row IS NULL THEN NULL
                ELSE md5(changed.after_row::text)
            END
        ) ORDER BY changed.table_name,changed.row_id)
        FROM (
            SELECT coalesce(original.table_name,current.table_name) AS table_name,
                   coalesce(original.row_id,current.row_id) AS row_id,
                   coalesce(original.session_id,current.session_id) AS session_id,
                   original.row_data AS before_row,
                   current.row_data AS after_row
            FROM _nc_original original
            FULL JOIN _nc_current_rows current
              ON current.table_name = original.table_name
             AND current.row_id = original.row_id
            WHERE original.row_data IS DISTINCT FROM current.row_data
        ) changed
    ),'[]'::jsonb),
    'unresolved_decisions','[]'::jsonb
) AS native_compaction_v1_report
FROM _nc_context context;
\set QUIET 1

\if :dry_run
ROLLBACK;
\else
COMMIT;
\endif
