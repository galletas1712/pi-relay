/**
 * web_search provider backed by the OpenAI Codex web_search API.
 *
 * Ported to @pi-relay/tool-kit's ToolProvider surface. Behavior is
 * unchanged from the classic registerTool version: it piggybacks on the
 * currently selected openai-codex model via
 * ModelRegistry.getApiKeyAndHeaders, rejects non-codex models, and emits
 * the same tool result shape (answer text + sourceUrls) so renderers,
 * tests, and prompts stay compatible.
 *
 * Auth model: relies on the selected model's credentials. No secret spec
 * here — `pi.registerToolProvider` sees an empty `secrets` array and never
 * throws ToolConfigMissingError. `perplexity-sonar.ts` is the contrasting
 * example.
 *
 * Provider id:  com.openai.codex.web-search
 * Implements:   web_search
 *
 * Provider ids are internal; the LLM only ever sees the user-chosen tool
 * name (defaults to `web_search` when this is the sole provider for the
 * interface, or whatever key the user writes in `pi.configureTools({...})`).
 */

import { createHash } from "node:crypto";
import { type Api, type AssistantMessage, completeSimple, type Model, StringEnum } from "@pi-relay/ai";
import type { ExtensionAPI } from "@pi-relay/coding-agent";
import { defineToolProvider, type ToolCallContext, type ToolHost } from "@pi-relay/tool-kit";
import { Text } from "@pi-relay/tool-kit/render";
import { Type } from "@sinclair/typebox";

type WebSearchReasoningEffort = "low" | "medium" | "high" | "xhigh";
type WebSearchContextSize = "low" | "medium" | "high";

type WebSearchToolParams = {
	query: string;
	reasoning_effort?: WebSearchReasoningEffort;
};

type WebSearchToolDetails = {
	provider: "openai-codex";
	model: string;
	query: string;
	reasoningEffort: WebSearchReasoningEffort;
	serviceTier: "priority";
	sourceUrls: string[];
};

type CodexWebSearchTool = {
	type: "web_search";
	external_web_access: boolean;
	search_context_size: WebSearchContextSize;
};

const SEARCH_SYSTEM_PROMPT = [
	"Use the native web_search tool before answering.",
	"Answer the user's query concisely and factually.",
	"If the search results are unclear, conflicting, or insufficient, say so plainly.",
	"Do not include source URLs in the visible answer.",
	"After the answer, append a final hidden block exactly named <pi-web-sources> containing one source URL per line and no other text.",
].join("\n");

function resolveCodexModel(host: ToolHost): Model<Api> {
	const ref = host.getModel();
	if (!ref) {
		throw new Error("web_search requires an active model in the current pi session.");
	}
	if (ref.provider !== "openai-codex") {
		throw new Error(
			`web_search requires the current pi session to use an openai-codex model. Current model: ${ref.provider}/${ref.id}.`,
		);
	}
	// ref.native is the host's native Model<Api> (see coding-agent host.ts).
	return ref.native as Model<Api>;
}

function buildWebSearchSessionId(host: ToolHost, cwd: string, modelId: string): string {
	// Fall back to cwd when the host doesn't expose a sessionManager via native.
	const native = host.native as
		| { sessionManager?: { getSessionFile?: () => string | undefined } }
		| undefined;
	const sessionFile =
		typeof native?.sessionManager?.getSessionFile === "function"
			? native.sessionManager.getSessionFile()
			: undefined;
	const scope = sessionFile || cwd;
	const digest = createHash("sha1").update(`${modelId}\0${scope}`).digest("hex");
	return `web-search-${digest.slice(0, 16)}`;
}

function buildNestedQuery(query: string): string {
	return query.trim();
}

function createWebSearchTool(searchContextSize: WebSearchContextSize): CodexWebSearchTool {
	return {
		type: "web_search",
		external_web_access: true,
		search_context_size: searchContextSize,
	};
}

function patchCodexPayload(payload: unknown, searchContextSize: WebSearchContextSize): Record<string, unknown> {
	const body = { ...(payload as Record<string, unknown>) };
	body.tools = [createWebSearchTool(searchContextSize)];
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

const parametersSchema = Type.Object({
	query: Type.String({ minLength: 1, description: "The web search query." }),
	reasoning_effort: Type.Optional(
		StringEnum(["low", "medium", "high", "xhigh"] as const, {
			description: "Optional reasoning depth for the nested Codex web-search request.",
		}),
	),
});

// Renderers keep their original shapes. `Theme` lives in @pi-relay/coding-agent
// and deliberately isn't re-exported from @pi-relay/tool-kit/render, so we
// accept an opaque `ThemeShim` to avoid pulling coding-agent into the
// tool-kit author graph. Authors that want full typing can
// `import type { Theme } from "@pi-relay/coding-agent"` directly.
// biome-ignore lint/suspicious/noExplicitAny: intentional shim, see above.
type ThemeShim = any;

function renderCall(args: WebSearchToolParams, theme: ThemeShim) {
	const reasoningEffort = args.reasoning_effort ?? "medium";
	return new Text(theme.fg("toolTitle", theme.bold(`web_search (${reasoningEffort})`)), 0, 0);
}

function renderResult(
	result: { content: Array<{ type: string; text?: string }>; details?: WebSearchToolDetails | undefined },
	{ expanded, isPartial }: { expanded: boolean; isPartial: boolean },
	theme: ThemeShim,
) {
	const answer = result.content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("\n")
		.trim();
	if (isPartial) {
		return new Text(theme.fg("warning", answer || "Searching the web..."), 0, 0);
	}

	const details = result.details;
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
}

const codexWebSearchProvider = defineToolProvider<Record<string, never>, Record<string, never>, typeof parametersSchema, WebSearchToolDetails>({
	id: "com.openai.codex.web-search",
	implements: "web_search",
	displayName: "OpenAI Codex Web Search",
	version: "0.1.0",
	description: "Search the web via OpenAI Codex's native web_search tool.",
	// Prompt guidelines are inherited from the `web_search` ToolInterface. The
	// codex impl has no provider-specific guidance to add on top.
	parameters: parametersSchema,
	renderCall: renderCall as unknown as never,
	renderResult: renderResult as unknown as never,
	async execute(params: WebSearchToolParams, ctx: ToolCallContext<WebSearchToolParams>) {
		const model = resolveCodexModel(ctx.host);
		const auth = await ctx.host.getApiKey("openai-codex");
		if (!auth.ok) {
			throw getMissingCodexAuthError(auth.error);
		}
		if (!auth.apiKey) {
			throw getMissingCodexAuthError();
		}

		const reasoningEffort = params.reasoning_effort ?? "medium";
		const searchContextSize: WebSearchContextSize =
			reasoningEffort === "xhigh" ? "high" : reasoningEffort;
		ctx.onUpdate?.({
			content: [{ type: "text", text: "Searching the web..." }],
			details: {
				provider: "openai-codex",
				model: model.id,
				query: params.query,
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
						content: [{ type: "text", text: buildNestedQuery(params.query) }],
						timestamp: Date.now(),
					},
				],
			},
			{
				apiKey: auth.apiKey,
				headers: auth.headers,
				signal: ctx.signal,
				transport: "auto",
				sessionId: buildWebSearchSessionId(ctx.host, ctx.cwd, model.id),
				reasoning: reasoningEffort,
				onPayload: (payload) => patchCodexPayload(payload, searchContextSize),
			},
		);

		if (response.stopReason === "aborted" || ctx.signal?.aborted) {
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
				reasoningEffort,
				serviceTier: "priority",
				sourceUrls,
			} satisfies WebSearchToolDetails,
		};
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerToolProvider(codexWebSearchProvider);
}
