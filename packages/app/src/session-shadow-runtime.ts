import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
	attachSessionShadowBridge,
	SessionShadowBridgeClient,
	type AgentSessionRuntimeDiagnostic,
	type SessionShadowBridgeController,
	type SessionShadowBridgeEvent,
} from "@pi-relay/coding-agent";
import type { RelayRuntimeEngineMode, RelayRuntimeNoticeStore, RelaySessionShadowState } from "./relay-runtime-host.js";

const SESSION_CORE_HOST_SHUTDOWN_TIMEOUT_MS = 1_000;
const MAX_CAPTURED_STDERR_LINES = 20;

export interface CreateRelaySessionShadowControllerOptions {
	engineMode: RelayRuntimeEngineMode;
	diagnostics: AgentSessionRuntimeDiagnostic[];
	noticeStore: RelayRuntimeNoticeStore;
	state: RelaySessionShadowState;
}

export interface RelaySessionShadowControllerDeps {
	spawnHost?: (
		command: string,
		args: string[],
		options: {
			cwd: string;
			stdio: ["pipe", "pipe", "pipe"];
		},
	) => ChildProcessWithoutNullStreams;
	resolveRustWorkspaceDir?: () => string;
}

function defaultResolveRustWorkspaceDir(): string {
	return resolve(fileURLToPath(new URL("../../../rust", import.meta.url)));
}

function delay(ms: number): Promise<void> {
	return new Promise((resolveDelay) => {
		setTimeout(resolveDelay, ms);
	});
}

function readLines(stream: NodeJS.ReadableStream, onLine: (line: string) => void): () => void {
	let buffer = "";

	const onData = (chunk: string | Buffer) => {
		buffer += typeof chunk === "string" ? chunk : chunk.toString("utf8");
		while (true) {
			const newlineIndex = buffer.indexOf("\n");
			if (newlineIndex === -1) {
				return;
			}
			const line = buffer.slice(0, newlineIndex).replace(/\r$/, "");
			buffer = buffer.slice(newlineIndex + 1);
			onLine(line);
		}
	};

	const onEnd = () => {
		const line = buffer.replace(/\r$/, "").trim();
		if (line.length > 0) {
			onLine(line);
		}
		buffer = "";
	};

	stream.on("data", onData);
	stream.on("end", onEnd);

	return () => {
		stream.off("data", onData);
		stream.off("end", onEnd);
	};
}

function formatError(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function pushDiagnostic(
	diagnostics: AgentSessionRuntimeDiagnostic[],
	noticeStore: RelayRuntimeNoticeStore,
	level: AgentSessionRuntimeDiagnostic["type"],
	message: string,
): void {
	diagnostics.push({ type: level, message });
	noticeStore.push({
		level: level === "warning" ? "warning" : level,
		message,
		source: "session-shadow",
	});
}

function applyShadowEvent(
	event: SessionShadowBridgeEvent,
	diagnostics: AgentSessionRuntimeDiagnostic[],
	noticeStore: RelayRuntimeNoticeStore,
	state: RelaySessionShadowState,
): void {
	if (event.type === "diagnostic") {
		pushDiagnostic(diagnostics, noticeStore, event.level === "warn" ? "warning" : event.level, event.message);
		if (event.level === "error") {
			state.lastError = event.message;
		}
		return;
	}

	if (event.type === "state_synced" || event.type === "command_applied") {
		state.status = "running";
		state.lastError = undefined;
	}
}

async function stopChildProcess(child: ChildProcessWithoutNullStreams, closePromise: Promise<void>): Promise<void> {
	const closedBeforeTimeout = await Promise.race([
		closePromise.then(() => true),
		delay(SESSION_CORE_HOST_SHUTDOWN_TIMEOUT_MS).then(() => false),
	]);
	if (closedBeforeTimeout) {
		return;
	}

	if (child.exitCode === null && child.signalCode === null) {
		child.kill();
	}
	await closePromise;
}

export function createRelaySessionShadowController(
	options: CreateRelaySessionShadowControllerOptions,
	deps: RelaySessionShadowControllerDeps = {},
): SessionShadowBridgeController | undefined {
	const { diagnostics, noticeStore, state } = options;
	state.authority = "ts";
	state.requestedMode = options.engineMode;

	if (options.engineMode === "legacy" || options.engineMode === "ts-core") {
		state.effectiveMode = "disabled";
		state.status = "disabled";
		state.lastError = undefined;
		return undefined;
	}

	if (options.engineMode === "rust") {
		pushDiagnostic(
			diagnostics,
			noticeStore,
			"warning",
			"PI_RELAY_SESSION_ENGINE=rust is not authoritative yet; continuing with TypeScript session authority and running the session-core bridge in shadow mode.",
		);
	}

	const rustWorkspaceDir = (deps.resolveRustWorkspaceDir ?? defaultResolveRustWorkspaceDir)();
	if (!existsSync(rustWorkspaceDir)) {
		const message = `Session shadow mode requested but Rust workspace was not found at ${rustWorkspaceDir}; continuing with TypeScript session authority.`;
		state.effectiveMode = "disabled";
		state.status = "disabled";
		state.lastError = message;
		pushDiagnostic(diagnostics, noticeStore, "warning", message);
		return undefined;
	}

	const spawnHost =
		deps.spawnHost ??
		((command, args, spawnOptions) =>
			spawn(command, args, {
				cwd: spawnOptions.cwd,
				stdio: spawnOptions.stdio,
			}));
	const child = spawnHost("cargo", ["run", "--quiet", "-p", "session-core-host"], {
		cwd: rustWorkspaceDir,
		stdio: ["pipe", "pipe", "pipe"],
	});

	state.effectiveMode = "shadow";
	state.status = "starting";
	state.lastError = undefined;

	const stderrLines: string[] = [];
	const detachStderr = readLines(child.stderr, (line) => {
		const trimmed = line.trim();
		if (trimmed.length === 0) {
			return;
		}
		stderrLines.push(trimmed);
		if (stderrLines.length > MAX_CAPTURED_STDERR_LINES) {
			stderrLines.shift();
		}
	});

	let stopPromise: Promise<void> | undefined;
	let stopped = false;
	let disconnectReported = false;

	const closePromise = new Promise<void>((resolveClose) => {
		child.once("close", (code, signal) => {
			detachStderr();
			if (!stopped && !disconnectReported) {
				const suffix = stderrLines.length > 0 ? ` Stderr: ${stderrLines.join(" | ")}` : "";
				const reason = code !== null ? `exited with code ${code}` : signal ? `was terminated by ${signal}` : "disconnected";
				const message = `Session-core shadow host ${reason}; continuing with TypeScript session authority.${suffix}`;
				state.status = "disconnected";
				state.lastError = message;
				disconnectReported = true;
				pushDiagnostic(diagnostics, noticeStore, "warning", message);
			}
			resolveClose();
		});
	});

	child.once("error", (error) => {
		if (stopped || disconnectReported) {
			return;
		}
		const message = `Failed to launch session-core shadow host: ${formatError(error)}`;
		state.status = "disconnected";
		state.lastError = message;
		disconnectReported = true;
		pushDiagnostic(diagnostics, noticeStore, "warning", message);
	});

	const observingClient = SessionShadowBridgeClient.fromChildProcess(child, {
		onEvent: (event) => applyShadowEvent(event, diagnostics, noticeStore, state),
		onDisconnect: () => {
			if (stopped) {
				return;
			}
			state.status = "disconnected";
			state.lastError = "Session-core shadow host disconnected; continuing with TypeScript session authority.";
		},
	});
	const observingBridge = attachSessionShadowBridge(observingClient);

	return {
		async start(initialState) {
			if (stopped || disconnectReported) {
				return;
			}
			state.status = "starting";
			try {
				await observingBridge.start(initialState);
				state.status = "running";
				state.lastError = undefined;
			} catch (error) {
				const message = `Failed to initialize session-core shadow bridge: ${formatError(error)}. Continuing with TypeScript session authority.`;
				state.status = "disconnected";
				state.lastError = message;
				disconnectReported = true;
				pushDiagnostic(diagnostics, noticeStore, "warning", message);
			}
		},

		async dispatch(command) {
			if (stopped || disconnectReported) {
				return;
			}
			try {
				await observingBridge.dispatch(command);
			} catch (error) {
				const message = `Session-core shadow bridge disconnected while mirroring ${command.type}; continuing with TypeScript session authority.`;
				state.status = "disconnected";
				state.lastError = `${message} ${formatError(error)}`;
				disconnectReported = true;
				pushDiagnostic(diagnostics, noticeStore, "warning", `${message} ${formatError(error)}`);
			}
		},

		async flush() {
			if (stopped || disconnectReported) {
				return;
			}
			try {
				await observingBridge.flush();
			} catch {
				// flush is best-effort for shadow-mode observability only
			}
		},

		async stop() {
			if (stopPromise) {
				return stopPromise;
			}

			stopped = true;
			state.status = "stopped";
			stopPromise = (async () => {
				try {
					await observingBridge.stop();
				} catch (error) {
					if (!disconnectReported) {
						pushDiagnostic(
							diagnostics,
							noticeStore,
							"warning",
							`Session-core shadow shutdown encountered an error: ${formatError(error)}`,
						);
					}
				} finally {
					observingClient.close();
					await stopChildProcess(child, closePromise);
				}
			})();
			return stopPromise;
		},
	};
}
