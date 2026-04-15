import {
	type AssistantMessage,
	type AssistantMessageEvent,
	EventStream,
	type Message,
	type Model,
	type UserMessage,
} from "@mariozechner/pi-ai";
import { Type } from "@sinclair/typebox";
import { describe, expect, it } from "vitest";
import { agentLoop, agentLoopContinue } from "../src/agent-loop.js";
import { Mailbox } from "../src/mailbox.js";
import type { MailboxItem } from "../src/mailbox-types.js";
import type { AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool } from "../src/types.js";

class MockAssistantStream extends EventStream<AssistantMessageEvent, AssistantMessage> {
	constructor() {
		super(
			(event) => event.type === "done" || event.type === "error",
			(event) => {
				if (event.type === "done") return event.message;
				if (event.type === "error") return event.error;
				throw new Error("Unexpected event type");
			},
		);
	}
}

function createUsage() {
	return {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

function createModel(): Model<"openai-responses"> {
	return {
		id: "mock",
		name: "mock",
		api: "openai-responses",
		provider: "openai",
		baseUrl: "https://example.invalid",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 8192,
		maxTokens: 2048,
	};
}

function createAssistantMessage(
	content: AssistantMessage["content"],
	stopReason: AssistantMessage["stopReason"] = "stop",
): AssistantMessage {
	return {
		role: "assistant",
		content,
		api: "openai-responses",
		provider: "openai",
		model: "mock",
		usage: createUsage(),
		stopReason,
		timestamp: Date.now(),
	};
}

function createUserMessage(text: string): UserMessage {
	return {
		role: "user",
		content: [{ type: "text", text }],
		timestamp: Date.now(),
	};
}

function convertToLlm(messages: AgentMessage[]): Message[] {
	return messages.flatMap((message) => {
		if (message.role === "user" || message.role === "assistant" || message.role === "toolResult") {
			return [message];
		}

		if (typeof message === "object" && message !== null && "role" in message && message.role === "custom") {
			const content =
				typeof message.content === "string" ? [{ type: "text" as const, text: message.content }] : message.content;
			return [
				{
					role: "user" as const,
					content,
					timestamp: message.timestamp,
				},
			];
		}

		return [];
	});
}

function createConfig(overrides: Partial<AgentLoopConfig> = {}): AgentLoopConfig {
	return {
		model: createModel(),
		convertToLlm,
		mailbox: new Mailbox<MailboxItem>(),
		backgroundAllowlist: ["bash"],
		...overrides,
	};
}

describe("agentLoop", () => {
	it("emits events and supports context transforms", async () => {
		const context: AgentContext = {
			systemPrompt: "You are helpful.",
			messages: [createUserMessage("old"), createAssistantMessage([{ type: "text", text: "old reply" }])],
			tools: [],
		};

		let transformedMessages: AgentMessage[] = [];
		let llmMessages: Message[] = [];
		const config = createConfig({
			transformContext: async (messages) => {
				transformedMessages = messages.slice(-2);
				return transformedMessages;
			},
			convertToLlm: (messages) => {
				llmMessages = convertToLlm(messages);
				return llmMessages;
			},
		});

		const stream = agentLoop([createUserMessage("hello")], context, config, undefined, () => {
			const mockStream = new MockAssistantStream();
			queueMicrotask(() => {
				mockStream.push({ type: "done", reason: "stop", message: createAssistantMessage([{ type: "text", text: "hi" }]) });
			});
			return mockStream;
		});

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		expect(transformedMessages).toHaveLength(2);
		expect(llmMessages).toHaveLength(2);
		expect(events.map((event) => event.type)).toEqual(
			expect.arrayContaining(["agent_start", "turn_start", "message_start", "message_end", "turn_end", "agent_end"]),
		);
	});

	it("executes foreground tools in parallel and emits results in source order", async () => {
		const schema = Type.Object({ value: Type.String() });
		let firstResolved = false;
		let parallelObserved = false;
		let releaseFirst: (() => void) | undefined;
		const firstDone = new Promise<void>((resolve) => {
			releaseFirst = resolve;
		});

		const tool: AgentTool<typeof schema, { value: string }> = {
			name: "echo",
			label: "Echo",
			description: "Echo",
			parameters: schema,
			async execute(_toolCallId, params) {
				if (params.value === "first") {
					await firstDone;
					firstResolved = true;
				}
				if (params.value === "second" && !firstResolved) {
					parallelObserved = true;
				}
				return {
					content: [{ type: "text", text: `echoed:${params.value}` }],
					details: { value: params.value },
				};
			},
		};

		const context: AgentContext = {
			systemPrompt: "",
			messages: [],
			tools: [tool],
		};

		let callIndex = 0;
		const stream = agentLoop([createUserMessage("run")], context, createConfig(), undefined, () => {
			const mockStream = new MockAssistantStream();
			queueMicrotask(() => {
				if (callIndex === 0) {
					mockStream.push({
						type: "done",
						reason: "toolUse",
						message: createAssistantMessage(
							[
								{ type: "toolCall", id: "tool-1", name: "echo", arguments: { value: "first" } },
								{ type: "toolCall", id: "tool-2", name: "echo", arguments: { value: "second" } },
							],
							"toolUse",
						),
					});
					setTimeout(() => releaseFirst?.(), 20);
				} else {
					mockStream.push({
						type: "done",
						reason: "stop",
						message: createAssistantMessage([{ type: "text", text: "done" }]),
					});
				}
				callIndex++;
			});
			return mockStream;
		});

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		const toolResultIds = events.flatMap((event) => {
			if (event.type !== "message_end" || event.message.role !== "toolResult") {
				return [];
			}
			return [event.message.toolCallId];
		});

		expect(parallelObserved).toBe(true);
		expect(toolResultIds).toEqual(["tool-1", "tool-2"]);
	});

	it("dispatches allowlisted tools in the background and routes completion through the mailbox", async () => {
		const schema = Type.Object({ command: Type.String() });
		let releaseTool: (() => void) | undefined;
		const toolDone = new Promise<void>((resolve) => {
			releaseTool = resolve;
		});

		const tool: AgentTool<typeof schema, { command: string }> = {
			name: "bash",
			label: "Bash",
			description: "Runs a command",
			parameters: schema,
			async execute(_toolCallId, params) {
				await toolDone;
				return {
					content: [{ type: "text", text: `finished ${params.command}` }],
					details: { command: params.command, fullOutputPath: "/tmp/tool-bg.log" },
				};
			},
		};

		const context: AgentContext = {
			systemPrompt: "",
			messages: [],
			tools: [tool],
		};

		let callIndex = 0;
		let sawPending = false;
		let sawCompletion = false;
		const stream = agentLoop([createUserMessage("start bg")], context, createConfig(), undefined, (_model, llmContext) => {
			const mockStream = new MockAssistantStream();
			queueMicrotask(() => {
				if (callIndex === 0) {
					mockStream.push({
						type: "done",
						reason: "toolUse",
						message: createAssistantMessage(
							[
								{
									type: "toolCall",
									id: "tool-bg",
									name: "bash",
									arguments: { command: "sleep 1", __background: true },
								},
							],
							"toolUse",
						),
					});
					setTimeout(() => releaseTool?.(), 10);
				} else {
					sawPending = llmContext.messages.some(
						(message) =>
							message.role === "toolResult" &&
							message.toolCallId === "tool-bg" &&
							message.content.some((block) => block.type === "text" && block.text.includes("[PENDING]")),
					);
					sawCompletion = llmContext.messages.some(
						(message) =>
							message.role === "user" &&
							Array.isArray(message.content) &&
							message.content.some((block) => block.type === "text" && block.text.includes("[Background tool completed]")) &&
							message.content.some((block) => block.type === "text" && block.text.includes("finished sleep 1")) &&
							message.content.some(
								(block) => block.type === "text" && block.text.includes("Combined stdout/stderr: /tmp/tool-bg.log"),
							),
					);
					mockStream.push({
						type: "done",
						reason: "stop",
						message: createAssistantMessage([{ type: "text", text: "background done" }]),
					});
				}
				callIndex++;
			});
			return mockStream;
		});

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}
		const messages = await stream.result();

		const turnEnds = events.filter((event): event is Extract<AgentEvent, { type: "turn_end" }> => event.type === "turn_end");
		const firstBgEnd = events.findIndex(
			(event) => event.type === "tool_execution_end" && event.toolCallId === "tool-bg",
		);
		const firstTurnEnd = events.findIndex((event) => event.type === "turn_end");
		const pendingMessage = events.find(
			(event) =>
				event.type === "message_end" &&
				event.message.role === "toolResult" &&
				event.message.toolCallId === "tool-bg" &&
				event.message.content.some((block) => block.type === "text" && block.text.includes("[PENDING]")),
		);
		const completionMessage = events.find(
			(event) =>
				event.type === "message_end" &&
				event.message.role === "custom" &&
				event.message.customType === "bg_tool_completion",
		);

		expect(turnEnds[0]?.toolResults).toHaveLength(0);
		expect(firstBgEnd).toBeGreaterThan(firstTurnEnd);
		expect(pendingMessage).toBeDefined();
		expect(completionMessage).toBeDefined();
		expect(completionMessage).toMatchObject({
			type: "message_end",
		});
		if (completionMessage?.type === "message_end" && completionMessage.message.role === "custom") {
			expect(completionMessage.message.content).toContainEqual({
				type: "text",
				text: "Combined stdout/stderr: /tmp/tool-bg.log",
			});
		}
		expect(sawPending).toBe(true);
		expect(sawCompletion).toBe(true);
		expect(messages.some((message) => message.role === "toolResult" && message.toolCallId === "tool-bg")).toBe(true);
		expect(
			messages.some(
				(message) => message.role === "custom" && message.customType === "bg_tool_completion",
			),
		).toBe(true);
	});

	it("preserves structured background tool errors with output paths", async () => {
		const schema = Type.Object({ command: Type.String() });
		let releaseTool: (() => void) | undefined;
		const toolDone = new Promise<void>((resolve) => {
			releaseTool = resolve;
		});

		const tool: AgentTool<typeof schema> = {
			name: "bash",
			label: "Bash",
			description: "Runs a command",
			parameters: schema,
			async execute() {
				await toolDone;
				const error = new Error("command failed") as Error & {
					toolResult: {
						content: Array<{ type: "text"; text: string }>;
						details: { fullOutputPath: string };
					};
				};
				error.toolResult = {
					content: [{ type: "text", text: "tail output\n\nCommand exited with code 1" }],
					details: { fullOutputPath: "/tmp/tool-bg-error.log" },
				};
				throw error;
			},
		};

		let callIndex = 0;
		let sawFailure = false;
		const stream = agentLoop(
			[createUserMessage("start bg")],
			{ systemPrompt: "", messages: [], tools: [tool] },
			createConfig(),
			undefined,
			(_model, llmContext) => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						mockStream.push({
							type: "done",
							reason: "toolUse",
							message: createAssistantMessage(
								[
									{
										type: "toolCall",
										id: "tool-bg-error",
										name: "bash",
										arguments: { command: "npm test", __background: true },
									},
								],
								"toolUse",
							),
						});
						setTimeout(() => releaseTool?.(), 10);
					} else {
						sawFailure = llmContext.messages.some(
							(message) =>
								message.role === "user" &&
								Array.isArray(message.content) &&
								message.content.some((block) => block.type === "text" && block.text.includes("[Background tool failed]")) &&
								message.content.some((block) => block.type === "text" && block.text.includes("tail output")) &&
								message.content.some(
									(block) =>
										block.type === "text" && block.text.includes("Combined stdout/stderr: /tmp/tool-bg-error.log"),
								),
						);
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "background failed" }]),
						});
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		const completionMessage = events.find(
			(event) =>
				event.type === "message_end" &&
				event.message.role === "custom" &&
				event.message.customType === "bg_tool_completion",
		);

		expect(sawFailure).toBe(true);
		expect(completionMessage).toBeDefined();
		if (completionMessage?.type === "message_end" && completionMessage.message.role === "custom") {
			expect(completionMessage.message.details.outputPath).toBe("/tmp/tool-bg-error.log");
			expect(completionMessage.message.content).toContainEqual({
				type: "text",
				text: "Combined stdout/stderr: /tmp/tool-bg-error.log",
			});
		}
	});

	it("ignores __background on tools outside the allowlist", async () => {
		const schema = Type.Object({ value: Type.String() });
		const tool: AgentTool<typeof schema, { value: string }> = {
			name: "echo",
			label: "Echo",
			description: "Echo",
			parameters: schema,
			async execute(_toolCallId, params) {
				return {
					content: [{ type: "text", text: `echoed:${params.value}` }],
					details: { value: params.value },
				};
			},
		};

		const context: AgentContext = {
			systemPrompt: "",
			messages: [],
			tools: [tool],
		};

		let callIndex = 0;
		const events: AgentEvent[] = [];
		const stream = agentLoop(
			[createUserMessage("run")],
			context,
			createConfig({ backgroundAllowlist: ["bash"] }),
			undefined,
			() => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						mockStream.push({
							type: "done",
							reason: "toolUse",
							message: createAssistantMessage(
								[
									{
										type: "toolCall",
										id: "tool-echo",
										name: "echo",
										arguments: { value: "hello", __background: true },
									},
								],
								"toolUse",
							),
						});
					} else {
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "done" }]),
						});
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		for await (const event of stream) {
			events.push(event);
		}

		expect(
			events.some(
				(event) =>
					event.type === "message_end" && event.message.role === "custom" && event.message.customType === "bg_tool_completion",
			),
		).toBe(false);
		expect(
			events.some(
				(event) =>
					event.type === "message_end" &&
					event.message.role === "toolResult" &&
					event.message.toolCallId === "tool-echo" &&
					event.message.content.some((block) => block.type === "text" && block.text === "echoed:hello"),
			),
		).toBe(true);
	});

	it("delivers follow-up messages only after the agent reaches quiescence", async () => {
		const mailbox = new Mailbox<MailboxItem>();
		const followUp = createUserMessage("follow up");
		mailbox.enqueue({ kind: "follow_up", message: followUp });

		let sawFollowUpOnFirstCall = false;
		let sawFollowUpOnSecondCall = false;
		let callIndex = 0;

		const stream = agentLoop(
			[createUserMessage("initial")],
			{ systemPrompt: "", messages: [], tools: [] },
			createConfig({ mailbox }),
			undefined,
			(_model, llmContext) => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						sawFollowUpOnFirstCall = llmContext.messages.some(
							(message) =>
								message.role === "user" &&
								Array.isArray(message.content) &&
								message.content.some((block) => block.type === "text" && block.text === "follow up"),
						);
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "first" }]),
						});
					} else {
						sawFollowUpOnSecondCall = llmContext.messages.some(
							(message) =>
								message.role === "user" &&
								Array.isArray(message.content) &&
								message.content.some((block) => block.type === "text" && block.text === "follow up"),
						);
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "second" }]),
						});
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		for await (const _event of stream) {
			// consume
		}

		expect(sawFollowUpOnFirstCall).toBe(false);
		expect(sawFollowUpOnSecondCall).toBe(true);
	});

	it("passes stripped args into beforeToolCall and afterToolCall", async () => {
		const schema = Type.Object({ command: Type.String() }, { additionalProperties: false });
		let beforeArgs: unknown;
		let afterArgs: unknown;

		const tool: AgentTool<typeof schema, { command: string }> = {
			name: "bash",
			label: "Bash",
			description: "Runs a command",
			parameters: schema,
			async execute(_toolCallId, params) {
				return {
					content: [{ type: "text", text: params.command }],
					details: { command: params.command },
				};
			},
		};

		const context: AgentContext = {
			systemPrompt: "",
			messages: [],
			tools: [tool],
		};

		let callIndex = 0;
		const stream = agentLoop(
			[createUserMessage("run")],
			context,
			createConfig({
				beforeToolCall: async ({ args }) => {
					beforeArgs = args;
					return undefined;
				},
				afterToolCall: async ({ args }) => {
					afterArgs = args;
					return undefined;
				},
			}),
			undefined,
			() => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						mockStream.push({
							type: "done",
							reason: "toolUse",
							message: createAssistantMessage(
								[
									{
										type: "toolCall",
										id: "tool-bg",
										name: "bash",
										arguments: { command: "npm test", __background: true },
									},
								],
								"toolUse",
							),
						});
					} else {
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "done" }]),
						});
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		for await (const _event of stream) {
			// consume
		}

		expect(beforeArgs).toEqual({ command: "npm test" });
		expect(afterArgs).toEqual({ command: "npm test" });
	});

	it("advertises __background only on allowlisted tools", async () => {
		const bashSchema = Type.Object({ command: Type.String() }, { additionalProperties: false });
		const readSchema = Type.Object({ path: Type.String() }, { additionalProperties: false });

		const bashTool: AgentTool<typeof bashSchema> = {
			name: "bash",
			label: "Bash",
			description: "Runs a command",
			parameters: bashSchema,
			async execute() {
				return {
					content: [{ type: "text", text: "ok" }],
					details: {},
				};
			},
		};

		const readTool: AgentTool<typeof readSchema> = {
			name: "read",
			label: "Read",
			description: "Reads a file",
			parameters: readSchema,
			async execute() {
				return {
					content: [{ type: "text", text: "ok" }],
					details: {},
				};
			},
		};

		let llmRoles: Message["role"][] | undefined;
		let llmToolParameters: Array<Record<string, unknown> | undefined> = [];

		const stream = agentLoop(
			[createUserMessage("run")],
			{ systemPrompt: "", messages: [], tools: [bashTool, readTool] },
			createConfig(),
			undefined,
			(_model, llmContext) => {
				llmRoles = llmContext.messages.map((message) => message.role);
				llmToolParameters = (llmContext.tools ?? []).map((tool) => tool.parameters as Record<string, unknown>);

				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					mockStream.push({
						type: "done",
						reason: "stop",
						message: createAssistantMessage([{ type: "text", text: "done" }]),
					});
				});
				return mockStream;
			},
		);

		for await (const _event of stream) {
			// consume
		}

		expect(llmRoles).toEqual(["user"]);
		expect(llmToolParameters).toHaveLength(2);
		expect(llmToolParameters[0]?.properties).toMatchObject({
			command: { type: "string" },
			__background: {
				type: "boolean",
			},
		});
		expect(llmToolParameters[0]?.additionalProperties).toBe(false);
		expect(llmToolParameters[1]?.properties).toMatchObject({
			path: { type: "string" },
		});
		expect((llmToolParameters[1]?.properties as Record<string, unknown>).__background).toBeUndefined();
		expect((bashTool.parameters.properties as Record<string, unknown>).__background).toBeUndefined();
	});

	it("returns an immediate error tool result when beforeToolCall blocks execution", async () => {
		const schema = Type.Object({ value: Type.String() });
		const tool: AgentTool<typeof schema, { value: string }> = {
			name: "echo",
			label: "Echo",
			description: "Echo",
			parameters: schema,
			async execute() {
				throw new Error("should not execute");
			},
		};

		let callIndex = 0;
		const stream = agentLoop(
			[createUserMessage("run")],
			{ systemPrompt: "", messages: [], tools: [tool] },
			createConfig({
				beforeToolCall: async () => ({
					block: true,
					reason: "blocked by test",
				}),
			}),
			undefined,
			() => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						mockStream.push({
							type: "done",
							reason: "toolUse",
							message: createAssistantMessage(
								[{ type: "toolCall", id: "tool-1", name: "echo", arguments: { value: "x" } }],
								"toolUse",
							),
						});
					} else {
						mockStream.push({
							type: "done",
							reason: "stop",
							message: createAssistantMessage([{ type: "text", text: "done" }]),
						});
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		expect(
			events.some(
				(event) =>
					event.type === "message_end" &&
					event.message.role === "toolResult" &&
					event.message.content.some((block) => block.type === "text" && block.text.includes("blocked by test")),
			),
		).toBe(true);
	});

	it("aborts background tools without enqueueing completion messages", async () => {
		const schema = Type.Object({ command: Type.String() });
		const tool: AgentTool<typeof schema, { command: string }> = {
			name: "bash",
			label: "Bash",
			description: "Runs a command",
			parameters: schema,
			async execute(_toolCallId, params, signal) {
				await new Promise<void>((resolve, reject) => {
					const onAbort = () => reject(new Error("aborted"));
					if (signal?.aborted) {
						onAbort();
						return;
					}
					signal?.addEventListener("abort", onAbort, { once: true });
				});
				return {
					content: [{ type: "text", text: params.command }],
					details: { command: params.command },
				};
			},
		};

		const controller = new AbortController();
		let callIndex = 0;
		const stream = agentLoop(
			[createUserMessage("run")],
			{ systemPrompt: "", messages: [], tools: [tool] },
			createConfig(),
			controller.signal,
			() => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					if (callIndex === 0) {
						mockStream.push({
							type: "done",
							reason: "toolUse",
							message: createAssistantMessage(
								[
									{
										type: "toolCall",
										id: "tool-bg",
										name: "bash",
										arguments: { command: "sleep", __background: true },
									},
								],
								"toolUse",
							),
						});
						setTimeout(() => controller.abort(), 10);
					}
					callIndex++;
				});
				return mockStream;
			},
		);

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		expect(
			events.some(
				(event) =>
					event.type === "message_end" &&
					event.message.role === "custom" &&
					event.message.customType === "bg_tool_completion",
			),
		).toBe(false);
	});
});

describe("agentLoopContinue", () => {
	it("throws when context has no messages", () => {
		expect(() =>
			agentLoopContinue(
				{
					systemPrompt: "You are helpful.",
					messages: [],
					tools: [],
				},
				createConfig(),
			),
		).toThrow("Cannot continue: no messages in context");
	});

	it("continues from existing context without replaying prior user message events", async () => {
		const stream = agentLoopContinue(
			{
				systemPrompt: "You are helpful.",
				messages: [createUserMessage("Hello")],
				tools: [],
			},
			createConfig(),
			undefined,
			() => {
				const mockStream = new MockAssistantStream();
				queueMicrotask(() => {
					mockStream.push({
						type: "done",
						reason: "stop",
						message: createAssistantMessage([{ type: "text", text: "Response" }]),
					});
				});
				return mockStream;
			},
		);

		const events: AgentEvent[] = [];
		for await (const event of stream) {
			events.push(event);
		}

		const messages = await stream.result();
		const messageEndEvents = events.filter((event) => event.type === "message_end");

		expect(messages).toHaveLength(1);
		expect(messages[0]?.role).toBe("assistant");
		expect(messageEndEvents).toHaveLength(1);
		expect((messageEndEvents[0] as Extract<AgentEvent, { type: "message_end" }>).message.role).toBe("assistant");
	});
});
