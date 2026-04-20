/**
 * web_search provider backed by Perplexity Sonar.
 *
 * Provider id:     com.perplexity.sonar
 * Implements:      web_search
 * Secret:          apiKey (env var fallback: PERPLEXITY_API_KEY)
 * Default model:   sonar-pro (override via per-tool config.model).
 *
 * Provider ids are internal; the LLM only ever sees the user-chosen tool
 * name (defaults to `web_search` when this is the sole provider for the
 * interface, or whatever key the user writes in `pi.configureTools({...})`).
 *
 * The request body was upgraded based on an audit of how openclaw
 * (openclaw/openclaw, extensions/perplexity) calls Perplexity's
 * chat-completions API. Notable additions vs. the naive v0 implementation:
 *
 *   - New per-call parameter: `recency` maps to Perplexity's
 *     `search_recency_filter`.
 *   - `sonar-reasoning-pro` is a selectable model.
 *   - Request timeout (30s) composed with `ctx.signal` via `AbortSignal.any`.
 *   - Attribution headers (`HTTP-Referer`, `X-Title`).
 *   - Status-specific error mapping for 401/403/429.
 *
 * `search_context_size` was dropped from the config schema: the
 * chat-completions endpoint doesn't honor it, and silently ignoring a
 * user-visible config is worse than not exposing it.
 *
 * Milestone 1 wiring:
 * - Config values are the provider defaults unless overridden via
 *   `pi.configureTools({ <toolName>: { provider: ..., config: {...} } })`.
 * - Secret is resolved from process.env[PERPLEXITY_API_KEY]; if unset, the
 *   adapter throws ToolConfigMissingError at call time.
 */

import type { ExtensionAPI } from "@pi-relay/coding-agent";
import { defineToolProvider, type ToolCallContext } from "@pi-relay/tool-kit";
import { Text } from "@pi-relay/tool-kit/render";
import { Type } from "@sinclair/typebox";

type PerplexityModel = "sonar" | "sonar-pro" | "sonar-reasoning" | "sonar-reasoning-pro";
type RecencyFilter = "day" | "week" | "month" | "year";

interface PerplexityConfig {
	model: PerplexityModel;
}

interface PerplexitySecrets {
	apiKey: string;
}

interface PerplexityParams {
	query: string;
	recency?: RecencyFilter;
}

interface PerplexityCitation {
	url: string;
	title?: string;
}

interface PerplexityDetails {
	provider: "perplexity";
	model: PerplexityModel;
	query: string;
	citations: PerplexityCitation[];
}

const PERPLEXITY_ENDPOINT = "https://api.perplexity.ai/chat/completions";
const REQUEST_TIMEOUT_MS = 30_000;

const parametersSchema = Type.Object({
	query: Type.String({ minLength: 1, description: "The web search query." }),
	recency: Type.Optional(
		Type.Union(
			[Type.Literal("day"), Type.Literal("week"), Type.Literal("month"), Type.Literal("year")],
			{ description: "Restrict results to the last day / week / month / year." },
		),
	),
});

// biome-ignore lint/suspicious/noExplicitAny: Theme shim; see codex-web-search for rationale.
type ThemeShim = any;

function renderCall(args: PerplexityParams, theme: ThemeShim) {
	return new Text(theme.fg("toolTitle", theme.bold(`perplexity_search: ${args.query}`)), 0, 0);
}

function renderResult(
	result: { content: Array<{ type: string; text?: string }>; details?: PerplexityDetails | undefined },
	{ expanded, isPartial }: { expanded: boolean; isPartial: boolean },
	theme: ThemeShim,
) {
	const answer = result.content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("\n")
		.trim();
	if (isPartial) {
		return new Text(theme.fg("warning", answer || "Querying Perplexity..."), 0, 0);
	}

	const citations = result.details?.citations ?? [];
	const answerLines = answer.split("\n");
	const visibleAnswer = expanded ? answer : answerLines.slice(0, 6).join("\n").trim();
	let text = visibleAnswer || theme.fg("dim", "(no answer text returned)");
	if (!expanded && answerLines.length > 6) {
		text += `\n${theme.fg("muted", "...")}`;
	}
	if (citations.length > 0) {
		const summary = expanded
			? citations.map((c) => c.url).join("\n")
			: uniqueDomains(citations).slice(0, 5).join(", ");
		text += `\n\n${theme.fg("accent", `${citations.length} citation${citations.length === 1 ? "" : "s"}`)}`;
		if (summary) text += `\n${theme.fg("dim", summary)}`;
	}
	return new Text(text, 0, 0);
}

function uniqueDomains(citations: PerplexityCitation[]): string[] {
	const seen = new Set<string>();
	const out: string[] = [];
	for (const c of citations) {
		try {
			const host = new URL(c.url).host;
			if (!seen.has(host)) {
				seen.add(host);
				out.push(host);
			}
		} catch {
			// skip malformed URLs
		}
	}
	return out;
}

function extractCitations(data: unknown): PerplexityCitation[] {
	// Perplexity's response body exposes "citations" (string URLs) alongside
	// the OpenAI-style message. Newer versions also expose "search_results"
	// with {url,title}. Prefer the richer form when present; fall back to
	// raw citations otherwise.
	const body = data as { citations?: unknown; search_results?: unknown } | undefined;
	if (Array.isArray(body?.search_results)) {
		const out: PerplexityCitation[] = [];
		for (const r of body.search_results) {
			if (r && typeof r === "object") {
				const rec = r as Record<string, unknown>;
				if (typeof rec.url === "string") {
					out.push({
						url: rec.url,
						title: typeof rec.title === "string" ? rec.title : undefined,
					});
				}
			}
		}
		return out;
	}
	if (Array.isArray(body?.citations)) {
		return body!.citations
			.filter((u: unknown): u is string => typeof u === "string")
			.map((url: string) => ({ url }));
	}
	return [];
}

function extractAnswer(data: unknown): string {
	const body = data as { choices?: Array<{ message?: { content?: unknown } }> } | undefined;
	const first = body?.choices?.[0]?.message?.content;
	if (typeof first === "string") return first;
	if (Array.isArray(first)) {
		// Defensive: some APIs emit chunked content arrays.
		return first
			.map((chunk) =>
				chunk && typeof chunk === "object" && typeof (chunk as Record<string, unknown>).text === "string"
					? (chunk as Record<string, string>).text
					: "",
			)
			.join("")
			.trim();
	}
	return "";
}

/**
 * Compose the tool's turn-abort signal with a local request-timeout signal.
 * Uses the standard `AbortSignal.any` (Node 20+). Falls back to a manual
 * linker on older Node runtimes so the provider doesn't take a runtime dep.
 */
function composeSignals(base: AbortSignal | undefined, timeoutMs: number): { signal: AbortSignal; cleanup: () => void } {
	const local = new AbortController();
	const timer = setTimeout(() => local.abort(new Error(`Perplexity request timed out after ${timeoutMs}ms`)), timeoutMs);

	// biome-ignore lint/suspicious/noExplicitAny: AbortSignal.any is part of the Node 20+ runtime surface but not in all lib.dom.d.ts.
	const anyFn = (AbortSignal as any).any as ((signals: AbortSignal[]) => AbortSignal) | undefined;
	let signal: AbortSignal;
	if (base && typeof anyFn === "function") {
		signal = anyFn([base, local.signal]);
	} else if (base) {
		// Polyfill: abort local when base aborts, so fetch only sees one signal.
		if (base.aborted) local.abort(base.reason);
		else base.addEventListener("abort", () => local.abort(base.reason), { once: true });
		signal = local.signal;
	} else {
		signal = local.signal;
	}
	return { signal, cleanup: () => clearTimeout(timer) };
}

function mapStatusError(status: number, statusText: string, body: string): Error {
	if (status === 401 || status === 403) {
		return new Error("Perplexity auth failed. Check PERPLEXITY_API_KEY.");
	}
	if (status === 429) {
		return new Error("Perplexity rate-limited. Retry later.");
	}
	const tail = body ? ` — ${body.slice(0, 400)}` : "";
	return new Error(`Perplexity search failed: HTTP ${status} ${statusText}${tail}`);
}

const perplexitySonarProvider = defineToolProvider<PerplexityConfig, PerplexitySecrets, typeof parametersSchema, PerplexityDetails>({
	id: "com.perplexity.sonar",
	implements: "web_search",
	displayName: "Perplexity Sonar",
	version: "0.1.0",
	description: "Search the web via Perplexity Sonar and return a cited answer.",
	// Prompt guidelines are inherited from the `web_search` ToolInterface. The
	// Perplexity-specific "cited answer" framing is intentionally not added
	// here because the LLM already gets the `recency` / `domainFilter`
	// parameter descriptions and citation info is in the result shape.
	// Provider-specific prompt hints would leak the provider identity.

	configSchema: Type.Object({
		model: Type.Union(
			[
				Type.Literal("sonar"),
				Type.Literal("sonar-pro"),
				Type.Literal("sonar-reasoning"),
				Type.Literal("sonar-reasoning-pro"),
			],
			{
				description:
					"Perplexity Sonar model id. `sonar` / `sonar-pro` are general search; `sonar-reasoning*` adds chain-of-thought.",
			},
		),
	}),
	defaultConfig: { model: "sonar-pro" },
	secrets: [
		{
			key: "apiKey",
			displayName: "Perplexity API Key",
			kind: "api_key",
			envVar: "PERPLEXITY_API_KEY",
			description: "Create at https://www.perplexity.ai/settings/api",
		},
	],
	parameters: parametersSchema,
	renderCall: renderCall as unknown as never,
	renderResult: renderResult as unknown as never,
	async execute(
		params: PerplexityParams,
		ctx: ToolCallContext<PerplexityParams, PerplexityConfig, PerplexitySecrets>,
	) {
		ctx.onUpdate?.({
			content: [{ type: "text", text: "Querying Perplexity..." }],
			details: {
				provider: "perplexity",
				model: ctx.config.model,
				query: params.query,
				citations: [],
			} satisfies PerplexityDetails,
		});

		const body: Record<string, unknown> = {
			model: ctx.config.model,
			messages: [{ role: "user", content: params.query }],
		};
		if (params.recency) body.search_recency_filter = params.recency;

		const { signal, cleanup } = composeSignals(ctx.signal, REQUEST_TIMEOUT_MS);
		let response: Response;
		try {
			response = await ctx.host.http(PERPLEXITY_ENDPOINT, {
				method: "POST",
				headers: {
					Authorization: `Bearer ${ctx.secrets.apiKey}`,
					"Content-Type": "application/json",
					// Attribution for Perplexity's logs / dashboards. Mirrors
					// what OpenRouter / other brokers expect; harmless to send
					// to api.perplexity.ai.
					"HTTP-Referer": "https://github.com/galletas1712/pi-relay",
					"X-Title": "pi-relay web_search",
				},
				body: JSON.stringify(body),
				signal,
			});
		} finally {
			cleanup();
		}

		if (!response.ok) {
			const errText = await response.text().catch(() => "");
			throw mapStatusError(response.status, response.statusText, errText);
		}

		const data = await response.json();
		const answer = extractAnswer(data);
		const citations = extractCitations(data);

		if (!answer) {
			throw new Error("Perplexity returned no answer text.");
		}

		return {
			content: [{ type: "text", text: answer }],
			details: {
				provider: "perplexity",
				model: ctx.config.model,
				query: params.query,
				citations,
			} satisfies PerplexityDetails,
		};
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerToolProvider(perplexitySonarProvider);
}
