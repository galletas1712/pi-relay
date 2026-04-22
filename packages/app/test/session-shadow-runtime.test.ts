import { EventEmitter } from "node:events";
import { mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	decodeSessionShadowBridgeMessage,
	encodeSessionShadowBridgeMessage,
	type SessionShadowBridgeMessage,
} from "../../coding-agent/src/core/session-shadow/codec.js";
import { createRelayRuntimeNoticeStore, type RelaySessionShadowState } from "../src/relay-runtime-host.js";
import {
	createRelaySessionShadowController,
	SESSION_CORE_HOST_FORCE_KILL_TIMEOUT_MS,
	SESSION_CORE_HOST_SHUTDOWN_TIMEOUT_MS,
} from "../src/session-shadow-runtime.js";

vi.mock("@pi-relay/coding-agent", async () => import("../../coding-agent/src/core/session-shadow/index.js"));

class FakeSessionShadowChild extends EventEmitter {
	stdin = new PassThrough();
	stdout = new PassThrough();
	stderr = new PassThrough();
	exitCode: number | null = null;
	signalCode: NodeJS.Signals | null = null;
	killed = false;
	readonly sent: SessionShadowBridgeMessage[] = [];
	readonly killSignals: NodeJS.Signals[] = [];
	private readonly closeOnDispose: boolean;
	private readonly killBehaviors: Array<"ignore" | "close">;
	private killAttempt = 0;

	constructor(options?: {
		closeOnDispose?: boolean;
		killBehaviors?: Array<"ignore" | "close">;
	}) {
		super();
		this.closeOnDispose = options?.closeOnDispose ?? true;
		this.killBehaviors = options?.killBehaviors ?? ["close"];
		this.stdin.on("data", (chunk) => {
			const lines = Buffer.from(chunk).toString("utf8").split("\n").filter(Boolean);
			for (const line of lines) {
				const frame = decodeSessionShadowBridgeMessage(line);
				this.sent.push(frame);
				if (frame.type !== "call") {
					continue;
				}

				this.stdout.write(
					encodeSessionShadowBridgeMessage({
						type: "result",
						id: frame.id,
						value: {
							acceptedCommand: frame.command.kind,
							acceptedAt: "2026-04-22T00:00:00.000Z",
						},
					}),
				);

				if (frame.command.kind === "dispose" && this.closeOnDispose) {
					this.exitCode = 0;
					queueMicrotask(() => {
						this.stdout.end();
						this.emit("close", 0, null);
					});
				}
			}
		});
	}

	kill = vi.fn((signal: NodeJS.Signals = "SIGTERM") => {
		this.killed = true;
		this.killSignals.push(signal);
		const behavior = this.killBehaviors[this.killAttempt] ?? this.killBehaviors[this.killBehaviors.length - 1] ?? "close";
		this.killAttempt += 1;
		if (behavior === "close") {
			queueMicrotask(() => {
				this.signalCode = signal;
				this.stdout.end();
				this.emit("close", null, signal);
			});
		}
		return true;
	});
}

describe("createRelaySessionShadowController", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		vi.useRealTimers();
		while (tempDirs.length > 0) {
			const dir = tempDirs.pop();
			if (dir) {
				rmSync(dir, { recursive: true, force: true });
			}
		}
	});

	function createRustDir(): string {
		const dir = join(tmpdir(), `pi-relay-session-shadow-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(dir, { recursive: true });
		tempDirs.push(dir);
		return dir;
	}

	it("records disconnect diagnostics while keeping TypeScript authoritative", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const noticeStore = createRelayRuntimeNoticeStore();
		const state: RelaySessionShadowState = {
			requestedMode: "rust-shadow" as const,
			effectiveMode: "disabled" as const,
			authority: "ts" as const,
			status: "disabled" as const,
		};
		const child = new FakeSessionShadowChild();
		const controller = createRelaySessionShadowController(
			{
				engineMode: "rust-shadow",
				diagnostics,
				noticeStore,
				state,
			},
			{
				resolveRustWorkspaceDir: () => createRustDir(),
				spawnHost: () => child as never,
			},
		);

		expect(controller).toBeDefined();
		await controller?.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});

		expect(state.status).toBe("running");
		await controller?.dispatch({
			type: "queue/enqueue-follow-up",
			text: "after tools",
		});

		child.stderr.write("shadow panic\n");
		child.exitCode = 7;
		child.stdout.end();
		child.emit("close", 7, null);
		await vi.waitFor(() => {
			expect(state.status).toBe("disconnected");
		});

		expect(diagnostics).toEqual(
			expect.arrayContaining([
				expect.objectContaining({
					type: "warning",
					message: expect.stringContaining("exited with code 7"),
				}),
			]),
		);
		expect(noticeStore.drain()).toEqual(
			expect.arrayContaining([
				expect.objectContaining({
					level: "warning",
					message: expect.stringContaining("TypeScript session authority"),
				}),
			]),
		);
	});

	it("sends dispose and marks the controller stopped during clean shutdown", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const state: RelaySessionShadowState = {
			requestedMode: "rust-shadow" as const,
			effectiveMode: "disabled" as const,
			authority: "ts" as const,
			status: "disabled" as const,
		};
		const child = new FakeSessionShadowChild();
		const controller = createRelaySessionShadowController(
			{
				engineMode: "rust-shadow",
				diagnostics,
				noticeStore: createRelayRuntimeNoticeStore(),
				state,
			},
			{
				resolveRustWorkspaceDir: () => createRustDir(),
				spawnHost: () => child as never,
			},
		);

		await controller?.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});
		await controller?.stop();

		expect(state.status).toBe("stopped");
		expect(child.kill).not.toHaveBeenCalled();
		expect(child.sent.at(-1)).toMatchObject({
			type: "call",
			command: {
				kind: "dispose",
			},
		});
	});

	it("escalates to SIGKILL when the host ignores the first shutdown signal", async () => {
		vi.useFakeTimers();
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const state: RelaySessionShadowState = {
			requestedMode: "rust-shadow" as const,
			effectiveMode: "disabled" as const,
			authority: "ts" as const,
			status: "disabled" as const,
		};
		const child = new FakeSessionShadowChild({
			closeOnDispose: false,
			killBehaviors: ["ignore", "close"],
		});
		const controller = createRelaySessionShadowController(
			{
				engineMode: "rust-shadow",
				diagnostics,
				noticeStore: createRelayRuntimeNoticeStore(),
				state,
			},
			{
				resolveRustWorkspaceDir: () => createRustDir(),
				spawnHost: () => child as never,
			},
		);

		await controller?.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});

		const stopPromise = controller?.stop();
		await vi.advanceTimersByTimeAsync(SESSION_CORE_HOST_SHUTDOWN_TIMEOUT_MS * 2 + SESSION_CORE_HOST_FORCE_KILL_TIMEOUT_MS);
		await stopPromise;

		expect(child.killSignals).toEqual(["SIGTERM", "SIGKILL"]);
		expect(diagnostics).toEqual(
			expect.arrayContaining([
				expect.objectContaining({
					type: "warning",
					message: expect.stringContaining("escalated to SIGKILL"),
				}),
			]),
		);
	});

	it("stops waiting after SIGKILL if the host never emits close", async () => {
		vi.useFakeTimers();
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const state: RelaySessionShadowState = {
			requestedMode: "rust-shadow" as const,
			effectiveMode: "disabled" as const,
			authority: "ts" as const,
			status: "disabled" as const,
		};
		const child = new FakeSessionShadowChild({
			closeOnDispose: false,
			killBehaviors: ["ignore", "ignore"],
		});
		const controller = createRelaySessionShadowController(
			{
				engineMode: "rust-shadow",
				diagnostics,
				noticeStore: createRelayRuntimeNoticeStore(),
				state,
			},
			{
				resolveRustWorkspaceDir: () => createRustDir(),
				spawnHost: () => child as never,
			},
		);

		await controller?.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});

		const stopPromise = controller?.stop();
		await vi.advanceTimersByTimeAsync(SESSION_CORE_HOST_SHUTDOWN_TIMEOUT_MS * 2 + SESSION_CORE_HOST_FORCE_KILL_TIMEOUT_MS);
		await stopPromise;

		expect(child.killSignals).toEqual(["SIGTERM", "SIGKILL"]);
		expect(diagnostics).toEqual(
			expect.arrayContaining([
				expect.objectContaining({
					type: "warning",
					message: expect.stringContaining("did not emit a close event"),
				}),
			]),
		);
	});
});
