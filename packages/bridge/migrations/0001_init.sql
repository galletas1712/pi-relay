-- M5 bridge control plane (control ONLY — transcripts stay as pi session files on disk)
BEGIN;

CREATE TABLE IF NOT EXISTS sessions (
  id              TEXT PRIMARY KEY,            -- pi sessionId (also keys kernel snapshot dir)
  cwd             TEXT NOT NULL,
  session_file    TEXT,                        -- pi session JSONL path (NULL until first flush)
  name            TEXT,
  state           TEXT NOT NULL DEFAULT 'starting'
                  CHECK (state IN ('starting','idle','running','compacting','host_down','respawning','closed')),
  host_pid        INTEGER,
  host_generation INTEGER NOT NULL DEFAULT 0,  -- incremented per (re)spawn; crash = new generation
  last_event_seq  BIGINT NOT NULL DEFAULT 0,   -- per-session event high-water (authoritative counter)
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Durable journal of every rpc command the bridge sends (or owes) to a session host.
-- This is the product-level durable queue (P2): entries pending while a host is
-- down are delivered after respawn, in seq order.
CREATE TABLE IF NOT EXISTS command_journal (
  seq              BIGSERIAL PRIMARY KEY,
  session_id       TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  host_generation  INTEGER NOT NULL,           -- generation the command was first sent to
  kind             TEXT NOT NULL,              -- prompt | steer | follow_up | abort
  payload          JSONB NOT NULL,
  status           TEXT NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending','acked','failed','maybe_lost')),
  error            TEXT,
  idempotency_key  TEXT,
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  acked_at         TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS command_journal_session ON command_journal(session_id, seq);
CREATE INDEX IF NOT EXISTS command_journal_pending ON command_journal(session_id) WHERE status = 'pending';

-- Resumable event stream: per-session ring buffer of contract events.
CREATE TABLE IF NOT EXISTS event_spool (
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq         BIGINT NOT NULL,                 -- per-session monotonic (1..N), the client's high-water mark
  event       TEXT NOT NULL,                   -- session.state | message.delta | tool.exec | subagent.lifecycle | session.error
  payload     JSONB NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (session_id, seq)
);

-- Idempotency keys for client-initiated mutations (transport-uncertain retry safety).
CREATE TABLE IF NOT EXISTS idempotency_keys (
  key          TEXT PRIMARY KEY,
  method       TEXT NOT NULL,
  session_id   TEXT,
  params_hash  TEXT NOT NULL,                  -- sha256 of canonical params (conflict detection)
  response     JSONB NOT NULL,                 -- stored successful response, replayed verbatim
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Migration bookkeeping
CREATE TABLE IF NOT EXISTS schema_migrations (
  version     INTEGER PRIMARY KEY,
  applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMIT;
