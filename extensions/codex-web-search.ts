import { createHash } from "node:crypto";
import { completeSimple, StringEnum, type AssistantMessage } from "@mariozechner/pi-ai";
import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { Text } from "@mariozechner/pi-tui";
import { Type } from "@sinclair/typebox";

type WebSearchReasoningEffort = "low" | "medium" | "high" | "xhigh";
type WebSearchContextSize = "low" | "medium" | "high";

type WebSearchToolParams = {
	query: string;
	allowed_domains?: string[];
	reasoning_effort?: WebSearchReasoningEffort;
};

type WebSearchToolDetails = {
	provider: "openai-codex";
	model: string;
	query: string;
	allowedDomains?: string[];
	reasoningEffort: WebSearchReasoningEffort;
	serviceTier: "priority";
	sourceUrls: string[];
};

type CodexWebSearchTool = {
	type: "web_search";
	external_web_access: boolean;
	search_context_size: WebSearchContextSize;
	filters?: {
		allowed_domains: string[];
	};
};

const SEARCH_SYSTEM_PROMPT = [
	"Use the native web_search tool before answering.",
	"Answer the user's query concisely and factually.",
	"If the search results are unclear, conflicting, or insufficient, say so plainly.",
	"Do not include source URLs in the visible answer.",
	"After the answer, append a final hidden block exactly named <pi-web-sources> containing one source URL per line and no other text.",
].join("\n");

function resolveCurrentCodexModel(ctx: ExtensionContext) {
	if (!ctx.model) {
		throw new Error("web_search requires an active model in the current pi session.");
	}
	if (ctx.model.provider !== "openai-codex") {
		throw new Error(
			`web_search requires the current pi session to use an openai-codex model. Current model: ${ctx.model.provider}/${ctx.model.id}.`,
		);
	}
	return ctx.model;
}

function buildWebSearchSessionId(ctx: ExtensionContext, modelId: string): string {
	const sessionFile = typeof ctx.sessionManager?.getSessionFile === "function" ? ctx.sessionManager.getSessionFile() : undefined;
	const scope = sessionFile || ctx.cwd;
	const digest = createHash("sha1").update(`${modelId}\0${scope}`).digest("hex");
	return `web-search-${digest.slice(0, 16)}`;
}

function normalizeAllowedDomains(rawDomains: string[] | undefined): string[] | undefined {
	if (!rawDomains || rawDomains.length === 0) {
		return undefined;
	}

	const normalizedDomains: string[] = [];
	const seen = new Set<string>();

	for (const rawDomain of rawDomains) {
		const trimmed = rawDomain.trim().toLowerCase();
		if (!trimmed) {
			continue;
		}

		let normalized = trimmed;
		try {
			const candidate = trimmed.includes("://") ? trimmed : `https://${trimmed}`;
			normalized = new URL(candidate).hostname.toLowerCase();
		} catch {
			normalized = trimmed.replace(/^https?:\/\//, "").split("/")[0]?.trim().toLowerCase() ?? "";
		}

		normalized = normalized.replace(/\.+$/, "");
		if (!normalized || seen.has(normalized)) {
			continue;
		}

		seen.add(normalized);
		normalizedDomains.push(normalized);
	}

	return normalizedDomains.length > 0 ? normalizedDomains : undefined;
}

function buildNestedQuery(query: string, allowedDomains: string[] | undefined): string {
	const lines = [query.trim()];
	if (allowedDomains && allowedDomains.length > 0) {
		lines.push(`Use only these domains: ${allowedDomains.join(", ")}.`);
		lines.push("If those domains do not contain enough information, say so plainly.");
	}
	return lines.join("\n\n");
}

function createWebSearchTool(
	allowedDomains: string[] | undefined,
	searchContextSize: WebSearchContextSize,
): CodexWebSearchTool {
	if (allowedDomains && allowedDomains.length > 0) {
		return {
			type: "web_search",
			external_web_access: true,
			search_context_size: searchContextSize,
			filters: { allowed_domains: allowedDomains },
		};
	}

	return {
		type: "web_search",
		external_web_access: true,
		search_context_size: searchContextSize,
	};
}

function patchCodexPayload(
	payload: unknown,
	allowedDomains: string[] | undefined,
	searchContextSize: WebSearchContextSize,
): Record<string, unknown> {
	const body = { ...(payload as Record<string, unknown>) };
	body.tools = [createWebSearchTool(allowedDomains, searchContextSize)];
	body.tool_choice = "auto";
	body.service_tier = "priority";
	return body;
}

function extractText(message: AssistantMessage): string {
	return message.content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("\n")
		.trim();
}

function normalizeExtractedUrl(rawUrl: string): string | undefined {
	const trimmed = rawUrl.trim().replace(/^[(<\[]+/, "").replace(/[)>\].,!?:;]+$/, "");
	if (!trimmed) {
		return undefined;
	}

	try {
		const url = new URL(trimmed);
		if (url.protocol !== "http:" && url.protocol !== "https:") {
			return undefined;
		}
		return url.toString();
	} catch {
		return undefined;
	}
}

function getWebSourcesBody(text: string): string | undefined {
	const openMatch = /<pi-web-sources>/i.exec(text);
	if (!openMatch) {
		return undefined;
	}

	const afterOpen = text.slice(openMatch.index + openMatch[0].length);
	const closeMatch = /<\/pi-web-sources>/i.exec(afterOpen);
	return closeMatch ? afterOpen.slice(0, closeMatch.index).trim() : afterOpen.trim();
}

function hasWebSourcesTag(text: string): boolean {
	return getWebSourcesBody(text) !== undefined;
}

function extractWebSourceUrls(text: string): string[] {
	const body = getWebSourcesBody(text);
	if (body === undefined) {
		return [];
	}

	const sourceUrls: string[] = [];
	const seen = new Set<string>();
	for (const line of body.split(/\r?\n/)) {
		const candidate = normalizeExtractedUrl(line);
		if (!candidate || seen.has(candidate)) {
			continue;
		}
		seen.add(candidate);
		sourceUrls.push(candidate);
	}
	return sourceUrls;
}

function stripWebSourcesTag(text: string): string {
	const openMatch = /<pi-web-sources>/i.exec(text);
	if (!openMatch) {
		return text.trim();
	}
	return text.slice(0, openMatch.index).trim();
}

function getMissingCodexAuthError(detail?: string): Error {
	const prefix = "web_search requires pi openai-codex OAuth. Run /login and select OpenAI Codex.";
	return detail ? new Error(`${prefix} ${detail}`) : new Error(prefix);
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "web_search",
		label: "Web Search",
		description: "search the web",
		promptGuidelines: [
			"Use this tool when the user needs current information, release notes, docs, or other knowledge outside the repo.",
			"Prefer this tool over ad-hoc bash or manual browser-style web lookup for normal web research.",
		],
		parameters: Type.Object({
			query: Type.String({ minLength: 1, description: "The web search query." }),
			allowed_domains: Type.Optional(
				Type.Array(Type.String({ minLength: 1 }), {
					description: "Optional list of domains to restrict results to, such as react.dev or docs.python.org.",
				}),
			),
			reasoning_effort: Type.Optional(
				StringEnum(["low", "medium", "high", "xhigh"] as const, {
					description: "Optional reasoning depth for the nested Codex web-search request.",
				}),
			),
		}),
		renderCall(args, theme) {
			const reasoningEffort = args.reasoning_effort ?? "medium";
			return new Text(theme.fg("toolTitle", theme.bold(`web_search (${reasoningEffort})`)), 0, 0);
		},
		renderResult(result, { expanded, isPartial }, theme) {
			const answer = result.content
				.filter((block): block is { type: "text"; text: string } => block.type === "text")
				.map((block) => block.text)
				.join("\n")
				.trim();
			if (isPartial) {
				return new Text(theme.fg("warning", answer || "Searching the web..."), 0, 0);
			}

			const details = result.details as WebSearchToolDetails | undefined;
			const sourceUrls = details?.sourceUrls ?? [];
			const answerLines = answer.split("\n");
			const visibleAnswer = expanded ? answer : answerLines.slice(0, 6).join("\n").trim();

			let text = visibleAnswer || theme.fg("dim", "(no answer text returned)");
			if (!expanded && answerLines.length > 6) {
				text += `\n${theme.fg("muted", "...")}`;
			}

			if (sourceUrls.length > 0) {
				text += `\n\n${theme.fg("accent", `${sourceUrls.length} source${sourceUrls.length === 1 ? "" : "s"}`)}`;
				if (expanded) {
					for (const url of sourceUrls) {
						text += `\n${theme.fg("dim", url)}`;
					}
				}
			}

			return new Text(text, 0, 0);
		},
		async execute(_toolCallId, params: WebSearchToolParams, signal, onUpdate, ctx) {
			const model = resolveCurrentCodexModel(ctx);
			const auth = await ctx.modelRegistry.getApiKeyAndHeaders(model);
			if (!auth.ok) {
				throw getMissingCodexAuthError(auth.error);
			}
			if (!auth.apiKey) {
				throw getMissingCodexAuthError();
			}

			const allowedDomains = normalizeAllowedDomains(params.allowed_domains);
			const reasoningEffort = params.reasoning_effort ?? "medium";
			const searchContextSize: WebSearchContextSize = reasoningEffort === "xhigh" ? "high" : reasoningEffort;
			onUpdate?.({
				content: [
					{
						type: "text",
						text:
							allowedDomains && allowedDomains.length > 0
								? `Searching in ${allowedDomains.join(", ")}...`
								: "Searching the web...",
					},
				],
				details: {
					provider: "openai-codex",
					model: model.id,
					query: params.query,
					allowedDomains,
					reasoningEffort,
					serviceTier: "priority",
					sourceUrls: [],
				} satisfies WebSearchToolDetails,
			});

			const response = await completeSimple(
				model,
				{
					systemPrompt: SEARCH_SYSTEM_PROMPT,
					messages: [
						{
							role: "user",
							content: [{ type: "text", text: buildNestedQuery(params.query, allowedDomains) }],
							timestamp: Date.now(),
						},
					],
				},
				{
					apiKey: auth.apiKey,
					headers: auth.headers,
					signal,
					transport: "auto",
					sessionId: buildWebSearchSessionId(ctx, model.id),
					reasoning: reasoningEffort,
					onPayload: (payload) => patchCodexPayload(payload, allowedDomains, searchContextSize),
				},
			);

			if (response.stopReason === "aborted" || signal?.aborted) {
				throw new Error("Web search was aborted.");
			}
			if (response.stopReason === "error") {
				throw new Error(response.errorMessage || "Web search failed.");
			}

			const rawAnswer = extractText(response);
			if (!rawAnswer) {
				throw new Error("Codex web search returned no answer text.");
			}

			const hasSourcesTag = hasWebSourcesTag(rawAnswer);
			const sourceUrls = hasSourcesTag ? extractWebSourceUrls(rawAnswer) : [];
			const answer = hasSourcesTag ? stripWebSourcesTag(rawAnswer) : rawAnswer.trim();
			return {
				content: [{ type: "text", text: answer }],
				details: {
					provider: "openai-codex",
					model: model.id,
					query: params.query,
					allowedDomains,
					reasoningEffort,
					serviceTier: "priority",
					sourceUrls,
				} satisfies WebSearchToolDetails,
			};
		},
	});
}
