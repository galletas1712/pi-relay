import type { ProviderReplayItem } from "./types.ts";

export interface HostedToolView {
	id: string;
	name: string;
	prettyName: string;
	inputSummary: string | null;
	provider: ProviderReplayItem["provider"];
	status: "completed" | "running" | "error";
	input: Record<string, unknown> | null;
	output: string | null;
}

export interface SourceCitation {
	id: string;
	title: string;
	url: string;
	citedText?: string;
	provider: ProviderReplayItem["provider"];
}

export interface ProviderReplayRaw {
	type?: string;
	id?: string;
	call_id?: string;
	name?: string;
	status?: string;
	action?: unknown;
	input?: unknown;
	tool_use_id?: string;
	content?: unknown;
	annotations?: unknown;
	citations?: unknown;
}

interface ParsedReplayItem {
	provider: ProviderReplayItem["provider"];
	raw: ProviderReplayRaw;
	display: ProviderReplayItem["display"];
}

interface HostedResult {
	status: HostedToolView["status"];
	output: string | null;
}

export function parsedProviderReplay(replay: ProviderReplayItem[] | undefined): ParsedReplayItem[] {
	return (replay ?? [])
		.map((item) => {
			try {
				const raw = JSON.parse(item.raw_json) as ProviderReplayRaw;
				return { provider: item.provider, raw, display: item.display };
			} catch {
				return null;
			}
		})
		.filter((item): item is ParsedReplayItem => item != null);
}

export function hostedToolsFromReplay(replay: ProviderReplayItem[] | undefined): HostedToolView[] {
	const parsed = parsedProviderReplay(replay);
	const anthropicResults = anthropicHostedResults(parsed);
	const tools: HostedToolView[] = [];
	for (const item of parsed) {
		if (item.raw.type === "web_search_call") {
			if (item.display?.kind === "hosted_tool" && item.raw.id) tools.push(openAiWebSearchTool(item, item.display, item.raw.id));
			continue;
		}
		if (item.raw.type === "server_tool_use" && isAnthropicHostedTool(item.raw.name)) {
			if (item.display?.kind !== "hosted_tool") continue;
			const id = item.raw.id;
			const name = item.raw.name;
			if (!id || !name) continue;
			const result = anthropicResults.get(id);
			tools.push({
				id,
				name,
				prettyName: item.display.pretty_name,
				inputSummary: item.display.input_summary ?? null,
				provider: item.provider,
				status: result?.status ?? "running",
				input: objectInput(item.raw.input),
				output: result?.output ?? null
			});
		}
	}
	return tools;
}

export function citationsFromReplay(replay: ProviderReplayItem[] | undefined): SourceCitation[] {
	const citations: SourceCitation[] = [];
	const seen = new Set<string>();
	for (const item of parsedProviderReplay(replay)) {
		const itemCitations: SourceCitation[] = [];
		collectCitationsFromValue(item.raw, item.provider, itemCitations);
		for (const citation of itemCitations) {
			const key = `${citation.url}\n${citation.title}\n${citation.citedText ?? ""}`;
			if (seen.has(key)) continue;
			seen.add(key);
			citations.push({ ...citation, id: `source-${citations.length + 1}` });
		}
	}
	return citations;
}

export function localToolCallIdFromReplay(raw: ProviderReplayRaw): string | null {
	switch (raw.type) {
		case "function_call":
		case "custom_tool_call":
			return raw.call_id ?? raw.id ?? null;
		case "tool_use":
			return raw.id ?? null;
		default:
			return null;
	}
}

export function replayContainsAssistantText(raw: ProviderReplayRaw): boolean {
	return raw.type === "message" || raw.type === "text";
}

function collectCitationsFromValue(value: unknown, provider: ProviderReplayItem["provider"], citations: SourceCitation[]) {
	if (!isRecord(value)) return;
	collectCitationArray(value.citations, provider, citations);
	collectCitationArray(value.annotations, provider, citations);
	const content = value.content;
	if (Array.isArray(content)) {
		for (const block of content) collectCitationsFromValue(block, provider, citations);
	} else if (isRecord(content)) {
		collectCitationsFromValue(content, provider, citations);
	}
}

function collectCitationArray(value: unknown, provider: ProviderReplayItem["provider"], citations: SourceCitation[]) {
	if (!Array.isArray(value)) return;
	for (const item of value) {
		if (!isRecord(item)) continue;
		const url = typeof item.url === "string" ? item.url : "";
		if (!url) continue;
		const title = typeof item.title === "string" && item.title ? item.title : url;
		const citedText = typeof item.cited_text === "string" && item.cited_text ? item.cited_text : undefined;
		citations.push({ id: "", title, url, citedText, provider });
	}
}

function openAiWebSearchTool(item: ParsedReplayItem, display: NonNullable<ProviderReplayItem["display"]>, id: string): HostedToolView {
	return {
		id,
		name: "web_search",
		prettyName: display.pretty_name,
		inputSummary: display.input_summary ?? null,
		provider: item.provider,
		status: item.raw.status === "completed" ? "completed" : item.raw.status === "failed" ? "error" : "running",
		input: objectInput(item.raw.action),
		output: null
	};
}

function anthropicHostedResults(parsed: ParsedReplayItem[]): Map<string, HostedResult> {
	const results = new Map<string, HostedResult>();
	for (const item of parsed) {
		if (item.raw.type !== "web_search_tool_result" && item.raw.type !== "web_fetch_tool_result") continue;
		const toolUseId = item.raw.tool_use_id;
		if (!toolUseId) continue;
		results.set(toolUseId, summarizeAnthropicHostedResult(item.raw.content));
	}
	return results;
}

function summarizeAnthropicHostedResult(content: unknown): HostedResult {
	if (isRecord(content) && typeof content.error_code === "string") {
		return { status: "error", output: content.error_code };
	}
	if (isRecord(content) && content.type === "web_fetch_result") {
		return { status: "completed", output: summarizeWebFetchResult(content) };
	}
	if (Array.isArray(content)) {
		const summaries = content
			.map((item) => {
				if (!isRecord(item)) return null;
				const title = typeof item.title === "string" ? item.title : "";
				const url = typeof item.url === "string" ? item.url : "";
				return [title, url].filter(Boolean).join(" - ") || null;
			})
			.filter((line): line is string => !!line);
		return { status: "completed", output: summaries.slice(0, 6).join("\n") || null };
	}
	if (typeof content === "string") {
		return { status: "completed", output: content };
	}
	return { status: "completed", output: null };
}

function summarizeWebFetchResult(content: Record<string, unknown>): string | null {
	const document = isRecord(content.content) ? content.content : null;
	const title = document && typeof document.title === "string" ? document.title : "";
	const url = typeof content.url === "string" ? content.url : "";
	const source = document && isRecord(document.source) && typeof document.source.data === "string" ? document.source.data : "";
	const excerpt = source
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line && !line.startsWith("---") && !line.startsWith("meta-") && !line.startsWith("title:"))
		.slice(0, 8)
		.join("\n");
	return [title, url, excerpt].filter(Boolean).join("\n") || null;
}

function isAnthropicHostedTool(name: string | undefined): boolean {
	return name === "web_search" || name === "web_fetch";
}

function objectInput(value: unknown): Record<string, unknown> | null {
	return isRecord(value) ? value : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}
