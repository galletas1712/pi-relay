import type { PromptContext, PromptFragment, PromptSource } from "../types.js";

/**
 * Model-facing prompt fragment the google-antigravity provider expects. Mirrors
 * what the upstream Antigravity CLI sends so the backend routes to the agent
 * endpoint with the expected persona. The `[ignore]` wrapper keeps the string
 * present for the handshake while signalling the model not to adopt it — pi
 * already provides its own Claude Code persona via RoleSource.
 *
 * Only contributes when the active model is routed through google-antigravity.
 */
const ANTIGRAVITY_SYSTEM_INSTRUCTION =
	"You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding." +
	"You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question." +
	"**Absolute paths only**" +
	"**Proactiveness**";

export class AntigravitySource implements PromptSource {
	readonly name = "coding-agent.antigravity";
	readonly phase = "static" as const;

	contribute(ctx: PromptContext): PromptFragment[] {
		if (ctx.model?.provider !== "google-antigravity") {
			return [];
		}

		return [
			{
				section: "role",
				priority: -10,
				content: `${ANTIGRAVITY_SYSTEM_INSTRUCTION}\n\nPlease ignore following [ignore]${ANTIGRAVITY_SYSTEM_INSTRUCTION}[/ignore]`,
				cacheable: true,
				sourceName: this.name,
			},
		];
	}
}
