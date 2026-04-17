import { Type } from "@sinclair/typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getModel } from "../src/models.js";
import type { Context, Model, Tool } from "../src/types.js";

const mockState = vi.hoisted(() => ({
	constructorOpts: undefined as Record<string, unknown> | undefined,
	streamParams: undefined as Record<string, unknown> | undefined,
}));

vi.mock("@anthropic-ai/sdk", () => {
	const fakeStream = {
		async *[Symbol.asyncIterator]() {
			yield {
				type: "message_start",
				message: {
					id: "msg_test",
					usage: { input_tokens: 10, output_tokens: 0 },
				},
			};
			yield {
				type: "message_delta",
				delta: { stop_reason: "end_turn" },
				usage: { output_tokens: 5 },
			};
		},
		finalMessage: async () => ({
			usage: { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
		}),
	};

	class FakeAnthropic {
		constructor(opts: Record<string, unknown>) {
			mockState.constructorOpts = opts;
		}
		messages = {
			stream: (params: Record<string, unknown>) => {
				mockState.streamParams = params;
				return fakeStream;
			},
		};
	}

	return { default: FakeAnthropic };
});

const dummyTool: Tool = {
	name: "read",
	description: "Read a file.",
	parameters: Type.Object({ path: Type.String() }),
};

function createContext(): Context {
	return {
		systemPrompt: "Keep replies short.",
		messages: [{ role: "user", content: "Please inspect src/main.ts", timestamp: Date.now() }],
		tools: [dummyTool],
	};
}

async function drain(stream: AsyncIterable<{ type: string }>): Promise<void> {
	for await (const event of stream) {
		if (event.type === "error") break;
	}
}

describe.sequential("Anthropic Claude Code envelope", () => {
	afterEach(() => {
		mockState.constructorOpts = undefined;
		mockState.streamParams = undefined;
		vi.unstubAllGlobals();
	});

	it("adds Claude Code identity headers, metadata, and prompt attribution for direct Anthropic requests", async () => {
		const model = getModel("anthropic", "claude-sonnet-4-5");
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		await drain(streamAnthropic(model, createContext(), { apiKey: "sk-ant-test", sessionId: "session-123" }));

		const headers = mockState.constructorOpts?.defaultHeaders as Record<string, string>;
		expect(headers["x-app"]).toBe("cli");
		expect(headers["User-Agent"]).toBe("claude-cli/2.1.75 (external, cli)");
		expect(headers["X-Claude-Code-Session-Id"]).toBe("session-123");
		expect(headers["x-client-request-id"]).toBeTruthy();
		expect(headers["anthropic-beta"]).toContain("claude-code-20250219");
		expect(headers["anthropic-beta"]).toContain("context-management-2025-06-27");
		expect(headers["anthropic-beta"]).toContain("interleaved-thinking-2025-05-14");
		expect(headers["anthropic-beta"]).not.toContain("context-1m-2025-08-07");

		const params = mockState.streamParams as {
			system?: Array<{ text: string }>;
			metadata?: { user_id?: string };
		};
		expect(params.system?.[0]?.text).toMatch(
			/^x-anthropic-billing-header: cc_version=2\.1\.75\.[0-9a-f]{3}; cc_entrypoint=cli;$/,
		);
		expect(params.system?.[1]?.text).toBe("You are Claude Code, Anthropic's official CLI for Claude.");
		expect(params.system?.[2]?.text).toBe("Keep replies short.");
		const metadata = JSON.parse(params.metadata?.user_id ?? "{}") as Record<string, string>;
		expect(metadata.session_id).toBe("session-123");
		expect(metadata.account_uuid).toBe("");
		expect(typeof metadata.device_id).toBe("string");
		expect(metadata.device_id.length).toBeGreaterThan(0);
	});

	it("adds the 1M context beta only for 1M Anthropic models", async () => {
		const model = getModel("anthropic", "claude-sonnet-4-6");
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		await drain(streamAnthropic(model, createContext(), { apiKey: "sk-ant-test", sessionId: "session-123" }));

		const headers = mockState.constructorOpts?.defaultHeaders as Record<string, string>;
		expect(headers["anthropic-beta"]).toContain("context-1m-2025-08-07");
		expect(headers["anthropic-beta"]).toContain("context-management-2025-06-27");
	});

	it("keeps proxy Anthropic requests on the minimal envelope", async () => {
		const proxyModel = {
			...getModel("anthropic", "claude-sonnet-4-5"),
			baseUrl: "https://proxy.example.com/anthropic",
		} as Model<"anthropic-messages">;
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		await drain(streamAnthropic(proxyModel, createContext(), { apiKey: "sk-ant-test", sessionId: "session-123" }));

		const headers = mockState.constructorOpts?.defaultHeaders as Record<string, string>;
		expect(headers["User-Agent"]).toBeUndefined();
		expect(headers["x-app"]).toBeUndefined();
		expect(headers["X-Claude-Code-Session-Id"]).toBeUndefined();
		expect(headers["x-client-request-id"]).toBeUndefined();
		expect(headers["anthropic-beta"]).not.toContain("claude-code-20250219");
		expect(headers["anthropic-beta"]).not.toContain("context-management-2025-06-27");
		expect(headers["anthropic-beta"]).not.toContain("context-1m-2025-08-07");

		const params = mockState.streamParams as {
			system?: Array<{ text: string }>;
			metadata?: { user_id?: string };
		};
		expect(params.system?.[0]?.text).toBe("Keep replies short.");
		expect(params.metadata).toBeUndefined();
	});
});
