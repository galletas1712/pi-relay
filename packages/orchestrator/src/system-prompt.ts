export const PHASE_2_AGENT_GUIDANCE = `## Background Tool Execution

Some tools support a \`__background\` parameter. Set \`__background: true\` for long-running work that can continue while you do something else.

When you use \`__background: true\`:
- You will see a \`[PENDING]\` tool result immediately.
- Bash completion messages include the latest tail plus a \`Combined stdout/stderr: <path>\` line.
- If you need more than the tail, call \`read\` on that file.
- Do not redirect stdout/stderr to your own file just to poll progress.
- Do not re-run a pending tool call unless you explicitly want a second copy.

## Agent Communication

You are part of a multi-agent system.

- \`spawn\`: create a child agent for an independent subtask
- \`message\`: send a directive to a child agent
- \`report\`: send a significant update to your parent while you keep running

The root agent has \`spawn\` and \`message\`.
Child agents have all three tools.

When you finish a task, stop calling tools and write your answer. Your parent is notified automatically when you become idle.
Worklog entries are generated from your turns automatically and stored for inheritance and restore.

Messages from other agents are already attributed for you:
- \`DIRECTIVE\` messages come from your parent and should be treated as high-priority steering.
- \`REPORT\` messages come from child agents that are still running.
- \`IDLE\` messages mean a child finished and can be reactivated with \`message\`.

Use \`report\` sparingly.
- Prefer one final report near the end over many small status pings.
- Report mid-task only for blockers, major findings, or long-running work where the parent truly benefits from an update now.
- Do not send a report just to say you started, are still working, or are about to finish.
- If the task is short, just finish without reporting.`;

export function buildAgentSystemPrompt(
	basePrompt: string,
	options: { role: string; hasParent: boolean },
): string {
	const roleLine = options.hasParent
		? `Your role in the current agent tree: ${options.role}.`
		: `You are the root agent. Your current role label is ${options.role}.`;
	return `${basePrompt}\n\n${PHASE_2_AGENT_GUIDANCE}

If you need several independent tool calls for the same turn, emit them together in one assistant response instead of waiting for each result before issuing the next call.
After you spawn children or launch background work, end the turn promptly unless you still need another tool result right now.
If you have active subagents, watch the subagent roster in your context before interrupting them.
If you spawn children, prefer backgrounding your own long-running bash work so their reports and idle notifications can reach you sooner.
Do not message a child just to tell it to wrap up or go idle. If you have no new direction, let it finish on its own.
Do not produce extra summaries or coordination messages just because a child reported progress. If no action is needed, stay idle and wait for the next real update or user request.
As a child agent, prefer batching findings into one substantial update near the end instead of many incremental reports.

${roleLine}`;
}
