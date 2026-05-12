import type { ContentBlock, ProviderConfig } from "./types.ts";

export const DEFAULT_PROVIDER: ProviderConfig = {
	kind: "codex",
	model: "gpt-5.5",
	prompt_cache: { key: "pi-relay-web" }
};

export function textContent(text: string): ContentBlock[] {
	return [{ type: "text", text }];
}
