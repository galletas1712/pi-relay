/**
 * Print mode (single-shot): Send prompts, output result, exit.
 *
 * Used for:
 * - `pi -p "prompt"` - text output
 * - `pi --mode json "prompt"` - JSON event stream
 */

import type { AssistantMessage, ImageContent } from "@pi-relay/ai";
import type { AgentSessionRuntime } from "../core/agent-session-runtime.js";
import { formatCacheLogLine } from "../core/cache-telemetry.js";
import { flushRawStdout, writeRawStdout } from "../core/output-guard.js";
import { killTrackedDetachedChildren } from "../utils/shell.js";

/**
 * Options for print mode.
 */
export interface PrintModeOptions {
	/** Output mode: "text" for final response only, "json" for all events */
	mode: "text" | "json";
	/** Array of additional prompts to send after initialMessage */
	messages?: string[];
	/** First message to send (may contain @file content) */
	initialMessage?: string;
	/** Images to attach to the initial message */
	initialImages?: ImageContent[];
}

/**
 * Run in print (single-shot) mode.
 * Sends prompts to the agent and outputs the result.
 */
export async function runPrintMode(runtimeHost: AgentSessionRuntime, options: PrintModeOptions): Promise<number> {
	const { mode, messages = [], initialMessage, initialImages } = options;
	let exitCode = 0;
	let session = runtimeHost.session;
	let unsubscribe: (() => void) | undefined;
	let disposed = false;
	let turnCounter = 0;
	const signalCleanupHandlers: Array<() => void> = [];

	const disposeRuntime = async (): Promise<void> => {
		if (disposed) return;
		disposed = true;
		unsubscribe?.();
		await runtimeHost.dispose();
	};

	const registerSignalHandlers = (): void => {
		const signals: NodeJS.Signals[] = ["SIGTERM"];
		if (process.platform !== "win32") {
			signals.push("SIGHUP");
		}

		for (const signal of signals) {
			const handler = () => {
				killTrackedDetachedChildren();
				void disposeRuntime().finally(() => {
					process.exit(signal === "SIGHUP" ? 129 : 143);
				});
			};
			process.on(signal, handler);
			signalCleanupHandlers.push(() => process.off(signal, handler));
		}
	};

	registerSignalHandlers();

	const rebindSession = async (): Promise<void> => {
		session = runtimeHost.session;
		await session.bindExtensions({
			commandContextActions: {
				waitForIdle: () => session.agent.waitForIdle(),
				newSession: async (newSessionOptions) => {
					const result = await runtimeHost.newSession(newSessionOptions);
					if (!result.cancelled) {
						await rebindSession();
					}
					return result;
				},
				fork: async (entryId) => {
					const result = await runtimeHost.fork(entryId);
					if (!result.cancelled) {
						await rebindSession();
					}
					return { cancelled: result.cancelled };
				},
				navigateTree: async (targetId, navigateOptions) => {
					const result = await session.navigateTree(targetId, {
						summarize: navigateOptions?.summarize,
						customInstructions: navigateOptions?.customInstructions,
						replaceInstructions: navigateOptions?.replaceInstructions,
						label: navigateOptions?.label,
					});
					return { cancelled: result.cancelled };
				},
				switchSession: async (sessionPath) => {
					const result = await runtimeHost.switchSession(sessionPath);
					if (!result.cancelled) {
						await rebindSession();
					}
					return result;
				},
				reload: async () => {
					await session.reload();
				},
			},
			onError: (err) => {
				console.error(`Extension error (${err.extensionPath}): ${err.error}`);
			},
		});

		unsubscribe?.();
		unsubscribe = session.subscribe((event) => {
			if (mode === "json") {
				writeRawStdout(`${JSON.stringify(event)}\n`);
			}
			// Dev-only per-turn cache telemetry: emits on every completed assistant
			// turn. Written to stderr so stdout stays pristine for text-mode piping
			// and JSON-mode parsing. Gated on the effective show-stats flag
			// (env `PI_SHOW_CACHE_STATS` with a `settings.cache.showStats` fallback).
			//
			// When the session's attached agent has descendants (tracked by an
			// orchestrator-style extension via setSubtreeUsageProvider), emit an
			// additional `tree` line with subtree-aggregated usage. Single-agent
			// sessions and agents without descendants stay on the existing format.
			if (
				session.settingsManager.getShowCacheStats() &&
				event.type === "message_end" &&
				event.message.role === "assistant"
			) {
				turnCounter += 1;
				const subtree = session.getSubtreeUsage();
				const showTree = subtree?.hasDescendants === true;
				if (showTree && subtree) {
					process.stderr.write(
						`${formatCacheLogLine({
							turn: turnCounter,
							scope: "self",
							usage: {
								input: subtree.self.tokens.input,
								output: subtree.self.tokens.output,
								cacheRead: subtree.self.tokens.cacheRead,
								cacheWrite: subtree.self.tokens.cacheWrite,
							},
						})}\n`,
					);
					process.stderr.write(
						`${formatCacheLogLine({
							turn: turnCounter,
							scope: "tree",
							usage: {
								input: subtree.tree.tokens.input,
								output: subtree.tree.tokens.output,
								cacheRead: subtree.tree.tokens.cacheRead,
								cacheWrite: subtree.tree.tokens.cacheWrite,
							},
						})}\n`,
					);
				} else {
					process.stderr.write(
						`${formatCacheLogLine({ turn: turnCounter, usage: event.message.usage })}\n`,
					);
				}
			}
		});
	};

	try {
		if (mode === "json") {
			const header = session.sessionManager.getHeader();
			if (header) {
				writeRawStdout(`${JSON.stringify(header)}\n`);
			}
		}

		await rebindSession();

		if (initialMessage) {
			await session.prompt(initialMessage, { images: initialImages });
		}

		for (const message of messages) {
			await session.prompt(message);
		}

		if (mode === "text") {
			const state = session.state;
			const lastMessage = state.messages[state.messages.length - 1];

			if (lastMessage?.role === "assistant") {
				const assistantMsg = lastMessage as AssistantMessage;
				if (assistantMsg.stopReason === "error" || assistantMsg.stopReason === "aborted") {
					console.error(assistantMsg.errorMessage || `Request ${assistantMsg.stopReason}`);
					exitCode = 1;
				} else {
					for (const content of assistantMsg.content) {
						if (content.type === "text") {
							writeRawStdout(`${content.text}\n`);
						}
					}
				}
			}
		}

		return exitCode;
	} catch (error: unknown) {
		console.error(error instanceof Error ? error.message : String(error));
		return 1;
	} finally {
		for (const cleanup of signalCleanupHandlers) {
			cleanup();
		}
		await disposeRuntime();
		await flushRawStdout();
	}
}
