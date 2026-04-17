import type { PromptContext, PromptFragment, PromptSource } from "@pi-relay/coding-agent";

/**
 * Static coordination guidance for agents participating in the relay tree.
 * `role` + `hasParent` come from the orchestrator AgentRecord at register time.
 */
export interface MultiAgentInstructionsOptions {
	role: string;
	hasParent: boolean;
}

const AGENT_COMMUNICATION = `## Agent Communication

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
- \`IDLE\` messages mean a child finished and can be reactivated with \`message\`. They are lifecycle signals, not result summaries.

Use \`report\` sparingly.
- Prefer one final report near the end over many small status pings.
- When you have solid findings, a concrete decision, or a completed result your parent is likely to need, send one concise \`report\` before finishing.
- Report mid-task only for blockers, major findings, or long-running work where the parent truly benefits from an update now.
- Use \`report\` when the update should change parent behavior now: reprioritize work, redirect a sibling, stop duplicate effort, or react to a blocker/risk.
- If the update would not change what the parent or sibling agents should do right now, do not report it yet.
- Do not send a report just to say you started, are still working, or are about to finish.
- Do not rely on \`IDLE\` to carry your substantive result to your parent.
- If the task is short but produced a concrete result the parent will need, send one concise \`report\` and then finish.`;

const BATCHING_GUIDANCE = `If you need several independent tool calls for the same turn, emit them together in one assistant response instead of waiting for each result before issuing the next call.
If you decide to delegate several independent subtasks, emit all of the \`spawn\` calls in the same assistant response so the children start together.
After you spawn children or launch background work, end the turn promptly unless you still need another tool result right now.
If you have running subagents, glance at the subagent roster in your context before interrupting them.
Treat the roster as advisory coordination context only.
Prefer fresh \`REPORT\` and \`IDLE\` messages over the roster when they disagree.
Do not restate or audit the roster unless it changes what you should do next.
If you spawn children, prefer backgrounding your own long-running bash work so their reports and idle notifications can reach you sooner.
Do not message a child just to tell it to wrap up or go idle. If you have no new direction, let it finish on its own.
Do not produce extra summaries or coordination messages just because a child reported progress. If no action is needed, stay idle and wait for the next real update or user request.
If several direct children are still active, wait for the remaining children instead of summarizing each finished child separately unless one child reported something that needs immediate action.
As a child agent, prefer batching findings into one substantial update near the end instead of many incremental reports.`;

export class MultiAgentInstructionsSource implements PromptSource {
	readonly name = "orchestrator.multi-agent";
	readonly phase = "static" as const;

	constructor(private readonly options: MultiAgentInstructionsOptions) {}

	contribute(_ctx: PromptContext): PromptFragment[] {
		const roleLine = this.options.hasParent
			? `Your role in the current agent tree: ${this.options.role}.`
			: `You are the root agent. Your current role label is ${this.options.role}.`;

		return [
			{
				section: "coordination",
				priority: 0,
				content: AGENT_COMMUNICATION,
				cacheable: true,
				sourceName: this.name,
			},
			{
				section: "coordination",
				priority: 10,
				content: BATCHING_GUIDANCE,
				cacheable: true,
				sourceName: this.name,
			},
			{
				section: "coordination",
				priority: 20,
				content: roleLine,
				cacheable: true,
				sourceName: this.name,
			},
		];
	}
}
