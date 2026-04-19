import {
	SECTION_ORDER,
	SECTION_RETENTION,
	type AssembledPrompt,
	type AssembledPromptBlock,
	type PromptContext,
	type PromptFragment,
	type PromptRetention,
	type PromptSection,
	type PromptSource,
} from "./types.js";

/**
 * Registers PromptSource instances and assembles a structured system prompt.
 *
 * Fragments emitted by sources are grouped by section, sorted by priority,
 * then coalesced into AssembledPromptBlocks by consecutive-matching retention
 * tier (per SECTION_RETENTION). Source name uniqueness is enforced at register
 * time — duplicates throw.
 */
export class PromptAssembly {
	private readonly sources = new Map<string, PromptSource>();

	constructor(sources: PromptSource[] = []) {
		for (const source of sources) {
			this.register(source);
		}
	}

	register(source: PromptSource): void {
		if (this.sources.has(source.name)) {
			throw new Error(`PromptSource already registered: ${source.name}`);
		}
		this.sources.set(source.name, source);
	}

	unregister(name: string): void {
		this.sources.delete(name);
	}

	has(name: string): boolean {
		return this.sources.has(name);
	}

	listSources(): readonly PromptSource[] {
		return Array.from(this.sources.values());
	}

	assemble(ctx: PromptContext): AssembledPrompt {
		return this.assembleFor(ctx, () => true);
	}

	assembleStatic(ctx: PromptContext): AssembledPrompt {
		return this.assembleFor(ctx, (source) => source.phase === "static");
	}

	private assembleFor(
		ctx: PromptContext,
		predicate: (source: PromptSource) => boolean,
	): AssembledPrompt {
		const fragmentsBySection = new Map<PromptSection, PromptFragment[]>();
		for (const section of SECTION_ORDER) {
			fragmentsBySection.set(section, []);
		}

		for (const source of this.sources.values()) {
			if (!predicate(source)) {
				continue;
			}
			const fragments = source.contribute(ctx);
			for (const fragment of fragments) {
				const bucket = fragmentsBySection.get(fragment.section);
				if (!bucket) {
					throw new Error(`Unknown prompt section "${fragment.section}" from source ${source.name}`);
				}
				bucket.push(fragment);
			}
		}

		for (const fragments of fragmentsBySection.values()) {
			fragments.sort((left, right) => left.priority - right.priority);
		}

		// Per-section rendered text. Skip empty sections.
		const renderedSections: Array<{ section: PromptSection; text: string }> = [];
		for (const section of SECTION_ORDER) {
			const fragments = fragmentsBySection.get(section) ?? [];
			if (fragments.length === 0) continue;
			const text = fragments.map((f) => f.content).join("\n\n");
			if (text.length === 0) continue;
			renderedSections.push({ section, text });
		}

		// Group consecutive sections sharing a retention tier into blocks.
		const blocks: AssembledPromptBlock[] = [];
		let currentSections: PromptSection[] = [];
		let currentTexts: string[] = [];
		let currentRetention: PromptRetention | null = null;
		for (const { section, text } of renderedSections) {
			const retention = SECTION_RETENTION[section];
			if (retention !== currentRetention) {
				if (currentSections.length > 0 && currentRetention !== null) {
					blocks.push({
						sections: currentSections,
						retention: currentRetention,
						text: currentTexts.join("\n\n"),
					});
				}
				currentSections = [section];
				currentTexts = [text];
				currentRetention = retention;
			} else {
				currentSections.push(section);
				currentTexts.push(text);
			}
		}
		if (currentSections.length > 0 && currentRetention !== null) {
			blocks.push({
				sections: currentSections,
				retention: currentRetention,
				text: currentTexts.join("\n\n"),
			});
		}

		const text = blocks.map((block) => block.text).join("\n\n");

		return {
			sections: fragmentsBySection,
			blocks,
			text,
		};
	}
}
