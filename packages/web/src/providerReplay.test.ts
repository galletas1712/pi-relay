import { describe, expect, it } from "vitest";
import { citationsFromReplay, hostedToolsFromReplay, localToolCallIdFromReplay, parsedProviderReplay } from "./providerReplay.ts";
import type { ProviderReplayItem } from "./types.ts";

describe("provider replay hosted tool views", () => {
	it("surfaces OpenAI Responses native web_search calls", () => {
		const tools = hostedToolsFromReplay([
			replay(
				"openai",
				{
					type: "web_search_call",
					id: "ws_123",
					status: "completed",
					action: { type: "search", query: "OpenAI Responses API tools", queries: ["OpenAI Responses API tools"] }
				},
				{ kind: "hosted_tool", pretty_name: "Web search", input_summary: "OpenAI Responses API tools" }
			)
		]);

		expect(tools).toEqual([
			{
				id: "ws_123",
				name: "web_search",
				prettyName: "Web search",
				inputSummary: "OpenAI Responses API tools",
				provider: "openai",
				status: "completed",
				input: { type: "search", query: "OpenAI Responses API tools", queries: ["OpenAI Responses API tools"] },
				output: null
			}
		]);
	});

	it("labels OpenAI web_search open_page actions as page fetches", () => {
		const tools = hostedToolsFromReplay([
			replay(
				"openai",
				{
					type: "web_search_call",
					id: "ws_456",
					status: "completed",
					action: { type: "open_page", url: "https://platform.openai.com/docs/guides/tools-web-search" }
				},
				{ kind: "hosted_tool", pretty_name: "Open page", input_summary: "https://platform.openai.com/docs/guides/tools-web-search" }
			)
		]);

		expect(tools).toEqual([
			{
				id: "ws_456",
				name: "web_search",
				prettyName: "Open page",
				inputSummary: "https://platform.openai.com/docs/guides/tools-web-search",
				provider: "openai",
				status: "completed",
				input: { type: "open_page", url: "https://platform.openai.com/docs/guides/tools-web-search" },
				output: null
			}
		]);
	});

	it("pairs Anthropic server_tool_use blocks with hosted tool results", () => {
		const tools = hostedToolsFromReplay([
			replay(
				"claude",
				{ type: "server_tool_use", id: "srv_1", name: "web_fetch", input: { url: "https://example.com" } },
				{ kind: "hosted_tool", pretty_name: "Web fetch", input_summary: "https://example.com" }
			),
			replay("claude", { type: "web_fetch_tool_result", tool_use_id: "srv_1", content: { type: "web_fetch_tool_result_error", error_code: "url_not_accessible" } })
		]);

		expect(tools).toEqual([
			{
				id: "srv_1",
				name: "web_fetch",
				prettyName: "Web fetch",
				inputSummary: "https://example.com",
				provider: "claude",
				status: "error",
				input: { url: "https://example.com" },
				output: "url_not_accessible"
			}
		]);
	});

	it("summarizes successful Anthropic web_fetch document results", () => {
		const tools = hostedToolsFromReplay([
			replay(
				"claude",
				{ type: "server_tool_use", id: "srv_2", name: "web_fetch", input: { url: "https://example.com/" } },
				{ kind: "hosted_tool", pretty_name: "Web fetch", input_summary: "https://example.com/" }
			),
			replay("claude", {
				type: "web_fetch_tool_result",
				tool_use_id: "srv_2",
				content: {
					type: "web_fetch_result",
					url: "https://example.com/",
					content: {
						type: "document",
						title: "Example Domain",
						source: {
							type: "text",
							data: "---\ntitle: Example Domain\n---\n\n# Example Domain\n\nThis domain is for use in documentation examples."
						}
					}
				}
			})
		]);

		expect(tools[0].output).toContain("Example Domain");
		expect(tools[0].output).toContain("https://example.com/");
		expect(tools[0].output).toContain("This domain is for use in documentation examples.");
	});

	it("keeps local replay ids available for assistant tool-call interleaving", () => {
		const [functionCall, toolUse] = parsedProviderReplay([
			replay("openai", { type: "function_call", call_id: "call_1", name: "grep" }),
			replay("claude", { type: "tool_use", id: "toolu_1", name: "bash_20250124" })
		]).map((item) => item.raw);

		expect(localToolCallIdFromReplay(functionCall)).toBe("call_1");
		expect(localToolCallIdFromReplay(toolUse)).toBe("toolu_1");
	});

	it("extracts source citations from provider text replay blocks", () => {
		const citations = citationsFromReplay([
			replay("claude", {
				type: "text",
				text: "quoted source text",
				citations: [
					{
						type: "web_search_result_location",
						title: "Responses Overview | OpenAI API Reference",
						url: "https://developers.openai.com/api/reference/responses/overview",
						cited_text: "OpenAI's most advanced interface..."
					}
				]
			}),
			replay("claude", {
				type: "text",
				text: "same citation again",
				citations: [
					{
						title: "Responses Overview | OpenAI API Reference",
						url: "https://developers.openai.com/api/reference/responses/overview",
						cited_text: "OpenAI's most advanced interface..."
					}
				]
			})
		]);

		expect(citations).toEqual([
			{
				id: "source-1",
				title: "Responses Overview | OpenAI API Reference",
				url: "https://developers.openai.com/api/reference/responses/overview",
				citedText: "OpenAI's most advanced interface...",
				provider: "claude"
			}
		]);
	});
});

function replay(provider: ProviderReplayItem["provider"], raw: unknown, display?: ProviderReplayItem["display"]): ProviderReplayItem {
	return {
		provider,
		raw_json: JSON.stringify(raw),
		display
	};
}
