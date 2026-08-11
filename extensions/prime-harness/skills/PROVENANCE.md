# Bundled skills provenance

M2 skills (written for pi-relay): demo-python, echo-markdown, refine.

M4 additions are verbatim copies from prime-agent's `packages/coding-agent/skills/<name>/`
(copy-with-provenance policy; PA is at `.pi/migration-research/repos/prime-agent`):

- `edit` — exact-string file edit skill; emits diffs via `display_data`
  (parsed host-side by prime-rlm's kernel, `application/vnd.prime-agent.diff+json`).
- `attach-image` — image attach skill; emits attachments via `display_data`
  (`application/vnd.prime-agent.attachment+json`); uses the `model.info` host
  request for the vision check. Needs Pillow in the kernel env (auto-installed).
- `websearch` — Serper search; pure kernel-side (httpx + SERPER_API_KEY).
- `goal`, `compact`, `rlm-heartbeat` — thin wrappers over host requests
  (`goal.*`, `compact.*`, `rlm_heartbeat.*`) served by the prime-autonomy extension.
- `linear`, `notion` — MCP integrations subclassing `rlm.McpIntegration`
  (ported into prime-rlm-runtime as `mcp_base.py`). Importable without the `mcp`
  SDK; calling their tools requires MCP credentials + the `mcp.refresh` host
  handler, which pi-relay does not implement (documented M4 delta).
- `prime-intellect`, `skill-creator` — markdown-only skills.

Unchanged from PA except this note. PA's agent-message/agent-observe skills are
NOT bundled: prime-comms provides equivalent kernel APIs directly (COMMS_KERNEL_PYTHON).
