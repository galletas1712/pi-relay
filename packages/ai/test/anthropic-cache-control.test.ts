import { describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import type { Context } from "../src/types.js";

/**
 * Tests for convertMessages cache_control placement.
 *
 * We observe the serialized Anthropic request via onPayload — these tests
 * don't need a live API key because streamAnthropic throws at the HTTP
 * boundary after onPayload fires. We catch the throw and inspect the
 * captured payload.
 */

async function capturePayload(context: Context, options?: { cacheRetention?: "none" | "short" | "long" }): Promise<any> {
	const baseModel = getModel("anthropic", "claude-3-5-haiku-20241022");
	let captured: any = null;

	const { streamAnthropic } = await import("../src/providers/anthropic.js");

	try {
		const s = streamAnthropic(baseModel, context, {
			apiKey: "sk-ant-fake-key-for-payload-capture",
			cacheRetention: options?.cacheRetention,
			onPayload: (payload) => {
				captured = payload;
			},
		});
		for await (const event of s) {
			if (event.type === "error") break;
		}
	} catch {
		// Expected: auth failure. onPayload fires before the HTTP call.
	}
	return captured;
}

function hasCacheControl(block: any): boolean {
	return block && typeof block === "object" && "cache_control" in block;
}

function tailBlock(m: any): any {
	const blocks = Array.isArray(m.content) ? m.content : [m.content];
	return blocks[blocks.length - 1];
}

describe("Anthropic convertMessages cache_control placement", () => {
	it("stamps cache_control on a single user message (no regression)", async () => {
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: "hi", timestamp: 1 }],
		});
		expect(payload).not.toBeNull();
		expect(payload.messages).toHaveLength(1);
		const msg0 = payload.messages[0];
		expect(msg0.role).toBe("user");
		// Single-user case: stamp on that one message.
		expect(hasCacheControl(tailBlock(msg0))).toBe(true);
	});

	it("stamps cache_control on the last TWO user messages when present", async () => {
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [
				{ role: "user", content: "turn 1 user", timestamp: 1 },
				{
					role: "assistant",
					content: [{ type: "text", text: "turn 1 assistant" }],
					api: "anthropic-messages",
					provider: "anthropic",
					model: "claude-3-5-haiku-20241022",
					usage: {
						input: 0,
						output: 0,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					},
					stopReason: "stop",
					timestamp: 2,
				},
				{ role: "user", content: "turn 2 user", timestamp: 3 },
			],
		});
		expect(payload).not.toBeNull();
		// Messages array: user, assistant, user
		expect(payload.messages).toHaveLength(3);
		expect(payload.messages[0].role).toBe("user");
		expect(payload.messages[1].role).toBe("assistant");
		expect(payload.messages[2].role).toBe("user");

		// Both user messages get stamped.
		expect(hasCacheControl(tailBlock(payload.messages[0]))).toBe(true);
		expect(hasCacheControl(tailBlock(payload.messages[2]))).toBe(true);
		// Assistant message does NOT get stamped.
		expect(hasCacheControl(tailBlock(payload.messages[1]))).toBe(false);
	});

	it("stamps only the last two user messages when more than two user messages exist", async () => {
		const makeAssistant = (text: string, ts: number) => ({
			role: "assistant" as const,
			content: [{ type: "text" as const, text }],
			api: "anthropic-messages" as const,
			provider: "anthropic" as const,
			model: "claude-3-5-haiku-20241022",
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop" as const,
			timestamp: ts,
		});

		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [
				{ role: "user", content: "turn 1", timestamp: 1 },
				makeAssistant("a1", 2),
				{ role: "user", content: "turn 2", timestamp: 3 },
				makeAssistant("a2", 4),
				{ role: "user", content: "turn 3", timestamp: 5 },
			],
		});
		expect(payload).not.toBeNull();
		expect(payload.messages).toHaveLength(5);
		// Only the last two user messages (indices 2 and 4 in the array) get stamped.
		expect(hasCacheControl(tailBlock(payload.messages[0]))).toBe(false); // turn 1 — too old
		expect(hasCacheControl(tailBlock(payload.messages[2]))).toBe(true); // turn 2
		expect(hasCacheControl(tailBlock(payload.messages[4]))).toBe(true); // turn 3
	});

	it("omits cache_control entirely when cacheRetention is none", async () => {
		const makeAssistant = (text: string, ts: number) => ({
			role: "assistant" as const,
			content: [{ type: "text" as const, text }],
			api: "anthropic-messages" as const,
			provider: "anthropic" as const,
			model: "claude-3-5-haiku-20241022",
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop" as const,
			timestamp: ts,
		});

		const payload = await capturePayload(
			{
				systemPrompt: "sys",
				messages: [
					{ role: "user", content: "turn 1", timestamp: 1 },
					makeAssistant("a", 2),
					{ role: "user", content: "turn 2", timestamp: 3 },
				],
			},
			{ cacheRetention: "none" },
		);
		expect(payload).not.toBeNull();
		expect(hasCacheControl(tailBlock(payload.messages[0]))).toBe(false);
		expect(hasCacheControl(tailBlock(payload.messages[2]))).toBe(false);
		// And system prompt should also not have cache_control when retention=none.
		if (payload.system && Array.isArray(payload.system) && payload.system[0]) {
			expect(hasCacheControl(payload.system[0])).toBe(false);
		}
	});

	it("stamps cache_control on a coalesced tool_result user message", async () => {
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [
				{ role: "user", content: "turn 1", timestamp: 1 },
				{
					role: "assistant",
					content: [{ type: "toolCall", id: "tc1", name: "bash", arguments: { cmd: "ls" } }],
					api: "anthropic-messages",
					provider: "anthropic",
					model: "claude-3-5-haiku-20241022",
					usage: {
						input: 0,
						output: 0,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					},
					stopReason: "toolUse",
					timestamp: 2,
				},
				{
					role: "toolResult",
					toolCallId: "tc1",
					toolName: "bash",
					content: [{ type: "text", text: "file1\nfile2" }],
					isError: false,
					timestamp: 3,
				},
			],
		});
		expect(payload).not.toBeNull();
		// After coalescing: user, assistant(toolCall), user(toolResult).
		expect(payload.messages).toHaveLength(3);
		expect(payload.messages[0].role).toBe("user");
		expect(payload.messages[1].role).toBe("assistant");
		expect(payload.messages[2].role).toBe("user");
		// Both user-role messages stamped — the original user AND the coalesced tool-result user.
		expect(hasCacheControl(tailBlock(payload.messages[0]))).toBe(true);
		expect(hasCacheControl(tailBlock(payload.messages[2]))).toBe(true);
		// Tail block on the tool_result user message should be tool_result type.
		const toolResultMsg = payload.messages[2];
		const blocks = Array.isArray(toolResultMsg.content) ? toolResultMsg.content : [];
		expect(blocks[blocks.length - 1].type).toBe("tool_result");
	});
});

describe("Anthropic params.system multi-block emission", () => {
	it("emits three text blocks when systemBlocks has three retention tiers", async () => {
		const payload = await capturePayload({
			systemBlocks: [
				{ text: "tier0 body", retention: "long" },
				{ text: "tier1 body", retention: "short" },
				{ text: "tier2 body", retention: "none" },
			],
			messages: [{ role: "user", content: "hi", timestamp: 1 }],
		});
		expect(payload.system).toHaveLength(3);
		expect(payload.system[0]).toMatchObject({
			type: "text",
			text: "tier0 body",
			cache_control: { type: "ephemeral", ttl: "1h" },
		});
		expect(payload.system[1]).toMatchObject({
			type: "text",
			text: "tier1 body",
			cache_control: { type: "ephemeral" },
		});
		// Tier 1 uses default ephemeral (no ttl).
		expect(payload.system[1].cache_control).not.toHaveProperty("ttl");
		// Tier 2 has NO cache_control at all.
		expect(payload.system[2]).toMatchObject({ type: "text", text: "tier2 body" });
		expect(payload.system[2]).not.toHaveProperty("cache_control");
	});

	it("omits 1h ttl on non-api.anthropic.com baseUrl even for long-retention blocks", async () => {
		const baseModel = getModel("anthropic", "claude-3-5-haiku-20241022");
		const proxyModel = { ...baseModel, baseUrl: "https://my-proxy.example.com/v1" };
		const { streamAnthropic } = await import("../src/providers/anthropic.js");
		let captured: any = null;
		try {
			const s = streamAnthropic(
				proxyModel,
				{
					systemBlocks: [{ text: "tier0", retention: "long" }],
					messages: [{ role: "user", content: "hi", timestamp: 1 }],
				},
				{
					apiKey: "sk-ant-fake-key-for-payload-capture",
					onPayload: (p) => {
						captured = p;
					},
				},
			);
			for await (const event of s) {
				if (event.type === "error") break;
			}
		} catch {
			// Expected HTTP failure.
		}
		expect(captured).not.toBeNull();
		expect(captured.system[0].cache_control).toEqual({ type: "ephemeral" });
		expect(captured.system[0].cache_control).not.toHaveProperty("ttl");
	});

	it("disables all system-block cache_control when PI_CACHE_RETENTION=none", async () => {
		const prev = process.env.PI_CACHE_RETENTION;
		process.env.PI_CACHE_RETENTION = "none";
		try {
			const payload = await capturePayload({
				systemBlocks: [
					{ text: "tier0 body", retention: "long" },
					{ text: "tier1 body", retention: "short" },
				],
				messages: [{ role: "user", content: "hi", timestamp: 1 }],
			});
			expect(payload.system).toHaveLength(2);
			expect(payload.system[0]).not.toHaveProperty("cache_control");
			expect(payload.system[1]).not.toHaveProperty("cache_control");
		} finally {
			if (prev === undefined) delete process.env.PI_CACHE_RETENTION;
			else process.env.PI_CACHE_RETENTION = prev;
		}
	});

	it("falls back to single-block path when systemBlocks is absent", async () => {
		const payload = await capturePayload({
			systemPrompt: "legacy flat string",
			messages: [{ role: "user", content: "hi", timestamp: 1 }],
		});
		expect(payload.system).toHaveLength(1);
		expect(payload.system[0]).toMatchObject({ type: "text", text: "legacy flat string" });
		expect(payload.system[0]).toHaveProperty("cache_control");
	});

	it("tools array has no cache_control stamps (covered by Tier 0 system breakpoint)", async () => {
		const payload = await capturePayload({
			systemBlocks: [{ text: "tier0", retention: "long" }],
			messages: [{ role: "user", content: "hi", timestamp: 1 }],
			tools: [
				{
					name: "read",
					description: "read a file",
					parameters: { type: "object" as const, properties: {} } as any,
				},
				{
					name: "write",
					description: "write a file",
					parameters: { type: "object" as const, properties: {} } as any,
				},
			],
		});
		expect(payload.tools).toHaveLength(2);
		for (const tool of payload.tools) {
			expect(tool).not.toHaveProperty("cache_control");
		}
	});
});
