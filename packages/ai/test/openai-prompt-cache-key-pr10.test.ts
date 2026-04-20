import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import type { Context } from "../src/types.js";

/**
 * Tests for PR-10 `messageCacheHints.promptCacheKey`: when present, it
 * OVERRIDES the default `sessionId`-based prompt_cache_key so sibling spawns
 * with an identical leading user-message prefix share the OpenAI prefix
 * cache. When absent, behavior matches pre-PR-10 (sessionId fallback).
 */
describe("openai-responses messageCacheHints.promptCacheKey (PR-10)", () => {
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

	const baseContext: Context = {
		systemPrompt: "You are a helpful assistant.",
		messages: [{ role: "user", content: "Hello", timestamp: Date.now() }],
	};

	async function capture(opts: {
		sessionId?: string;
		cacheHints?: { promptCacheKey?: string; stableUserPrefixBytes?: number };
	}): Promise<any> {
		const { streamOpenAIResponses } = await import("../src/providers/openai-responses.js");
		const model = getModel("openai", "gpt-4.1");
		const ctx: Context = {
			...baseContext,
			...(opts.cacheHints ? { messageCacheHints: opts.cacheHints } : {}),
		};
		let captured: any = null;
		try {
			const s = streamOpenAIResponses(model, ctx, {
				apiKey: "sk-fake-key-for-payload-capture",
				sessionId: opts.sessionId,
				onPayload: (payload) => {
					captured = payload;
				},
			});
			for await (const event of s) {
				if (event.type === "error") break;
			}
		} catch {
			// expected auth failure
		}
		return captured;
	}

	it("falls back to sessionId when no hint is present", async () => {
		const payload = await capture({ sessionId: "my-session-id" });
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBe("my-session-id");
	});

	it("uses promptCacheKey hint when provided, overriding sessionId", async () => {
		const payload = await capture({
			sessionId: "my-session-id",
			cacheHints: { promptCacheKey: "root:abc123def4567890" },
		});
		expect(payload).not.toBeNull();
		expect(payload.prompt_cache_key).toBe("root:abc123def4567890");
	});

	it("two siblings with identical promptCacheKey produce identical prompt_cache_key in request body", async () => {
		const key = "parent-xyz:0123456789abcdef";
		const a = await capture({
			sessionId: "child-a-session",
			cacheHints: { promptCacheKey: key },
		});
		const b = await capture({
			sessionId: "child-b-session",
			cacheHints: { promptCacheKey: key },
		});
		expect(a.prompt_cache_key).toBe(key);
		expect(b.prompt_cache_key).toBe(key);
		expect(a.prompt_cache_key).toBe(b.prompt_cache_key);
	});

	it("PI_CACHE_RETENTION=none overrides the hint (cache disabled globally)", async () => {
		process.env.PI_CACHE_RETENTION = "none";
		const payload = await capture({
			sessionId: "my-session-id",
			cacheHints: { promptCacheKey: "root:shared" },
		});
		expect(payload.prompt_cache_key).toBeUndefined();
	});
});

describe("azure-openai-responses messageCacheHints.promptCacheKey (PR-10)", () => {
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

	async function capture(opts: {
		sessionId?: string;
		cacheHints?: { promptCacheKey?: string };
	}): Promise<any> {
		const { streamAzureOpenAIResponses } = await import("../src/providers/azure-openai-responses.js");
		const model = getModel("azure-openai-responses", "gpt-4o-mini");
		const ctx: Context = {
			systemPrompt: "sys",
			messages: [{ role: "user", content: "Hi", timestamp: Date.now() }],
			...(opts.cacheHints ? { messageCacheHints: opts.cacheHints } : {}),
		};
		let captured: any = null;
		try {
			const s = streamAzureOpenAIResponses(model, ctx, {
				apiKey: "fake-key",
				sessionId: opts.sessionId,
				azureBaseUrl: "https://fake-azure.example.com/openai/v1",
				onPayload: (payload) => {
					captured = payload;
				},
			});
			for await (const event of s) {
				if (event.type === "error") break;
			}
		} catch {
			// expected 404 / auth failure
		}
		return captured;
	}

	it("falls back to sessionId when no hint", async () => {
		const payload = await capture({ sessionId: "azure-session" });
		expect(payload?.prompt_cache_key).toBe("azure-session");
	});

	it("uses promptCacheKey hint when provided", async () => {
		const payload = await capture({
			sessionId: "azure-session",
			cacheHints: { promptCacheKey: "parent:hash" },
		});
		expect(payload?.prompt_cache_key).toBe("parent:hash");
	});
});
