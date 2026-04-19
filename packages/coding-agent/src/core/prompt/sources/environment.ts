import type { PromptContext, PromptFragment, PromptSource } from "../types.js";

/**
 * Emits the current date and working directory. These are the two trailing
 * lines the old buildSystemPrompt appended at the end. The cwd is posix-normalized
 * for stable cross-platform output.
 */
export class EnvironmentSource implements PromptSource {
	readonly name = "coding-agent.environment";
	readonly phase = "static" as const;

	contribute(ctx: PromptContext): PromptFragment[] {
		const date = ctx.now.toISOString().slice(0, 10);
		const promptCwd = ctx.cwd.replace(/\\/g, "/");
		const content = `Current date: ${date}\nCurrent working directory: ${promptCwd}`;

		return [
			{
				section: "environment",
				priority: 0,
				content,
				cacheable: true,
				sourceName: this.name,
			},
		];
	}
}
