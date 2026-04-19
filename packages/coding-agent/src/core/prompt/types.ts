/**
 * PromptAssembly — structured system-prompt composition.
 *
 * Each layer of the stack (coding-agent core, orchestrator, extensions) contributes
 * PromptSource implementations that emit ordered fragments. The assembly concatenates
 * fragments section-by-section into the final system prompt. Sections are ordered by
 * SECTION_ORDER; fragments within a section are ordered by priority.
 *
 * Each section has a retention tier (SECTION_RETENTION). Consecutive sections with
 * the same retention get coalesced into a single AssembledPromptBlock, which the
 * Anthropic provider emits as a distinct {type:"text", cache_control?} entry.
 */

import type { Model } from "@pi-relay/ai";

export type PromptSection =
	| "role"
	| "capabilities"
	| "coordination"
	| "project"
	| "skills"
	| "role_per_agent"
	| "environment"
	| "custom";

export type PromptRetention = "none" | "short" | "long";

// SECTION_ORDER puts universal (`long`-retention) sections first, then
// session-specific (`short`) sections, then per-agent + volatile (`none`)
// at the tail. Retention tiers MUST be monotonic in SECTION_ORDER:
// long → short → none. Assembly relies on this to produce 3 contiguous blocks.
export const SECTION_ORDER: readonly PromptSection[] = [
	"role",
	"capabilities",
	"coordination",
	"project",
	"skills",
	"role_per_agent",
	"environment",
	"custom",
];

export const SECTION_RETENTION: Record<PromptSection, PromptRetention> = {
	role: "long",
	capabilities: "long",
	coordination: "long",
	project: "short",
	skills: "short",
	role_per_agent: "none",
	environment: "none",
	custom: "none",
};

export type PromptPhase = "static" | "dynamic";

export interface PromptContext {
	readonly sessionId: string;
	readonly cwd: string;
	readonly model?: Model<any>;
	readonly now: Date;
	readonly toolNames: readonly string[];
	/** Extra fields downstream sources may read. */
	readonly extras?: Readonly<Record<string, unknown>>;
}

export interface PromptFragment {
	readonly section: PromptSection;
	readonly priority: number;
	readonly content: string;
	readonly sourceName: string;
}

export interface PromptSource {
	readonly name: string;
	readonly phase: PromptPhase;
	contribute(ctx: PromptContext): PromptFragment[];
}

export interface AssembledPromptBlock {
	readonly sections: readonly PromptSection[];
	readonly retention: PromptRetention;
	readonly text: string;
}

export interface AssembledPrompt {
	readonly sections: ReadonlyMap<PromptSection, readonly PromptFragment[]>;
	readonly blocks: readonly AssembledPromptBlock[];
	readonly text: string;
}
