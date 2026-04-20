import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import type { Context } from "../src/types.js";

/**
 * Offline payload-capture tests for OpenAI Chat Completions `prompt_cache_key`
 * plumbing. These tests construct a fake API key, intercept the request
 * payload via `onPayload`, and then discard the HTTP 401 that inevitably
 * follows. No live API credentials are needed.
 */
describe("openai-completions prompt_cache_key", () => {
	const originalEnv = process.env.PI_CACHE_RETENTION;

	beforeEach(() => {
		delete process.env.PI_CACHE_RETENTION;
	});

	afterEach(() => {
		if (originalEnv !== undefined) {
			process.env.PI_CACHE_RETENTION = originalEnv;
		} else {
			delete process.env.PI_CACHE_RETENTION;
		}
	});

	const context: Context = {
		systemPrompt: "You are a helpful assistant.",
		messages: [{ role: "user", content: "Hello", timestamp: Date.now() }],
	};

	async function capturePayload(opts: {
		sessionId?: string;
		cacheRetention?: "none" | "short" | "long";
	}): Promise<any> {
		const { streamOpenAICompletions } = await import("../src/providers/openai-completions.js");
		// `gpt-oss-120b` on Cerebras is an `openai-completions` API model, used
		// here purely for its Model<"openai-completions"> shape; no network
		// round-trip occurs because the fake API key yields a 401.
		const model = getModel("cerebras", "gpt-oss-120b");

		let captured: any = null;
		try {
			const s = streamOpenAICompletions(model, context, {
				apiKey: "sk-fake-key-for-payload-capture",
				sessionId: opts.sessionId,
				cacheRetention: opts.cacheRetention,
				onPayload: (payload) => {
					captured = payload;
				},
			});
			for await (const event of s) {
				if (event.type === "error") break;
			}
		} catch {
			// Expected to fail on the HTTP round-trip.
		}
		return captured;
	}

	it("stamps prompt_cache_key from sessionId when provided", async () => {
		const payload = await capturePayload({ sessionId: "session-xyz" });
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBe("session-xyz");
	});

	it("omits prompt_cache_key when sessionId is absent", async () => {
		const payload = await capturePayload({});
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBeUndefined();
	});

	it("omits prompt_cache_key when PI_CACHE_RETENTION=none", async () => {
		process.env.PI_CACHE_RETENTION = "none";
		const payload = await capturePayload({ sessionId: "session-xyz" });
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBeUndefined();
	});

	it("omits prompt_cache_key when cacheRetention option is 'none'", async () => {
		const payload = await capturePayload({ sessionId: "session-xyz", cacheRetention: "none" });
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBeUndefined();
	});
});
