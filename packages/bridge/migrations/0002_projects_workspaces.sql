-- M8: projects (product grouping + default workspace decls), session workspace
-- materialization records, per-session MCP selection.
BEGIN;

CREATE TABLE IF NOT EXISTS projects (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  workspaces JSONB NOT NULL DEFAULT '[]',   -- WorkspaceDecl[] defaults for new sessions
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
  ADD COLUMN IF NOT EXISTS workspaces JSONB NOT NULL DEFAULT '[]',  -- SessionWorkspace[] from materialize
  ADD COLUMN IF NOT EXISTS mcp_selection JSONB;                      -- McpSessionSelection | NULL

COMMIT;
