import type { PromptContext, PromptFragment, PromptSource } from "../types.js";

/**
 * Role preamble prepended to sessions routed through the direct Anthropic provider,
 * matching the identity-mode gate that used to live inside the Anthropic provider
 * itself. Claude models served by Bedrock, Antigravity, OpenRouter, Vercel AI
 * Gateway, etc. get a clean system prompt with no Claude Code impersonation line.
 * Providers must NOT inject this themselves.
 */
const CLAUDE_CODE_ROLE_PREAMBLE = "You are Claude Code, Anthropic's official CLI for Claude.";

export interface RoleSourceOptions {
	/** Custom system prompt. If set, replaces the default pi body. */
	customPrompt?: string;
	/** Text appended after the role preamble + custom/default body. */
	appendSystemPrompt?: string;
	/** Active tool names. Drives the "Available tools" / guidelines listing when no customPrompt. */
	selectedTools?: readonly string[];
	/** One-line tool snippets keyed by tool name. Tools without snippets are hidden. */
	toolSnippets?: Readonly<Record<string, string>>;
	/** Extra guideline bullets appended to the default guidelines. */
	promptGuidelines?: readonly string[];
}

export class RoleSource implements PromptSource {
	readonly name = "coding-agent.role";
	readonly phase = "static" as const;

	constructor(private readonly options: RoleSourceOptions = {}) {}

	contribute(ctx: PromptContext): PromptFragment[] {
		const fragments: PromptFragment[] = [];
		const includePreamble = ctx.model?.provider === "anthropic";
		const base = this.options.customPrompt ?? this.buildDefaultBody();
		const body = includePreamble ? `${CLAUDE_CODE_ROLE_PREAMBLE}\n\n${base}` : base;

		fragments.push({
			section: "role",
			priority: 0,
			content: body,
			cacheable: true,
			sourceName: this.name,
		});

		if (this.options.appendSystemPrompt) {
			fragments.push({
				section: "role",
				priority: 10,
				content: this.options.appendSystemPrompt,
				cacheable: true,
				sourceName: this.name,
			});
		}

		return fragments;
	}

	private buildDefaultBody(): string {
		const tools = this.options.selectedTools ?? ["read", "bash", "edit", "write"];
		const visibleTools = tools.filter((name) => !!this.options.toolSnippets?.[name]);
		const toolsList =
			visibleTools.length > 0
				? visibleTools.map((name) => `- ${name}: ${this.options.toolSnippets?.[name]}`).join("\n")
				: "(none)";

		const guidelines = this.buildGuidelines(tools);

		return `You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
${toolsList}

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
${guidelines}`;
	}

	private buildGuidelines(tools: readonly string[]): string {
		const collected: string[] = [];
		const seen = new Set<string>();
		const add = (guideline: string): void => {
			if (!seen.has(guideline)) {
				seen.add(guideline);
				collected.push(guideline);
			}
		};

		const hasBash = tools.includes("bash");
		const hasGrep = tools.includes("grep");
		const hasFind = tools.includes("find");
		const hasLs = tools.includes("ls");

		if (hasBash && !hasGrep && !hasFind && !hasLs) {
			add("Use bash for file operations like ls, rg, find");
		} else if (hasBash && (hasGrep || hasFind || hasLs)) {
			add("Prefer grep/find/ls tools over bash for file exploration (faster, respects .gitignore)");
		}

		for (const guideline of this.options.promptGuidelines ?? []) {
			const normalized = guideline.trim();
			if (normalized.length > 0) {
				add(normalized);
			}
		}

		add("Be concise in your responses");
		add("Show file paths clearly when working with files");

		return collected.map((g) => `- ${g}`).join("\n");
	}
}
