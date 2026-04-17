import type { PromptContext, PromptFragment, PromptSource } from "@pi-relay/coding-agent";

/**
 * Describes how `__background: true` tool invocation works. Only registered on
 * sessions that might spawn children or launch background work — today the
 * orchestrator extension registers this on every session it attaches to.
 */
const BACKGROUND_CAPABILITIES = `## Background Tool Execution

Some tools support a \`__background\` parameter. Set \`__background: true\` for long-running work that can continue while you do something else.

When you use \`__background: true\`:
- You will see a \`[PENDING]\` tool result immediately.
- Bash completion messages include the latest tail plus a \`Combined stdout/stderr: <path>\` line.
- If you need more than the tail, call \`read\` on that file.
- Do not redirect stdout/stderr to your own file just to poll progress.
- Do not re-run a pending tool call unless you explicitly want a second copy.`;

export class BackgroundCapabilitiesSource implements PromptSource {
	readonly name = "orchestrator.background-capabilities";
	readonly phase = "static" as const;

	contribute(_ctx: PromptContext): PromptFragment[] {
		return [
			{
				section: "capabilities",
				priority: 0,
				content: BACKGROUND_CAPABILITIES,
				sourceName: this.name,
			},
		];
	}
}
