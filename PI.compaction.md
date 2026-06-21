Produce a concise continuation summary under 6,000 tokens.

Prefer bullet points. Preserve only actionable state, decisions, constraints, file paths, commands run, tool results needed to continue, and open tasks.

Do not try to preserve or reconstruct current delegation/orchestration state
from old transcript snippets. The daemon appends fresh parent-session delegation
state after compaction when needed. Subagent compactions should summarize only
the subagent's own role contract, delegated task, transcript/model history, and
own tool results/facts; do not infer parent or sibling delegation state.

Do not quote large files or logs.
