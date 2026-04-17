import type { PromptContext, PromptFragment, PromptSource } from "../types.js";

export interface ContextFile {
	path: string;
	content: string;
}

/**
 * Emits the `# Project Context` block with all discovered CLAUDE.md / AGENTS.md
 * files. Discovery (walking up from cwd plus the global agent dir) is handled by
 * the resource loader and passed in. If no files are present, emits nothing.
 */
export class ProjectSource implements PromptSource {
	readonly name = "coding-agent.project";
	readonly phase = "static" as const;

	constructor(private readonly contextFiles: readonly ContextFile[]) {}

	contribute(_ctx: PromptContext): PromptFragment[] {
		if (this.contextFiles.length === 0) {
			return [];
		}

		const lines = ["# Project Context", "", "Project-specific instructions and guidelines:", ""];
		for (const { path, content } of this.contextFiles) {
			lines.push(`## ${path}`, "", content, "");
		}

		return [
			{
				section: "project",
				priority: 0,
				content: lines.join("\n").trimEnd(),
				cacheable: true,
				sourceName: this.name,
			},
		];
	}
}
