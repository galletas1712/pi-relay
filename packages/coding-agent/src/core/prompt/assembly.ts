import {
	SECTION_ORDER,
	type AssembledPrompt,
	type AssembledPromptBlock,
	type PromptContext,
	type PromptFragment,
	type PromptSection,
	type PromptSource,
} from "./types.js";

/**
 * Registers PromptSource instances and assembles a structured system prompt.
 *
 * Fragments emitted by sources are grouped by section, sorted by priority,
 * then concatenated (fragments with two newlines between them, sections with
 * two newlines between them). Source name uniqueness is enforced at register
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

		const blocks: AssembledPromptBlock[] = [];
		for (const section of SECTION_ORDER) {
			const fragments = fragmentsBySection.get(section) ?? [];
			if (fragments.length === 0) {
				continue;
			}
			const text = fragments.map((fragment) => fragment.content).join("\n\n");
			const cacheable = fragments.every((fragment) => fragment.cacheable);
			blocks.push({ section, text, cacheable });
		}

		const text = blocks.map((block) => block.text).join("\n\n");

		return {
			sections: fragmentsBySection,
			blocks,
			text,
		};
	}
}
