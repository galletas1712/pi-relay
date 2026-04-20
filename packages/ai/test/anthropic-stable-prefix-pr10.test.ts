import { describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import type { Context } from "../src/types.js";

/**
 * Tests for the PR-10 `messageCacheHints.stableUserPrefixBytes` breakpoint
 * placement on the FIRST user message. Intercepts the serialized Anthropic
 * request body via `onPayload`; no live API key is needed.
 */

async function capturePayload(context: Context): Promise<any> {
	const baseModel = getModel("anthropic", "claude-3-5-haiku-20241022");
	let captured: any = null;

	const { streamAnthropic } = await import("../src/providers/anthropic.js");

	try {
		const s = streamAnthropic(baseModel, context, {
			apiKey: "sk-ant-fake-key-for-payload-capture",
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

function countEphemeralBreakpoints(blocks: any[]): number {
	return blocks.filter((b) => b && typeof b === "object" && "cache_control" in b).length;
}

describe("Anthropic messageCacheHints.stableUserPrefixBytes (PR-10)", () => {
	it("no hint: first user message emits a single text block (no extra breakpoint)", async () => {
		const body = "x".repeat(4000);
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: body, timestamp: 1 }],
		});
		expect(payload).not.toBeNull();
		expect(payload.messages).toHaveLength(1);
		const blocks = payload.messages[0].content;
		// One text block (no split).
		expect(Array.isArray(blocks) ? blocks.length : 1).toBe(1);
	});

	it("hint present: splits first user message text block on byte boundary with cache_control", async () => {
		const header = "A".repeat(2048); // exactly 2048 bytes ASCII
		const tail = "B".repeat(500);
		const text = header + tail;
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: text, timestamp: 1 }],
			messageCacheHints: { stableUserPrefixBytes: 2048 },
		});
		expect(payload).not.toBeNull();
		const firstUserMsg = payload.messages[0];
		expect(Array.isArray(firstUserMsg.content)).toBe(true);
		const contentBlocks = firstUserMsg.content as any[];
		expect(contentBlocks).toHaveLength(2);
		expect(contentBlocks[0].type).toBe("text");
		expect(contentBlocks[0].text).toBe(header);
		expect(contentBlocks[0].cache_control).toEqual({ type: "ephemeral" });
		expect(contentBlocks[1].type).toBe("text");
		expect(contentBlocks[1].text).toBe(tail);
		// NOTE: the tail block may ALSO receive a cache_control from the
		// last-two-user-message stamping logic in convertMessages; the
		// invariant we care about here is that the PREFIX block has
		// ephemeral cache_control AND the text split is correct. The tail's
		// stamping is orthogonal and covered by the existing cache-control
		// placement tests.
	});

	it("hint below minimum: ignored (no split)", async () => {
		const text = "A".repeat(4000);
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: text, timestamp: 1 }],
			messageCacheHints: { stableUserPrefixBytes: 512 }, // < 1024 threshold
		});
		expect(payload).not.toBeNull();
		const firstUserMsg = payload.messages[0];
		// Not split — string content passes through as string.
		expect(typeof firstUserMsg.content === "string" || (Array.isArray(firstUserMsg.content) && firstUserMsg.content.length === 1)).toBe(true);
	});

	it("hint larger than text: no split", async () => {
		const text = "A".repeat(1500);
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: text, timestamp: 1 }],
			messageCacheHints: { stableUserPrefixBytes: 10_000 },
		});
		expect(payload).not.toBeNull();
		const firstUserMsg = payload.messages[0];
		expect(typeof firstUserMsg.content === "string" || (Array.isArray(firstUserMsg.content) && firstUserMsg.content.length === 1)).toBe(true);
	});

	it("UTF-8 boundary: snaps DOWN to keep prefix well-formed (no mid-codepoint split)", async () => {
		// Emoji 🚀 is 4 bytes in UTF-8 (F0 9F 9A 80). Construct a string where
		// the requested byte offset falls in the middle of the emoji. The
		// implementation must snap down to the byte BEFORE the emoji.
		const lead = "A".repeat(2047); // 2047 bytes
		const emoji = "🚀"; // 4 bytes → starts at byte index 2047
		const tail = "B".repeat(100);
		const text = lead + emoji + tail;
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: text, timestamp: 1 }],
			// Ask for 2049 bytes — this falls inside the emoji's 4-byte sequence
			// (which starts at byte 2047 and ends at byte 2050).
			messageCacheHints: { stableUserPrefixBytes: 2049 },
		});
		expect(payload).not.toBeNull();
		const firstUserMsg = payload.messages[0];
		expect(Array.isArray(firstUserMsg.content)).toBe(true);
		const blocks = firstUserMsg.content as any[];
		expect(blocks).toHaveLength(2);
		// The prefix must NOT include any part of the emoji — snap down to
		// byte 2047, so prefix is exactly `lead`.
		expect(blocks[0].text).toBe(lead);
		expect(blocks[0].cache_control).toEqual({ type: "ephemeral" });
		// Tail receives the full emoji plus `B...`.
		expect(blocks[1].text.startsWith(emoji)).toBe(true);
	});

	it("hint applies only to the FIRST user message, not later ones", async () => {
		const body1 = "A".repeat(2048);
		const body2 = "C".repeat(2048);
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [
				{ role: "user", content: body1, timestamp: 1 },
				{
					role: "assistant",
					content: [{ type: "text", text: "ack" }],
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
				{ role: "user", content: body2, timestamp: 3 },
			],
			messageCacheHints: { stableUserPrefixBytes: 1024 },
		});
		expect(payload).not.toBeNull();
		// First user message: split into 2 blocks.
		expect(Array.isArray(payload.messages[0].content)).toBe(true);
		expect((payload.messages[0].content as any[]).length).toBe(2);
		// Second user message: NOT split by the hint. (The last-two-user-msg
		// cache_control logic may still stamp it, but the content should remain
		// a single block, not two text blocks created by our split helper.)
		const secondUser = payload.messages[2];
		const secondContent = secondUser.content;
		// String or single-block content is the signal it wasn't split.
		const secondLen = Array.isArray(secondContent) ? secondContent.length : 1;
		expect(secondLen).toBe(1);
	});

	it("stableUserPrefixBytes of 0: treated as absent (no split)", async () => {
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [{ role: "user", content: "A".repeat(4000), timestamp: 1 }],
			messageCacheHints: { stableUserPrefixBytes: 0 },
		});
		expect(payload).not.toBeNull();
		const firstUserMsg = payload.messages[0];
		expect(typeof firstUserMsg.content === "string" || (Array.isArray(firstUserMsg.content) && firstUserMsg.content.length === 1)).toBe(true);
	});

	it("when split is applied, the stable-prefix breakpoint counts toward the 4 allowed breakpoints (no crash)", async () => {
		// Smoke test: build a conversation with enough user messages that
		// the last-two-user-msg stamping AND our prefix stamping both fire.
		// Total breakpoints shouldn't exceed Anthropic's limit of 4 per request.
		const first = "A".repeat(2048);
		const mid = "M".repeat(100);
		const last = "Z".repeat(100);
		const payload = await capturePayload({
			systemPrompt: "sys",
			messages: [
				{ role: "user", content: first, timestamp: 1 },
				{
					role: "assistant",
					content: [{ type: "text", text: "ack" }],
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
				{ role: "user", content: mid, timestamp: 3 },
				{
					role: "assistant",
					content: [{ type: "text", text: "ack2" }],
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
					timestamp: 4,
				},
				{ role: "user", content: last, timestamp: 5 },
			],
			messageCacheHints: { stableUserPrefixBytes: 1024 },
		});
		expect(payload).not.toBeNull();
		// Count cache_control breakpoints across ALL content blocks in ALL messages.
		let total = 0;
		for (const msg of payload.messages) {
			const blocks = Array.isArray(msg.content) ? msg.content : [msg.content];
			total += countEphemeralBreakpoints(blocks);
		}
		// Prefix breakpoint (1) + last-two-user-msg (2) = up to 3. Must fit
		// within Anthropic's 4-per-request limit regardless.
		expect(total).toBeLessThanOrEqual(4);
		expect(total).toBeGreaterThanOrEqual(1);
	});
});
