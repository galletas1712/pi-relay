/**
 * PromptAssembly — structured system-prompt composition.
 *
 * Each layer of the stack (coding-agent core, orchestrator, extensions) contributes
 * PromptSource implementations that emit ordered fragments. The assembly concatenates
 * fragments section-by-section into the final system prompt. Sections are ordered by
 * SECTION_ORDER; fragments within a section are ordered by priority.
 */

import type { Model } from "@pi-relay/ai";

export type PromptSection =
	| "role"
	| "environment"
	| "project"
	| "skills"
	| "capabilities"
	| "coordination"
	| "custom";

// Order preserves the pre-PromptAssembly output: environment (date + cwd) stays
// at the tail so the prompt the model sees is semantically identical.
export const SECTION_ORDER: readonly PromptSection[] = [
	"role",
	"project",
	"skills",
	"capabilities",
	"coordination",
	"environment",
	"custom",
];

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

export interface AssembledPrompt {
	readonly sections: ReadonlyMap<PromptSection, readonly PromptFragment[]>;
	readonly text: string;
}
