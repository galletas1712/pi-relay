/**
 * Agent loop that works with AgentMessage throughout.
 * Transforms to Message[] only at the LLM call boundary.
 */

import {
	type AssistantMessage,
	type Context,
	EventStream,
	streamSimple,
	type ToolResultMessage,
	validateToolArguments,
} from "@pi-relay/ai";
import type { MailboxItem } from "./mailbox-types.js";
import { createPendingToolResult, formatBackgroundToolCompletion } from "./pending-results.js";
import type {
	AgentContext,
	AgentEvent,
	AgentLoopConfig,
	AgentMessage,
	AgentTool,
	AgentToolCall,
	AgentToolResult,
	StreamFn,
} from "./types.js";

export type AgentEventSink = (event: AgentEvent) => Promise<void> | void;

type PreparedToolCall = {
	kind: "prepared";
	toolCall: AgentToolCall;
	tool: AgentTool<any>;
	args: unknown;
	backgroundRequested: boolean;
};

type ImmediateToolCallOutcome = {
	kind: "immediate";
	toolCall: AgentToolCall;
	result: AgentToolResult<any>;
	isError: boolean;
};

type ExecutedToolCallOutcome = {
	result: AgentToolResult<any>;
	isError: boolean;
};

type FinalizedToolCallOutcome = {
	result: AgentToolResult<any>;
	isError: boolean;
};

type BackgroundToolRecord = {
	toolCallId: string;
	toolName: string;
	abortController: AbortController;
	startedAt: number;
	status: "running" | "completed" | "aborted" | "timed_out";
	outputPath?: string;
	promise: Promise<void>;
};

type Deferred = {
	promise: Promise<void>;
	resolve: () => void;
};

type ToolExecutionBatch = {
	toolResults: ToolResultMessage[];
	releaseBackgroundEvents: () => void;
};

type ExecutePreparedToolCallOptions = {
	updateGate?: Promise<void>;
	serializeUpdates?: <T>(work: () => Promise<T> | T) => Promise<T>;
	suppressUpdatesAfterAbort?: boolean;
	onUpdate?: (partialResult: AgentToolResult<any>) => void;
};

function isFollowUpItem(item: MailboxItem): boolean {
	return item.kind === "follow_up";
}

function isDeliverableItem(item: MailboxItem): boolean {
	return item.kind !== "follow_up";
}

function isAbortError(error: unknown): boolean {
	return error instanceof Error && error.name === "AbortError";
}

function createSerializedEmitter(emit: AgentEventSink): AgentEventSink {
	let chain = Promise.resolve();

	return async (event) => {
		const next = chain.then(
			() => Promise.resolve(emit(event)),
			() => Promise.resolve(emit(event)),
		);
		chain = next.then(
			() => undefined,
			() => undefined,
		);
		return next;
	};
}

function createSerialExecutor() {
	let chain = Promise.resolve();

	return async <T>(work: () => Promise<T> | T): Promise<T> => {
		const next = chain.then(
			() => Promise.resolve(work()),
			() => Promise.resolve(work()),
		);
		chain = next.then(
			() => undefined,
			() => undefined,
		);
		return next;
	};
}

function createAgentStream(): EventStream<AgentEvent, AgentMessage[]> {
	return new EventStream<AgentEvent, AgentMessage[]>(
		(event: AgentEvent) => event.type === "agent_end",
		(event: AgentEvent) => (event.type === "agent_end" ? event.messages : []),
	);
}

function createDeferred(): Deferred {
	let resolve = () => {};
	const promise = new Promise<void>((resolvePromise) => {
		resolve = resolvePromise;
	});
	return { promise, resolve };
}

function formatArgsPreview(args: unknown): string {
	if (args === undefined) {
		return "";
	}

	try {
		const rendered = JSON.stringify(args);
		if (!rendered || rendered === "{}") {
			return "";
		}
		return rendered.length > 120 ? `${rendered.slice(0, 117)}...` : rendered;
	} catch {
		return "";
	}
}

function extractOutputPath(details: unknown): string | undefined {
	if (!details || typeof details !== "object") {
		return undefined;
	}

	if ("fullOutputPath" in details && typeof details.fullOutputPath === "string") {
		return details.fullOutputPath;
	}

	if ("outputPath" in details && typeof details.outputPath === "string") {
		return details.outputPath;
	}

	return undefined;
}

function combineAbortSignals(signals: Array<AbortSignal | undefined>): AbortSignal | undefined {
	const activeSignals = signals.filter((signal): signal is AbortSignal => signal !== undefined);
	if (activeSignals.length === 0) {
		return undefined;
	}

	if (activeSignals.length === 1) {
		return activeSignals[0];
	}

	if (typeof AbortSignal.any === "function") {
		return AbortSignal.any(activeSignals);
	}

	const controller = new AbortController();
	const abort = () => controller.abort();
	for (const signal of activeSignals) {
		if (signal.aborted) {
			controller.abort();
			break;
		}
		signal.addEventListener("abort", abort, { once: true });
	}
	return controller.signal;
}

const BACKGROUND_PARAMETER_DESCRIPTION =
	"Dispatch this tool in the background. Use true only when you do not need the result before the next turn; the real result arrives later.";

function exposeBackgroundParameter(tool: AgentTool<any>): NonNullable<Context["tools"]>[number] {
	if (tool.parameters.type !== "object") {
		return tool;
	}

	const properties =
		tool.parameters.properties && typeof tool.parameters.properties === "object" ? tool.parameters.properties : {};
	if ("__background" in properties) {
		return tool;
	}

	return {
		name: tool.name,
		description: tool.description,
		parameters: {
			...tool.parameters,
			properties: {
				...properties,
				__background: {
					type: "boolean",
					description: BACKGROUND_PARAMETER_DESCRIPTION,
				},
			},
		},
	};
}

function createAdvertisedTools(tools: AgentContext["tools"], backgroundAllowlist: readonly string[]): Context["tools"] {
	if (!tools || tools.length === 0) {
		return tools;
	}

	const allowBackground = new Set(backgroundAllowlist);
	return tools.map((tool) => {
		if (!allowBackground.has(tool.name)) {
			return tool;
		}

		return exposeBackgroundParameter(tool);
	});
}

function prepareToolCallArguments(tool: AgentTool<any>, toolCall: AgentToolCall): AgentToolCall {
	if (!tool.prepareArguments) {
		return toolCall;
	}

	const preparedArguments = tool.prepareArguments(toolCall.arguments);
	if (preparedArguments === toolCall.arguments) {
		return toolCall;
	}

	return {
		...toolCall,
		arguments: preparedArguments as Record<string, any>,
	};
}

function stripRuntimeFlags(toolCall: AgentToolCall): { toolCall: AgentToolCall; backgroundRequested: boolean } {
	if (!toolCall.arguments || typeof toolCall.arguments !== "object") {
		return {
			toolCall,
			backgroundRequested: false,
		};
	}

	const { __background, ...args } = toolCall.arguments as Record<string, unknown>;
	return {
		toolCall: {
			...toolCall,
			arguments: args,
		},
		backgroundRequested: __background === true,
	};
}

/**
 * Start an agent loop with a new prompt message.
 * The prompt is added to the context and events are emitted for it.
 */
export function agentLoop(
	prompts: AgentMessage[],
	context: AgentContext,
	config: AgentLoopConfig,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): EventStream<AgentEvent, AgentMessage[]> {
	const stream = createAgentStream();

	void runAgentLoop(
		prompts,
		context,
		config,
		async (event) => {
			stream.push(event);
		},
		signal,
		streamFn,
	).then((messages) => {
		stream.end(messages);
	});

	return stream;
}

/**
 * Continue an agent loop from the current context without adding a new message.
 * Used for retries - context already has user message or tool results.
 *
 * **Important:** The last message in context must convert to a `user` or `toolResult` message
 * via `convertToLlm`. If it doesn't, the LLM provider will reject the request.
 * This cannot be validated here since `convertToLlm` is only called once per turn.
 */
export function agentLoopContinue(
	context: AgentContext,
	config: AgentLoopConfig,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): EventStream<AgentEvent, AgentMessage[]> {
	if (context.messages.length === 0) {
		throw new Error("Cannot continue: no messages in context");
	}

	if (context.messages[context.messages.length - 1].role === "assistant") {
		throw new Error("Cannot continue from message role: assistant");
	}

	const stream = createAgentStream();

	void runAgentLoopContinue(
		context,
		config,
		async (event) => {
			stream.push(event);
		},
		signal,
		streamFn,
	).then((messages) => {
		stream.end(messages);
	});

	return stream;
}

export async function runAgentLoop(
	prompts: AgentMessage[],
	context: AgentContext,
	config: AgentLoopConfig,
	emit: AgentEventSink,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): Promise<AgentMessage[]> {
	const emitSerialized = createSerializedEmitter(emit);
	const newMessages: AgentMessage[] = [...prompts];
	const currentContext: AgentContext = {
		...context,
		messages: [...context.messages, ...prompts],
	};

	await emitSerialized({ type: "agent_start" });
	await emitSerialized({ type: "turn_start" });
	for (const prompt of prompts) {
		await emitSerialized({ type: "message_start", message: prompt });
		await emitSerialized({ type: "message_end", message: prompt });
	}

	await runLoop(currentContext, newMessages, config, signal, emitSerialized, streamFn);
	return newMessages;
}

export async function runAgentLoopContinue(
	context: AgentContext,
	config: AgentLoopConfig,
	emit: AgentEventSink,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): Promise<AgentMessage[]> {
	if (context.messages.length === 0) {
		throw new Error("Cannot continue: no messages in context");
	}

	if (context.messages[context.messages.length - 1].role === "assistant") {
		throw new Error("Cannot continue from message role: assistant");
	}

	const emitSerialized = createSerializedEmitter(emit);
	const newMessages: AgentMessage[] = [];
	const currentContext: AgentContext = { ...context, messages: [...context.messages] };

	await emitSerialized({ type: "agent_start" });
	await emitSerialized({ type: "turn_start" });

	await runLoop(currentContext, newMessages, config, signal, emitSerialized, streamFn);
	return newMessages;
}

async function appendMessages(
	messages: AgentMessage[],
	currentContext: AgentContext,
	newMessages: AgentMessage[],
	emit: AgentEventSink,
): Promise<void> {
	for (const message of messages) {
		await emit({ type: "message_start", message });
		await emit({ type: "message_end", message });
		currentContext.messages.push(message);
		newMessages.push(message);
	}
}

async function waitForBackgroundToolsToSettle(pendingBgTools: Map<string, BackgroundToolRecord>): Promise<void> {
	await Promise.allSettled([...pendingBgTools.values()].map((record) => record.promise));
}

async function gatherMessagesForTurn(
	firstIteration: boolean,
	forceLlmCall: boolean,
	config: AgentLoopConfig,
	pendingBgTools: Map<string, BackgroundToolRecord>,
	signal: AbortSignal | undefined,
): Promise<{ messages: AgentMessage[]; shouldExit: boolean }> {
	if (firstIteration || forceLlmCall) {
		return {
			messages: config.mailbox.tryDrain(isDeliverableItem).map((item) => item.message),
			shouldExit: false,
		};
	}

	const immediate = config.mailbox.tryDrain(isDeliverableItem);
	if (immediate.length > 0) {
		return {
			messages: immediate.map((item) => item.message),
			shouldExit: false,
		};
	}

	if (pendingBgTools.size > 0) {
		try {
			const blocked = await config.mailbox.drain(isDeliverableItem, signal);
			return {
				messages: blocked.map((item) => item.message),
				shouldExit: blocked.length === 0 && config.mailbox.closed,
			};
		} catch (error) {
			if (isAbortError(error)) {
				return { messages: [], shouldExit: true };
			}
			throw error;
		}
	}

	const followUps = config.mailbox.tryDrain(isFollowUpItem);
	if (followUps.length > 0) {
		return {
			messages: followUps.map((item) => item.message),
			shouldExit: false,
		};
	}

	return { messages: [], shouldExit: true };
}

/**
 * Main loop logic shared by agentLoop and agentLoopContinue.
 */
async function runLoop(
	currentContext: AgentContext,
	newMessages: AgentMessage[],
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	streamFn?: StreamFn,
): Promise<void> {
	const pendingBgTools = new Map<string, BackgroundToolRecord>();
	const serializeToolHooks = createSerialExecutor();
	let firstIteration = true;
	let forceLlmCall = true;

	while (true) {
		const gathered = await gatherMessagesForTurn(firstIteration, forceLlmCall, config, pendingBgTools, signal);
		if (gathered.shouldExit) {
			if (pendingBgTools.size > 0) {
				await waitForBackgroundToolsToSettle(pendingBgTools);
			}
			break;
		}

		if (!firstIteration) {
			await emit({ type: "turn_start" });
		}
		firstIteration = false;
		forceLlmCall = false;

		if (gathered.messages.length > 0) {
			await appendMessages(gathered.messages, currentContext, newMessages, emit);
		}

		const message = await streamAssistantResponse(currentContext, config, signal, emit, streamFn);
		newMessages.push(message);

		if (message.stopReason === "error" || message.stopReason === "aborted") {
			await emit({ type: "turn_end", message, toolResults: [] });
			await waitForBackgroundToolsToSettle(pendingBgTools);
			break;
		}

		const toolCalls = message.content.filter((content): content is AgentToolCall => content.type === "toolCall");
		if (toolCalls.length === 0) {
			await emit({ type: "turn_end", message, toolResults: [] });
			continue;
		}

		const executionBatch = await executeToolCalls(
			currentContext,
			newMessages,
			message,
			toolCalls,
			config,
			pendingBgTools,
			serializeToolHooks,
			signal,
			emit,
		);

		try {
			await emit({ type: "turn_end", message, toolResults: executionBatch.toolResults });
		} finally {
			executionBatch.releaseBackgroundEvents();
		}
		forceLlmCall = executionBatch.toolResults.length > 0;
	}

	await emit({ type: "agent_end", messages: newMessages });
}

/**
 * Stream an assistant response from the LLM.
 * This is where AgentMessage[] gets transformed to Message[] for the LLM.
 */
async function streamAssistantResponse(
	context: AgentContext,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	streamFn?: StreamFn,
): Promise<AssistantMessage> {
	let messages = context.messages;
	if (config.transformContext) {
		messages = await config.transformContext(messages, signal);
	}

	const llmMessages = await config.convertToLlm(messages);
	if (llmMessages.length > 0 && llmMessages[llmMessages.length - 1]?.role === "assistant") {
		throw new Error("Invalid transcript: last LLM-visible message cannot be assistant");
	}

	const llmContext: Context = {
		systemPrompt: context.systemPrompt,
		systemBlocks: context.systemBlocks,
		messageCacheHints: context.messageCacheHints,
		messages: llmMessages,
		tools: createAdvertisedTools(context.tools, config.backgroundAllowlist),
	};

	const streamFunction = streamFn || streamSimple;
	const resolvedApiKey =
		(config.getApiKey ? await config.getApiKey(config.model.provider) : undefined) || config.apiKey;
	const { mailbox: _mailbox, backgroundAllowlist: _backgroundAllowlist, ...providerConfig } = config;

	const response = await streamFunction(config.model, llmContext, {
		...providerConfig,
		apiKey: resolvedApiKey,
		signal,
	});

	let partialMessage: AssistantMessage | null = null;
	let addedPartial = false;

	for await (const event of response) {
		switch (event.type) {
			case "start":
				partialMessage = event.partial;
				context.messages.push(partialMessage);
				addedPartial = true;
				await emit({ type: "message_start", message: { ...partialMessage } });
				break;

			case "text_start":
			case "text_delta":
			case "text_end":
			case "thinking_start":
			case "thinking_delta":
			case "thinking_end":
			case "toolcall_start":
			case "toolcall_delta":
			case "toolcall_end":
				if (partialMessage) {
					partialMessage = event.partial;
					context.messages[context.messages.length - 1] = partialMessage;
					await emit({
						type: "message_update",
						assistantMessageEvent: event,
						message: { ...partialMessage },
					});
				}
				break;

			case "done":
			case "error": {
				const finalMessage = await response.result();
				if (addedPartial) {
					context.messages[context.messages.length - 1] = finalMessage;
				} else {
					context.messages.push(finalMessage);
					await emit({ type: "message_start", message: { ...finalMessage } });
				}
				await emit({ type: "message_end", message: finalMessage });
				return finalMessage;
			}
		}
	}

	const finalMessage = await response.result();
	if (addedPartial) {
		context.messages[context.messages.length - 1] = finalMessage;
	} else {
		context.messages.push(finalMessage);
		await emit({ type: "message_start", message: { ...finalMessage } });
	}
	await emit({ type: "message_end", message: finalMessage });
	return finalMessage;
}

async function executeToolCalls(
	currentContext: AgentContext,
	newMessages: AgentMessage[],
	assistantMessage: AssistantMessage,
	toolCalls: AgentToolCall[],
	config: AgentLoopConfig,
	pendingBgTools: Map<string, BackgroundToolRecord>,
	serializeToolHooks: <T>(work: () => Promise<T> | T) => Promise<T>,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ToolExecutionBatch> {
	const allowBackground = new Set(config.backgroundAllowlist);
	const immediateToolResults: ToolResultMessage[] = [];
	const foregroundCalls: PreparedToolCall[] = [];
	const backgroundCalls: PreparedToolCall[] = [];
	const turnFinished = createDeferred();

	try {
		for (const rawToolCall of toolCalls) {
			const { toolCall, backgroundRequested } = stripRuntimeFlags(rawToolCall);
			await emit({
				type: "tool_execution_start",
				toolCallId: toolCall.id,
				toolName: toolCall.name,
				args: toolCall.arguments,
			});

			const preparation = await prepareToolCall(
				currentContext,
				assistantMessage,
				toolCall,
				backgroundRequested,
				config,
				signal,
			);

			if (preparation.kind === "immediate") {
				const finalized = await settleImmediateToolCallOutcome(preparation, emit, serializeToolHooks);
				const toolResult = await emitToolResultMessage(preparation.toolCall, finalized.result, finalized.isError, emit);
				currentContext.messages.push(toolResult);
				newMessages.push(toolResult);
				immediateToolResults.push(toolResult);
				continue;
			}

			if (preparation.backgroundRequested && allowBackground.has(preparation.toolCall.name)) {
				backgroundCalls.push(preparation);
				continue;
			}

			foregroundCalls.push(preparation);
		}

		for (const prepared of backgroundCalls) {
			const pendingResult = createPendingToolResult(
				prepared.toolCall.id,
				prepared.toolCall.name,
				formatArgsPreview(prepared.toolCall.arguments),
			);
			currentContext.messages.push(pendingResult);
			newMessages.push(pendingResult);
			await emit({ type: "message_start", message: pendingResult });
			await emit({ type: "message_end", message: pendingResult });

			const record: BackgroundToolRecord = {
				toolCallId: prepared.toolCall.id,
				toolName: prepared.toolCall.name,
				abortController: new AbortController(),
				startedAt: Date.now(),
				status: "running",
				promise: Promise.resolve(),
			};
			pendingBgTools.set(prepared.toolCall.id, record);
			await config.onBackgroundToolStart?.(
				{
					toolCallId: record.toolCallId,
					toolName: record.toolName,
					abortController: record.abortController,
					startedAt: record.startedAt,
				},
				signal,
			);
			record.promise = dispatchBackgroundTool(
				record,
				currentContext,
				assistantMessage,
				prepared,
				turnFinished.promise,
				config,
				pendingBgTools,
				serializeToolHooks,
				signal,
				emit,
			);
		}

		const executedForeground = foregroundCalls.map((prepared) => ({
			prepared,
			execution: executePreparedToolCall(prepared, signal, emit),
		}));

		const foregroundToolResults = [...immediateToolResults];
		for (const running of executedForeground) {
			const executed = await running.execution;
			const finalized = await settlePreparedToolCallOutcome(
				currentContext,
				assistantMessage,
				running.prepared,
				executed,
				config,
				signal,
				emit,
				serializeToolHooks,
			);
			const toolResult = await emitToolResultMessage(
				running.prepared.toolCall,
				finalized.result,
				finalized.isError,
				emit,
			);
			currentContext.messages.push(toolResult);
			newMessages.push(toolResult);
			foregroundToolResults.push(toolResult);
		}

		return {
			toolResults: foregroundToolResults,
			releaseBackgroundEvents: turnFinished.resolve,
		};
	} catch (error) {
		turnFinished.resolve();
		throw error;
	}
}

async function dispatchBackgroundTool(
	record: BackgroundToolRecord,
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	prepared: PreparedToolCall,
	turnFinished: Promise<void>,
	config: AgentLoopConfig,
	pendingBgTools: Map<string, BackgroundToolRecord>,
	serializeToolHooks: <T>(work: () => Promise<T> | T) => Promise<T>,
	runSignal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<void> {
	const signal = combineAbortSignals([runSignal, record.abortController.signal]);

	try {
		const executed = await executePreparedToolCall(prepared, signal, emit, {
			updateGate: turnFinished,
			serializeUpdates: serializeToolHooks,
			suppressUpdatesAfterAbort: true,
			onUpdate: (partialResult) => {
				record.outputPath ??= extractOutputPath(partialResult.details);
			},
		});
		record.outputPath ??= extractOutputPath(executed.result.details);
		await turnFinished;

		if (signal?.aborted) {
			record.status = "aborted";
			await config.onBackgroundToolEnd?.(
				{
					toolCallId: record.toolCallId,
					toolName: record.toolName,
					status: record.status,
					outputPath: record.outputPath,
				},
				signal,
			);
			return;
		}

		const finalized = await settlePreparedToolCallOutcome(
			currentContext,
			assistantMessage,
			prepared,
			executed,
			config,
			signal,
			emit,
			serializeToolHooks,
		);

		record.status = "completed";
		await config.onBackgroundToolEnd?.(
			{
				toolCallId: record.toolCallId,
				toolName: record.toolName,
				status: record.status,
				outputPath: record.outputPath,
				isError: finalized.isError,
			},
			signal,
		);
		config.mailbox.enqueue({
			kind: "tool_result",
			message: formatBackgroundToolCompletion({
				toolCallId: prepared.toolCall.id,
				toolName: prepared.toolCall.name,
				content: finalized.result.content,
				details: finalized.result.details,
				isError: finalized.isError,
				outputPath: record.outputPath,
			}),
		});
	} finally {
		pendingBgTools.delete(record.toolCallId);
	}
}

async function prepareToolCall(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	toolCall: AgentToolCall,
	backgroundRequested: boolean,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
): Promise<PreparedToolCall | ImmediateToolCallOutcome> {
	const tool = currentContext.tools?.find((candidate) => candidate.name === toolCall.name);
	if (!tool) {
		return {
			kind: "immediate",
			toolCall,
			result: createErrorToolResult(`Tool ${toolCall.name} not found`),
			isError: true,
		};
	}

	try {
		const preparedToolCall = prepareToolCallArguments(tool, toolCall);
		const validatedArgs = validateToolArguments(tool, preparedToolCall);

		if (config.beforeToolCall) {
			const beforeResult = await config.beforeToolCall(
				{
					assistantMessage,
					toolCall: preparedToolCall,
					args: validatedArgs,
					context: currentContext,
				},
				signal,
			);
			if (beforeResult?.block) {
				return {
					kind: "immediate",
					toolCall: preparedToolCall,
					result: createErrorToolResult(beforeResult.reason || "Tool execution was blocked"),
					isError: true,
				};
			}
		}

		return {
			kind: "prepared",
			toolCall: preparedToolCall,
			tool,
			args: validatedArgs,
			backgroundRequested,
		};
	} catch (error) {
		return {
			kind: "immediate",
			toolCall,
			result: createErrorToolResult(error instanceof Error ? error.message : String(error)),
			isError: true,
		};
	}
}

async function executePreparedToolCall(
	prepared: PreparedToolCall,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	options: ExecutePreparedToolCallOptions = {},
): Promise<ExecutedToolCallOutcome> {
	const updateEvents: Promise<void>[] = [];

	try {
		const result = await prepared.tool.execute(
			prepared.toolCall.id,
			prepared.args as never,
			signal,
			(partialResult) => {
				options.onUpdate?.(partialResult);
				updateEvents.push(
					Promise.resolve(options.updateGate)
						.then(async () => {
							if (options.suppressUpdatesAfterAbort && signal?.aborted) {
								return;
							}

							const emitUpdate = () =>
								emit({
									type: "tool_execution_update",
									toolCallId: prepared.toolCall.id,
									toolName: prepared.toolCall.name,
									args: prepared.toolCall.arguments,
									partialResult,
								});

							if (options.serializeUpdates) {
								await options.serializeUpdates(emitUpdate);
								return;
							}

							await emitUpdate();
						}),
				);
			},
		);
		await Promise.all(updateEvents);
		return { result, isError: false };
	} catch (error) {
		await Promise.all(updateEvents);
		return {
			result: getToolErrorResult(error),
			isError: true,
		};
	}
}

async function settleImmediateToolCallOutcome(
	outcome: ImmediateToolCallOutcome,
	emit: AgentEventSink,
	serializeToolHooks: <T>(work: () => Promise<T> | T) => Promise<T>,
): Promise<FinalizedToolCallOutcome> {
	return serializeToolHooks(async () => {
		await emit({
			type: "tool_execution_end",
			toolCallId: outcome.toolCall.id,
			toolName: outcome.toolCall.name,
			result: outcome.result,
			isError: outcome.isError,
		});

		return {
			result: outcome.result,
			isError: outcome.isError,
		};
	});
}

async function settlePreparedToolCallOutcome(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	prepared: PreparedToolCall,
	executed: ExecutedToolCallOutcome,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	serializeToolHooks: <T>(work: () => Promise<T> | T) => Promise<T>,
): Promise<FinalizedToolCallOutcome> {
	return serializeToolHooks(async () => {
		let result = executed.result;
		let isError = executed.isError;

		if (config.afterToolCall) {
			const afterResult = await config.afterToolCall(
				{
					assistantMessage,
					toolCall: prepared.toolCall,
					args: prepared.args,
					result,
					isError,
					context: currentContext,
				},
				signal,
			);

			if (afterResult) {
				result = {
					content: afterResult.content ?? result.content,
					details: afterResult.details ?? result.details,
				};
				isError = afterResult.isError ?? isError;
			}
		}

		await emit({
			type: "tool_execution_end",
			toolCallId: prepared.toolCall.id,
			toolName: prepared.toolCall.name,
			result,
			isError,
		});

		return { result, isError };
	});
}

function createErrorToolResult(message: string): AgentToolResult<any> {
	return {
		content: [{ type: "text", text: message }],
		details: {},
	};
}

function getToolErrorResult(error: unknown): AgentToolResult<any> {
	if (typeof error === "object" && error !== null && "toolResult" in error) {
		const toolResult = (error as { toolResult?: unknown }).toolResult;
		if (
			typeof toolResult === "object" &&
			toolResult !== null &&
			"content" in toolResult &&
			Array.isArray((toolResult as { content?: unknown[] }).content)
		) {
			return toolResult as AgentToolResult<any>;
		}
	}

	return createErrorToolResult(error instanceof Error ? error.message : String(error));
}

async function emitToolResultMessage(
	toolCall: AgentToolCall,
	result: AgentToolResult<any>,
	isError: boolean,
	emit: AgentEventSink,
): Promise<ToolResultMessage> {
	const toolResultMessage: ToolResultMessage = {
		role: "toolResult",
		toolCallId: toolCall.id,
		toolName: toolCall.name,
		content: result.content,
		details: result.details,
		isError,
		timestamp: Date.now(),
	};

	await emit({ type: "message_start", message: toolResultMessage });
	await emit({ type: "message_end", message: toolResultMessage });
	return toolResultMessage;
}
