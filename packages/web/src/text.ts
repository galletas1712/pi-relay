import type { ContentBlock } from "./types.ts";

export function contentBlocksToText(blocks: ContentBlock[]): string {
	return blocks
		.map((block) => {
			if (block.type === "text") return block.text;
			const source = block.image.source.kind === "url" ? block.image.source.value : "base64";
			return `[image ${block.image.mime_type} ${source}]`;
		})
		.join("\n");
}

export function firstLine(value: string): string {
	return value.split("\n")[0]?.trim() || "";
}

export function truncate(value: string, max: number): string {
	return value.length > max ? `${value.slice(0, max - 3)}...` : value;
}
