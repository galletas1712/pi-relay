import type { AssistantMessage, ImageContent } from "@pi-relay/ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentSessionEvent } from "../src/core/agent-session.js";
import { runPrintMode } from "../src/modes/print-mode.js";

type EmitEvent = { type: string };

type FakeExtensionRunner = {
	hasHandlers: (eventType: string) => boolean;
	emit: ReturnType<typeof vi.fn<(event: EmitEvent) => Promise<void>>>;
};

type FakeSession = {
	sessionManager: { getHeader: () => object | undefined };
	agent: { waitForIdle: () => Promise<void> };
	state: { messages: AssistantMessage[] };
	extensionRunner: FakeExtensionRunner;
	bindExtensions: ReturnType<typeof vi.fn>;
	subscribe: ReturnType<typeof vi.fn>;
	prompt: ReturnType<typeof vi.fn>;
	reload: ReturnType<typeof vi.fn>;
	getSubtreeUsage?: () => undefined;
	/** Captures the latest subscriber so tests can inject events. */
	_latestListener?: (event: AgentSessionEvent) => void;
};

type FakeRuntimeHost = {
	session: FakeSession;
	newSession: ReturnType<typeof vi.fn>;
	fork: ReturnType<typeof vi.fn>;
	switchSession: ReturnType<typeof vi.fn>;
	dispose: ReturnType<typeof vi.fn>;
};

function createAssistantMessage(options?: {
	text?: string;
	stopReason?: AssistantMessage["stopReason"];
	errorMessage?: string;
}): AssistantMessage {
	return {
		role: "assistant",
		content: options?.text ? [{ type: "text", text: options.text }] : [],
		api: "openai-responses",
		provider: "openai",
		model: "gpt-4o-mini",
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: options?.stopReason ?? "stop",
		errorMessage: options?.errorMessage,
		timestamp: Date.now(),
	};
}

function createRuntimeHost(assistantMessage: AssistantMessage): FakeRuntimeHost {
	const extensionRunner: FakeExtensionRunner = {
		hasHandlers: (eventType: string) => eventType === "session_shutdown",
		emit: vi.fn(async () => {}),
	};

	const state = { messages: [assistantMessage] };

	const session: FakeSession = {
		sessionManager: { getHeader: () => undefined },
		agent: { waitForIdle: async () => {} },
		state,
		extensionRunner,
		bindExtensions: vi.fn(async () => {}),
		subscribe: vi.fn((listener: (event: AgentSessionEvent) => void) => {
			session._latestListener = listener;
			return () => {
				if (session._latestListener === listener) session._latestListener = undefined;
			};
		}),
		prompt: vi.fn(async () => {}),
		reload: vi.fn(async () => {}),
		getSubtreeUsage: () => undefined,
	};

	return {
		session,
		newSession: vi.fn(async () => undefined),
		fork: vi.fn(async () => ({ selectedText: "" })),
		switchSession: vi.fn(async () => undefined),
		dispose: vi.fn(async () => {
			await session.extensionRunner.emit({ type: "session_shutdown" });
		}),
	};
}

afterEach(() => {
	vi.restoreAllMocks();
});

describe("runPrintMode", () => {
	it("emits session_shutdown in text mode", async () => {
		const runtimeHost = createRuntimeHost(createAssistantMessage({ text: "done" }));
		const { session } = runtimeHost;
		const images: ImageContent[] = [{ type: "image", mimeType: "image/png", data: "abc" }];

		const exitCode = await runPrintMode(runtimeHost as unknown as Parameters<typeof runPrintMode>[0], {
			mode: "text",
			initialMessage: "Say done",
			initialImages: images,
		});

		expect(exitCode).toBe(0);
		expect(session.prompt).toHaveBeenCalledWith("Say done", { images });
		expect(session.extensionRunner.emit).toHaveBeenCalledTimes(1);
		expect(session.extensionRunner.emit).toHaveBeenCalledWith({ type: "session_shutdown" });
	});

	it("emits session_shutdown in json mode", async () => {
		const runtimeHost = createRuntimeHost(createAssistantMessage({ text: "done" }));
		const { session } = runtimeHost;

		const exitCode = await runPrintMode(runtimeHost as unknown as Parameters<typeof runPrintMode>[0], {
			mode: "json",
			messages: ["hello"],
		});

		expect(exitCode).toBe(0);
		expect(session.prompt).toHaveBeenCalledWith("hello");
		expect(session.extensionRunner.emit).toHaveBeenCalledTimes(1);
		expect(session.extensionRunner.emit).toHaveBeenCalledWith({ type: "session_shutdown" });
	});

	it("emits per-background-call cache stderr lines with scope when enabled", async () => {
		const runtimeHost = createRuntimeHost(createAssistantMessage({ text: "done" }));
		const { session } = runtimeHost;
		const stderrWrites: string[] = [];
		const stderrSpy = vi.spyOn(process.stderr, "write").mockImplementation(((chunk: string) => {
			stderrWrites.push(chunk.toString());
			return true;
		}) as typeof process.stderr.write);

		const prior = process.env.PI_SHOW_CACHE_STATS;
		process.env.PI_SHOW_CACHE_STATS = "1";

		// Inject a background_usage event *during* session.prompt so it fires
		// after runPrintMode has subscribed.
		session.prompt = vi.fn(async () => {
			session._latestListener?.({
				type: "background_usage",
				scope: "worklog",
				usage: {
					input: 200,
					output: 80,
					cacheRead: 300,
					cacheWrite: 20,
					totalTokens: 600,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
			});
			session._latestListener?.({
				type: "background_usage",
				scope: "compaction",
				usage: {
					input: 4000,
					output: 300,
					cacheRead: 0,
					cacheWrite: 4000,
					totalTokens: 8300,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
			});
		});

		try {
			const exitCode = await runPrintMode(runtimeHost as unknown as Parameters<typeof runPrintMode>[0], {
				mode: "text",
				initialMessage: "hi",
			});
			expect(exitCode).toBe(0);
		} finally {
			if (prior === undefined) delete process.env.PI_SHOW_CACHE_STATS;
			else process.env.PI_SHOW_CACHE_STATS = prior;
			stderrSpy.mockRestore();
		}

		const lines = stderrWrites.filter((l) => l.startsWith("[pi:cache]"));
		expect(lines).toHaveLength(2);
		expect(lines[0]).toBe(
			"[pi:cache] turn=0 worklog cacheRead=300 cacheWrite=20 input=200 output=80\n",
		);
		expect(lines[1]).toBe(
			"[pi:cache] turn=0 compaction cacheRead=0 cacheWrite=4000 input=4000 output=300\n",
		);
	});

	it("does not emit per-background-call lines when PI_SHOW_CACHE_STATS is unset", async () => {
		const runtimeHost = createRuntimeHost(createAssistantMessage({ text: "done" }));
		const { session } = runtimeHost;
		const stderrWrites: string[] = [];
		const stderrSpy = vi.spyOn(process.stderr, "write").mockImplementation(((chunk: string) => {
			stderrWrites.push(chunk.toString());
			return true;
		}) as typeof process.stderr.write);

		const prior = process.env.PI_SHOW_CACHE_STATS;
		delete process.env.PI_SHOW_CACHE_STATS;

		session.prompt = vi.fn(async () => {
			session._latestListener?.({
				type: "background_usage",
				scope: "branch",
				usage: {
					input: 100,
					output: 50,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 150,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
			});
		});

		try {
			await runPrintMode(runtimeHost as unknown as Parameters<typeof runPrintMode>[0], {
				mode: "text",
				initialMessage: "hi",
			});
		} finally {
			if (prior !== undefined) process.env.PI_SHOW_CACHE_STATS = prior;
			stderrSpy.mockRestore();
		}

		const lines = stderrWrites.filter((l) => l.startsWith("[pi:cache]"));
		expect(lines).toHaveLength(0);
	});

	it("emits session_shutdown and returns non-zero on assistant error", async () => {
		const runtimeHost = createRuntimeHost(
			createAssistantMessage({ stopReason: "error", errorMessage: "provider failure" }),
		);
		const { session } = runtimeHost;
		const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

		const exitCode = await runPrintMode(runtimeHost as unknown as Parameters<typeof runPrintMode>[0], {
			mode: "text",
		});

		expect(exitCode).toBe(1);
		expect(errorSpy).toHaveBeenCalledWith("provider failure");
		expect(session.extensionRunner.emit).toHaveBeenCalledTimes(1);
		expect(session.extensionRunner.emit).toHaveBeenCalledWith({ type: "session_shutdown" });
	});
});
