-- M11b: fork/switch lineage. parent_session_id links a forked child session
-- to the session it branched from (null for roots and pre-M11b rows).
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS parent_session_id TEXT;
