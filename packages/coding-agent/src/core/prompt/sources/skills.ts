import { formatSkillsForPrompt, type Skill } from "../../skills.js";
import type { PromptContext, PromptFragment, PromptSource } from "../types.js";

/**
 * Emits the `<available_skills>` XML block. Skills are only usable if the `read`
 * tool is active (the agent loads a skill by reading its SKILL.md), so when the
 * read tool is absent we contribute nothing.
 *
 * Task 8 follow-up: replace this entirely with an on-demand skill-loading tool.
 */
export class SkillsSource implements PromptSource {
	readonly name = "coding-agent.skills";
	readonly phase = "static" as const;

	constructor(private readonly skills: readonly Skill[]) {}

	contribute(ctx: PromptContext): PromptFragment[] {
		if (this.skills.length === 0 || !ctx.toolNames.includes("read")) {
			return [];
		}

		const formatted = formatSkillsForPrompt([...this.skills]).trimStart();
		if (formatted.length === 0) {
			return [];
		}

		return [
			{
				section: "skills",
				priority: 0,
				content: formatted,
				cacheable: true,
				sourceName: this.name,
			},
		];
	}
}
